//! HTTP server: JSON API + embedded Web UI, plus the guards that make a
//! LAN-facing default bind defensible.
//!
//! Request path, outermost first:
//!
//! 1. `security_headers` — frame / sniffing / referrer headers on **every**
//!    response, static assets and error replies included.
//! 2. `network_guard` — CIDR allowlist and `Host` allowlist, also on every
//!    request. Applied outside the router so it covers the static fallback too.
//! 3. `api_middleware` — same-origin enforcement for state changes, then bearer
//!    auth. Only wraps the `/api/*` routes; the Web UI shell itself is public so
//!    a LAN client can load the page and *then* paste a token.
//! 4. Handler. Every Core Audio touch goes through [`AudioGate`].

use std::collections::{HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, Path as AxumPath, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::about;
use crate::assets::WebAssets;
use crate::audio::{self, AudioDevice};
use crate::auth::AuthState;
use crate::config::{self, Config, SortPolicy};
use crate::diagnostics::{Diagnostics, Marker, Operation, Phase, StopReason};
use crate::history::History;
use crate::net;

/// Content-Security-Policy for the whole surface. `frame-ancestors 'none'` is the
/// clickjacking half; the rest keeps the embedded UI to first-party assets.
/// Deliberately without `'unsafe-inline'` — the UI carries no inline `<script>`
/// and no inline `style` attributes, and it should stay that way.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
img-src 'self' data:; connect-src 'self'; font-src 'self'; base-uri 'none'; \
form-action 'none'; frame-ancestors 'none'; object-src 'none'";

/// Longest a request waits for the audio gate before giving up. Real Core Audio
/// calls take single-digit milliseconds, so a wait this long means something is
/// wedged — and a fast 503 beats a queue of stuck clients.
const AUDIO_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How many audio calls may be queued on the gate before further requests are
/// turned away. Sized for the real client mix: one page load fires three calls,
/// so this leaves room for several clients polling at once without letting a
/// request flood spawn an unbounded number of blocking threads.
const MAX_QUEUED_AUDIO_CALLS: usize = 16;

/// Head start the `202 Accepted` gets before the restart request goes out.
///
/// Long enough for a LAN round trip on a wire this app already assumes is
/// local, short enough that nobody notices the pause.
const RESTART_GRACE: Duration = Duration::from_millis(300);

#[derive(Clone)]
pub struct AppState {
    /// Startup snapshot. Everything here needs a restart to change (bind, port,
    /// `allowed_networks`, `device_sort`) — tokens live in `auth` instead.
    pub config: Arc<Config>,
    /// Live token set, re-read from `config.toml` when the CLI edits it.
    pub auth: Arc<AuthState>,
    pub audio: AudioGate,
    pub history: Arc<Mutex<History>>,
    pub history_path: Arc<PathBuf>,
    pub diagnostics: Arc<std::sync::Mutex<Diagnostics>>,
    pub diagnostic_operations: Arc<std::sync::Mutex<OperationQueue>>,
    /// AR-02 rebinding guard: allowed `Host` header values, built once at
    /// startup from loopback names + current LAN IPs (see `net::build_allowed_hosts`).
    pub allowed_hosts: Arc<HashSet<String>>,
    /// Whether a supervisor is watching this process. `POST /api/restart` is
    /// only answerable when it is — without a parent to start us again, honouring
    /// the request would take the host offline until somebody walks over to it.
    pub supervised: bool,
}

/// Serializes every Core Audio operation and keeps them off the async workers.
///
/// Two problems, one gate:
///
/// * `IPolicyConfig::SetDefaultEndpoint` changes the three role defaults one at
///   a time. Two overlapping switches interleave, and both can report success
///   while Console / Multimedia / Communications end up on different devices —
///   exactly the failure this app exists to prevent.
/// * The Core Audio calls are synchronous COM. Run directly inside an `async`
///   handler, a burst of requests parks every Tokio worker in blocking code.
///
/// A blocking COM call cannot be cancelled, so the timeout bounds how long a
/// *request* waits, not the call: if one wedges, later requests answer 503
/// instead of piling up behind it.
#[derive(Clone)]
pub struct AudioGate {
    /// Serializes the COM work. Locked on the blocking thread, not by the async
    /// caller — see `run`.
    slot: Arc<std::sync::Mutex<()>>,
    /// Admission control: bounds how many blocking threads can be parked on the
    /// gate at once. A resource guard only; correctness comes from `slot`.
    admission: Arc<tokio::sync::Semaphore>,
}

impl Default for AudioGate {
    fn default() -> Self {
        Self::new()
    }
}

enum GateError {
    /// Could not get the gate within `AUDIO_WAIT_TIMEOUT`.
    Busy,
    /// The blocking task died (panicked or was aborted).
    Failed,
}

impl AudioGate {
    pub fn new() -> Self {
        Self {
            slot: Arc::new(std::sync::Mutex::new(())),
            admission: Arc::new(tokio::sync::Semaphore::new(MAX_QUEUED_AUDIO_CALLS)),
        }
    }

    /// One diagnostic read without waiting behind another audio operation.
    /// The callback runs only while this gate owns the serialization slot.
    fn try_sample<T>(&self, f: impl FnOnce() -> T) -> Option<T> {
        let _slot = self.slot.try_lock().ok()?;
        Some(f())
    }

    async fn run<T, F>(&self, f: F) -> Result<T, GateError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let slot = self.slot.clone();
        let admission = self.admission.clone();

        let work = async move {
            // `acquire_owned`, not `acquire`: the permit has to be `'static`
            // so it can move onto the blocking thread below.
            let permit = admission
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| GateError::Failed)?;
            let handle = tokio::task::spawn_blocking(move || {
                // The permit moves in here, onto the blocking thread, and is
                // only released when this closure finishes. Held by the async
                // layer instead, a timeout — or a client disconnect dropping
                // the future — would hand it back while the work it admitted
                // was still queued behind a wedged COM call: every retry would
                // add one more parked thread for the duration of the wedge.
                let _admitted = permit;
                // Locked here, on the blocking thread, rather than by the async
                // caller. If the client disconnects, the awaiting future is
                // dropped — and a guard held out there would be released while
                // this COM call is still running, letting a second switch
                // interleave. That is the exact bug the gate exists to prevent.
                let _serialized = slot.lock().unwrap_or_else(|e| e.into_inner());
                f()
            });
            handle.await.map_err(|e| {
                eprintln!("[error] audio task did not complete: {e}");
                GateError::Failed
            })
        };

        match tokio::time::timeout(AUDIO_WAIT_TIMEOUT, work).await {
            Ok(result) => result,
            Err(_) => Err(GateError::Busy),
        }
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/api/status", get(status_handler))
        .route("/api/devices", get(devices_handler))
        .route("/api/devices/:id/default", post(set_default_handler))
        .route("/api/volume", get(volume_handler).post(set_volume_handler))
        .route("/api/test-sound", post(test_sound_handler))
        .route("/api/restart", post(restart_handler))
        .route("/api/diagnostics", get(diagnostics_list))
        .route("/api/diagnostics/start", post(diagnostics_start))
        .route("/api/diagnostics/stop", post(diagnostics_stop))
        .route("/api/diagnostics/marker", post(diagnostics_marker))
        .route("/api/diagnostics/purge", post(diagnostics_purge))
        .route("/api/diagnostics/:id/download", get(diagnostics_download))
        .route("/api/about", get(about_handler))
        .route("/api/languages", get(languages_handler))
        // `route_layer` so an unmatched `/api/...` path skips auth and lands on
        // the fallback, which answers it with JSON 404 rather than the HTML shell.
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            api_middleware,
        ))
        .fallback(static_handler)
        .with_state(state.clone())
        // Applied after `.fallback(...)` so the network guards cover static
        // assets as well — the router-level layer above does not.
        .layer(middleware::from_fn_with_state(state, network_guard))
        .layer(middleware::from_fn(security_headers))
}

pub async fn serve(state: AppState, addr: SocketAddr) -> Result<(), std::io::Error> {
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("[listening] http://{addr}/");
    let sampler = spawn_diagnostic_sampler(state.clone());
    let result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await;
    sampler.abort();
    let mut recorder = state.diagnostics.lock().unwrap_or_else(|e| e.into_inner());
    let _ = finish_capture(&mut recorder, &state.diagnostic_operations, StopReason::Shutdown);
    result
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    println!("\n[shutdown] Ctrl+C received");
}

// Diagnostics never hold their recorder lock while entering COM. Capture IDs
// fence off delayed work so a stopped capture cannot leak into its successor.
fn with_capture(
    recorder: &std::sync::Mutex<Diagnostics>,
    capture: &Option<String>,
    f: impl FnOnce(&mut Diagnostics) -> std::io::Result<()>,
) {
    let mut recorder = recorder.lock().unwrap_or_else(|e| e.into_inner());
    if capture.as_deref().is_some_and(|id| recorder.is_active(id)) {
        let _ = f(&mut recorder);
    }
}

fn event_time() -> (std::time::Instant, u64) {
    let instant = std::time::Instant::now();
    let utc_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    (instant, utc_ms)
}

const MAX_OPERATION_EVENTS: usize = 256;

struct OperationEvent {
    capture_id: String,
    operation: Operation,
    phase: Phase,
    hresult: Option<i32>,
    level: Option<f32>,
    muted: Option<bool>,
    occurred: std::time::Instant,
    utc_ms: u64,
}

/// In-memory fixed-shape queue, independent of the recorder/file mutex. The
/// audio gate never waits for disk. At most the admitted audio calls can add
/// stale capture IDs; drains clear overflow state along with queued events.
#[derive(Default)]
pub struct OperationQueue {
    active: Option<String>,
    events: VecDeque<OperationEvent>,
    overflowed: HashSet<String>,
}

impl OperationQueue {
    fn push(&mut self, event: OperationEvent) {
        if self.active.as_deref() != Some(event.capture_id.as_str()) {
            return;
        }
        if self.events.len() >= MAX_OPERATION_EVENTS {
            self.overflowed.insert(event.capture_id);
        } else {
            self.events.push_back(event);
        }
    }
}

fn drain_operations(recorder: &mut Diagnostics, queue: &std::sync::Mutex<OperationQueue>) {
    let (events, overflowed) = {
        let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
        (std::mem::take(&mut queue.events), std::mem::take(&mut queue.overflowed))
    };
    for event in events {
        if recorder.is_active(&event.capture_id) {
            let _ = recorder.operation_at(
                event.operation, event.phase, event.hresult, event.level,
                event.muted, (event.occurred, event.utc_ms),
            );
        }
    }
    if recorder.active_id().is_some_and(|id| overflowed.contains(&id)) {
        let _ = recorder.stop(StopReason::QueueOverflow);
    }
}

fn finish_capture(
    recorder: &mut Diagnostics,
    queue: &std::sync::Mutex<OperationQueue>,
    reason: StopReason,
) -> std::io::Result<()> {
    // Fence producers first, then persist everything already queued before
    // writing the terminal record. Producers never acquire the recorder mutex.
    queue.lock().unwrap_or_else(|e| e.into_inner()).active = None;
    drain_operations(recorder, queue);
    recorder.stop(reason)
}

async fn run_recorded_operation<T, F>(
    gate: &AudioGate,
    queue: Arc<std::sync::Mutex<OperationQueue>>,
    capture: Option<String>,
    operation: Operation,
    level: Option<f32>,
    muted: Option<bool>,
    f: F,
) -> Result<Result<T, audio::AudioError>, GateError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, audio::AudioError> + Send + 'static,
{
    gate.run(move || {
        let enqueue = |phase, hresult| {
            if let Some(capture_id) = &capture {
                let (occurred, utc_ms) = event_time();
                queue.lock().unwrap_or_else(|e| e.into_inner()).push(OperationEvent {
                    capture_id: capture_id.clone(), operation, phase, hresult,
                    level, muted, occurred, utc_ms,
                });
            }
        };
        enqueue(Phase::Started, None);
        let result = f();
        match &result {
            Ok(_) => enqueue(Phase::Completed, None),
            Err(e) => enqueue(Phase::Failed, Some(e.source.code().0)),
        }
        result
    }).await
}

fn spawn_diagnostic_sampler(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut pending: Option<tokio::task::JoinHandle<()>> = None;
        loop {
            interval.tick().await;
            let recorder = state.diagnostics.clone();
            let operations = state.diagnostic_operations.clone();
            let capture = match tokio::task::spawn_blocking(move || {
                let mut recorder = recorder.lock().unwrap_or_else(|e| e.into_inner());
                drain_operations(&mut recorder, &operations);
                // Independent of gate availability, including a hung COM call.
                let _ = recorder.tick();
                let active = recorder.active_id();
                operations.lock().unwrap_or_else(|e| e.into_inner()).active = active.clone();
                active
            })
            .await
            {
                Ok(capture) => capture,
                Err(_) => continue,
            };
            if capture.is_none() {
                continue;
            }
            if pending.as_ref().is_some_and(|task| !task.is_finished()) {
                continue;
            }
            let recorder = state.diagnostics.clone();
            let gate = state.audio.clone();
            pending = Some(tokio::task::spawn_blocking(move || {
                // Never enqueue a sampling call behind real user operations.
                let still_active = recorder
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .active_id() == capture;
                if !still_active {
                    return;
                }
                // The slot covers COM acquisition/read/release only. Durable
                // logging is outside it, so a slow file cannot hold this gate.
                let result = gate.try_sample(audio::sample_output);
                with_capture(&recorder, &capture, |d| match result {
                    Some(Ok(sample)) => d.sample_at(
                        &sample.endpoint_id,
                        sample.peak,
                        sample.volume,
                        sample.muted,
                        sample.acquired,
                        sample.acquired_utc_ms,
                    ),
                    Some(Err(e)) => d.meter_unavailable(e.source.code().0),
                    None => d.meter_busy(),
                });
            }));
        }
    })
}

fn diagnostics_error(error: std::io::Error) -> Response {
    use std::io::ErrorKind;
    let status = match error.kind() {
        ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
        ErrorKind::AlreadyExists | ErrorKind::WouldBlock => StatusCode::CONFLICT,
        ErrorKind::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    // No OS error strings or paths are exposed to callers or durable records.
    (status, Json(serde_json::json!({"error": "diagnostics_unavailable"}))).into_response()
}

async fn diagnostics_request(
    recorder: Arc<std::sync::Mutex<Diagnostics>>,
    operations: Arc<std::sync::Mutex<OperationQueue>>,
    f: impl FnOnce(&mut Diagnostics) -> Response + Send + 'static,
) -> Response {
    match tokio::task::spawn_blocking(move || {
        let mut recorder = recorder.lock().unwrap_or_else(|e| e.into_inner());
        drain_operations(&mut recorder, &operations);
        let response = f(&mut recorder);
        operations.lock().unwrap_or_else(|e| e.into_inner()).active = recorder.active_id();
        response
    })
    .await
    {
        Ok(response) => response,
        Err(_) => diagnostics_error(std::io::Error::other("diagnostic task failed")),
    }
}

async fn diagnostics_list(State(state): State<AppState>) -> Response {
    diagnostics_request(state.diagnostics, state.diagnostic_operations, move |recorder| {
        match recorder.list() {
            Ok(captures) => {
                Json(serde_json::json!({"active": recorder.active_id(), "captures": captures}))
                    .into_response()
            }
            Err(e) => diagnostics_error(e),
        }
    })
    .await
}

async fn diagnostics_start(State(state): State<AppState>) -> Response {
    diagnostics_request(state.diagnostics, state.diagnostic_operations, move |recorder| {
        match recorder.start() {
            Ok(capture_id) => Json(serde_json::json!({"capture_id": capture_id})).into_response(),
            Err(e) => diagnostics_error(e),
        }
    })
    .await
}

async fn diagnostics_stop(State(state): State<AppState>) -> Response {
    let operations = state.diagnostic_operations.clone();
    diagnostics_request(state.diagnostics, state.diagnostic_operations, move |recorder| {
        match finish_capture(recorder, &operations, StopReason::User) {
            Ok(()) => Json(serde_json::json!({"stopped": true})).into_response(),
            Err(e) => diagnostics_error(e),
        }
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkerRequest {
    marker: Marker,
}

async fn diagnostics_marker(
    State(state): State<AppState>,
    body: Result<Json<MarkerRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return bad_request("a fixed diagnostic marker is required");
    };
    let received = std::time::Instant::now();
    let received_utc_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let capture = state.diagnostics.lock().unwrap_or_else(|e| e.into_inner()).active_id();
    diagnostics_request(state.diagnostics, state.diagnostic_operations, move |recorder| {
        if !capture.as_deref().is_some_and(|id| recorder.is_active(id)) {
            return diagnostics_error(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "capture changed",
            ));
        }
        match recorder.marker_at(body.marker, received, received_utc_ms) {
            Ok(()) => Json(serde_json::json!({"marked": true})).into_response(),
            Err(e) => diagnostics_error(e),
        }
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PurgeRequest {
    confirm_count: usize,
}

async fn diagnostics_purge(
    State(state): State<AppState>,
    body: Result<Json<PurgeRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return bad_request("confirm_count is required");
    };
    diagnostics_request(state.diagnostics, state.diagnostic_operations, move |recorder| {
        match recorder.purge(body.confirm_count) {
            Ok(deleted) => Json(serde_json::json!({"deleted": deleted})).into_response(),
            Err(e) => diagnostics_error(e),
        }
    })
    .await
}

async fn diagnostics_download(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    diagnostics_request(state.diagnostics, state.diagnostic_operations, move |recorder| {
        match recorder.download(&id) {
            Ok(bytes) => {
                // Only a path-validated, app-owned capture gets this far. Header
                // construction still fails closed rather than accepting user text.
                let Ok(disposition) =
                    HeaderValue::from_str(&format!("attachment; filename=\"{id}.jsonl\""))
                else {
                    return bad_request("invalid capture ID");
                };
                let mut response = bytes.into_response();
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/x-ndjson; charset=utf-8"),
                );
                response
                    .headers_mut()
                    .insert(header::CONTENT_DISPOSITION, disposition);
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                response
            }
            Err(e) => diagnostics_error(e),
        }
    })
    .await
}

// ---- Handlers ---------------------------------------------------------------

#[derive(Serialize)]
struct StatusBody {
    version: &'static str,
    build_id: &'static str,
    bind: String,
    port: u16,
    lan_exposed: bool,
    require_token: bool,
    device_sort: SortPolicy,
    /// Whether `POST /api/restart` will do anything. The UI hides its restart
    /// button when this is false rather than offering a button that answers 501.
    supervised: bool,
    /// LAN URLs with the token pre-embedded in `#t=` — the Web UI shows these
    /// as one-tap-copy in a "connect from another machine" panel. Sorted:
    /// physical NICs first, virtual switches last.
    share_urls: Vec<net::ShareEntry>,
    default_console: Option<String>,
    default_multimedia: Option<String>,
    default_communications: Option<String>,
}

async fn status_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    let devices = match state.audio.run(audio::list_devices).await {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return audio_error(e),
        Err(e) => return gate_error(e),
    };
    let pick = |flag: fn(&AudioDevice) -> bool| -> Option<String> {
        devices.iter().find(|d| flag(d)).map(|d| d.id.clone())
    };
    let body = StatusBody {
        version: env!("CARGO_PKG_VERSION"),
        build_id: env!("AUDIOREMOTE_BUILD_ID"),
        bind: state.config.server.bind.clone(),
        port: state.config.server.port,
        lan_exposed: state.config.lan_exposed(),
        require_token: state.auth.require_token(),
        device_sort: state.config.audio.device_sort,
        supervised: state.supervised,
        // AR-14: hand the token-bearing share URLs only to loopback (the host
        // itself). LAN clients already hold a token and don't need every NIC's
        // URL; withholding them shrinks the token's exposure surface.
        share_urls: if is_loopback(peer.ip()) {
            net::build_share_entries(&state.config, state.auth.share_token().as_deref())
        } else {
            Vec::new()
        },
        default_console: pick(|d| d.is_default_console),
        default_multimedia: pick(|d| d.is_default_multimedia),
        default_communications: pick(|d| d.is_default_communications),
    };
    Json(body).into_response()
}

#[derive(Serialize)]
struct DevicesBody {
    devices: Vec<AudioDevice>,
    sort: SortPolicy,
}

async fn devices_handler(State(state): State<AppState>) -> Response {
    let mut devices = match state.audio.run(audio::list_devices).await {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return audio_error(e),
        Err(e) => return gate_error(e),
    };
    let sort = state.config.audio.device_sort;
    match sort {
        SortPolicy::State => sort_by_state(&mut devices),
        SortPolicy::Name => devices.sort_by_key(|a| a.name.to_lowercase()),
        SortPolicy::Recent => {
            let history = state.history.lock().await;
            devices.sort_by(|a, b| {
                let ta = history.last_used_at(&a.id);
                let tb = history.last_used_at(&b.id);
                match (ta, tb) {
                    (Some(x), Some(y)) => y.cmp(&x),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    // Fall back to state order when neither has history.
                    (None, None) => state_rank(a).cmp(&state_rank(b)),
                }
            });
        }
    }
    Json(DevicesBody { devices, sort }).into_response()
}

async fn set_default_handler(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    // The switch *and* its 3-role verification happen inside one gated blocking
    // call, so a second switch cannot slip between them.
    let target = id.clone();
    let capture = state.diagnostic_operations
        .lock().unwrap_or_else(|e| e.into_inner()).active.clone();
    match run_recorded_operation(
        &state.audio, state.diagnostic_operations.clone(), capture, Operation::DefaultDevice, None, None,
        move || audio::set_default(&target),
    ).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return audio_error(e),
        Err(e) => return gate_error(e),
    }
    {
        let mut history = state.history.lock().await;
        history.touch(&id);
        if let Err(e) = crate::history::save(&state.history_path, &history) {
            eprintln!("[warn] failed to persist history.toml: {e}");
        }
    }
    (StatusCode::NO_CONTENT).into_response()
}

#[derive(Debug, Deserialize)]
struct VolumePatch {
    level: Option<f32>,
    muted: Option<bool>,
}

impl VolumePatch {
    fn validate(&self) -> Result<(), &'static str> {
        if self.level.is_none() && self.muted.is_none() {
            return Err("at least one of level or muted is required");
        }
        if let Some(level) = self.level {
            if !level.is_finite() || !(0.0..=1.0).contains(&level) {
                return Err("level must be a finite number between 0 and 1");
            }
        }
        Ok(())
    }
}

async fn volume_handler(State(state): State<AppState>) -> Response {
    match state.audio.run(audio::get_master_volume).await {
        Ok(Ok(volume)) => Json(volume).into_response(),
        Ok(Err(e)) => audio_error(e),
        Err(e) => gate_error(e),
    }
}

async fn set_volume_handler(
    State(state): State<AppState>,
    body: Result<Json<VolumePatch>, JsonRejection>,
) -> Response {
    let Json(patch) = match body {
        Ok(body) => body,
        Err(e) => return bad_request(&format!("invalid JSON body: {e}")),
    };
    if let Err(message) = patch.validate() {
        return bad_request(message);
    }

    // Only the fields the client actually sent are applied; the Web UI sends
    // just the one it changed so a concurrent mute/level change elsewhere is not
    // silently reverted.
    let (level, muted) = (patch.level, patch.muted);
    let capture = state.diagnostic_operations
        .lock().unwrap_or_else(|e| e.into_inner()).active.clone();
    let result = run_recorded_operation(
        &state.audio, state.diagnostic_operations.clone(), capture, Operation::Volume, level, muted,
        move || audio::update_master_volume(level, muted),
    ).await;
    match result {
        Ok(Ok(volume)) => Json(volume).into_response(),
        Ok(Err(e)) => audio_error(e),
        Err(e) => gate_error(e),
    }
}

async fn test_sound_handler(State(state): State<AppState>) -> Response {
    let capture = state.diagnostic_operations
        .lock().unwrap_or_else(|e| e.into_inner()).active.clone();
    match run_recorded_operation(
        &state.audio, state.diagnostic_operations.clone(), capture, Operation::TestSound, None, None,
        audio::play_test_sound,
    ).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => audio_error(e),
        Err(e) => gate_error(e),
    }
}

/// Ask the supervisor to restart this server, picking up a staged build on the
/// way through.
///
/// This handler never kills its own process. The request goes to the parent and
/// the parent does the killing, so the only way to end up with no server is for
/// the parent to fail to start one — which it answers with a rollback. A server
/// that shut itself down would have no such safety net.
async fn restart_handler(State(state): State<AppState>) -> Response {
    if !state.supervised {
        let body = serde_json::json!({
            "error": "not_supervised",
            "message": "this server was started directly (`RemoteAudio serve`), so there is nobody to restart it",
        });
        return (StatusCode::NOT_IMPLEMENTED, Json(body)).into_response();
    }

    // Asked for on a delay, not here. The supervisor kills us as soon as it
    // reads the marker, and this response has not been written yet — the client
    // would see a dropped connection and report the restart as failed.
    #[cfg(windows)]
    tokio::spawn(async move {
        tokio::time::sleep(RESTART_GRACE).await;
        let mut recorder = state.diagnostics.lock().unwrap_or_else(|e| e.into_inner());
        let _ = finish_capture(&mut recorder, &state.diagnostic_operations, StopReason::Shutdown);
        crate::supervisor::ask_parent_to_restart();
    });

    // 202, not 200: the work has been handed off and this process is about to
    // be killed. The client's real confirmation is the server answering again a
    // few seconds later.
    let body = serde_json::json!({ "status": "restarting" });
    (StatusCode::ACCEPTED, Json(body)).into_response()
}

async fn about_handler() -> Response {
    Json(about::build()).into_response()
}

/// Enumerate language packs available under `web/lang/*.json` and return their
/// display metadata. Adding a language = dropping in a new JSON file with a
/// `_lang` block — no server-side code change required.
async fn languages_handler() -> Response {
    #[derive(serde::Serialize)]
    struct LangInfo {
        code: String,
        name: String,
    }

    let mut out: Vec<LangInfo> = Vec::new();
    for path in WebAssets::iter() {
        let p = path.as_ref();
        if !p.starts_with("lang/") || !p.ends_with(".json") {
            continue;
        }
        let code = p
            .trim_start_matches("lang/")
            .trim_end_matches(".json")
            .to_string();

        // Try to pull the friendly name from the pack itself; fall back to
        // the file's code if the pack lacks a `_lang` block.
        let mut name = code.clone();
        if let Some(file) = WebAssets::get(p) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&file.data) {
                if let Some(n) = v
                    .get("_lang")
                    .and_then(|l| l.get("name"))
                    .and_then(|s| s.as_str())
                {
                    name = n.to_string();
                }
            }
        }
        out.push(LangInfo { code, name });
    }
    out.sort_by(|a, b| a.code.cmp(&b.code));
    Json(out).into_response()
}

async fn static_handler(uri: Uri) -> Response {
    let raw = uri.path().trim_start_matches('/');

    // An unknown /api/* path must not answer with the HTML shell: callers parse
    // these as JSON, and a 200 page is the most confusing possible reply to a
    // mistyped endpoint.
    if raw == "api" || raw.starts_with("api/") {
        return not_found_json(uri.path());
    }

    let path = if raw.is_empty() { "index.html" } else { raw };

    if let Some(file) = WebAssets::get(path) {
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime_from_path(path))],
            file.data.into_owned(),
        )
            .into_response();
    }

    // Unknown path — fall back to index.html so client-side routing works and
    // typos still land on the UI.
    if let Some(file) = WebAssets::get("index.html") {
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            file.data.into_owned(),
        )
            .into_response();
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn mime_from_path(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

// ---- Middleware -------------------------------------------------------------

/// Response hardening for the whole surface.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    // Clickjacking: the loopback UI needs no token, so a hostile page able to
    // frame it could steer real clicks into device switches. `frame-ancestors`
    // is the modern rule; `X-Frame-Options` is the one older embedders honour.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

/// CIDR allowlist + `Host` allowlist, for every request including static assets.
async fn network_guard(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    let ip = peer.ip();

    // AR-04: CIDR allowlist. When `allowed_networks` is non-empty, a
    // non-loopback peer whose IP is outside every listed network is refused
    // before anything else. Loopback is always allowed so the host never
    // locks itself out.
    if !state.config.server.allowed_networks.is_empty()
        && !is_loopback(ip)
        && !ip_in_networks(ip, &state.config.server.allowed_networks)
    {
        return forbidden("source address not allowed");
    }

    // AR-02: Host header allowlist — defeats DNS rebinding. An attacker page
    // whose DNS re-resolves to 127.0.0.1 makes the TCP peer look like loopback,
    // but its request still carries the attacker's own Host, which is not in
    // the allowlist.
    if !host_allowed(request.headers(), &state.allowed_hosts) {
        return forbidden("host not allowed");
    }

    next.run(request).await
}

/// Same-origin enforcement for state changes, then bearer auth. `/api/*` only.
async fn api_middleware(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    if !is_safe_method(request.method()) {
        if let Err(reason) = same_origin(request.headers()) {
            return forbidden(reason);
        }
    }

    let header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    match authorize(
        state.auth.require_token(),
        is_loopback(peer.ip()),
        header,
        |token| state.auth.matches(token),
    ) {
        Authorized::Yes => next.run(request).await,
        Authorized::No => unauthorized(),
    }
}

enum Authorized {
    Yes,
    No,
}

/// The bearer decision, as a pure function so it can be table-tested.
///
/// Loopback bypass: connections from 127.0.0.1 / ::1 are OS-guaranteed to
/// originate on the host itself, where the user already has full control of the
/// audio settings. LAN clients always need a token.
fn authorize(
    require_token: bool,
    loopback: bool,
    auth_header: Option<&str>,
    matches: impl Fn(&str) -> bool,
) -> Authorized {
    if !require_token || loopback {
        return Authorized::Yes;
    }
    match auth_header.and_then(parse_bearer) {
        Some(token) if matches(token) => Authorized::Yes,
        _ => Authorized::No,
    }
}

/// Extract the credential from an `Authorization` header. The scheme name is
/// case-insensitive per RFC 7235, so `bearer x` has to work as well as `Bearer x`.
fn parse_bearer(value: &str) -> Option<&str> {
    let value = value.trim_start();
    let (scheme, rest) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = rest.trim();
    (!token.is_empty()).then_some(token)
}

fn is_safe_method(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Reject cross-origin state changes without breaking non-browser clients.
///
/// This is the check that closes the loopback bypass. Any website can fire
/// `fetch("http://127.0.0.1:17650/api/devices/<id>/default", {method:"POST",
/// mode:"no-cors"})`: the request carries no body, so nothing forces a preflight;
/// the TCP peer is loopback, so auth is bypassed; and `Host` is `127.0.0.1`, so
/// the rebinding guard passes too. What a browser will *not* do is misreport
/// `Sec-Fetch-Site` or `Origin`, and page script cannot set either.
///
/// * `Sec-Fetch-Site` is authoritative when present: `same-origin` / `none` pass,
///   anything else is refused. `same-site` is refused too — a different port on
///   the same host is same-site but not same-origin. The browser computed this
///   itself, so it needs no corroboration from `Origin`, which is what lets a TLS
///   reverse proxy rewrite `Host` without breaking writes.
/// * `Origin` is the fallback for browsers old enough to omit Fetch Metadata: its
///   authority must match the `Host` the request was routed to.
/// * Neither header present means the caller is not a browser (curl, a script,
///   the v0.2 tray client). There is no ambient credential to abuse there, so it
///   continues to the token check.
fn same_origin(headers: &HeaderMap) -> Result<(), &'static str> {
    const REFUSED: &str = "cross-origin request refused";

    if let Some(site) = header_str(headers, "sec-fetch-site") {
        return if matches!(site, "same-origin" | "none") {
            Ok(())
        } else {
            Err(REFUSED)
        };
    }
    if let Some(origin) = header_str(headers, header::ORIGIN.as_str()) {
        let Some(host) = header_str(headers, header::HOST.as_str()) else {
            return Err(REFUSED);
        };
        // "null" (sandboxed iframe, file://) has no authority and lands here too.
        let authority = origin.split_once("://").map(|(_, rest)| rest).unwrap_or("");
        if authority.is_empty() || !authority.eq_ignore_ascii_case(host) {
            return Err(REFUSED);
        }
    }
    Ok(())
}

fn header_str<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_loopback(),
        IpAddr::V6(a) => a.is_loopback(),
    }
}

/// True if `ip` falls inside any configured network. Unparseable entries are
/// ignored (treated as non-matching) so one typo can't silently allow-all; the
/// startup banner reports them so the operator can fix the typo instead.
fn ip_in_networks(ip: IpAddr, networks: &[String]) -> bool {
    networks
        .iter()
        .filter_map(|entry| config::parse_network(entry))
        .any(|net| net.contains(&ip))
}

/// AR-02: accept only requests whose `Host` header is in the startup allowlist
/// (loopback names + LAN IPs, with and without `:port`). Missing `Host` is
/// refused — HTTP/1.1 requires it, and its absence is characteristic of crafted
/// requests. An empty allowlist disables the check (defensive; not expected).
fn host_allowed(headers: &HeaderMap, allowed: &HashSet<String>) -> bool {
    if allowed.is_empty() {
        return true;
    }
    match header_str(headers, header::HOST.as_str()) {
        Some(h) => allowed.contains(h.to_ascii_lowercase().as_str()),
        None => false,
    }
}

// ---- Helpers ----------------------------------------------------------------

fn audio_error(e: audio::AudioError) -> Response {
    let body = serde_json::json!({
        "error": "audio",
        "context": e.context,
        "message": e.source.to_string(),
        "hresult": format!("0x{:08x}", e.source.code().0 as u32),
    });
    let status = if e.is_invalid_input() {
        StatusCode::BAD_REQUEST
    } else if e.is_role_split() {
        // The request was well-formed; the host state just did not settle where
        // it was told to. Reported separately so the UI can say so.
        StatusCode::CONFLICT
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, Json(body)).into_response()
}

fn gate_error(e: GateError) -> Response {
    let (status, message) = match e {
        GateError::Busy => (
            StatusCode::SERVICE_UNAVAILABLE,
            "the audio subsystem is busy; retry shortly",
        ),
        GateError::Failed => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "the audio operation did not complete",
        ),
    };
    let body = serde_json::json!({ "error": "audio_unavailable", "message": message });
    (status, Json(body)).into_response()
}

fn bad_request(message: &str) -> Response {
    let body = serde_json::json!({
        "error": "invalid_request",
        "message": message,
    });
    (StatusCode::BAD_REQUEST, Json(body)).into_response()
}

fn forbidden(message: &'static str) -> Response {
    (StatusCode::FORBIDDEN, message).into_response()
}

fn unauthorized() -> Response {
    let mut resp = (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"audioremote\""),
    );
    resp
}

fn not_found_json(path: &str) -> Response {
    let body = serde_json::json!({
        "error": "not_found",
        "message": format!("no such endpoint: {path}"),
    });
    (StatusCode::NOT_FOUND, Json(body)).into_response()
}

fn sort_by_state(devices: &mut [AudioDevice]) {
    devices.sort_by_key(state_rank);
}

fn state_rank(d: &AudioDevice) -> u8 {
    use crate::audio::DeviceState::*;
    match d.state {
        Active => 0,
        Unplugged => 1,
        Disabled => 2,
        NotPresent => 3,
        Unknown => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    #[tokio::test]
    async fn blocked_recorder_does_not_delay_audio_or_hold_its_gate() {
        let state = test_state(Vec::new());
        let id = state.diagnostics.lock().expect("recorder").start().expect("start");
        state.diagnostic_operations.lock().unwrap().active = Some(id.clone());
        let recorder = state.diagnostics.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _held = recorder.lock().unwrap();
            ready_tx.send(()).expect("signal held");
            release_rx.recv().expect("release");
        });
        ready_rx.await.expect("recorder held");
        let result = tokio::time::timeout(Duration::from_secs(1), run_recorded_operation(
            &state.audio,
            state.diagnostic_operations.clone(),
            Some(id.clone()),
            Operation::TestSound,
            None,
            None,
            || Ok(()),
        )).await;
        // Keep the recorder blocked while another audio call uses the gate.
        let next = tokio::time::timeout(Duration::from_secs(1), state.audio.run(|| 7)).await;
        release_tx.send(()).expect("release recorder");
        holder.join().expect("holder");
        let mut held = state.diagnostics.lock().unwrap();
        finish_capture(&mut held, &state.diagnostic_operations, StopReason::User).expect("stop");
        let bytes = held.download(&id).expect("download");
        held.purge(1).expect("cleanup");
        drop(held);
        assert!(matches!(result, Ok(Ok(Ok(())))));
        assert!(matches!(next, Ok(Ok(7))));
        let lines: Vec<serde_json::Value> = String::from_utf8(bytes).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(lines.last().unwrap()["event"], "capture_stopped");
        assert!(lines.iter().any(|v| v["phase"] == "started"));
        assert!(lines.iter().any(|v| v["phase"] == "completed"));
    }

    #[test]
    fn operation_queue_overflow_stops_capture_and_never_grows_unbounded() {
        let state = test_state(Vec::new());
        let mut recorder = state.diagnostics.lock().unwrap();
        let id = recorder.start().expect("start");
        {
            let mut queue = state.diagnostic_operations.lock().unwrap();
            queue.active = Some(id.clone());
            let (occurred, utc_ms) = event_time();
            queue.push(OperationEvent {
                capture_id: "retired-synthetic-capture".to_owned(),
                operation: Operation::TestSound, phase: Phase::Completed,
                hresult: None, level: None, muted: None, occurred, utc_ms,
            });
            assert!(queue.events.is_empty(), "stale producers are fenced out");
            for _ in 0..MAX_OPERATION_EVENTS + 1 {
                let (occurred, utc_ms) = event_time();
                queue.push(OperationEvent {
                    capture_id: id.clone(), operation: Operation::TestSound,
                    phase: Phase::Started, hresult: None, level: None, muted: None,
                    occurred, utc_ms,
                });
            }
            assert_eq!(queue.events.len(), MAX_OPERATION_EVENTS);
        }
        drain_operations(&mut recorder, &state.diagnostic_operations);
        assert!(recorder.active_id().is_none());
        {
            let queue = state.diagnostic_operations.lock().unwrap();
            assert!(queue.events.is_empty());
            assert!(queue.overflowed.is_empty());
        }
        let text = String::from_utf8(recorder.download(&id).unwrap()).unwrap();
        assert!(text.contains("queue_overflow"));
        recorder.purge(1).expect("cleanup");
    }

    #[test]
    fn diagnostic_sample_skips_a_busy_audio_gate_without_calling_com() {
        let gate = AudioGate::new();
        let held = gate.slot.lock().expect("hold gate");
        assert!(gate.try_sample(|| panic!("busy gate must not run a sample")).is_none());
        drop(held);
        assert_eq!(gate.try_sample(|| 7), Some(7));
    }

    #[tokio::test]
    async fn all_diagnostics_routes_require_lan_authentication() {
        async fn synthetic_lan_peer(mut request: Request, next: Next) -> Response {
            request.extensions_mut().insert(ConnectInfo(
                "192.0.2.10:42000".parse::<SocketAddr>().expect("synthetic peer"),
            ));
            next.run(request).await
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let app = build_router(test_state(Vec::new()))
            .layer(middleware::from_fn(synthetic_lan_peer));
        let task = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .expect("serve");
        });
        tokio::task::spawn_blocking(move || {
            for (method, path) in [
                ("GET", "/api/diagnostics"),
                ("POST", "/api/diagnostics/start"),
                ("POST", "/api/diagnostics/stop"),
                ("POST", "/api/diagnostics/marker"),
                ("POST", "/api/diagnostics/purge"),
                ("GET", "/api/diagnostics/00000000000000000000000000000000/download"),
            ] {
                let response = request(addr, &format!("{method} {path} HTTP/1.1"), Some("localhost"));
                assert!(response.starts_with("HTTP/1.1 401"), "{path}: {response}");
            }
        }).await.expect("join");
        task.abort();
    }

    #[tokio::test]
    async fn diagnostics_routes_reject_cross_origin_mutations() {
        let addr = spawn_test_server().await;
        let host = format!("127.0.0.1:{}", addr.port());
        for path in ["start", "stop", "marker", "purge"] {
            let host = host.clone();
            let response = tokio::task::spawn_blocking(move || {
                request(addr, &format!("POST /api/diagnostics/{path} HTTP/1.1\r\nOrigin: https://evil.example\r\nContent-Length: 0"), Some(&host))
            }).await.expect("join");
            assert!(response.starts_with("HTTP/1.1 403"), "{path}: {response}");
        }
    }

    #[tokio::test]
    async fn diagnostics_capture_download_and_confirmed_purge_are_wired() {
        let addr = spawn_test_server().await;
        let host = format!("127.0.0.1:{}", addr.port());
        tokio::task::spawn_blocking(move || {
            let send = |method: &str, path: &str, body: &str| {
                let mut stream = TcpStream::connect(addr).expect("connect");
                stream.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
                let raw = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                stream.write_all(raw.as_bytes()).expect("write");
                let mut response = String::new();
                stream.read_to_string(&mut response).expect("read");
                response
            };
            let response = send("GET", "/api/diagnostics", "");
            assert!(response.contains("\"active\":null"), "{response}");
            let response = send("POST", "/api/diagnostics/start", "");
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            let body: serde_json::Value = serde_json::from_str(response.split("\r\n\r\n").nth(1).expect("body")).expect("JSON");
            let id = body["capture_id"].as_str().expect("capture ID");
            let response = send("POST", "/api/diagnostics/purge", r#"{"confirm_count":1}"#);
            assert!(response.starts_with("HTTP/1.1 409"), "{response}");
            let response = send("POST", "/api/diagnostics/marker", r#"{"marker":"stop_pressed"}"#);
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            let response = send("POST", "/api/diagnostics/marker", r#"{"marker":"arbitrary free text"}"#);
            assert!(response.starts_with("HTTP/1.1 400"), "{response}");
            let response = send("POST", "/api/diagnostics/stop", "");
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            let response = send("GET", &format!("/api/diagnostics/{id}/download"), "");
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            assert!(response.contains("attachment; filename="), "{response}");
            assert!(response.contains("stop_pressed"), "{response}");
            let response = send("POST", "/api/diagnostics/purge", r#"{"confirm_count":0}"#);
            assert!(response.starts_with("HTTP/1.1 409"), "{response}");
            let response = send("POST", "/api/diagnostics/purge", r#"{"confirm_count":1}"#);
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            assert!(response.contains("\"deleted\":1"), "{response}");
        }).await.expect("join");
    }

    #[test]
    fn delayed_capture_work_cannot_write_into_a_new_capture() {
        let state = test_state(Vec::new());
        let mut recorder = state.diagnostics.lock().unwrap();
        let old = Some(recorder.start().expect("start"));
        recorder.stop(StopReason::User).expect("stop");
        let current = recorder.start().expect("start successor");
        drop(recorder);
        with_capture(&state.diagnostics, &old, |d| d.marker(Marker::SoundStopped));
        let mut recorder = state.diagnostics.lock().unwrap();
        recorder.stop(StopReason::User).expect("stop successor");
        let bytes = recorder.download(&current).expect("download");
        assert!(!String::from_utf8(bytes).expect("UTF-8").contains("sound_stopped"));
        recorder.purge(2).expect("cleanup");
    }

    // ---- VolumePatch -------------------------------------------------------

    fn patch(json: &str) -> VolumePatch {
        serde_json::from_str(json).expect("valid JSON fixture")
    }

    #[test]
    fn volume_patch_requires_at_least_one_field() {
        assert!(patch("{}").validate().is_err());
    }

    #[test]
    fn volume_patch_accepts_boundary_levels_and_mute_values() {
        for json in [
            r#"{"level":0}"#,
            r#"{"level":0.5}"#,
            r#"{"level":1}"#,
            r#"{"muted":true}"#,
            r#"{"muted":false}"#,
            r#"{"level":0.5,"muted":false}"#,
        ] {
            assert!(patch(json).validate().is_ok(), "{json}");
        }
    }

    #[test]
    fn volume_patch_rejects_out_of_range_levels() {
        for json in [r#"{"level":-0.01}"#, r#"{"level":1.01}"#] {
            assert!(patch(json).validate().is_err(), "{json}");
        }
    }

    #[test]
    fn volume_patch_rejects_non_numbers() {
        assert!(serde_json::from_str::<VolumePatch>(r#"{"level":"half"}"#).is_err());
        assert!(VolumePatch {
            level: Some(f32::NAN),
            muted: None,
        }
        .validate()
        .is_err());
    }

    #[test]
    fn volume_patch_keeps_absent_fields_absent() {
        // The whole point of the dirty-field protocol: a mute-only request must
        // not carry a level, or it would clobber a concurrent level change.
        let only_mute = patch(r#"{"muted":true}"#);
        assert_eq!(only_mute.level, None);
        let only_level = patch(r#"{"level":0.25}"#);
        assert_eq!(only_level.muted, None);
    }

    // ---- Bearer auth (AR-AUD-01 / AR-AUD-12) -------------------------------

    fn allow(token: &str) -> impl Fn(&str) -> bool + '_ {
        move |presented: &str| presented == token
    }

    fn allowed(outcome: Authorized) -> bool {
        matches!(outcome, Authorized::Yes)
    }

    #[test]
    fn lan_clients_need_a_valid_bearer() {
        let ok = allow("ar_live_good");
        for (header, expected) in [
            (Some("Bearer ar_live_good"), true),
            (Some("bearer ar_live_good"), true), // scheme is case-insensitive
            (Some("Bearer  ar_live_good  "), true), // padding tolerated
            (Some("Bearer ar_live_bad"), false),
            (Some("Bearer "), false),
            (Some("Bearer"), false),
            (Some("ar_live_good"), false), // no scheme
            (Some("Basic ar_live_good"), false),
            (Some(""), false),
            (None, false),
        ] {
            assert_eq!(
                allowed(authorize(true, false, header, &ok)),
                expected,
                "header = {header:?}"
            );
        }
    }

    #[test]
    fn loopback_and_disabled_tokens_bypass_the_bearer_check() {
        let never = |_: &str| false;
        assert!(allowed(authorize(true, true, None, never)), "loopback peer");
        assert!(
            allowed(authorize(false, false, None, never)),
            "require_token = false"
        );
        assert!(
            !allowed(authorize(true, false, None, never)),
            "LAN peer with tokens required"
        );
    }

    // ---- Host allowlist ----------------------------------------------------

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).expect("header name"),
                HeaderValue::from_str(v).expect("header value"),
            );
        }
        map
    }

    #[test]
    fn host_allowlist_defeats_rebinding_and_requires_a_host() {
        let allowed: HashSet<String> = ["127.0.0.1:17650", "203.0.113.5:17650", "localhost:17650"]
            .into_iter()
            .map(str::to_string)
            .collect();

        assert!(host_allowed(
            &headers(&[("host", "127.0.0.1:17650")]),
            &allowed
        ));
        assert!(host_allowed(
            &headers(&[("host", "203.0.113.5:17650")]),
            &allowed
        ));
        assert!(host_allowed(
            &headers(&[("host", "LOCALHOST:17650")]),
            &allowed
        ));
        assert!(host_allowed(
            &headers(&[("host", " 127.0.0.1:17650 ")]),
            &allowed
        ));

        assert!(!host_allowed(
            &headers(&[("host", "evil.example:17650")]),
            &allowed
        ));
        assert!(!host_allowed(&headers(&[("host", "127.0.0.1")]), &allowed));
        assert!(!host_allowed(&headers(&[]), &allowed), "missing Host");

        // An empty allowlist is the documented escape hatch, not a silent hole.
        assert!(host_allowed(&headers(&[]), &HashSet::new()));
    }

    // ---- CIDR allowlist ----------------------------------------------------

    #[test]
    fn cidr_allowlist_matches_networks_bare_hosts_and_fails_closed() {
        let nets = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        let ip = |s: &str| s.parse::<IpAddr>().expect("ip");

        let list = nets(&["203.0.113.0/24", "198.51.100.7"]);
        assert!(ip_in_networks(ip("203.0.113.42"), &list));
        assert!(ip_in_networks(ip("198.51.100.7"), &list));
        assert!(!ip_in_networks(ip("198.51.100.8"), &list));
        assert!(!ip_in_networks(ip("192.0.2.1"), &list));

        // A typo must never widen the allowlist.
        let broken = nets(&["oops", "203.0.113.0/33"]);
        assert!(!ip_in_networks(ip("203.0.113.42"), &broken));
    }

    // ---- Same-origin enforcement (AR-AUD-04) -------------------------------

    #[test]
    fn safe_methods_are_exempt_from_the_origin_check() {
        assert!(is_safe_method(&Method::GET));
        assert!(is_safe_method(&Method::HEAD));
        assert!(is_safe_method(&Method::OPTIONS));
        assert!(!is_safe_method(&Method::POST));
        assert!(!is_safe_method(&Method::PUT));
        assert!(!is_safe_method(&Method::DELETE));
    }

    #[test]
    fn same_origin_accepts_the_apps_own_requests() {
        for pairs in [
            vec![
                ("sec-fetch-site", "same-origin"),
                ("origin", "http://127.0.0.1:17650"),
                ("host", "127.0.0.1:17650"),
            ],
            vec![
                ("sec-fetch-site", "same-origin"),
                ("origin", "http://203.0.113.5:17650"),
                ("host", "203.0.113.5:17650"),
            ],
            // Behind a TLS reverse proxy the scheme differs; the authority does not.
            vec![
                ("sec-fetch-site", "same-origin"),
                ("origin", "https://203.0.113.5:17650"),
                ("host", "203.0.113.5:17650"),
            ],
            // A reverse proxy that rewrites Host to the upstream: the authorities
            // no longer match, but the browser already said same-origin, and page
            // script cannot set that header.
            vec![
                ("sec-fetch-site", "same-origin"),
                ("origin", "https://audio.example.local"),
                ("host", "127.0.0.1:17650"),
            ],
            // Address-bar / non-browser callers send neither header.
            vec![("host", "127.0.0.1:17650")],
            vec![("sec-fetch-site", "none"), ("host", "127.0.0.1:17650")],
        ] {
            assert!(same_origin(&headers(&pairs)).is_ok(), "{pairs:?}");
        }
    }

    #[test]
    fn same_origin_refuses_the_loopback_csrf_shapes() {
        for pairs in [
            // The real attack: a page on any site POSTing at 127.0.0.1.
            vec![
                ("sec-fetch-site", "cross-site"),
                ("origin", "http://evil.example"),
                ("host", "127.0.0.1:17650"),
            ],
            // Same host, different port — same-site but not same-origin.
            vec![
                ("sec-fetch-site", "same-site"),
                ("origin", "http://127.0.0.1:8080"),
                ("host", "127.0.0.1:17650"),
            ],
            // Older browsers send Origin but no Sec-Fetch-Site.
            vec![
                ("origin", "http://evil.example"),
                ("host", "127.0.0.1:17650"),
            ],
            // Sandboxed iframe / file:// origin.
            vec![("origin", "null"), ("host", "127.0.0.1:17650")],
            // Origin without any Host to compare against.
            vec![("origin", "http://127.0.0.1:17650")],
        ] {
            assert!(same_origin(&headers(&pairs)).is_err(), "{pairs:?}");
        }
    }

    // ---- Audio gate (AR-AUD-02 / AR-AUD-11) --------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_audio_gate_never_lets_two_calls_overlap() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let gate = AudioGate::new();
        let inside = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..12 {
            let gate = gate.clone();
            let inside = inside.clone();
            let peak = peak.clone();
            tasks.push(tokio::spawn(async move {
                gate.run(move || {
                    let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    // Long enough that overlapping calls would be observed.
                    std::thread::sleep(Duration::from_millis(5));
                    inside.fetch_sub(1, Ordering::SeqCst);
                })
                .await
                .is_ok()
            }));
        }
        for task in tasks {
            assert!(task.await.expect("join"), "every call should complete");
        }
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "two Core Audio calls ran at the same time"
        );
    }

    // ---- Wiring (the layer order the audit found broken) --------------------

    /// Minimal state for the request paths that never touch Core Audio.
    fn test_state(allowed_hosts: Vec<String>) -> AppState {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "audioremote-srv-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&dir);
        AppState {
            config: Arc::new(Config::default()),
            auth: Arc::new(AuthState::new(
                dir.join("config.toml"),
                crate::config::AuthConfig::default(),
            )),
            audio: AudioGate::new(),
            history: Arc::new(Mutex::new(History::default())),
            history_path: Arc::new(dir.join("history.toml")),
            diagnostics: Arc::new(std::sync::Mutex::new(Diagnostics::new(dir.join("diagnostics")))),
            diagnostic_operations: Arc::new(std::sync::Mutex::new(OperationQueue::default())),
            allowed_hosts: Arc::new(allowed_hosts.into_iter().collect()),
            // Matches a bare `RemoteAudio serve`, which is what these tests
            // model. The supervised path is covered by its own test below.
            supervised: false,
        }
    }

    /// One raw HTTP/1.1 exchange. Deliberately socket-level: it exercises the
    /// real `axum::serve` stack (ConnectInfo, layer order, the static fallback)
    /// without pulling in an HTTP client dependency.
    fn request(addr: SocketAddr, request_line: &str, host: Option<&str>) -> String {
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        let mut raw = format!("{request_line}\r\n");
        if let Some(host) = host {
            raw.push_str(&format!("Host: {host}\r\n"));
        }
        raw.push_str("Connection: close\r\n\r\n");
        stream.write_all(raw.as_bytes()).expect("write request");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("read response");
        String::from_utf8_lossy(&response).to_string()
    }

    /// Bind an ephemeral port first, then build the Host allowlist from it — the
    /// real one is assembled at startup from the configured port the same way.
    async fn spawn_test_server() -> SocketAddr {
        spawn_supervised_test_server(false).await
    }

    async fn spawn_supervised_test_server(supervised: bool) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let hosts = vec![
            "127.0.0.1".to_string(),
            format!("127.0.0.1:{}", addr.port()),
            "localhost".to_string(),
            format!("localhost:{}", addr.port()),
        ];
        let app = build_router(AppState {
            supervised,
            ..test_state(hosts)
        });
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        addr
    }

    #[tokio::test]
    async fn static_assets_carry_the_security_headers_and_the_host_guard() {
        let addr = spawn_test_server().await;
        let host = format!("127.0.0.1:{}", addr.port());

        // The static fallback is registered after the router-level auth layer, so
        // this is the response that used to escape every guard.
        let ok = tokio::task::spawn_blocking({
            let host = host.clone();
            move || request(addr, "GET / HTTP/1.1", Some(&host))
        })
        .await
        .expect("join");
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        let lower = ok.to_ascii_lowercase();
        assert!(lower.contains("frame-ancestors 'none'"), "{ok}");
        assert!(lower.contains("x-frame-options: deny"), "{ok}");
        assert!(lower.contains("x-content-type-options: nosniff"), "{ok}");
        assert!(lower.contains("referrer-policy: no-referrer"), "{ok}");

        // …and the Host allowlist now covers it too.
        let rebound = tokio::task::spawn_blocking(move || {
            request(addr, "GET / HTTP/1.1", Some("evil.example"))
        })
        .await
        .expect("join");
        assert!(rebound.starts_with("HTTP/1.1 403"), "{rebound}");
        assert!(
            rebound.to_ascii_lowercase().contains("x-frame-options"),
            "error responses need the headers too: {rebound}"
        );
    }

    #[tokio::test]
    async fn unknown_api_paths_answer_json_404() {
        let addr = spawn_test_server().await;
        let host = format!("127.0.0.1:{}", addr.port());
        let response = tokio::task::spawn_blocking(move || {
            request(addr, "GET /api/nope HTTP/1.1", Some(&host))
        })
        .await
        .expect("join");

        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        assert!(
            response.to_ascii_lowercase().contains("application/json"),
            "{response}"
        );
        assert!(response.contains("\"not_found\""), "{response}");
    }

    #[tokio::test]
    async fn cross_origin_post_is_refused_even_from_loopback() {
        let addr = spawn_test_server().await;
        let host = format!("127.0.0.1:{}", addr.port());
        let response = tokio::task::spawn_blocking(move || {
            let mut stream = TcpStream::connect(addr).expect("connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("read timeout");
            let raw = format!(
                "POST /api/test-sound HTTP/1.1\r\nHost: {host}\r\n\
                 Origin: http://evil.example\r\nSec-Fetch-Site: cross-site\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(raw.as_bytes()).expect("write");
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).expect("read");
            String::from_utf8_lossy(&buf).to_string()
        })
        .await
        .expect("join");

        // 403 before any host-side sound happens — the handler is never reached.
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(
            response.contains("cross-origin request refused"),
            "{response}"
        );
    }

    /// A POST with the headers a real same-origin browser request carries.
    fn same_origin_post(addr: SocketAddr, path: &str) -> String {
        let host = format!("127.0.0.1:{}", addr.port());
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        let raw = format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\n\
             Origin: http://{host}\r\nSec-Fetch-Site: same-origin\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(raw.as_bytes()).expect("write");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read");
        String::from_utf8_lossy(&buf).to_string()
    }

    #[tokio::test]
    async fn restart_is_refused_when_nothing_is_supervising() {
        // `RemoteAudio serve` run by hand. Honouring the request here would end
        // with no server at all and nobody on the host to notice.
        let addr = spawn_test_server().await;
        let response = tokio::task::spawn_blocking(move || same_origin_post(addr, "/api/restart"))
            .await
            .expect("join");

        assert!(response.starts_with("HTTP/1.1 501"), "{response}");
        assert!(response.contains("\"not_supervised\""), "{response}");
    }

    #[tokio::test]
    async fn a_supervised_server_accepts_the_restart() {
        let addr = spawn_supervised_test_server(true).await;
        let response = tokio::task::spawn_blocking(move || same_origin_post(addr, "/api/restart"))
            .await
            .expect("join");

        // 202: handed to the parent, not done yet.
        assert!(response.starts_with("HTTP/1.1 202"), "{response}");
        assert!(response.contains("\"restarting\""), "{response}");
    }

    #[tokio::test]
    async fn restart_is_behind_the_same_origin_guard() {
        // The one endpoint that can take the host offline, and loopback skips
        // the token — so the origin check is the only thing standing between a
        // random web page and a restart loop.
        let addr = spawn_supervised_test_server(true).await;
        let host = format!("127.0.0.1:{}", addr.port());
        let response = tokio::task::spawn_blocking(move || {
            let mut stream = TcpStream::connect(addr).expect("connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("read timeout");
            let raw = format!(
                "POST /api/restart HTTP/1.1\r\nHost: {host}\r\n\
                 Origin: http://evil.example\r\nSec-Fetch-Site: cross-site\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(raw.as_bytes()).expect("write");
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).expect("read");
            String::from_utf8_lossy(&buf).to_string()
        })
        .await
        .expect("join");

        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    }
}
