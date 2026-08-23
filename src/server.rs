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

use std::collections::HashSet;
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

    async fn run<T, F>(&self, f: F) -> Result<T, GateError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let slot = self.slot.clone();
        let admission = self.admission.clone();

        let work = async move {
            let _permit = admission.acquire().await.map_err(|_| GateError::Failed)?;
            let handle = tokio::task::spawn_blocking(move || {
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
        .route("/api/restart", post(restart_handler))
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
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("[listening] http://{addr}/");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    println!("\n[shutdown] Ctrl+C received");
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
    match state.audio.run(move || audio::set_default(&target)).await {
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
    let result = state
        .audio
        .run(move || audio::update_master_volume(level, muted))
        .await;
    match result {
        Ok(Ok(volume)) => Json(volume).into_response(),
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
    tokio::spawn(async {
        tokio::time::sleep(RESTART_GRACE).await;
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
        let dir = std::env::temp_dir().join(format!("audioremote-srv-{}", std::process::id()));
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
                "POST /api/devices/whatever/default HTTP/1.1\r\nHost: {host}\r\n\
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

        // 403 before any Core Audio call happens — the handler is never reached.
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
