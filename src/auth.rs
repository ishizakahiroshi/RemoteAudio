//! Live bearer-token state.
//!
//! `RemoteAudio token add|revoke` runs as a **separate process** and only edits
//! `config.toml`. The server used to freeze the token set into an `Arc<Config>`
//! at startup, so a revoke reported success while the leaked token kept working
//! until the next restart — and a freshly added token was rejected with 401.
//!
//! This module owns the one piece of config that must not be a startup snapshot.
//! It re-reads the `[auth]` section when the file changes, at most once per
//! `RECHECK_AFTER`, and keeps the last good token set if the file is mid-save,
//! truncated, or deleted (locking every client out is not a safe failure mode
//! either).
//!
//! Everything else in `Config` (bind, port, `allowed_networks`, `device_sort`)
//! stays a startup snapshot on purpose: those need a restart to take effect, and
//! pretending otherwise would be worse than saying so.

use std::path::{Path, PathBuf};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant, SystemTime};

use crate::config::{self, AuthConfig};

/// How long a snapshot is trusted before the file is checked again. The trade is
/// explicit: one `stat` per second on a busy server instead of one per request,
/// and a revoked token stops being accepted within a second.
const RECHECK_AFTER: Duration = Duration::from_millis(1000);

/// Cheap "did the file change" signal. mtime alone is enough in practice; the
/// length is included because two saves inside one filesystem timestamp tick
/// would otherwise look identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    mtime: Option<SystemTime>,
    len: u64,
}

fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Fingerprint {
        mtime: meta.modified().ok(),
        len: meta.len(),
    })
}

pub struct AuthState {
    path: PathBuf,
    recheck_after: Duration,
    inner: RwLock<Snapshot>,
}

struct Snapshot {
    auth: AuthConfig,
    /// Fingerprint the snapshot was parsed from. `None` when the file could not
    /// be read at all.
    seen: Option<Fingerprint>,
    checked_at: Instant,
    /// Set while reloads are failing, so a broken file warns once per breakage
    /// instead of once per second.
    warned: bool,
}

impl AuthState {
    /// Take ownership of the `[auth]` section loaded at startup. `path` is the
    /// same `config.toml` the CLI writes.
    pub fn new(path: PathBuf, auth: AuthConfig) -> Self {
        Self::with_recheck(path, auth, RECHECK_AFTER)
    }

    fn with_recheck(path: PathBuf, auth: AuthConfig, recheck_after: Duration) -> Self {
        let seen = fingerprint(&path);
        Self {
            path,
            recheck_after,
            inner: RwLock::new(Snapshot {
                auth,
                seen,
                checked_at: Instant::now(),
                warned: false,
            }),
        }
    }

    /// True when non-loopback clients must present a bearer token.
    pub fn require_token(&self) -> bool {
        self.refresh_if_due();
        self.read().auth.require_token
    }

    /// True if `presented` matches any non-revoked token (constant-time compare
    /// against every candidate — see `AuthConfig::matches`).
    pub fn matches(&self, presented: &str) -> bool {
        self.refresh_if_due();
        self.read().auth.matches(presented)
    }

    /// The token to embed in shareable LAN URLs. Returned by value because the
    /// snapshot behind it can be replaced by the next reload.
    pub fn share_token(&self) -> Option<String> {
        self.refresh_if_due();
        self.read().auth.share_token().map(str::to_string)
    }

    fn read(&self) -> RwLockReadGuard<'_, Snapshot> {
        // A panic while holding this lock cannot corrupt the snapshot (every
        // write replaces whole fields), so recovering beats taking the server
        // down over a poisoned lock.
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Snapshot> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    fn refresh_if_due(&self) {
        if self.read().checked_at.elapsed() < self.recheck_after {
            return;
        }

        // Do the filesystem work *outside* the lock. Two concurrent requests may
        // both stat and parse, which is harmless; neither blocks the other, and
        // no request ever waits on file I/O held under a write lock.
        let seen = fingerprint(&self.path);
        if seen.is_some() && self.read().seen == seen {
            let mut snap = self.write();
            snap.checked_at = Instant::now();
            return;
        }

        match config::load_auth(&self.path) {
            Ok(auth) => {
                let mut snap = self.write();
                let recovered = snap.warned;
                snap.auth = auth;
                snap.seen = seen;
                snap.checked_at = Instant::now();
                snap.warned = false;
                drop(snap);
                if recovered {
                    println!("[auth] config.toml parsed again; token set reloaded");
                }
            }
            Err(e) => {
                let mut snap = self.write();
                // Keep serving the last good token set: a truncated or mid-save
                // file must not lock every client out — and must not open the
                // door either. `seen` is deliberately not updated so the next
                // check retries.
                snap.checked_at = Instant::now();
                let first = !snap.warned;
                snap.warned = true;
                drop(snap);
                if first {
                    eprintln!(
                        "[warn] cannot reload tokens from {}: {e} (keeping the tokens loaded so far)",
                        self.path.display()
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{add_named_token, load_or_init, revoke_token_value, save};
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let mut dir = std::env::temp_dir();
            dir.push(format!(
                "audioremote-auth-{tag}-{}-{}",
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

    /// Zero debounce so the tests exercise reload behaviour, not the clock.
    fn state(path: &Path, auth: AuthConfig) -> AuthState {
        AuthState::with_recheck(path.to_path_buf(), auth, Duration::ZERO)
    }

    #[test]
    fn revoked_token_stops_working_without_a_restart() {
        let scratch = Scratch::new("revoke");
        let path = scratch.config();
        let (mut cfg, _) = load_or_init(&path).expect("first load");
        let original = cfg.share_token().expect("token").to_string();

        let auth = state(&path, cfg.auth.clone());
        assert!(auth.matches(&original));
        assert_eq!(auth.share_token().as_deref(), Some(original.as_str()));

        // Stand in for `RemoteAudio token add phone && RemoteAudio token revoke default`
        // running in another console.
        let added = add_named_token(&mut cfg, "phone").expect("add phone token");
        revoke_token_value(&mut cfg, &original);
        save(&path, &cfg).expect("save");

        assert!(
            !auth.matches(&original),
            "revoked token must stop authenticating"
        );
        assert!(auth.matches(&added), "a newly added token must be accepted");
        assert_eq!(auth.share_token().as_deref(), Some(added.as_str()));
    }

    #[test]
    fn require_token_flag_is_picked_up() {
        let scratch = Scratch::new("require");
        let path = scratch.config();
        let (mut cfg, _) = load_or_init(&path).expect("load");
        let auth = state(&path, cfg.auth.clone());
        assert!(auth.require_token());

        cfg.auth.require_token = false;
        save(&path, &cfg).expect("save");
        assert!(!auth.require_token());
    }

    #[test]
    fn unparseable_file_keeps_the_last_good_tokens() {
        let scratch = Scratch::new("broken");
        let path = scratch.config();
        let (cfg, _) = load_or_init(&path).expect("load");
        let original = cfg.share_token().expect("token").to_string();
        let auth = state(&path, cfg.auth.clone());

        fs::write(&path, "this is not toml {{{").expect("corrupt the file");
        assert!(
            auth.matches(&original),
            "a corrupt config must not lock clients out"
        );
        assert!(!auth.matches("ar_live_unrelated"));

        // …and recovers once the file is valid again.
        save(&path, &cfg).expect("restore");
        assert!(auth.matches(&original));
    }

    #[test]
    fn deleted_file_keeps_the_last_good_tokens() {
        let scratch = Scratch::new("deleted");
        let path = scratch.config();
        let (cfg, _) = load_or_init(&path).expect("load");
        let original = cfg.share_token().expect("token").to_string();
        let auth = state(&path, cfg.auth.clone());

        fs::remove_file(&path).expect("delete config");
        assert!(auth.matches(&original));
        assert!(!auth.matches(""));
    }

    #[test]
    fn debounce_defers_reloads_inside_the_window() {
        let scratch = Scratch::new("debounce");
        let path = scratch.config();
        let (mut cfg, _) = load_or_init(&path).expect("load");
        let original = cfg.share_token().expect("token").to_string();

        // A long window: the revoke below must NOT be visible yet.
        let auth =
            AuthState::with_recheck(path.clone(), cfg.auth.clone(), Duration::from_secs(3600));
        assert!(auth.matches(&original));

        revoke_token_value(&mut cfg, &original);
        add_named_token(&mut cfg, "next").expect("add next token");
        save(&path, &cfg).expect("save");

        assert!(
            auth.matches(&original),
            "inside the debounce window the snapshot is expected to be stale"
        );
    }
}
