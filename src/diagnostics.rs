//! Opt-in, bounded, metadata-only diagnostic captures. No free-form data is serializable.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const MAX_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_DURATION: Duration = Duration::from_secs(30 * 60);
const TERMINAL_RESERVE: u64 = 1024;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Marker {
    PlaybackStarted,
    StopPressed,
    SoundStopped,
}
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    User,
    DurationLimit,
    SizeLimit,
    QueueOverflow,
    Shutdown,
}
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    DefaultDevice,
    Volume,
    TestSound,
}
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Started,
    Completed,
    Failed,
}

#[derive(Serialize)]
pub struct CaptureInfo {
    pub capture_id: String,
    pub bytes: u64,
    pub status: &'static str,
}

struct Capture {
    id: String,
    file: File,
    started: Instant,
    sequence: u64,
    bytes: u64,
    endpoints: HashMap<String, usize>,
    meter_state: Option<Option<i32>>,
    meter_busy: bool,
    meter_invalid: bool,
}

pub struct Diagnostics {
    directory: PathBuf,
    active: Option<Capture>,
    max_bytes: u64,
    max_duration: Duration,
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid diagnostic request")
}
fn conflict() -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, "diagnostic state changed")
}
fn is_link(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}
fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn utc_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl Diagnostics {
    /// Does not create files or resume any capture at startup.
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            active: None,
            max_bytes: MAX_BYTES,
            max_duration: MAX_DURATION,
        }
    }
    pub fn active_id(&self) -> Option<String> {
        self.active.as_ref().map(|c| c.id.clone())
    }
    pub fn is_active(&self, id: &str) -> bool {
        self.active.as_ref().is_some_and(|c| c.id == id)
    }

    fn directory_safe(&self) -> io::Result<()> {
        let meta = fs::symlink_metadata(&self.directory)?;
        if !meta.is_dir() || is_link(&meta) {
            return Err(invalid());
        }
        Ok(())
    }
    fn owned_file(&self, id: &str) -> io::Result<File> {
        if !valid_id(id) {
            return Err(invalid());
        }
        self.directory_safe()?;
        let path = self.directory.join(format!("capture-{id}.jsonl"));
        let meta = fs::symlink_metadata(&path)?;
        if !meta.is_file() || is_link(&meta) || meta.len() > self.max_bytes {
            return Err(invalid());
        }
        let file = File::open(path)?;
        // Namespace alone is insufficient: never purge a user's unrelated JSONL.
        let mut first = String::new();
        BufReader::new(&file)
            .take(TERMINAL_RESERVE)
            .read_line(&mut first)?;
        let v: Value = serde_json::from_str(&first).map_err(|_| invalid())?;
        if v["schema_version"] != 1
            || v["source"] != "host"
            || v["capture_id"] != id
            || v["event"] != "capture_started"
            || v["sequence"] != 1
        {
            return Err(invalid());
        }
        Ok(file)
    }
    pub fn start(&mut self) -> io::Result<String> {
        self.start_with(
            |capture, fields| {
                Self::write_record(capture, "capture_started", fields)?;
                capture.file.sync_data()
            },
            Self::publish_new,
        )
    }

    fn publish_new(pending: &Path, published: &Path) -> io::Result<()> {
        fs::hard_link(pending, published)
    }

    fn start_with(
        &mut self,
        initialize: impl FnOnce(&mut Capture, Value) -> io::Result<()>,
        publish: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> io::Result<String> {
        if self.active.is_some() {
            return Err(conflict());
        }
        if !self.directory.exists() {
            fs::create_dir_all(&self.directory)?;
        }
        self.directory_safe()?;
        let mut random = [0u8; 16];
        getrandom::getrandom(&mut random)
            .map_err(|_| io::Error::other("capture ID unavailable"))?;
        let id: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let pending = self.directory.join(format!("capture-{id}.pending"));
        let published = self.directory.join(format!("capture-{id}.jsonl"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pending)?;
        let mut capture = Capture {
            id: id.clone(),
            file,
            started: Instant::now(),
            sequence: 0,
            bytes: 0,
            endpoints: HashMap::new(),
            meter_state: None,
            meter_busy: false,
            meter_invalid: false,
        };
        let initialized = initialize(
            &mut capture,
            json!({"sample_interval_ms":100,"max_bytes":self.max_bytes,
            "max_duration_ms":self.max_duration.as_millis() as u64,"endpoint_role":"multimedia"}),
        );
        drop(capture.file);
        if let Err(error) = initialized {
            let _ = fs::remove_file(&pending);
            return Err(error);
        }
        // Hard-link publication is atomic and never replaces an existing file.
        // Unsupported filesystems fail closed instead of risking an overwrite.
        if let Err(error) = publish(&pending, &published) {
            let _ = fs::remove_file(&pending);
            return Err(error);
        }
        // Do not append samples while a staging alias remains. A failure here
        // leaves only the complete header, visible as an interrupted capture.
        fs::remove_file(&pending)?;
        capture.file = OpenOptions::new().append(true).open(&published)?;
        self.active = Some(capture);
        Ok(id)
    }
    fn write_record(c: &mut Capture, event: &'static str, data: Value) -> io::Result<()> {
        let mut row = data;
        let map = row.as_object_mut().ok_or_else(invalid)?;
        map.insert("schema_version".into(), json!(1));
        map.insert("source".into(), json!("host"));
        map.insert("capture_id".into(), json!(c.id));
        map.insert("sequence".into(), json!(c.sequence + 1));
        map.insert("utc_unix_ms".into(), json!(utc_ms()));
        map.insert(
            "elapsed_ms".into(),
            json!(c.started.elapsed().as_millis() as u64),
        );
        map.insert("event".into(), json!(event));
        let mut bytes = serde_json::to_vec(&row)?;
        bytes.push(b'\n');
        if let Err(error) = c.file.write_all(&bytes).and_then(|_| c.file.flush()) {
            // Best effort rollback of a partial write. A process/power interruption
            // can still leave a partial last line, which readers must report.
            let _ = c.file.set_len(c.bytes);
            return Err(error);
        }
        c.bytes += bytes.len() as u64;
        c.sequence += 1;
        Ok(())
    }
    fn record(&mut self, event: &'static str, data: Value) -> io::Result<()> {
        self.tick()?;
        let Some(c) = self.active.as_mut() else {
            return Ok(());
        };
        // Every accepted payload is a closed enum or bounded numeric value; a
        // generous reserve covers both the next row and the final reason row.
        if c.bytes + TERMINAL_RESERVE * 2 > self.max_bytes {
            return self.stop(StopReason::SizeLimit);
        }
        let result = Self::write_record(c, event, data);
        if result.is_err() {
            self.active = None;
        }
        result
    }
    pub fn tick(&mut self) -> io::Result<()> {
        if let Some(c) = &self.active {
            if c.started.elapsed() >= self.max_duration {
                return self.stop(StopReason::DurationLimit);
            }
        }
        Ok(())
    }
    pub fn stop(&mut self, reason: StopReason) -> io::Result<()> {
        if let Some(mut c) = self.active.take() {
            Self::write_record(&mut c, "capture_stopped", json!({"reason":reason}))?;
            c.file.sync_data()?;
        }
        Ok(())
    }
    #[cfg(test)]
    pub fn marker(&mut self, marker: Marker) -> io::Result<()> {
        self.marker_at(marker, Instant::now(), utc_ms())
    }
    /// Separate arrival timestamps preserve the handler receipt time while the
    /// envelope remains ordered by serialization time and sequence.
    pub fn marker_at(
        &mut self,
        marker: Marker,
        received: Instant,
        received_utc_ms: u64,
    ) -> io::Result<()> {
        self.tick()?;
        let Some(c) = self.active.as_ref() else {
            return Err(conflict());
        };
        let received_elapsed_ms = received.saturating_duration_since(c.started).as_millis() as u64;
        self.record(
            "marker",
            json!({"marker":marker, "received_elapsed_ms":received_elapsed_ms,
            "received_utc_unix_ms":received_utc_ms}),
        )
    }
    #[cfg(test)]
    pub fn sample(
        &mut self,
        endpoint_id: &str,
        peak: f32,
        volume: f32,
        muted: bool,
    ) -> io::Result<()> {
        self.sample_at(endpoint_id, peak, volume, muted, Instant::now(), utc_ms())
    }
    pub fn sample_at(
        &mut self,
        endpoint_id: &str,
        peak: f32,
        volume: f32,
        muted: bool,
        acquired: Instant,
        acquired_utc_ms: u64,
    ) -> io::Result<()> {
        if !peak.is_finite()
            || !volume.is_finite()
            || !(0.0..=1.0).contains(&peak)
            || !(0.0..=1.0).contains(&volume)
        {
            return self.meter_invalid();
        }
        self.tick()?;
        let Some(c) = self.active.as_mut() else {
            return Ok(());
        };
        let acquired_elapsed_ms = acquired.saturating_duration_since(c.started).as_millis() as u64;
        let next = c.endpoints.len() + 1;
        // IDs stay only in memory and the map is bounded by the bounded capture.
        let slot = *c
            .endpoints
            .entry(endpoint_id.to_ascii_lowercase())
            .or_insert(next);
        let changed = c.meter_state != Some(None) || c.meter_busy || c.meter_invalid;
        c.meter_state = Some(None);
        c.meter_busy = false;
        c.meter_invalid = false;
        if changed {
            self.record("meter_state", json!({"state":"available"}))?;
        }
        self.record(
            "meter_sample",
            json!({"endpoint_slot":format!("endpoint_{slot}"),
            "peak":peak,"volume":volume,"muted":muted, "acquired_elapsed_ms":acquired_elapsed_ms,
            "acquired_utc_unix_ms":acquired_utc_ms}),
        )
    }
    pub fn meter_unavailable(&mut self, hresult: i32) -> io::Result<()> {
        self.tick()?;
        let Some(c) = self.active.as_mut() else {
            return Ok(());
        };
        if c.meter_state == Some(Some(hresult)) && !c.meter_busy && !c.meter_invalid {
            return Ok(());
        }
        c.meter_state = Some(Some(hresult));
        c.meter_busy = false;
        c.meter_invalid = false;
        self.record(
            "meter_state",
            json!({"state":"unavailable","hresult":hresult}),
        )
    }
    pub fn meter_invalid(&mut self) -> io::Result<()> {
        self.tick()?;
        let Some(c) = self.active.as_mut() else {
            return Ok(());
        };
        if c.meter_invalid {
            return Ok(());
        }
        c.meter_invalid = true;
        c.meter_busy = false;
        self.record(
            "meter_state",
            json!({"state":"unknown","reason":"invalid_value"}),
        )
    }
    pub fn meter_busy(&mut self) -> io::Result<()> {
        self.tick()?;
        let Some(c) = self.active.as_mut() else {
            return Ok(());
        };
        if c.meter_busy {
            return Ok(());
        }
        c.meter_busy = true;
        c.meter_invalid = false;
        self.record("meter_state", json!({"state":"busy"}))
    }
    #[cfg(test)]
    pub fn operation(
        &mut self,
        operation: Operation,
        phase: Phase,
        hresult: Option<i32>,
        level: Option<f32>,
        muted: Option<bool>,
    ) -> io::Result<()> {
        self.operation_at(
            operation,
            phase,
            hresult,
            level,
            muted,
            (Instant::now(), utc_ms()),
        )
    }
    pub fn operation_at(
        &mut self,
        operation: Operation,
        phase: Phase,
        hresult: Option<i32>,
        level: Option<f32>,
        muted: Option<bool>,
        occurred: (Instant, u64),
    ) -> io::Result<()> {
        let (occurred, occurred_utc_ms) = occurred;
        if level.is_some_and(|v| !v.is_finite() || !(0.0..=1.0).contains(&v)) {
            return Err(invalid());
        }
        let Some(c) = self.active.as_ref() else {
            return Ok(());
        };
        let occurred_elapsed_ms = occurred.saturating_duration_since(c.started).as_millis() as u64;
        self.record(
            "operation",
            json!({"operation":operation,"phase":phase,"hresult":hresult,
            "level":level,"muted":muted,"occurred_elapsed_ms":occurred_elapsed_ms,
            "occurred_utc_unix_ms":occurred_utc_ms}),
        )
    }
    pub fn list(&self) -> io::Result<Vec<CaptureInfo>> {
        if !self.directory.exists() {
            return Ok(Vec::new());
        }
        self.directory_safe()?;
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(id) = name
                .strip_prefix("capture-")
                .and_then(|s| s.strip_suffix(".jsonl"))
            else {
                continue;
            };
            let Ok(mut file) = self.owned_file(id) else {
                continue;
            };
            let bytes = file.metadata()?.len();
            // Only the final bounded row is needed; retained history never requires
            // rereading every 16 MiB capture on each UI status poll.
            file.seek(SeekFrom::Start(bytes.saturating_sub(TERMINAL_RESERVE)))?;
            let mut data = Vec::new();
            file.take(TERMINAL_RESERVE).read_to_end(&mut data)?;
            let last = data.split(|b| *b == b'\n').rev().find(|v| !v.is_empty());
            let stopped = last
                .and_then(|v| serde_json::from_slice::<Value>(v).ok())
                .is_some_and(|v| v["event"] == "capture_stopped" && v["capture_id"] == id);
            let status = if self.is_active(id) {
                "active"
            } else if stopped && data.ends_with(b"\n") {
                "stopped"
            } else {
                "interrupted"
            };
            out.push(CaptureInfo {
                capture_id: id.to_string(),
                bytes,
                status,
            });
        }
        out.sort_by(|a, b| a.capture_id.cmp(&b.capture_id));
        Ok(out)
    }
    pub fn download(&self, id: &str) -> io::Result<Vec<u8>> {
        let mut file = self.owned_file(id)?;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.take(self.max_bytes + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > self.max_bytes {
            return Err(invalid());
        }
        Ok(bytes)
    }
    pub fn purge(&mut self, confirm_count: usize) -> io::Result<usize> {
        if self.active.is_some() {
            return Err(conflict());
        }
        let captures = self.list()?;
        if captures.len() != confirm_count {
            return Err(conflict());
        }
        for capture in &captures {
            // Recheck ownership and links immediately before each deletion.
            self.owned_file(&capture.capture_id)?;
            fs::remove_file(
                self.directory
                    .join(format!("capture-{}.jsonl", capture.capture_id)),
            )?;
        }
        Ok(captures.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Sandbox(PathBuf);
    impl Sandbox {
        fn new() -> Self {
            let mut random = [0u8; 16];
            getrandom::getrandom(&mut random).unwrap();
            let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
            Self(std::env::temp_dir().join(format!("audioremote-diagnostics-test-{name}")))
        }
        fn store(&self) -> Diagnostics {
            Diagnostics::new(self.0.clone())
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn rows(d: &Diagnostics, id: &str) -> Vec<Value> {
        String::from_utf8(d.download(id).unwrap())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    #[test]
    fn opt_in_order_anonymity_and_fixed_markers() {
        let sandbox = Sandbox::new();
        let mut d = sandbox.store();
        assert!(d.active_id().is_none());
        assert!(!sandbox.0.exists());
        let id = d.start().unwrap();
        assert!(valid_id(&id));
        assert!(d.start().is_err());
        d.marker(Marker::PlaybackStarted).unwrap();
        d.sample("synthetic-device-a", 0.5, 0.8, false).unwrap();
        d.sample("SYNTHETIC-DEVICE-A", 0.0, 0.8, false).unwrap();
        d.sample("synthetic-device-b", 0.1, 0.8, true).unwrap();
        d.marker(Marker::StopPressed).unwrap();
        d.operation(Operation::TestSound, Phase::Completed, None, None, None)
            .unwrap();
        d.stop(StopReason::User).unwrap();
        let data = rows(&d, &id);
        for (index, row) in data.iter().enumerate() {
            assert_eq!(row["sequence"], index + 1);
            assert_eq!(row["capture_id"], id);
            assert_eq!(row["source"], "host");
            assert!(row["utc_unix_ms"].as_u64().unwrap() > 0);
            if index > 0 {
                assert!(row["elapsed_ms"].as_u64() >= data[index - 1]["elapsed_ms"].as_u64());
            }
        }
        let samples: Vec<_> = data
            .iter()
            .filter(|r| r["event"] == "meter_sample")
            .collect();
        assert_eq!(samples[0]["endpoint_slot"], "endpoint_1");
        assert_eq!(samples[1]["endpoint_slot"], "endpoint_1");
        assert_eq!(samples[2]["endpoint_slot"], "endpoint_2");
        assert!(!String::from_utf8(d.download(&id).unwrap())
            .unwrap()
            .contains("synthetic-device"));
        assert_eq!(d.list().unwrap()[0].status, "stopped");
        assert!(serde_json::from_str::<Marker>("\"free text\"").is_err());
    }
    #[test]
    fn acquisition_time_survives_delayed_serialization() {
        let sandbox = Sandbox::new();
        let mut d = sandbox.store();
        let id = d.start().unwrap();
        let acquired = d.active.as_ref().unwrap().started;
        d.sample_at("synthetic", 0.0, 0.5, false, acquired, 1000)
            .unwrap();
        d.operation_at(
            Operation::TestSound,
            Phase::Started,
            None,
            None,
            None,
            (acquired, 1000),
        )
        .unwrap();
        let data = rows(&d, &id);
        let sample = data.iter().find(|r| r["event"] == "meter_sample").unwrap();
        assert_eq!(sample["acquired_elapsed_ms"], 0);
        assert_eq!(sample["acquired_utc_unix_ms"], 1000);
        let operation = data.iter().find(|r| r["event"] == "operation").unwrap();
        assert_eq!(operation["occurred_elapsed_ms"], 0);
        assert_eq!(operation["occurred_utc_unix_ms"], 1000);
    }
    #[test]
    fn repeated_meter_failures_are_transitions_only() {
        let sandbox = Sandbox::new();
        let mut d = sandbox.store();
        let id = d.start().unwrap();
        for _ in 0..20 {
            d.meter_unavailable(-1).unwrap();
        }
        d.meter_unavailable(-2).unwrap();
        d.sample("synthetic", 0.0, 1.0, false).unwrap();
        d.meter_busy().unwrap();
        d.meter_busy().unwrap();
        d.stop(StopReason::User).unwrap();
        assert_eq!(
            rows(&d, &id)
                .iter()
                .filter(|r| r["event"] == "meter_state")
                .count(),
            4
        );
    }
    #[test]
    fn invalid_values_emit_one_unknown_transition_then_recover() {
        let sandbox = Sandbox::new();
        let mut d = sandbox.store();
        let id = d.start().unwrap();
        d.sample("synthetic", 0.1, 0.5, false).unwrap();
        for bad in [f32::NAN, f32::INFINITY, -0.1, 1.1] {
            d.sample("synthetic", bad, 0.5, false).unwrap();
        }
        d.sample("synthetic", 0.2, 0.5, false).unwrap();
        d.stop(StopReason::User).unwrap();
        let data = rows(&d, &id);
        assert_eq!(
            data.iter()
                .filter(|r| r["reason"] == "invalid_value")
                .count(),
            1
        );
        assert_eq!(
            data.iter().filter(|r| r["event"] == "meter_sample").count(),
            2
        );
    }
    #[test]
    fn size_and_duration_limits_leave_terminal_reason() {
        let sandbox = Sandbox::new();
        let mut d = sandbox.store();
        d.max_bytes = 4096;
        let id = d.start().unwrap();
        for _ in 0..100 {
            d.sample("synthetic", 0.0, 1.0, false).unwrap();
        }
        assert!(d.active_id().is_none());
        assert!(d.download(&id).unwrap().len() <= 4096);
        assert_eq!(rows(&d, &id).last().unwrap()["reason"], "size_limit");
        let second = d.start().unwrap();
        d.max_duration = Duration::ZERO;
        d.tick().unwrap();
        assert!(d.active_id().is_none());
        assert_eq!(
            rows(&d, &second).last().unwrap()["reason"],
            "duration_limit"
        );
    }
    #[test]
    fn restart_is_off_and_partial_tail_is_interrupted() {
        let sandbox = Sandbox::new();
        let mut d = sandbox.store();
        let id = d.start().unwrap();
        drop(d);
        let d = sandbox.store();
        assert!(d.active_id().is_none());
        assert_eq!(d.list().unwrap()[0].status, "interrupted");
        let path = sandbox.0.join(format!("capture-{id}.jsonl"));
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"{\"event\":")
            .unwrap();
        assert_eq!(d.list().unwrap()[0].status, "interrupted");
        assert!(d.download(&id).unwrap().ends_with(b"{\"event\":"));
    }
    #[test]
    fn purge_requires_exact_count_and_preserves_unowned_files() {
        let sandbox = Sandbox::new();
        let mut d = sandbox.store();
        let id = d.start().unwrap();
        assert!(d.purge(1).is_err());
        d.stop(StopReason::User).unwrap();
        fs::write(sandbox.0.join("notes.jsonl"), b"keep").unwrap();
        let foreign = "00000000000000000000000000000000";
        fs::write(sandbox.0.join(format!("capture-{foreign}.jsonl")), b"{}\n").unwrap();
        assert!(d.download("../outside").is_err());
        assert!(d.download(foreign).is_err());
        assert!(d.purge(0).is_err());
        assert_eq!(d.purge(1).unwrap(), 1);
        assert!(!sandbox.0.join(format!("capture-{id}.jsonl")).exists());
        assert!(sandbox.0.join("notes.jsonl").exists());
        assert!(sandbox.0.join(format!("capture-{foreign}.jsonl")).exists());
    }
    #[test]
    fn failed_initial_header_never_publishes_empty_or_partial_jsonl() {
        for partial in [b"".as_slice(), b"{\"schema_version\":".as_slice()] {
            let sandbox = Sandbox::new();
            let mut d = sandbox.store();
            let result = d.start_with(
                |capture, _| {
                    capture.file.write_all(partial)?;
                    Err(io::Error::other("synthetic initial write failure"))
                },
                |_, _| panic!("failed header must not be published"),
            );
            assert!(result.is_err());
            assert!(d.active_id().is_none());
            assert!(d.list().unwrap().is_empty());
            assert_eq!(fs::read_dir(&sandbox.0).unwrap().count(), 0);
        }
    }

    #[test]
    fn failed_publication_cleans_only_its_staging_file_and_preserves_collision() {
        let sandbox = Sandbox::new();
        let mut d = sandbox.store();
        let result = d.start_with(
            |capture, fields| {
                Diagnostics::write_record(capture, "capture_started", fields)?;
                capture.file.sync_data()
            },
            |pending, published| {
                fs::write(published, b"user-owned collision")?;
                fs::hard_link(pending, published)
            },
        );
        assert!(result.is_err());
        assert!(d.active_id().is_none());
        let entries: Vec<_> = fs::read_dir(&sandbox.0).unwrap().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            fs::read(entries[0].as_ref().unwrap().path()).unwrap(),
            b"user-owned collision"
        );
        assert_eq!(d.purge(0).unwrap(), 0);
    }

    #[test]
    fn abandoned_staging_is_not_misidentified_as_user_capture_history() {
        let sandbox = Sandbox::new();
        fs::create_dir_all(&sandbox.0).unwrap();
        let empty = sandbox
            .0
            .join("capture-00000000000000000000000000000000.pending");
        let partial = sandbox
            .0
            .join("capture-11111111111111111111111111111111.pending");
        fs::write(&empty, b"").unwrap();
        fs::write(&partial, b"{\"schema_version\":").unwrap();
        let mut d = sandbox.store();
        let id = d.start().unwrap();
        d.stop(StopReason::User).unwrap();
        assert_eq!(d.list().unwrap().len(), 1);
        assert_eq!(d.purge(1).unwrap(), 1);
        assert!(empty.exists());
        assert!(partial.exists());
        assert!(d.download(&id).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_linked_files_and_directories() {
        use std::os::unix::fs::symlink;
        let sandbox = Sandbox::new();
        let mut d = sandbox.store();
        let id = d.start().unwrap();
        d.stop(StopReason::User).unwrap();
        let alias = "11111111111111111111111111111111";
        symlink(
            sandbox.0.join(format!("capture-{id}.jsonl")),
            sandbox.0.join(format!("capture-{alias}.jsonl")),
        )
        .unwrap();
        assert!(d.download(alias).is_err());
        assert_eq!(d.list().unwrap().len(), 1);
        let linked = Sandbox::new();
        symlink(&sandbox.0, &linked.0).unwrap();
        assert!(linked.store().start().is_err());
        // Remove the link itself, never recursively traverse it in cleanup.
        fs::remove_file(&linked.0).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_reparse_files_and_directories_cannot_be_downloaded_or_purged() {
        use std::os::windows::fs::{symlink_dir, symlink_file, MetadataExt};

        fn supported(result: io::Result<()>) -> bool {
            match result {
                Ok(()) => true,
                // Windows CI may lack Developer Mode / SeCreateSymbolicLinkPrivilege,
                // or its temporary filesystem may not support symbolic links.
                Err(error) if matches!(error.raw_os_error(), Some(1314) | Some(50)) => {
                    eprintln!(
                        "SKIP Windows reparse test: symlink privilege or filesystem support unavailable ({:?})",
                        error.raw_os_error()
                    );
                    false
                }
                Err(error) => panic!("unexpected synthetic symlink creation failure: {error}"),
            }
        }

        let target = Sandbox::new();
        let mut target_store = target.store();
        let target_id = target_store.start().unwrap();
        target_store.stop(StopReason::User).unwrap();
        let target_path = target.0.join(format!("capture-{target_id}.jsonl"));
        let original = fs::read(&target_path).unwrap();

        let sandbox = Sandbox::new();
        let mut d = sandbox.store();
        let owned_id = d.start().unwrap();
        d.stop(StopReason::User).unwrap();
        // Match the target's real ID/header: exclusion must rely on the reparse
        // guard, rather than merely rejecting a mismatching ownership header.
        let alias = sandbox.0.join(format!("capture-{target_id}.jsonl"));
        if !supported(symlink_file(&target_path, &alias)) {
            return;
        }
        assert_ne!(
            fs::symlink_metadata(&alias).unwrap().file_attributes() & 0x400,
            0
        );
        assert!(d.download(&target_id).is_err());
        let listed = d.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].capture_id, owned_id);
        assert!(d.purge(2).is_err());
        assert_eq!(d.purge(1).unwrap(), 1);
        assert!(fs::symlink_metadata(&alias).is_ok());
        assert!(d.list().unwrap().is_empty());
        assert_eq!(d.purge(0).unwrap(), 0);
        assert_eq!(fs::read(&target_path).unwrap(), original);
        fs::remove_file(&alias).unwrap();

        let linked = Sandbox::new();
        if !supported(symlink_dir(&target.0, &linked.0)) {
            return;
        }
        assert_ne!(
            fs::symlink_metadata(&linked.0).unwrap().file_attributes() & 0x400,
            0
        );
        let mut linked_store = linked.store();
        assert!(linked_store.start().is_err());
        assert!(linked_store.list().is_err());
        assert!(linked_store.download(&target_id).is_err());
        assert!(linked_store.purge(1).is_err());
        assert_eq!(fs::read(&target_path).unwrap(), original);
        // Remove the directory link itself; never traverse its target in cleanup.
        fs::remove_dir(&linked.0).unwrap();
    }
}
