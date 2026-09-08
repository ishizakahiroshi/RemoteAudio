//! Device usage history for `device_sort = "recent"`. Kept in a separate TOML
//! file from `config.toml` so hand-edits to config don't clobber timestamps,
//! and vice versa.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct History {
    /// device_id -> unix seconds when last set as default.
    #[serde(default)]
    pub last_used: HashMap<String, u64>,
}

impl History {
    pub fn touch(&mut self, device_id: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.last_used.insert(device_id.to_string(), now);
    }

    pub fn last_used_at(&self, device_id: &str) -> Option<u64> {
        self.last_used.get(device_id).copied()
    }
}

pub fn load(path: &Path) -> std::io::Result<History> {
    match fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("history parse: {e}"),
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(History::default()),
        Err(e) => Err(e),
    }
}

pub fn save(path: &Path, history: &History) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(history).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("history write: {e}"),
        )
    })?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "history.toml".to_string());
    let tmp = path.with_file_name(format!("{file_name}.{}.tmp", std::process::id()));

    let written = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
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

pub fn default_history_path() -> PathBuf {
    crate::config::data_dir().join("history.toml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let mut dir = std::env::temp_dir();
            dir.push(format!(
                "audioremote-history-test-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).expect("create scratch directory");
            Self(dir)
        }

        fn path(&self) -> PathBuf {
            self.0.join("history.toml")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_files() {
        let scratch = Scratch::new();
        let path = scratch.path();
        let mut history = History::default();
        history.touch("device-1");

        save(&path, &history).expect("save history");
        let loaded = load(&path).expect("load history");
        assert_eq!(
            loaded.last_used_at("device-1"),
            history.last_used_at("device-1")
        );

        let leftovers: Vec<_> = fs::read_dir(&scratch.0)
            .expect("read scratch directory")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }
}
