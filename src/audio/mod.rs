//! Windows Core Audio wrapper.
//!
//! Public API kept small on purpose so the HTTP server does not need to know
//! about COM or unsafe Windows API details.

use windows::core::PWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{BOOL, E_INVALIDARG, E_UNEXPECTED};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eCommunications, eConsole, eMultimedia, eRender, ERole, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, DEVICE_STATE, DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED,
    DEVICE_STATE_NOTPRESENT, DEVICE_STATE_UNPLUGGED,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Diagnostics::Debug::Beep;

mod policyconfig;

/// One playback endpoint on the host.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub state: DeviceState,
    pub is_default_console: bool,
    pub is_default_multimedia: bool,
    pub is_default_communications: bool,
}

/// Master volume state for the current default Multimedia render endpoint.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MasterVolume {
    pub device_id: String,
    pub level: f32,
    pub muted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceState {
    Active,
    Unplugged,
    Disabled,
    NotPresent,
    Unknown,
}

impl DeviceState {
    fn from_raw(raw: DEVICE_STATE) -> Self {
        match raw {
            DEVICE_STATE_ACTIVE => Self::Active,
            DEVICE_STATE_UNPLUGGED => Self::Unplugged,
            DEVICE_STATE_DISABLED => Self::Disabled,
            DEVICE_STATE_NOTPRESENT => Self::NotPresent,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Unplugged => "unplugged",
            Self::Disabled => "disabled",
            Self::NotPresent => "not_present",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug)]
pub struct AudioError {
    pub context: &'static str,
    pub source: windows::core::Error,
}

const INVALID_VOLUME_LEVEL_CONTEXT: &str = "invalid master volume level";
/// Raised when the three role defaults do not all point at the requested device
/// after a switch (see `verify_all_roles`).
const ROLE_SPLIT_CONTEXT: &str = "default endpoint roles diverged after switch";

impl AudioError {
    pub fn is_invalid_input(&self) -> bool {
        self.context == INVALID_VOLUME_LEVEL_CONTEXT
    }

    /// True for the "roles ended up split" failure. The HTTP layer reports it as
    /// a conflict rather than a generic internal error because the request was
    /// well-formed — the host state simply did not settle where it was asked to.
    pub fn is_role_split(&self) -> bool {
        self.context == ROLE_SPLIT_CONTEXT
    }
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.context, self.source)
    }
}
impl std::error::Error for AudioError {}

type Result<T> = std::result::Result<T, AudioError>;

fn wrap<T>(ctx: &'static str, r: windows::core::Result<T>) -> Result<T> {
    r.map_err(|e| AudioError {
        context: ctx,
        source: e,
    })
}

/// RAII guard for CoInitializeEx / CoUninitialize (MTA). Nested creation is
/// safe: Windows refcounts CoInitialize per thread.
struct ComGuard;

impl ComGuard {
    fn new() -> Result<Self> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            if hr.is_err() {
                return Err(AudioError {
                    context: "CoInitializeEx",
                    source: windows::core::Error::from_hresult(hr),
                });
            }
        }
        Ok(Self)
    }
}
impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}

fn create_enumerator() -> Result<IMMDeviceEnumerator> {
    wrap("CoCreateInstance(MMDeviceEnumerator)", unsafe {
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
    })
}

/// Owning guard for a `PWSTR` that Windows allocated with `CoTaskMemAlloc`
/// (`IMMDevice::GetId` is the one we use). windows-rs hands back the raw pointer
/// without taking ownership, so **the caller must free it** — dropping this
/// guard is the only place that happens. Wrap the pointer the moment it is
/// obtained so early returns and conversion failures cannot leak it.
struct CoTaskString(PWSTR);

impl CoTaskString {
    /// Copy the NUL-terminated UTF-16 buffer into an owned `String`. Empty when
    /// the pointer is null.
    fn to_rust_string(&self) -> String {
        if self.0.is_null() {
            return String::new();
        }
        unsafe {
            let mut len = 0usize;
            while *self.0 .0.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(self.0 .0, len))
        }
    }
}

impl Drop for CoTaskString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CoTaskMemFree(Some(self.0 .0 as *const std::ffi::c_void)) };
            self.0 = PWSTR::null();
        }
    }
}

fn device_id(dev: &IMMDevice) -> Result<String> {
    let raw = CoTaskString(wrap("IMMDevice::GetId", unsafe { dev.GetId() })?);
    Ok(raw.to_rust_string())
}

fn device_state(dev: &IMMDevice) -> Result<DEVICE_STATE> {
    wrap("IMMDevice::GetState", unsafe { dev.GetState() })
}

fn device_friendly_name(dev: &IMMDevice) -> Result<String> {
    let store = wrap("IMMDevice::OpenPropertyStore", unsafe {
        dev.OpenPropertyStore(STGM_READ)
    })?;
    let prop = wrap("IPropertyStore::GetValue(FriendlyName)", unsafe {
        store.GetValue(&PKEY_Device_FriendlyName)
    })?;
    // windows-core 0.58 exposes PROPVARIANT as an opaque type; its Display impl
    // formats VT_LPWSTR / VT_BSTR as the underlying string. Non-string variants
    // would render as "<vt=N>" which is fine for a fallback label.
    Ok(prop.to_string())
}

/// Enumerate all endpoints (active / unplugged / disabled) and mark which one
/// is the current default per ERole.
pub fn list_devices() -> Result<Vec<AudioDevice>> {
    let _com = ComGuard::new()?;
    let enumerator = create_enumerator()?;

    let mask = DEVICE_STATE_ACTIVE.0 | DEVICE_STATE_UNPLUGGED.0 | DEVICE_STATE_DISABLED.0;
    let collection = wrap("EnumAudioEndpoints", unsafe {
        enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE(mask))
    })?;
    let count = wrap("IMMDeviceCollection::GetCount", unsafe {
        collection.GetCount()
    })?;

    let default_ids = current_defaults(&enumerator);

    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let dev = wrap("IMMDeviceCollection::Item", unsafe { collection.Item(i) })?;
        let id = device_id(&dev)?;
        let name = device_friendly_name(&dev).unwrap_or_else(|_| "(unnamed)".to_string());
        let state = DeviceState::from_raw(device_state(&dev)?);
        out.push(AudioDevice {
            is_default_console: default_ids.console.as_deref() == Some(&id),
            is_default_multimedia: default_ids.multimedia.as_deref() == Some(&id),
            is_default_communications: default_ids.communications.as_deref() == Some(&id),
            id,
            name,
            state,
        });
    }
    Ok(out)
}

#[derive(Default)]
struct DefaultIds {
    console: Option<String>,
    multimedia: Option<String>,
    communications: Option<String>,
}

fn current_defaults(enumerator: &IMMDeviceEnumerator) -> DefaultIds {
    fn one(enumerator: &IMMDeviceEnumerator, role: ERole) -> Option<String> {
        unsafe {
            let dev = enumerator.GetDefaultAudioEndpoint(eRender, role).ok()?;
            let id = CoTaskString(dev.GetId().ok()?);
            Some(id.to_rust_string())
        }
    }
    DefaultIds {
        console: one(enumerator, eConsole),
        multimedia: one(enumerator, eMultimedia),
        communications: one(enumerator, eCommunications),
    }
}

/// Switch the default output device for all three roles (Console / Multimedia
/// / Communications) at once. Uses non-public IPolicyConfig; tries Win10/11
/// IID first, falls back to the Vista IID.
///
/// Callers must serialize this against every other audio operation — see
/// `server::AudioGate`. `IPolicyConfig` sets the roles one at a time, so two
/// interleaved switches can leave the roles pointing at different devices;
/// `verify_all_roles` turns that into an error instead of a silent success.
pub fn set_default(device_id: &str) -> Result<()> {
    let _com = ComGuard::new()?;
    policyconfig::set_default_for_all_roles(device_id).map_err(|e| AudioError {
        context: "IPolicyConfig::SetDefaultEndpoint",
        source: e,
    })?;
    verify_all_roles(device_id)
}

/// Re-read the three role defaults and confirm they all point at `device_id`.
/// The non-negotiable contract of this app is that the roles move together, so a
/// split (partial `SetDefaultEndpoint` failure, a device that Windows refused to
/// adopt, or a concurrent switch that slipped through) is reported as a failure.
fn verify_all_roles(device_id: &str) -> Result<()> {
    let enumerator = create_enumerator()?;
    let actual = current_defaults(&enumerator);
    let settled = |role: &Option<String>| {
        role.as_deref()
            // Endpoint IDs are compared case-insensitively: the string Windows
            // hands back is stable, but the one the client echoes back to us
            // came through a URL path and may have been re-cased on the way.
            .map(|id| id.eq_ignore_ascii_case(device_id))
            .unwrap_or(false)
    };
    if settled(&actual.console) && settled(&actual.multimedia) && settled(&actual.communications) {
        Ok(())
    } else {
        Err(AudioError {
            context: ROLE_SPLIT_CONTEXT,
            source: windows::core::Error::from_hresult(E_UNEXPECTED),
        })
    }
}

fn invalid_volume_level() -> AudioError {
    AudioError {
        context: INVALID_VOLUME_LEVEL_CONTEXT,
        source: windows::core::Error::from_hresult(E_INVALIDARG),
    }
}

fn validate_volume_level(level: f32) -> Result<()> {
    if level.is_finite() && (0.0..=1.0).contains(&level) {
        Ok(())
    } else {
        Err(invalid_volume_level())
    }
}

fn default_multimedia_volume() -> Result<(String, IAudioEndpointVolume)> {
    let enumerator = create_enumerator()?;
    let device = wrap("GetDefaultAudioEndpoint(eRender, eMultimedia)", unsafe {
        enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)
    })?;
    let id = device_id(&device)?;
    let endpoint = wrap("IMMDevice::Activate(IAudioEndpointVolume)", unsafe {
        device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
    })?;
    Ok((id, endpoint))
}

fn read_master_volume(device_id: String, endpoint: &IAudioEndpointVolume) -> Result<MasterVolume> {
    let level = wrap("IAudioEndpointVolume::GetMasterVolumeLevelScalar", unsafe {
        endpoint.GetMasterVolumeLevelScalar()
    })?;
    let muted = wrap("IAudioEndpointVolume::GetMute", unsafe {
        endpoint.GetMute()
    })?
    .0 != 0;
    Ok(MasterVolume {
        device_id,
        level,
        muted,
    })
}

/// Read the master level and mute state from the default Multimedia endpoint.
pub fn get_master_volume() -> Result<MasterVolume> {
    let _com = ComGuard::new()?;
    let (id, endpoint) = default_multimedia_volume()?;
    read_master_volume(id, &endpoint)
}

/// Apply exactly the master-volume fields the caller passed and return the
/// resulting state. `None` means "leave this one alone" — the HTTP layer relies
/// on that to avoid writing back a value it never intended to change.
pub fn update_master_volume(level: Option<f32>, muted: Option<bool>) -> Result<MasterVolume> {
    if level.is_none() && muted.is_none() {
        return Err(invalid_volume_level());
    }
    if let Some(level) = level {
        validate_volume_level(level)?;
    }

    let _com = ComGuard::new()?;
    let (id, endpoint) = default_multimedia_volume()?;
    if let Some(level) = level {
        wrap("IAudioEndpointVolume::SetMasterVolumeLevelScalar", unsafe {
            endpoint.SetMasterVolumeLevelScalar(level, std::ptr::null())
        })?;
    }
    if let Some(muted) = muted {
        wrap("IAudioEndpointVolume::SetMute", unsafe {
            endpoint.SetMute(BOOL(muted as i32), std::ptr::null())
        })?;
    }
    read_master_volume(id, &endpoint)
}

/// Play a short, host-side confirmation tone through Windows' current default
/// output. The HTTP layer runs this through `AudioGate`, just like every other
/// audio operation, so a test tone cannot overlap a device switch.
pub fn play_test_sound() -> Result<()> {
    wrap("Beep(test sound)", unsafe { Beep(880, 180) })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Everything here is deliberately COM-free: these run on any Windows box,
    // including CI runners with no real audio endpoint.

    #[test]
    fn volume_level_bounds_are_inclusive() {
        for ok in [0.0, 0.5, 1.0] {
            assert!(validate_volume_level(ok).is_ok(), "{ok}");
        }
        for bad in [-0.001, 1.001, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(validate_volume_level(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn invalid_level_is_the_only_client_error() {
        assert!(invalid_volume_level().is_invalid_input());
        assert!(!invalid_volume_level().is_role_split());

        let split = AudioError {
            context: ROLE_SPLIT_CONTEXT,
            source: windows::core::Error::from_hresult(E_UNEXPECTED),
        };
        assert!(split.is_role_split());
        assert!(!split.is_invalid_input());
    }

    #[test]
    fn device_state_maps_every_known_mask() {
        assert_eq!(
            DeviceState::from_raw(DEVICE_STATE_ACTIVE),
            DeviceState::Active
        );
        assert_eq!(
            DeviceState::from_raw(DEVICE_STATE_UNPLUGGED),
            DeviceState::Unplugged
        );
        assert_eq!(
            DeviceState::from_raw(DEVICE_STATE_DISABLED),
            DeviceState::Disabled
        );
        assert_eq!(
            DeviceState::from_raw(DEVICE_STATE_NOTPRESENT),
            DeviceState::NotPresent
        );
        assert_eq!(
            DeviceState::from_raw(DEVICE_STATE(0xff)),
            DeviceState::Unknown
        );
    }

    #[test]
    fn null_cotask_string_is_empty_and_frees_nothing() {
        // Exercises the null branch of the RAII guard: no CoTaskMemFree call and
        // no panic on drop.
        let guard = CoTaskString(PWSTR::null());
        assert_eq!(guard.to_rust_string(), "");
    }
}
