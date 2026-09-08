//! Keeps exactly one `RemoteAudio serve` child alive for the length of the
//! logon session.
//!
//! v0.1 pointed HKCU Run straight at `serve --no-open`: a crash left the host
//! silent until somebody walked over to it. From v0.2 the Run value points here
//! instead. The supervisor owns the child, restarts it with a bounded backoff,
//! and (from the tray work that follows) carries the notification-area icon.
//!
//! The monitor lives on its own thread because the tray needs the *main* thread
//! for the Windows message loop. Everything the tray has to know is reachable
//! through [`Handle`], which is cheap to clone and safe to poll from that loop.

use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often the monitor wakes up when no request arrives. Also the worst-case
/// delay between the child dying and the tray showing it.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Set on the child so `POST /api/restart` knows a supervisor is listening.
/// Without one, asking for a restart would kill the server for good.
pub const SUPERVISED_ENV: &str = "AUDIOREMOTE_SUPERVISED";

/// The line the server prints on stdout to ask for a restart.
///
/// A line on the pipe we already own, rather than a second socket: the app's
/// whole security posture is "one listener, guarded". Opening a control port —
/// even on loopback — to talk to our own parent would add an unauthenticated
/// way to bounce the server, and every local process can reach loopback.
pub const RESTART_MARKER: &str = "__audioremote_restart__";

/// How long a freshly installed build has to stay up before we stop holding its
/// predecessor in reserve.
///
/// A build that is going to fail on this host — missing runtime, port taken,
/// panic on startup — does it in the first second or two. Ten seconds is
/// generous enough to cover a slow first COM init and short enough that nobody
/// watching the tray wonders whether the update took.
const PROBATION: Duration = Duration::from_secs(10);

/// Where a new build is dropped for the next restart to pick up.
///
/// Under `%LOCALAPPDATA%` because that is writable by the logon user without
/// elevation, which is the whole point: the deploy step is a file copy over a
/// redirected drive, not an installer.
#[cfg(windows)]
pub fn staging_exe_path() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(
        PathBuf::from(local)
            .join("audioremote")
            .join("staging")
            .join("RemoteAudio.exe"),
    )
}

/// Whether this process is a supervised child.
#[cfg(windows)]
pub fn is_supervised() -> bool {
    std::env::var_os(SUPERVISED_ENV).is_some_and(|v| v == "1")
}

/// Ask the supervising parent for a restart.
///
/// Returns once the request is on the wire. The answer, if it comes, is this
/// process being killed — so callers must reply to their own client first.
#[cfg(windows)]
pub fn ask_parent_to_restart() {
    use std::io::Write;

    println!("{RESTART_MARKER}");
    // stdout is a pipe here, and a pipe is block-buffered in some runtimes even
    // when a terminal would be line-buffered. An unflushed request is a restart
    // that silently never happens.
    let _ = io::stdout().flush();
}

/// Restarts granted inside one [`RESTART_WINDOW`]. The death that would need one
/// *more* than this drops the supervisor to [`SupervisorState::Failed`] instead
/// of restarting: a server that can never start (bad `bind`, port already taken)
/// would otherwise spin forever and bury the real error in its own log.
pub const MAX_RESTARTS_PER_WINDOW: u32 = 5;

/// Sliding window for the restart budget. Surviving longer than this resets the
/// counter, so a machine that crashes once a day never reaches `Failed`.
pub const RESTART_WINDOW: Duration = Duration::from_secs(10 * 60);

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Ask the monitor to stop and take the child with it.
///
/// An atomic rather than a channel send because the console control handler
/// runs on a thread of Windows' choosing and `mpsc::Sender` is not `Sync`.
pub fn request_shutdown() {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// What the supervisor is doing right now. The tray tooltip renders exactly
/// this and nothing else, so anything the user needs to see has to be a variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorState {
    /// A child is alive.
    Running,
    /// The child died; waiting out the backoff before the next attempt.
    Restarting,
    /// Stopped on request. Nothing restarts until [`Request::Start`].
    Stopped,
    /// Gave up after exhausting the restart budget.
    Failed,
    /// The child stopped, and the user chose not to retry it automatically.
    AutoRestartOff,
}

impl SupervisorState {
    /// Key into `web/lang/*.json`. The variant names must never reach the UI
    /// untranslated, so the tray looks the state up through this.
    pub fn lang_key(self) -> &'static str {
        match self {
            Self::Running => "tray.state.running",
            Self::Restarting => "tray.state.restarting",
            Self::Stopped => "tray.state.stopped",
            Self::Failed => "tray.state.failed",
            Self::AutoRestartOff => "tray.state.autoRestartOff",
        }
    }
}

/// A message to the monitor. Sent by the tray menu and, later, by the
/// `POST /api/restart` handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Kill the child and start a fresh one immediately.
    Restart,
    /// Kill the child and stay down.
    Stop,
    /// Start again after `Stop` or `Failed`.
    Start,
    /// Kill the child and end the supervisor.
    Quit,
}

/// Remote control for a running monitor.
#[derive(Clone)]
pub struct Handle {
    tx: Sender<Request>,
    state: Arc<Mutex<SupervisorState>>,
}

impl Handle {
    pub fn state(&self) -> SupervisorState {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Fire-and-forget. A dead monitor means the process is on its way out
    /// anyway, so there is nothing useful to report back to a menu click.
    pub fn send(&self, request: Request) {
        let _ = self.tx.send(request);
    }
}

/// Delay before restart attempt `attempt` (1-based): 1s, 2s, 4s, 8s, 16s, then
/// flat.
///
/// A pure function of the attempt count — no clock, no state — so the schedule
/// can be asserted in `cargo test` without sleeping through it.
pub fn backoff_delay(attempt: u32) -> Duration {
    let steps = attempt.clamp(1, MAX_RESTARTS_PER_WINDOW) - 1;
    Duration::from_secs(1u64 << steps)
}

/// Spawn the monitor thread and return the control handle plus its join handle.
///
/// The caller must keep the [`Handle`] alive for as long as it wants the child
/// supervised: dropping every handle disconnects the channel, which the monitor
/// reads as "nobody can ask for anything again" and shuts down.
pub fn start(auto_restart: bool) -> io::Result<(Handle, JoinHandle<()>)> {
    let exe = std::env::current_exe()?;
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(Mutex::new(SupervisorState::Restarting));
    let handle = Handle {
        tx,
        state: Arc::clone(&state),
    };
    let joiner = std::thread::Builder::new()
        .name("audioremote-supervisor".to_string())
        .spawn(move || Monitor::new(exe, state, auto_restart).run(rx))?;
    Ok((handle, joiner))
}

/// Route Ctrl+C and console-close to [`request_shutdown`] so the monitor kills
/// the child before the process disappears.
///
/// Without this the default handler terminates the supervisor outright: the job
/// object below still reaps the child, but only after Windows notices, and the
/// child gets no chance to shut its listener down cleanly. Silently does
/// nothing when there is no console (release build, or launched from Explorer).
#[cfg(windows)]
pub fn install_console_ctrl_handler() {
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::System::Console::SetConsoleCtrlHandler;

    unsafe extern "system" fn handler(_ctrl_type: u32) -> BOOL {
        request_shutdown();
        // TRUE = handled, so Windows does not terminate us here. The monitor
        // notices within one poll interval and unwinds the normal exit path.
        BOOL(1)
    }

    let _ = unsafe { SetConsoleCtrlHandler(Some(handler), true) };
}

// ---- monitor ----------------------------------------------------------------

struct Monitor {
    exe: PathBuf,
    shared: Arc<Mutex<SupervisorState>>,
    /// Whether an unexpected child exit should be retried. This is a resident
    /// mode preference; direct `serve` never creates a Monitor at all.
    auto_restart: bool,
    /// Requests forwarded by a child over its stdout pipe.
    ///
    /// A channel of its own, not the caller's: the monitor has to hold a sender
    /// to hand out to each reader thread, and a sender it owns would keep the
    /// caller's channel alive forever — turning "every handle was dropped" from
    /// a clean exit into a hang.
    child_tx: Sender<Request>,
    child_rx: Receiver<Request>,
    job: Option<Job>,
    child: Option<Child>,
    /// Restarts already spent in the current window.
    failures: u32,
    window_start: Instant,
    /// When the next spawn is due. `None` in `Running`, `Stopped` and `Failed`.
    retry_at: Option<Instant>,
    /// The build a just-installed update displaced, and the deadline it has to
    /// survive to. Both `None` unless an update is on probation right now.
    rollback_from: Option<PathBuf>,
    probation_until: Option<Instant>,
}

impl Monitor {
    fn new(exe: PathBuf, shared: Arc<Mutex<SupervisorState>>, auto_restart: bool) -> Self {
        let (child_tx, child_rx) = mpsc::channel();
        Self {
            exe,
            shared,
            auto_restart,
            child_tx,
            child_rx,
            job: Job::new(),
            child: None,
            failures: 0,
            window_start: Instant::now(),
            retry_at: None,
            rollback_from: None,
            probation_until: None,
        }
    }

    fn run(mut self, rx: Receiver<Request>) {
        // Backups from earlier sessions only become deletable once the process
        // that had them mapped is gone — which, by the time we run, it is.
        sweep_backups(&self.exe);
        self.start_child_now();
        loop {
            if SHUTDOWN.load(Ordering::SeqCst) {
                break;
            }
            match rx.recv_timeout(POLL_INTERVAL) {
                Ok(request) => {
                    if !self.handle(request) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                // Every handle was dropped, so no further request can arrive.
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if !self.drain_child_requests() {
                break;
            }
            self.tick();
        }
        self.kill_child();
        self.set_state(SupervisorState::Stopped);
    }

    /// Act on anything a child asked for since the last pass.
    ///
    /// Collected before handling because `handle` takes `&mut self` and the
    /// receiver lives there too. Returns `false` when the monitor should exit.
    fn drain_child_requests(&mut self) -> bool {
        let pending: Vec<Request> = self.child_rx.try_iter().collect();
        for request in pending {
            if !self.handle(request) {
                return false;
            }
        }
        true
    }

    /// Returns `false` when the monitor should exit.
    fn handle(&mut self, request: Request) -> bool {
        match request {
            Request::Restart => {
                // A deliberate restart is not a crash. It must not eat into the
                // budget, or three taps on the tray menu would land the
                // supervisor in `Failed` with nothing actually wrong.
                self.kill_child();
                self.reset_budget();
                // The only moment the exe on disk is not in use by a child, so
                // the only moment a staged build can take its place.
                self.install_staged_build();
                self.start_child_now();
            }
            Request::Stop => {
                self.kill_child();
                self.retry_at = None;
                self.set_state(SupervisorState::Stopped);
            }
            Request::Start => {
                if self.child.is_none() {
                    self.reset_budget();
                    // Also staged here, not just on `Restart`: this is the way
                    // out of `Failed`. A build that crashed through the whole
                    // budget is fixed by staging a good one and pressing Start,
                    // and skipping the install would leave that door shut.
                    self.install_staged_build();
                    self.start_child_now();
                }
            }
            Request::Quit => return false,
        }
        true
    }

    fn tick(&mut self) {
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                // Still alive: nothing to do beyond seeing out any probation.
                Ok(None) => {
                    self.settle_probation();
                    return;
                }
                Ok(Some(status)) => {
                    self.child = None;
                    if self.roll_back(&format!("the new build exited ({status})")) {
                        return;
                    }
                    self.handle_child_failure(&format!("server exited ({status})"));
                }
                Err(e) => {
                    // The handle is unusable; treat it as a death rather than
                    // polling a corpse forever.
                    self.child = None;
                    self.handle_child_failure(&format!("cannot poll the server ({e})"));
                }
            }
        }

        if let Some(at) = self.retry_at {
            if Instant::now() >= at {
                self.retry_at = None;
                self.start_child_now();
            }
        }
    }

    fn start_child_now(&mut self) {
        match self.spawn() {
            Ok(child) => {
                self.child = Some(child);
                self.set_state(SupervisorState::Running);
            }
            Err(e) => {
                let reason = format!("cannot start {}: {e}", self.exe.display());
                // A staged build too broken to even load lands here. Put the
                // old one back before the backoff starts, or every retry in the
                // budget is spent on the same corpse.
                if self.roll_back(&reason) {
                    return;
                }
                self.handle_child_failure(&reason);
            }
        }
    }

    fn handle_child_failure(&mut self, reason: &str) {
        if !self.auto_restart {
            eprintln!(
                "[supervisor] {reason}; automatic restart is disabled, leaving the server stopped"
            );
            self.retry_at = None;
            self.set_state(SupervisorState::AutoRestartOff);
            return;
        }
        self.schedule_restart(reason);
    }

    fn spawn(&mut self) -> io::Result<Child> {
        // `--no-open` because the supervisor is what runs at logon: a browser
        // tab every time you sign in is not a feature. Running `RemoteAudio
        // serve` by hand still opens one.
        let mut child = Command::new(&self.exe)
            .args(["serve", "--no-open"])
            .env(SUPERVISED_ENV, "1")
            // stdout is the child's channel back to us; stderr stays inherited
            // so a crash message goes wherever ours goes.
            .stdout(Stdio::piped())
            .spawn()?;
        if let Some(job) = self.job.as_ref() {
            job.assign(&child);
        }
        if let Some(stdout) = child.stdout.take() {
            if let Err(e) = self.watch_child_output(stdout) {
                // Nothing would drain the pipe, so the child would wedge as soon
                // as it filled — and a wedged server looks alive to `try_wait`.
                // Kill it now and let the ordinary backoff try again.
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
        Ok(child)
    }

    /// Read the child's stdout on its own thread, watching for the restart
    /// marker and echoing everything else.
    ///
    /// A thread and not a poll: reading a pipe blocks, and the monitor loop has
    /// a tray waiting on it. The thread ends by itself when the child dies and
    /// the pipe closes.
    fn watch_child_output(&self, stdout: std::process::ChildStdout) -> io::Result<()> {
        let requests = self.child_tx.clone();
        std::thread::Builder::new()
            .name("audioremote-child-stdout".to_string())
            .spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if line.trim() == RESTART_MARKER {
                        // Queued like any other request, so it goes through the
                        // same kill / stage / spawn path as the tray menu.
                        if requests.send(Request::Restart).is_err() {
                            break;
                        }
                        continue;
                    }
                    // Goes nowhere in a release build, where the supervisor has
                    // no stdio handle of its own — `println!` is a no-op there,
                    // not an error. In `cargo run` it lands in the terminal.
                    println!("{}", crate::sanitize_terminal_text(&line));
                }
            })
            .map(|_| ())
    }

    // ---- staged builds ------------------------------------------------------

    /// Move a staged build into place, if one is waiting.
    ///
    /// Only ever called with no child running: the exe is a mapped image while a
    /// process runs it, and Windows will not let it be replaced. It *will* let
    /// it be renamed, which is what makes the whole scheme work — see
    /// `docs/local/pending_remote-restart-from-guest.md`.
    ///
    /// Note what this does not do: the *supervisor's* own image is the file we
    /// just renamed away, so this process keeps running the old code until the
    /// next logon. Only the server child is updated today.
    fn install_staged_build(&mut self) {
        let Some(staged) = staging_exe_path() else {
            return;
        };
        match std::fs::metadata(&staged) {
            // Nothing staged is the normal case, and not worth a word.
            Err(_) => return,
            Ok(meta) if meta.len() == 0 => {
                eprintln!(
                    "[supervisor] ignoring an empty staged build at {}",
                    staged.display()
                );
                return;
            }
            Ok(_) => {}
        }

        let Some(backup) = free_backup_path(&self.exe) else {
            eprintln!(
                "[supervisor] no free backup name next to {}; leaving the staged build alone",
                self.exe.display()
            );
            return;
        };
        if let Err(e) = std::fs::rename(&self.exe, &backup) {
            eprintln!("[supervisor] cannot move the running build aside ({e}); keeping it");
            return;
        }
        if let Err(e) = std::fs::copy(&staged, &self.exe) {
            eprintln!("[supervisor] cannot install the staged build ({e}); restoring the old one");
            // Nothing is at the exe path right now, so this cannot clobber
            // anything — and failing it would leave the host with no exe at all.
            if let Err(e) = std::fs::rename(&backup, &self.exe) {
                eprintln!(
                    "[supervisor] RESTORE FAILED ({e}). The working build is at {}",
                    backup.display()
                );
            }
            return;
        }
        // Consumed, so the next restart does not install the same build again
        // and burn another backup slot.
        let _ = std::fs::remove_file(&staged);

        eprintln!(
            "[supervisor] installed the staged build; {}s probation",
            PROBATION.as_secs()
        );
        self.rollback_from = Some(backup);
        self.probation_until = Some(Instant::now() + PROBATION);
    }

    /// Put the displaced build back, if an update is still on probation.
    ///
    /// Returns whether it did — the caller skips its own error handling when it
    /// did, because a restart is already scheduled.
    fn roll_back(&mut self, reason: &str) -> bool {
        let Some(backup) = self.rollback_from.take() else {
            return false;
        };
        self.probation_until = None;

        eprintln!("[supervisor] {reason}; rolling back to the previous build");
        // Remove the failed build first: `rename` onto an existing path fails on
        // Windows, and being left with no exe is worse than being left with a
        // bad one.
        if let Err(e) = std::fs::remove_file(&self.exe) {
            eprintln!(
                "[supervisor] cannot remove the failed build ({e}); the old one stays at {}",
                backup.display()
            );
            return false;
        }
        if let Err(e) = std::fs::rename(&backup, &self.exe) {
            eprintln!(
                "[supervisor] ROLLBACK FAILED ({e}). The working build is at {}",
                backup.display()
            );
            return false;
        }
        // Straight back up, without a backoff: the build being restored is the
        // one that was running a moment ago.
        self.reset_budget();
        self.start_child_now();
        true
    }

    /// Retire the rollback copy once the new build has proven itself.
    fn settle_probation(&mut self) {
        let Some(until) = self.probation_until else {
            return;
        };
        if Instant::now() < until {
            return;
        }
        self.probation_until = None;
        if let Some(backup) = self.rollback_from.take() {
            // Expected to fail while we run: this file is our own mapped image
            // under its new name. `sweep_backups` collects it next session.
            let _ = std::fs::remove_file(&backup);
        }
        eprintln!("[supervisor] the new build survived its probation");
    }

    fn schedule_restart(&mut self, reason: &str) {
        if self.window_start.elapsed() >= RESTART_WINDOW {
            self.failures = 0;
            self.window_start = Instant::now();
        }
        self.failures += 1;

        if self.failures > MAX_RESTARTS_PER_WINDOW {
            eprintln!(
                "[supervisor] {reason}; giving up after {MAX_RESTARTS_PER_WINDOW} restarts in {} minutes",
                RESTART_WINDOW.as_secs() / 60
            );
            self.retry_at = None;
            self.set_state(SupervisorState::Failed);
            return;
        }

        let delay = backoff_delay(self.failures);
        eprintln!(
            "[supervisor] {reason}; restart {}/{MAX_RESTARTS_PER_WINDOW} in {}s",
            self.failures,
            delay.as_secs()
        );
        self.retry_at = Some(Instant::now() + delay);
        self.set_state(SupervisorState::Restarting);
    }

    fn reset_budget(&mut self) {
        self.failures = 0;
        self.window_start = Instant::now();
        self.retry_at = None;
    }

    fn kill_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            // Reap it. Skipping the wait leaves a zombie handle, and the next
            // `tasklist` still lists a server that stopped answering long ago.
            let _ = child.wait();
        }
    }

    fn set_state(&self, state: SupervisorState) {
        *self.shared.lock().unwrap_or_else(|e| e.into_inner()) = state;
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.kill_child();
    }
}

// ---- backup files -----------------------------------------------------------

/// How many displaced builds may pile up beside the exe in one session.
///
/// There is normally one. A second only appears when the first is still mapped
/// by this very process, which is exactly what happens when someone updates
/// twice without signing out.
const MAX_BACKUPS: u32 = 8;

/// `RemoteAudio.exe` → `RemoteAudio.exe.old`, `.old.1`, `.old.2`, …
fn backup_path(exe: &Path, index: u32) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(if index == 0 {
        ".old".to_string()
    } else {
        format!(".old.{index}")
    });
    exe.with_file_name(name)
}

/// First backup name not already taken.
///
/// "Taken" includes a file we cannot delete: the previous session's backup stays
/// locked for as long as this process runs on it, so reusing the name would fail
/// halfway through the swap.
fn free_backup_path(exe: &Path) -> Option<PathBuf> {
    (0..MAX_BACKUPS)
        .map(|i| backup_path(exe, i))
        .find(|p| !p.exists())
}

/// Delete every backup left beside `exe`. Silent about failures — one that is
/// still locked simply waits for the session after this one.
fn sweep_backups(exe: &Path) {
    for index in 0..MAX_BACKUPS {
        let path = backup_path(exe, index);
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

// ---- single instance --------------------------------------------------------

/// Proof that this process is the supervisor for this logon session. Must stay
/// alive for as long as the process does; Windows releases it either way when
/// the process ends, so a crash cannot leave a stale lock behind.
#[cfg(windows)]
pub struct InstanceLock(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for InstanceLock {
    fn drop(&mut self) {
        let windows::Win32::Foundation::HANDLE(raw) = self.0;
        if raw.is_null() {
            return;
        }
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Claim the right to be *the* supervisor in this logon session.
///
/// `None` means one is already running, and the caller should say so and leave.
/// Letting a second one start is not harmless: only one process can bind port
/// 17650, so the newcomer's child dies on every attempt, spends the whole
/// restart budget finding that out, and settles into `Failed` — leaving a second
/// notification-area icon that reads "stopped after repeated failures" next to
/// the one that works. Found on the host on 2026-08-04, where a stray second
/// launch also made the first V1 measurement read as a failure.
///
/// `Local\` rather than `Global\`: the local namespace is per logon session,
/// which is exactly the scope that matters — audioremote has to run in the
/// interactive session that owns the audio devices — and the global namespace
/// needs a privilege a standard user does not have.
#[cfg(windows)]
pub fn acquire_instance_lock() -> Option<InstanceLock> {
    claim(windows::core::w!("Local\\audioremote.supervisor"))
}

#[cfg(windows)]
fn claim(name: windows::core::PCWSTR) -> Option<InstanceLock> {
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
    use windows::Win32::System::Threading::CreateMutexW;

    let handle = match unsafe { CreateMutexW(None, false, name) } {
        Ok(handle) => handle,
        // Fail open. Refusing to start because a mutex could not be created
        // would be a worse fault than the duplicate this exists to prevent.
        Err(_) => return Some(InstanceLock(HANDLE::default())),
    };
    // `CreateMutexW` reports success either way; the last-error code is the only
    // thing that says somebody got here first.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return None;
    }
    Some(InstanceLock(handle))
}

#[cfg(not(windows))]
pub struct InstanceLock;

#[cfg(not(windows))]
pub fn acquire_instance_lock() -> Option<InstanceLock> {
    Some(InstanceLock)
}

// ---- job object -------------------------------------------------------------

/// A Windows job object that kills its members when the last handle closes.
///
/// This is what makes `taskkill /F` on the supervisor take the server with it.
/// Without it the child survives as an orphan still holding port 17650, and
/// every supervisor started afterwards fails to bind — the one failure mode the
/// restart budget cannot recover from, because the port never frees up.
#[cfg(windows)]
struct Job(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Job {
    fn new() -> Option<Self> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        let handle = match unsafe { CreateJobObjectW(None, None) } {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[supervisor] no job object ({e}); the server may outlive a forced kill");
                return None;
            }
        };

        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if let Err(e) = set {
            eprintln!("[supervisor] job object rejected the kill-on-close limit ({e})");
            unsafe {
                let _ = CloseHandle(handle);
            }
            return None;
        }
        Some(Self(handle))
    }

    fn assign(&self, child: &Child) {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::AssignProcessToJobObject;

        if let Err(e) = unsafe { AssignProcessToJobObject(self.0, HANDLE(child.as_raw_handle())) } {
            eprintln!("[supervisor] cannot put the server in the job object ({e})");
        }
    }
}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(not(windows))]
struct Job;

#[cfg(not(windows))]
impl Job {
    fn new() -> Option<Self> {
        None
    }
    fn assign(&self, _child: &Child) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_then_flattens() {
        let seconds: Vec<u64> = (1..=8).map(|n| backoff_delay(n).as_secs()).collect();
        assert_eq!(seconds, vec![1, 2, 4, 8, 16, 16, 16, 16]);
    }

    #[test]
    fn backoff_never_returns_zero() {
        // `attempt` is 1-based. A stray 0 must not collapse into a spin loop
        // that restarts the server as fast as it can die.
        assert_eq!(backoff_delay(0), Duration::from_secs(1));
    }

    #[test]
    fn the_whole_budget_fits_inside_one_window() {
        // If the backoff schedule ever outgrew the window, the counter would
        // reset before the budget ran out and `Failed` would be unreachable.
        let total: u64 = (1..=MAX_RESTARTS_PER_WINDOW)
            .map(|n| backoff_delay(n).as_secs())
            .sum();
        assert!(
            total < RESTART_WINDOW.as_secs(),
            "backoff total {total}s must stay under the {}s window",
            RESTART_WINDOW.as_secs()
        );
    }

    #[test]
    fn backups_sit_next_to_the_exe_and_never_collide() {
        let exe = PathBuf::from(r"C:\tools\RemoteAudio\RemoteAudio.exe");
        assert_eq!(
            backup_path(&exe, 0),
            PathBuf::from(r"C:\tools\RemoteAudio\RemoteAudio.exe.old")
        );
        assert_eq!(
            backup_path(&exe, 1),
            PathBuf::from(r"C:\tools\RemoteAudio\RemoteAudio.exe.old.1")
        );

        let names: std::collections::HashSet<PathBuf> =
            (0..MAX_BACKUPS).map(|i| backup_path(&exe, i)).collect();
        assert_eq!(
            names.len(),
            MAX_BACKUPS as usize,
            "backup names must be unique"
        );
        for name in &names {
            assert_eq!(
                name.parent(),
                exe.parent(),
                "{name:?} left the exe's folder"
            );
        }
    }

    #[test]
    fn a_free_backup_name_skips_the_ones_already_there() {
        let dir = std::env::temp_dir().join(format!("audioremote-sup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let exe = dir.join("RemoteAudio.exe");

        assert_eq!(free_backup_path(&exe), Some(backup_path(&exe, 0)));
        std::fs::write(backup_path(&exe, 0), b"x").expect("write backup");
        assert_eq!(free_backup_path(&exe), Some(backup_path(&exe, 1)));

        sweep_backups(&exe);
        assert!(!backup_path(&exe, 0).exists(), "sweep left a backup behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_restart_marker_cannot_be_mistaken_for_a_log_line() {
        // It shares stdout with the server's own output, so it has to be a
        // string nothing else would ever print.
        assert!(RESTART_MARKER.starts_with("__audioremote"));
        assert!(!RESTART_MARKER.contains(char::is_whitespace));
    }

    #[cfg(windows)]
    #[test]
    fn a_second_supervisor_is_refused() {
        // A name of its own. The real one may be held by an audioremote actually
        // running on this machine, and a test that fails because the product is
        // working is worse than no test.
        let name = windows::core::w!("Local\\audioremote.supervisor.test");
        let first = claim(name).expect("the first claim must succeed");
        assert!(claim(name).is_none(), "a second claim must be refused");
        drop(first);
        assert!(
            claim(name).is_some(),
            "the lock must be released when the holder goes away"
        );
    }

    #[test]
    fn every_state_has_a_translation_key() {
        for state in [
            SupervisorState::Running,
            SupervisorState::Restarting,
            SupervisorState::Stopped,
            SupervisorState::Failed,
            SupervisorState::AutoRestartOff,
        ] {
            let key = state.lang_key();
            assert!(key.starts_with("tray.state."), "{state:?} -> {key}");
        }
    }
}
