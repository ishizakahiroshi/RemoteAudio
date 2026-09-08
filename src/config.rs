//! Persisted server settings. Lives in `%APPDATA%\audioremote\config.toml`
//! (or `$XDG_CONFIG_HOME/audioremote/config.toml` as a courtesy on non-Windows,
//! though the server itself only runs on Windows).

use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub audio: AudioConfig,
    pub tray: TrayConfig,
}

/// Host-side settings, i.e. the notification-area icon. Deliberately not shared
/// with the Web UI's own language toggle: that one is per-browser and belongs to
/// whoever is holding the phone, this one belongs to the machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TrayConfig {
    /// `auto` follows the Windows UI language; `ja` / `en` pin it.
    pub ui_language: String,
    /// Retry a crashed `serve` child from resident mode. Direct `serve` runs
    /// never consult this flag because they do not have a supervisor.
    pub auto_restart: bool,
    /// Show the welcome window at startup.
    ///
    /// On until the user ticks "don't show this again" in the window itself.
    /// Deliberately not a first-run flag: an app with no window of its own is
    /// easy to lose track of, and deciding on someone's behalf that they have
    /// been told enough is precisely the call that went wrong when this was a
    /// one-shot notification.
    pub show_welcome: bool,
    /// Whether the one-click setup has completed at least once. Used only to
    /// stop offering it — and its UAC prompt — to someone who already ran it.
    pub setup_done: bool,
}

impl Default for TrayConfig {
    fn default() -> Self {
        Self {
            ui_language: "auto".to_string(),
            auto_restart: true,
            show_welcome: true,
            setup_done: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: String,
    pub port: u16,
    /// CIDR allowlist (e.g. `"203.0.113.0/24"`, `"198.51.100.5/32"`). Empty = allow
    /// any source address (only the bearer token gates non-loopback clients).
    /// When non-empty, a **non-loopback** peer whose IP is outside every listed
    /// network is refused (403) before token checking. Loopback is always
    /// allowed so the host itself never locks out.
    pub allowed_networks: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            // LAN-first: the whole point is to control the host from another
            // machine on the LAN (Win11 guest / phone / VM). Non-loopback
            // clients still need a bearer token; loopback is bypassed. Set to
            // "127.0.0.1" via `RemoteAudio setup` to lock it down.
            bind: "0.0.0.0".to_string(),
            port: 17650,
            allowed_networks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// Legacy single token from earlier v0.1 dev builds. Migrated into `tokens`
    /// on load and then dropped from the serialized form. Do not read this at
    /// runtime — use `tokens` / `AuthConfig::matches`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Named bearer tokens. A request authenticates if its bearer matches any
    /// entry with `revoked = false`. Managed via `RemoteAudio token add|revoke`.
    #[serde(default)]
    pub tokens: Vec<TokenEntry>,
    pub require_token: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            token: None,
            tokens: Vec::new(),
            require_token: true,
        }
    }
}

/// One named bearer token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenEntry {
    /// Human label shown by `RemoteAudio token list`. New active entries use a
    /// unique label; hand-edited legacy files can still contain duplicates, so
    /// revoke-by-name acts on every non-revoked match.
    pub name: String,
    pub token: String,
    /// Unix seconds when issued (0 if hand-added / unknown).
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    pub device_sort: SortPolicy,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            device_sort: SortPolicy::State,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortPolicy {
    State,
    Name,
    Recent,
}

impl AuthConfig {
    /// True if `presented` matches any non-revoked token. Compares against every
    /// candidate in constant time (no early return on first match / mismatch)
    /// so timing does not leak which token — or how much of one — was correct.
    pub fn matches(&self, presented: &str) -> bool {
        let mut ok = false;
        for t in &self.tokens {
            if t.revoked {
                continue;
            }
            ok |= ct_eq(presented.as_bytes(), t.token.as_bytes());
        }
        ok
    }

    /// The token to embed in shareable LAN URLs: the first still-valid entry.
    pub fn share_token(&self) -> Option<&str> {
        self.tokens
            .iter()
            .find(|t| !t.revoked)
            .map(|t| t.token.as_str())
    }

    pub fn active_count(&self) -> usize {
        self.tokens.iter().filter(|t| !t.revoked).count()
    }
}

impl Config {
    /// True when the bind address is anything other than a loopback address.
    /// Used to power the guest UI's "LAN exposed" badge and the startup warning.
    /// An unparseable `bind` is reported as exposed: the badge and the warning
    /// are safety signals, so the ambiguous case must not read as "locked down".
    pub fn lan_exposed(&self) -> bool {
        parse_bind(&self.server.bind)
            .map(|ip| !ip.is_loopback())
            .unwrap_or(true)
    }

    /// The address the listener will actually bind, or a human-readable reason
    /// why it cannot. Resolved once before the runtime starts so a hand-edited
    /// `config.toml` fails with an explanation instead of an opaque parse error.
    pub fn socket_addr(&self) -> std::result::Result<SocketAddr, String> {
        if self.server.port == 0 {
            return Err(
                "port must be 1-65535 (0 asks Windows for an ephemeral port, which the \
                 startup banner, share URLs and Host allowlist would then all report wrong)"
                    .to_string(),
            );
        }
        Ok(SocketAddr::new(
            parse_bind(&self.server.bind)?,
            self.server.port,
        ))
    }

    /// `allowed_networks` entries that are not parseable. They fail closed at
    /// request time (an unparseable entry never matches any peer), which looks
    /// exactly like "the server ignores my LAN" — so the operator has to be told
    /// at startup instead.
    pub fn invalid_networks(&self) -> Vec<&str> {
        self.server
            .allowed_networks
            .iter()
            .map(|s| s.trim())
            .filter(|s| parse_network(s).is_none())
            .collect()
    }

    pub fn share_token(&self) -> Option<&str> {
        self.auth.share_token()
    }
}

/// Accept an IPv4 / IPv6 literal (bracketed or not) or `localhost`.
///
/// `SocketAddr`'s own parser rejects both `localhost` and a bare `::1`, while the
/// old `lan_exposed` accepted them — that disagreement is what turned a
/// hand-edited `bind = "::1"` into a startup failure with an opaque message.
pub fn parse_bind(bind: &str) -> std::result::Result<IpAddr, String> {
    let raw = bind.trim();
    if raw.is_empty() {
        return Err(
            "bind must not be empty (use \"0.0.0.0\" for LAN or \"127.0.0.1\" for host-only)"
                .to_string(),
        );
    }
    if raw.eq_ignore_ascii_case("localhost") {
        return Ok(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    let unbracketed = raw
        .strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
        .unwrap_or(raw);
    unbracketed.parse::<IpAddr>().map_err(|_| {
        format!(
            "bind must be an IPv4/IPv6 literal or \"localhost\" (got {raw:?}; \
             host names are not resolved)"
        )
    })
}

/// Parse one `allowed_networks` entry. A bare address is accepted as a
/// single-host network (`/32`, `/128`) because that is what people actually
/// write; without this, `allowed_networks = ["203.0.113.20"]` would silently
/// match nothing and lock every LAN client out.
pub fn parse_network(entry: &str) -> Option<ipnet::IpNet> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }
    if let Ok(net) = entry.parse::<ipnet::IpNet>() {
        return Some(net);
    }
    match entry.parse::<IpAddr>().ok()? {
        IpAddr::V4(v4) => ipnet::Ipv4Net::new(v4, 32).ok().map(ipnet::IpNet::V4),
        IpAddr::V6(v6) => ipnet::Ipv6Net::new(v6, 128).ok().map(ipnet::IpNet::V6),
    }
}

/// Constant-time byte-slice equality. Unequal lengths return false without a
/// byte-by-byte early exit; equal lengths accumulate all differences before
/// comparing. Tokens are fixed-length (`ar_live_` + 48 hex), so the length
/// branch is not a meaningful oracle in practice.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Load config from disk. Migrates a legacy single `token` into `tokens`,
/// ensures at least one active token exists, and persists **only when something
/// changed** (migration or token generation) so a valid hand-edited config
/// keeps its comments/formatting and a read-only config still boots. Returns
/// `(config, was_token_generated)`.
pub fn load_or_init(path: &Path) -> std::io::Result<(Config, bool)> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let existed = path.exists();
    let mut config: Config = match fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("config parse: {e}"),
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(e),
    };

    let mut dirty = false;

    // Migrate a legacy single `token` into `tokens`, then forget the field
    // (it is `skip_serializing_if = None`, so taking it drops it from the file).
    if let Some(legacy) = config.auth.token.take() {
        dirty = true; // dropping the legacy field changes the serialized form
        let legacy = legacy.trim().to_string();
        if !legacy.is_empty() && !config.auth.tokens.iter().any(|t| t.token == legacy) {
            config.auth.tokens.push(TokenEntry {
                name: "default".to_string(),
                token: legacy,
                created_at: now_unix(),
                revoked: false,
            });
        }
    }

    // Guarantee at least one usable token (first run, or all revoked).
    let has_active = config.auth.tokens.iter().any(|t| !t.revoked);
    let generated = if !has_active {
        config.auth.tokens.push(TokenEntry {
            name: "default".to_string(),
            token: generate_token(),
            created_at: now_unix(),
            revoked: false,
        });
        dirty = true;
        true
    } else {
        false
    };

    if !existed || dirty {
        save(path, &config)?;
    }
    Ok((config, generated))
}

/// Write the config **atomically**: serialize to a per-process temp file next to
/// the target, then rename over it (`MoveFileEx` with replace-existing on
/// Windows). A running server re-reads this file to pick up token changes, so a
/// plain `fs::write` would let it observe a half-written TOML and fall back to a
/// stale token set for no reason.
pub fn save(path: &Path, config: &Config) -> std::io::Result<()> {
    let text = toml::to_string_pretty(config).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("config write: {e}"),
        )
    })?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "config.toml".to_string());
    let tmp = path.with_file_name(format!("{file_name}.{}.tmp", std::process::id()));

    let written = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        // Flush to the disk *before* the rename publishes the new contents.
        // The rename alone is atomic but not durable: a power cut in the wrong
        // instant could leave a zero-length config.toml behind — valid TOML,
        // zero tokens, every LAN client locked out on next boot. This is the
        // same failure `load_auth` refuses at its end, kept from ever forming.
        file.sync_all()
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Re-read just the `[auth]` section. The running server calls this to pick up
/// `RemoteAudio token add|revoke` without a restart (see `crate::auth`); it never
/// writes and never migrates the file — startup already did that.
pub fn load_auth(path: &Path) -> std::io::Result<AuthConfig> {
    let text = fs::read_to_string(path)?;
    let mut config: Config = toml::from_str(&text).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("config parse: {e}"),
        )
    })?;
    // Fold in a legacy single `token` in case the file was restored by hand from
    // an old dev build, so it keeps authenticating until the next save drops it.
    if let Some(legacy) = config.auth.token.take() {
        let legacy = legacy.trim().to_string();
        if !legacy.is_empty() && !config.auth.tokens.iter().any(|t| t.token == legacy) {
            config.auth.tokens.push(TokenEntry {
                name: "default".to_string(),
                token: legacy,
                created_at: now_unix(),
                revoked: false,
            });
        }
    }
    // A file that demands tokens but carries none active must not be honored:
    // installing it would 401 every non-loopback client until someone repairs
    // the file by hand. Refusing keeps the server on its last known-good set
    // (see `crate::auth`). Startup never reaches this because `load_or_init`
    // mints a replacement before the first `load_auth` ever runs.
    if config.auth.require_token && config.auth.active_count() == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "[auth] requires a token but none are active; refusing to lock every client out",
        ));
    }
    Ok(config.auth)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenNameError {
    Empty,
    Duplicate,
}

impl std::fmt::Display for TokenNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("token name must not be empty"),
            Self::Duplicate => f.write_str("an active token already uses that name"),
        }
    }
}

impl std::error::Error for TokenNameError {}

/// Append a new named token and return the generated token string.
pub fn add_named_token(
    config: &mut Config,
    name: &str,
) -> std::result::Result<String, TokenNameError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(TokenNameError::Empty);
    }
    if config
        .auth
        .tokens
        .iter()
        .any(|t| !t.revoked && t.name.trim() == name)
    {
        return Err(TokenNameError::Duplicate);
    }

    let token = generate_token();
    config.auth.tokens.push(TokenEntry {
        name: name.to_string(),
        token: token.clone(),
        created_at: now_unix(),
        revoked: false,
    });
    Ok(token)
}

/// Revoke every non-revoked token matching a label. Returns how many were
/// revoked. New tokens reject duplicate active labels, but hand-edited legacy
/// files can still contain duplicates, so the operation remains all-match.
pub fn revoke_named_token(config: &mut Config, name: &str) -> usize {
    let name = name.trim();
    let mut n = 0;
    for t in &mut config.auth.tokens {
        if !t.revoked && t.name.trim() == name {
            t.revoked = true;
            n += 1;
        }
    }
    n
}

/// Revoke every non-revoked token matching an exact token value. Keeping this
/// separate from label revocation makes the credential-bearing operation an
/// explicit CLI choice instead of an accidental fallback.
pub fn revoke_token_value(config: &mut Config, token: &str) -> usize {
    let mut n = 0;
    for t in &mut config.auth.tokens {
        if !t.revoked && t.token == token {
            t.revoked = true;
            n += 1;
        }
    }
    n
}

/// `%APPDATA%\audioremote\config.toml` on Windows.
pub fn default_config_path() -> PathBuf {
    data_dir().join("config.toml")
}

pub fn data_dir() -> PathBuf {
    // Prefer APPDATA (Windows). Fall back to home for other OSes so dev on
    // WSL / macOS at least does not panic.
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata).join("audioremote");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".audioremote");
    }
    PathBuf::from(".audioremote")
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 24 random bytes hex-encoded, prefixed with `ar_live_`.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 24];
    getrandom::getrandom(&mut bytes).expect("OS RNG unavailable");
    let mut hex = String::with_capacity(48);
    for b in bytes {
        hex.push(nibble(b >> 4));
        hex.push(nibble(b & 0x0f));
    }
    format!("ar_live_{hex}")
}

fn nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + n - 10) as char,
        _ => '0',
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// Fresh scratch directory per test. Kept out of `%APPDATA%` so a test run
    /// never touches the developer's real config.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let mut dir = std::env::temp_dir();
            dir.push(format!(
                "audioremote-test-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).expect("create scratch dir");
            Self(dir)
        }
        fn config(&self) -> PathBuf {
            self.0.join("config.toml")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn with_bind(bind: &str, port: u16) -> Config {
        let mut c = Config::default();
        c.server.bind = bind.to_string();
        c.server.port = port;
        c
    }

    #[test]
    fn resident_crash_recovery_defaults_to_on() {
        assert!(Config::default().tray.auto_restart);
        let parsed: Config = toml::from_str("[tray]\n").expect("config with omitted recovery flag");
        assert!(parsed.tray.auto_restart);
    }

    // ---- bind / port boundaries (AR-AUD-10) --------------------------------

    #[test]
    fn parse_bind_accepts_literals_and_localhost() {
        for (input, expected) in [
            ("0.0.0.0", "0.0.0.0"),
            ("127.0.0.1", "127.0.0.1"),
            ("  127.0.0.1  ", "127.0.0.1"),
            ("localhost", "127.0.0.1"),
            ("LOCALHOST", "127.0.0.1"),
            ("::1", "::1"),
            ("[::1]", "::1"),
            ("::", "::"),
            ("203.0.113.5", "203.0.113.5"),
        ] {
            let got = parse_bind(input).unwrap_or_else(|e| panic!("{input:?}: {e}"));
            assert_eq!(got.to_string(), expected, "{input:?}");
        }
    }

    #[test]
    fn parse_bind_rejects_names_ports_and_blanks() {
        for input in [
            "",
            "   ",
            "example.com",
            "127.0.0.1:17650",
            "0.0.0.0/0",
            "::1]",
        ] {
            assert!(parse_bind(input).is_err(), "{input:?} should be rejected");
        }
    }

    #[test]
    fn socket_addr_rejects_port_zero() {
        let err = with_bind("0.0.0.0", 0).socket_addr().unwrap_err();
        assert!(err.contains("port"), "{err}");
        assert_eq!(
            with_bind("localhost", 17650).socket_addr().unwrap(),
            "127.0.0.1:17650".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            with_bind("::1", 17650).socket_addr().unwrap(),
            "[::1]:17650".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn lan_exposed_agrees_with_the_parsed_bind() {
        for (bind, exposed) in [
            ("0.0.0.0", true),
            ("::", true),
            ("203.0.113.5", true),
            ("127.0.0.1", false),
            ("localhost", false),
            ("::1", false),
            ("[::1]", false),
            // Unparseable must read as exposed so the warning still fires.
            ("not-an-address", true),
        ] {
            assert_eq!(
                with_bind(bind, 17650).lan_exposed(),
                exposed,
                "bind = {bind:?}"
            );
        }
    }

    // ---- allowed_networks --------------------------------------------------

    #[test]
    fn parse_network_accepts_cidr_and_bare_addresses() {
        let net = parse_network("203.0.113.0/24").expect("cidr");
        assert!(net.contains(&"203.0.113.20".parse::<IpAddr>().unwrap()));
        assert!(!net.contains(&"198.51.100.20".parse::<IpAddr>().unwrap()));

        let host = parse_network("203.0.113.20").expect("bare v4 host");
        assert!(host.contains(&"203.0.113.20".parse::<IpAddr>().unwrap()));
        assert!(!host.contains(&"203.0.113.21".parse::<IpAddr>().unwrap()));

        assert!(parse_network("::1").is_some());
        assert!(parse_network("2001:db8::/32").is_some());
        for bad in ["", "   ", "nonsense", "203.0.113.0/33", "203.0.113.0-24"] {
            assert!(parse_network(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn invalid_networks_lists_only_the_broken_entries() {
        let mut cfg = Config::default();
        cfg.server.allowed_networks = vec![
            "203.0.113.0/24".to_string(),
            "oops".to_string(),
            "198.51.100.5".to_string(),
            "203.0.113.0/33".to_string(),
        ];
        assert_eq!(cfg.invalid_networks(), vec!["oops", "203.0.113.0/33"]);
    }

    // ---- token matching ----------------------------------------------------

    #[test]
    fn token_matches_only_active_entries() {
        let mut cfg = Config::default();
        cfg.auth.tokens = vec![
            TokenEntry {
                name: "revoked".to_string(),
                token: "ar_live_dead".to_string(),
                created_at: 0,
                revoked: true,
            },
            TokenEntry {
                name: "live".to_string(),
                token: "ar_live_good".to_string(),
                created_at: 0,
                revoked: false,
            },
        ];
        assert!(cfg.auth.matches("ar_live_good"));
        assert!(!cfg.auth.matches("ar_live_dead"));
        assert!(!cfg.auth.matches(""));
        assert!(!cfg.auth.matches("ar_live_goo"));
        assert!(!cfg.auth.matches("ar_live_goodx"));
        assert_eq!(cfg.share_token(), Some("ar_live_good"));
        assert_eq!(cfg.auth.active_count(), 1);
    }

    #[test]
    fn generated_tokens_have_the_documented_shape() {
        let token = generate_token();
        assert!(token.starts_with("ar_live_"), "{token}");
        let hex = &token["ar_live_".len()..];
        assert_eq!(hex.len(), 48);
        assert!(hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        assert_ne!(token, generate_token());
    }

    // ---- load / save lifecycle --------------------------------------------

    #[test]
    fn first_run_generates_one_token_and_persists_it() {
        let scratch = Scratch::new("firstrun");
        let path = scratch.config();

        let (cfg, generated) = load_or_init(&path).expect("first load");
        assert!(generated);
        assert_eq!(cfg.auth.active_count(), 1);
        assert!(path.exists());

        let (again, generated_again) = load_or_init(&path).expect("second load");
        assert!(!generated_again, "a second load must not mint a new token");
        assert_eq!(again.share_token(), cfg.share_token());
    }

    #[test]
    fn legacy_single_token_is_migrated_and_the_old_key_disappears() {
        let scratch = Scratch::new("legacy");
        let path = scratch.config();
        fs::write(
            &path,
            "[auth]\ntoken = \"ar_live_legacy\"\nrequire_token = true\n",
        )
        .expect("seed legacy config");

        let (cfg, generated) = load_or_init(&path).expect("load legacy");
        assert!(!generated, "migrating an existing token is not a new token");
        assert!(cfg.auth.matches("ar_live_legacy"));

        // The migrated value now lives under `[[auth.tokens]]`; what must be gone
        // is the bare `token = ...` key inside the `[auth]` table itself.
        let text = fs::read_to_string(&path).expect("read back");
        let auth_section = text
            .split_once("[auth]\n")
            .map(|(_, rest)| rest.split("\n[").next().unwrap_or("").to_string())
            .expect("[auth] section present");
        assert!(
            !auth_section
                .lines()
                .any(|line| line.trim_start().starts_with("token =")),
            "legacy key survived in [auth]:\n{text}"
        );
        assert!(text.contains("[[auth.tokens]]"), "{text}");
    }

    #[test]
    fn revoking_the_last_token_leaves_the_config_usable() {
        let scratch = Scratch::new("revoke");
        let path = scratch.config();
        let (mut cfg, _) = load_or_init(&path).expect("load");
        let first = cfg.share_token().expect("token").to_string();

        let added = add_named_token(&mut cfg, "phone").expect("add phone token");
        assert_eq!(cfg.auth.active_count(), 2);
        assert_eq!(revoke_named_token(&mut cfg, "phone"), 1);
        assert!(!cfg.auth.matches(&added));
        assert!(cfg.auth.matches(&first));
        assert_eq!(revoke_named_token(&mut cfg, "phone"), 0, "already revoked");

        assert_eq!(revoke_token_value(&mut cfg, &first), 1);
        assert_eq!(cfg.auth.active_count(), 0);
        save(&path, &cfg).expect("save");
        // Reloading an all-revoked file must mint a replacement, never boot with
        // zero usable tokens.
        let (reloaded, generated) = load_or_init(&path).expect("reload");
        assert!(generated);
        assert_eq!(reloaded.auth.active_count(), 1);
        assert!(!reloaded.auth.matches(&first));
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_files() {
        let scratch = Scratch::new("atomic");
        let path = scratch.config();
        let (mut cfg, _) = load_or_init(&path).expect("load");
        cfg.server.bind = "127.0.0.1".to_string();
        save(&path, &cfg).expect("save");

        let leftovers: Vec<_> = fs::read_dir(&scratch.0)
            .expect("read scratch")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );

        let (reloaded, _) = load_or_init(&path).expect("reload");
        assert_eq!(reloaded.server.bind, "127.0.0.1");
        assert!(!reloaded.lan_exposed());
    }

    #[test]
    fn load_auth_sees_changes_written_by_another_process() {
        let scratch = Scratch::new("loadauth");
        let path = scratch.config();
        let (mut cfg, _) = load_or_init(&path).expect("load");
        let original = cfg.share_token().expect("token").to_string();

        // Stand in for `RemoteAudio token add` running as a separate process.
        let added = add_named_token(&mut cfg, "guest").expect("add guest token");
        revoke_token_value(&mut cfg, &original);
        save(&path, &cfg).expect("save");

        let auth = load_auth(&path).expect("load_auth");
        assert!(auth.matches(&added));
        assert!(!auth.matches(&original));
        assert!(auth.require_token);
    }

    #[test]
    fn load_auth_refuses_a_required_but_empty_active_set() {
        let scratch = Scratch::new("lockout");
        let path = scratch.config();
        // A hand-truncated or half-restored file: valid TOML, zero tokens. The
        // running server must reject it (and keep its last good set) rather
        // than 401 every LAN client until the file is repaired.
        fs::write(&path, "[auth]\nrequire_token = true\n").expect("seed empty auth");
        assert!(
            load_auth(&path).is_err(),
            "required-but-empty must be refused"
        );

        // The same shape is fine once tokens are not required.
        fs::write(&path, "[auth]\nrequire_token = false\n").expect("reseed open auth");
        assert!(load_auth(&path).is_ok());

        // And a revoked-only token set counts as empty.
        fs::write(
            &path,
            "[auth]\nrequire_token = true\n\n[[auth.tokens]]\nname = \"old\"\n\
             token = \"ar_live_dead\"\nrevoked = true\n",
        )
        .expect("reseed revoked-only");
        assert!(
            load_auth(&path).is_err(),
            "all-revoked is still zero active"
        );
    }

    #[test]
    fn token_names_are_trimmed_and_active_duplicates_are_rejected() {
        let mut cfg = Config::default();
        let _ = add_named_token(&mut cfg, "  phone  ").expect("trim token name");
        assert_eq!(cfg.auth.tokens[0].name, "phone");
        assert_eq!(
            add_named_token(&mut cfg, "phone"),
            Err(TokenNameError::Duplicate)
        );
        assert_eq!(add_named_token(&mut cfg, "   "), Err(TokenNameError::Empty));

        assert_eq!(revoke_named_token(&mut cfg, " phone "), 1);
        assert!(add_named_token(&mut cfg, "phone").is_ok());
    }

    #[test]
    fn name_and_value_revocation_are_separate_operations() {
        let mut cfg = Config::default();
        let token = add_named_token(&mut cfg, "phone").expect("add phone token");

        assert_eq!(revoke_named_token(&mut cfg, &token), 0);
        assert!(cfg.auth.matches(&token));
        assert_eq!(revoke_token_value(&mut cfg, &token), 1);
        assert!(!cfg.auth.matches(&token));
    }

    #[test]
    fn retired_keys_in_an_existing_file_are_ignored() {
        let scratch = Scratch::new("retired");
        let path = scratch.config();
        // `unify_roles` was a no-op setting removed before v0.1.0; a config file
        // carrying it must still load.
        fs::write(
            &path,
            "[server]\nbind = \"0.0.0.0\"\nport = 17650\n\n[audio]\nunify_roles = true\n\
             device_sort = \"name\"\n",
        )
        .expect("seed config");

        let (cfg, _) = load_or_init(&path).expect("load");
        assert_eq!(cfg.audio.device_sort, SortPolicy::Name);
        assert_eq!(cfg.server.port, 17650);
    }
}
