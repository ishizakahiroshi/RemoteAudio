//! Per-user Windows logon autostart, plus the inbound firewall rule that makes
//! the server reachable from the LAN.
//!
//! Two separate privileges are involved and they must not be bundled: the HKCU
//! Run value needs none, the firewall rule needs elevation. So installing does
//! the registry work first and unconditionally, then asks for the one UAC
//! prompt. Declining the prompt costs the LAN rule, not the autostart.
//!
//! The Microsoft Store build is a third case again, and neither half applies to
//! it — see [`packaged`].

use std::io;
use std::path::Path;

pub const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const VALUE_NAME: &str = "AudioRemote";

/// Name of the inbound firewall rule. Deliberately the same string as
/// [`VALUE_NAME`]: uninstall has to find both without reading any config.
pub const FIREWALL_RULE_NAME: &str = "AudioRemote";

/// Network profiles the inbound rule covers. Public is left out on purpose —
/// this is a home/office LAN tool, and a coffee-shop network has no business
/// reaching the host even though non-loopback clients still need a token.
const FIREWALL_PROFILES: &str = "private,domain";

/// The port the MSIX build's inbound rule covers.
///
/// A package manifest cannot read `config.toml`, so its `windows.firewallRules`
/// declaration (`packaging/msix/AppxManifest.xml`) is pinned to one number, and
/// that number has to be the default port. Move the server off it with
/// `RemoteAudio setup` and the Store build needs a hand-added rule — which is
/// only findable if the app says so, hence this constant existing at all.
///
/// Kept honest by `packaged_firewall_port_matches_the_default` below; the
/// manifest side is a static file, so the pairing is checked there by eye.
pub const PACKAGED_FIREWALL_PORT: u16 = 17650;

/// Build the exact command stored in the Run value.
///
/// `supervise`, not `serve`: from v0.2 the thing that survives a logon is the
/// supervisor, which owns the server and restarts it. It passes `--no-open` to
/// its child, so signing in still does not pop a browser tab.
pub fn command_value(exe: &Path) -> String {
    format!("\"{}\" supervise", exe.display())
}

/// Whether this process was started from an MSIX package (the Store build).
///
/// Three things follow from it, and all three are the opposite of what the
/// portable build does:
///
/// 1. **Logon startup is not ours to install.** A Run value written from inside
///    a package lands in the package's own registry hive, and the logon path
///    never reads it — it would look like it worked and do nothing. The package
///    manifest declares `windows.startupTask` instead, which is also what puts
///    the on/off switch in Task Manager where users expect to find it.
/// 2. **We cannot elevate ourselves.** `elevate_self` re-launches
///    `current_exe()`, which here is under `%ProgramFiles%\WindowsApps\` — a
///    directory whose ACL exists precisely to stop that.
/// 3. **The inbound rule is not ours to add either.** That same path carries the
///    package version, so a program-scoped `netsh` rule would silently stop
///    matching the first time the Store ships a new build. The package declares
///    `windows.firewallRules` instead, which Windows binds to the package and
///    keeps across updates — see `packaging/msix/AppxManifest.xml`.
///
/// `GetCurrentPackageFullName` is the documented test: it answers
/// `APPMODEL_ERROR_NO_PACKAGE` when the process has no package identity, and
/// `ERROR_INSUFFICIENT_BUFFER` when it has one (a zero-length buffer can never
/// succeed, so anything else means "packaged").
#[cfg(windows)]
pub fn packaged() -> bool {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::APPMODEL_ERROR_NO_PACKAGE;
    use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;

    let mut len: u32 = 0;
    let status = unsafe { GetCurrentPackageFullName(&mut len, PWSTR::null()) };
    status != APPMODEL_ERROR_NO_PACKAGE
}

#[cfg(not(windows))]
pub fn packaged() -> bool {
    false
}

/// What [`install`] did about starting at logon.
pub enum Installed {
    /// The HKCU Run value was written. Carries the exact command stored.
    RunValue(String),
    /// Nothing was written, and nothing needed to be: this is the MSIX build,
    /// where logon startup comes from the package manifest. See [`packaged`].
    PackagedStartupTask,
}

/// The command a user has to run by hand when they decline the UAC prompt (or,
/// in the Store build, instead of one we cannot raise). Printed verbatim, so it
/// has to be pasteable as-is.
///
/// `program` is `None` for the Store build. Scoping the rule to the port alone
/// is wider than naming an executable, but the alternative is a rule that stops
/// matching at the next package update and takes the LAN with it, silently. The
/// exposure it adds is bounded: the rule still skips public networks, and the
/// API behind the port refuses every non-loopback request without a bearer
/// token.
pub fn firewall_command_hint(program: Option<&Path>, port: u16) -> String {
    let program = match program {
        Some(exe) => format!(" program=\"{}\"", exe.display()),
        None => String::new(),
    };
    format!(
        "netsh advfirewall firewall add rule name=\"{FIREWALL_RULE_NAME}\" dir=in action=allow\
         {program} enable=yes profile={FIREWALL_PROFILES} protocol=TCP localport={port}"
    )
}

#[cfg(windows)]
fn registry_error(code: windows::Win32::Foundation::WIN32_ERROR) -> io::Error {
    io::Error::from_raw_os_error(code.0 as i32)
}

#[cfg(windows)]
pub fn install(exe: &Path) -> io::Result<Installed> {
    use std::slice;

    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyW, RegSetValueExW, HKEY_CURRENT_USER, REG_SZ,
    };

    // Guarded here rather than at the call sites: a Run value written from
    // inside a package is accepted, stored somewhere private, and never read.
    // Every future caller gets the same answer without having to know why.
    if packaged() {
        return Ok(Installed::PackagedStartupTask);
    }

    if !exe.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "current executable path is not absolute",
        ));
    }

    let value = command_value(exe);
    let data: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = unsafe { slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2) };

    let mut key = HKEY_CURRENT_USER;
    let status = unsafe {
        RegCreateKeyW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
            &mut key,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(registry_error(status));
    }

    let status = unsafe { RegSetValueExW(key, w!("AudioRemote"), 0, REG_SZ, Some(bytes)) };
    let close_status = unsafe { RegCloseKey(key) };
    if status != ERROR_SUCCESS {
        return Err(registry_error(status));
    }
    if close_status != ERROR_SUCCESS {
        return Err(registry_error(close_status));
    }
    Ok(Installed::RunValue(value))
}

/// Remove the logon entry. A missing value is success — uninstall is idempotent.
///
/// In the Store build there is nothing to remove and nothing we *could* remove:
/// the delete would be redirected into the package hive, leaving any real value
/// a portable install left behind untouched while reporting success. Startup is
/// switched off from Task Manager there instead.
#[cfg(windows)]
pub fn uninstall() -> io::Result<()> {
    use windows::core::w;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, HKEY_CURRENT_USER, KEY_SET_VALUE,
    };

    if packaged() {
        return Ok(());
    }

    let mut key = HKEY_CURRENT_USER;
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
            0,
            KEY_SET_VALUE,
            &mut key,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    if status != ERROR_SUCCESS {
        return Err(registry_error(status));
    }

    let status = unsafe { RegDeleteValueW(key, w!("AudioRemote")) };
    let close_status = unsafe { RegCloseKey(key) };
    if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
        return Err(registry_error(status));
    }
    if close_status != ERROR_SUCCESS {
        return Err(registry_error(close_status));
    }
    Ok(())
}

// ---- firewall ---------------------------------------------------------------

/// What came of asking for elevation.
#[derive(Debug)]
pub enum Elevation {
    /// The elevated helper ran and reported success.
    Done,
    /// The user dismissed the UAC prompt. Not an error — the caller carries on
    /// and tells them how to do it by hand later.
    Declined,
    /// It could not be started, or it ran and failed.
    Failed(String),
}

/// Replace the inbound rule with one for `port`, scoped to `program` when there
/// is a stable path to scope it to (see [`firewall_command_hint`]).
///
/// **Must already be running elevated** — this is the body of the
/// `--firewall-install` helper, not something to call from a normal session.
/// Deletes first because `netsh ... add rule` happily stacks a second rule with
/// the same name, and a stale rule pointing at an old exe path is exactly the
/// "it worked yesterday" failure this is meant to avoid.
#[cfg(windows)]
pub fn firewall_install(program: Option<&Path>, port: u16) -> io::Result<()> {
    let _ = netsh(&[
        "advfirewall",
        "firewall",
        "delete",
        "rule",
        &format!("name={FIREWALL_RULE_NAME}"),
    ]);

    let mut args: Vec<String> = vec![
        "advfirewall".into(),
        "firewall".into(),
        "add".into(),
        "rule".into(),
        format!("name={FIREWALL_RULE_NAME}"),
        "dir=in".into(),
        "action=allow".into(),
    ];
    if let Some(exe) = program {
        // No quoting: this goes into argv, not through a shell, so a path with
        // spaces arrives as one argument already.
        args.push(format!("program={}", exe.display()));
    }
    args.extend([
        "enable=yes".into(),
        format!("profile={FIREWALL_PROFILES}"),
        "protocol=TCP".into(),
        format!("localport={port}"),
    ]);

    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let status = netsh(&borrowed)?;

    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("netsh exited with {status}")))
    }
}

/// Remove the inbound rule. Must already be running elevated. A missing rule is
/// success: uninstall has to be idempotent.
#[cfg(windows)]
pub fn firewall_uninstall() -> io::Result<()> {
    // netsh returns a non-zero status when nothing matched, which is the normal
    // case for a second uninstall — so the status is deliberately not checked.
    let _ = netsh(&[
        "advfirewall",
        "firewall",
        "delete",
        "rule",
        &format!("name={FIREWALL_RULE_NAME}"),
    ])?;
    Ok(())
}

#[cfg(windows)]
fn netsh(args: &[&str]) -> io::Result<std::process::ExitStatus> {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW: without it a console flashes on screen for each call,
    // which is the one thing the resident build is trying to stop doing.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("netsh")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
}

/// Re-run this same exe elevated with `args`, and wait for it to finish.
///
/// Elevating our own binary rather than `netsh` directly is the honest option:
/// the UAC dialog names AudioRemote, which is what is actually asking. It also
/// keeps the netsh arguments in argv instead of a hand-quoted command line, and
/// lets the helper report failure through its exit code.
#[cfg(windows)]
pub fn elevate_self(args: &[&str]) -> Elevation {
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return Elevation::Failed(format!("cannot locate this executable: {e}")),
    };

    // Both buffers have to outlive the call — `PCWSTR` does not own anything.
    let file = wide(&exe.to_string_lossy());
    let parameters = wide(&args.join(" "));

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };

    if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
        // 1223 is what a dismissed UAC prompt looks like. It is a decision, not
        // a fault, so it must not be reported as one.
        return if e.code() == windows::core::HRESULT::from_win32(ERROR_CANCELLED.0) {
            Elevation::Declined
        } else {
            Elevation::Failed(e.to_string())
        };
    }

    unsafe { WaitForSingleObject(info.hProcess, INFINITE) };
    let mut code = 0u32;
    let read = unsafe { GetExitCodeProcess(info.hProcess, &mut code) };
    unsafe {
        let _ = CloseHandle(info.hProcess);
    }

    match read {
        Ok(()) if code == 0 => Elevation::Done,
        Ok(()) => Elevation::Failed(format!("the elevated helper exited with {code}")),
        Err(e) => Elevation::Failed(format!("cannot read the helper's exit code: {e}")),
    }
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(not(windows))]
pub fn firewall_install(_program: Option<&Path>, _port: u16) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "firewall rules are supported on Windows only",
    ))
}

#[cfg(not(windows))]
pub fn firewall_uninstall() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "firewall rules are supported on Windows only",
    ))
}

#[cfg(not(windows))]
pub fn elevate_self(_args: &[&str]) -> Elevation {
    Elevation::Failed("elevation is supported on Windows only".to_string())
}

#[cfg(not(windows))]
pub fn install(_exe: &Path) -> io::Result<Installed> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "autostart is supported on Windows only",
    ))
}

#[cfg(not(windows))]
pub fn uninstall() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "autostart is supported on Windows only",
    ))
}

#[cfg(test)]
mod tests {
    use super::{command_value, firewall_command_hint, FIREWALL_RULE_NAME};
    use std::path::Path;

    #[test]
    fn command_quotes_paths_with_spaces_and_unicode() {
        let path = Path::new(r"C:\Tools\音声 Remote\RemoteAudio.exe");
        assert_eq!(
            command_value(path),
            r#""C:\Tools\音声 Remote\RemoteAudio.exe" supervise"#
        );
    }

    #[test]
    fn command_keeps_argument_boundary() {
        assert_eq!(
            command_value(Path::new(r"C:\RemoteAudio.exe")),
            r#""C:\RemoteAudio.exe" supervise"#
        );
    }

    #[test]
    fn logon_starts_the_supervisor_not_a_bare_server() {
        // v0.1 registered `serve --no-open` here. A logon entry that starts the
        // server directly cannot restart it after a crash, which is the whole
        // reason the supervisor exists — so the day this reverts, it fails.
        let value = command_value(Path::new(r"C:\RemoteAudio.exe"));
        assert!(value.ends_with(" supervise"), "{value}");
        assert!(!value.contains("serve"), "{value}");
    }

    #[test]
    fn the_manual_firewall_hint_is_pasteable() {
        let hint =
            firewall_command_hint(Some(Path::new(r"C:\Program Files\RemoteAudio.exe")), 17650);
        // The path has a space in it, so an unquoted `program=` would silently
        // register a rule for `C:\Program`.
        assert!(
            hint.contains(r#"program="C:\Program Files\RemoteAudio.exe""#),
            "{hint}"
        );
        assert!(
            hint.contains(&format!("name=\"{FIREWALL_RULE_NAME}\"")),
            "{hint}"
        );
        assert!(hint.contains("localport=17650"), "{hint}");
        // Public networks stay closed.
        assert!(!hint.contains("public"), "{hint}");
    }

    /// The MSIX manifest pins its inbound rule to one port and cannot read the
    /// config, so the default port is the only one it can be right about. If the
    /// default ever moves, `packaging/msix/AppxManifest.xml` has to move with it
    /// — this is what makes that impossible to forget.
    #[cfg(windows)]
    #[test]
    fn packaged_firewall_port_matches_the_default() {
        assert_eq!(
            super::PACKAGED_FIREWALL_PORT,
            crate::config::Config::default().server.port
        );
    }

    #[test]
    fn the_store_firewall_hint_names_no_executable() {
        // The MSIX exe path carries the package version, so naming it would
        // produce a rule that stops matching at the next Store update.
        let hint = firewall_command_hint(None, 17650);
        assert!(!hint.contains("program="), "{hint}");
        assert!(hint.contains("localport=17650"), "{hint}");
        assert!(hint.contains("action=allow enable=yes"), "{hint}");
        assert!(!hint.contains("public"), "{hint}");
    }
}
