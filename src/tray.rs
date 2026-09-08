//! Notification-area icon for the resident supervisor.
//!
//! Hand-rolled on `Shell_NotifyIcon` instead of a tray crate, for one concrete
//! reason: the first-run notification has to be **clickable** — one click
//! registers autostart, adds the firewall rule and copies the share URL — and a
//! balloon click arrives as `NIN_BALLOONUSERCLICK` on the icon's own window.
//! No tray crate surfaces that callback. Owning the window keeps it in reach
//! and costs no dependency, since the `windows` crate is already here for Core
//! Audio.
//!
//! Threading: Windows delivers these messages to the thread that created the
//! window, so [`run`] must be called on the **main** thread and never returns
//! until the user quits. The supervisor's own monitor loop lives on its own
//! thread and is driven from here through [`supervisor::Handle`].

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, SHGetKnownFolderPath, Shell_NotifyIconW, KF_FLAG_DEFAULT, NIF_ICON,
    NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW,
    NOTIFYICON_VERSION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics,
    GetWindowLongPtrW, KillTimer, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, TrackPopupMenu,
    TranslateMessage, GWLP_USERDATA, HICON, HMENU, MENU_ITEM_FLAGS, MF_GRAYED, MF_POPUP,
    MF_SEPARATOR, MF_STRING, MSG, SM_CXSMICON, SM_CYSMICON, TPM_LEFTALIGN, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};

use crate::assets;
use crate::callout;
use crate::config::{self, Config};
use crate::lang::Strings;
use crate::net::{self, ShareEntry};
use crate::supervisor::{self, Request, SupervisorState};
use crate::welcome;

/// Our callback message. Anything in the `WM_APP` range is ours to define.
const WM_TRAY: u32 = WM_APP + 1;

/// `RegisterWindowMessageW("TaskbarCreated")`, resolved once in [`run`]. The
/// shell broadcasts it whenever Explorer restarts — and every notification-area
/// icon dies with that restart. Without listening for it, killing and restarting
/// Explorer permanently removes the app's only UI while the process lives on.
static WM_TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

/// Identifies our one icon within this window. Shared with [`crate::callout`],
/// which has to name the same icon to ask the shell where it ended up.
const ICON_ID: u32 = 1;

const TIMER_ID: usize = 1;
/// Tooltip refresh cadence. Matches the supervisor's own poll interval, so the
/// tray is never more than a poll behind what the monitor knows.
const TIMER_MS: u32 = 1000;

// Menu command ids. Share entries start at `ID_SHARE_BASE` because there is one
// per NIC and the count is only known when the menu opens.
const ID_OPEN: usize = 1;
const ID_RESTART: usize = 2;
const ID_TOGGLE: usize = 3;
const ID_QUIT: usize = 4;
const ID_SHARE_BASE: usize = 100;
const ID_SHORTCUT_BASE: usize = 200;

/// Show the tray icon and pump messages until the user picks Quit.
///
/// Returns as soon as the window is gone; the caller is responsible for telling
/// the supervisor to stop.
pub fn run(handle: supervisor::Handle) -> std::io::Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None).map_err(to_io)?;
        let class_name = w!("AudioRemoteTray");

        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        // A zero return means the class could not be registered. It is also what
        // a *duplicate* registration returns, which cannot happen here (one tray
        // per process), so treating it as fatal is safe.
        if RegisterClassW(&class) == 0 {
            return Err(std::io::Error::last_os_error());
        }

        // Before the window exists, so no message can be dispatched before the
        // id it is compared against is known.
        WM_TASKBAR_CREATED.store(
            RegisterWindowMessageW(w!("TaskbarCreated")),
            Ordering::Relaxed,
        );

        // Never shown. It exists to receive the icon's callbacks — but it is a
        // real top-level window rather than a message-only one, because
        // `TrackPopupMenu` needs a window that can take the foreground or the
        // menu refuses to close when you click away from it.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("Remote Audio"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            None,
        )
        .map_err(to_io)?;

        let config_path = config::default_config_path();
        let cfg = load_config(&config_path);
        // `RefCell` rather than a bare pointer: the popup menu and the elevation
        // prompt both run their own modal message loops, so the window procedure
        // *will* be re-entered while an earlier call is still on the stack.
        // Handing out two `&mut Tray` there would be undefined behaviour; here
        // the inner call simply finds the cell busy and skips itself, which is
        // also the right behaviour — a tooltip refresh can wait for the menu to
        // close.
        let tray = Box::new(RefCell::new(Tray {
            hwnd,
            icon: assets::load_icon(GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)),
            supervisor: handle,
            strings: Strings::load(&cfg.tray.ui_language),
            config_path,
            last_tooltip: String::new(),
            shares: Vec::new(),
        }));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*tray as *const RefCell<Tray> as isize);

        tray.borrow().add_icon()?;
        tray.borrow_mut().refresh_tooltip();
        SetTimer(hwnd, TIMER_ID, TIMER_MS, None);

        if cfg.tray.show_welcome {
            // Its own `Strings`, rather than a borrow of the tray's: the dialog
            // pumps this thread's messages while it is up, and any of them can
            // re-enter the window procedure and want the cell.
            let strings = Strings::load(&cfg.tray.ui_language);
            let choice = welcome::ask(&strings, cfg.tray.setup_done, crate::autostart::packaged());
            if !choice.keep_showing {
                tray.borrow().stop_showing_welcome();
            }
            match choice.action {
                welcome::WelcomeAction::Setup => {
                    let outcome = tray.borrow_mut().run_first_run_setup();
                    welcome::report(&strings, outcome);
                }
                welcome::WelcomeAction::Open => tray.borrow().open_ui(),
                welcome::WelcomeAction::CopyShare => {
                    tray.borrow().copy_first_share();
                }
                welcome::WelcomeAction::CreateShortcut => {
                    tray.borrow().create_first_shortcut();
                }
                welcome::WelcomeAction::None => {}
            }
            // Last, once every dialog is off the screen: an arrow pointing at
            // where the app actually went. Words alone leave the user hunting
            // through a row of icons, and this is the moment they have to find
            // it — the app has no other window to come back to.
            callout::point_at_tray_icon(&strings, hwnd, ICON_ID);
        }

        let mut msg = MSG::default();
        // `> 0` and not `as_bool()`: `GetMessageW` answers -1 on error, which is
        // "true" to a BOOL and would spin this loop forever on a bad message.
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Destroying the window already retires its timer and icon. Doing it
        // explicitly covers the other way out of the loop — a `WM_QUIT` posted
        // by the shell at logoff, where `WM_DESTROY` never arrives.
        KillTimer(hwnd, TIMER_ID).ok();
        let tray = tray.borrow();
        tray.remove_icon();
        if !tray.icon.is_invalid() {
            let _ = DestroyIcon(tray.icon);
        }
        Ok(())
    }
}

struct Tray {
    hwnd: HWND,
    icon: HICON,
    supervisor: supervisor::Handle,
    strings: Strings,
    config_path: PathBuf,
    /// Last tooltip written, so the icon is only touched when something changed.
    last_tooltip: String,
    /// Share entries as of the last menu open. The menu id of an entry is its
    /// index plus [`ID_SHARE_BASE`], so this has to survive until the click is
    /// handled.
    shares: Vec<ShareEntry>,
}

impl Tray {
    // ---- icon ---------------------------------------------------------------

    fn base_data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            ..Default::default()
        }
    }

    fn add_icon(&self) -> std::io::Result<()> {
        let mut data = self.base_data();
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = self.icon;

        unsafe {
            if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
                return Err(std::io::Error::other(
                    "the shell refused to add the notification-area icon",
                ));
            }
            // Opt in to the v5 notification codes. Without this the balloon is
            // still shown but its click is swallowed, and the first-run setup
            // would silently do nothing.
            data.Anonymous.uVersion = NOTIFYICON_VERSION;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
        }
        Ok(())
    }

    fn remove_icon(&self) {
        let data = self.base_data();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }

    fn refresh_tooltip(&mut self) {
        let cfg = load_config(&self.config_path);
        let state = self.strings.get(self.supervisor.state().lang_key());
        let address = listening_address(&cfg);
        let tip = self
            .strings
            .format("tray.tooltip", &[("state", &state), ("address", &address)]);
        if tip == self.last_tooltip {
            return;
        }

        let mut data = self.base_data();
        data.uFlags = NIF_TIP;
        fill(&mut data.szTip, &tip);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
        self.last_tooltip = tip;
    }

    // ---- first run ----------------------------------------------------------

    /// Remember that the user ticked "don't show this again".
    fn stop_showing_welcome(&self) {
        self.edit_config(|cfg| cfg.tray.show_welcome = false);
    }

    /// What the welcome window's action button does: register autostart, add the
    /// firewall rule (one UAC prompt), copy the share URL.
    ///
    /// Returns the language key describing how it went, for the caller to show.
    /// Reporting is deliberately not done here: this is the one place that must
    /// not depend on a notification channel that can be switched off.
    #[must_use]
    fn run_first_run_setup(&mut self) -> &'static str {
        let Ok(exe) = std::env::current_exe() else {
            return "tray.setup.failedBody";
        };
        let packaged = crate::autostart::packaged();

        // The Store build has neither half to do: `windows.startupTask` and
        // `windows.firewallRules` in the package manifest already did both, at
        // install time and without a UAC prompt. Attempting them again would
        // fail — `%ProgramFiles%\WindowsApps` is ACL'd against re-launching
        // ourselves elevated — and report a shortfall the user cannot act on.
        let (autostart_ok, firewall_ok) = if packaged {
            (true, true)
        } else {
            let ok = crate::autostart::install(&exe).is_ok();
            let cfg = load_config(&self.config_path);
            let firewall = matches!(
                crate::autostart::elevate_self(&[
                    "--firewall-install",
                    &cfg.server.port.to_string()
                ]),
                crate::autostart::Elevation::Done
            );
            (ok, firewall)
        };

        let cfg = load_config(&self.config_path);
        let shares = net::build_share_entries(&cfg, cfg.share_token());
        let copied = match shares.first() {
            // The physical NIC sorts first, which is the right guess on a
            // Hyper-V host where vEthernet adapters are always present too. A
            // wrong guess is one menu click away from being corrected.
            Some(entry) => self.copy(&entry.url),
            None => false,
        };

        if autostart_ok {
            // Recorded even when the firewall half was declined: the next
            // welcome window should offer "open", not a second UAC prompt.
            self.edit_config(|cfg| cfg.tray.setup_done = true);
        }
        setup_outcome(packaged, autostart_ok, firewall_ok, copied)
    }

    /// Read-modify-write `config.toml`.
    ///
    /// Re-read immediately before writing, every time. The server child may have
    /// minted a token into this same file seconds ago, and saving a snapshot
    /// taken at startup would throw it away.
    fn edit_config(&self, change: impl FnOnce(&mut Config)) {
        let Ok((mut cfg, _)) = config::load_or_init(&self.config_path) else {
            return;
        };
        change(&mut cfg);
        let _ = config::save(&self.config_path, &cfg);
    }

    // ---- menu ---------------------------------------------------------------

    fn show_menu(&mut self) {
        let cfg = load_config(&self.config_path);
        self.shares = net::build_share_entries(&cfg, cfg.share_token());

        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            append(
                menu,
                MF_STRING,
                ID_OPEN,
                &self.strings.get("tray.menu.open"),
            );
            self.append_share_item(menu, &cfg);
            self.append_shortcut_item(menu, &cfg);
            append(menu, MF_SEPARATOR, 0, "");
            append(
                menu,
                MF_STRING,
                ID_RESTART,
                &self.strings.get("tray.menu.restart"),
            );
            let toggle = if self.supervisor.state() == SupervisorState::Running {
                "tray.menu.stop"
            } else {
                "tray.menu.start"
            };
            append(menu, MF_STRING, ID_TOGGLE, &self.strings.get(toggle));
            append(menu, MF_SEPARATOR, 0, "");
            append(
                menu,
                MF_STRING,
                ID_QUIT,
                &self.strings.get("tray.menu.quit"),
            );

            let mut point = POINT::default();
            let _ = GetCursorPos(&mut point);
            // Documented dance: the menu only dismisses on an outside click if
            // its owner is the foreground window, and the trailing post is what
            // lets it close when the owner never becomes active.
            let _ = SetForegroundWindow(self.hwnd);
            let chosen = TrackPopupMenu(
                menu,
                TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
                point.x,
                point.y,
                0,
                self.hwnd,
                None,
            );
            let _ = PostMessageW(self.hwnd, WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);

            self.on_command(chosen.0 as usize);
        }
    }

    /// "Copy share URL" is not one item on a Hyper-V host: there is a URL per
    /// NIC and the physical one is only a good guess, not a certainty. So the
    /// item turns into a submenu as soon as there is a choice to make, and into
    /// a greyed-out *explanation* when there is nothing to copy — never a
    /// clickable item that silently puts an empty string on the clipboard.
    unsafe fn append_share_item(&self, menu: HMENU, cfg: &Config) {
        let label = self.strings.get("tray.menu.copyShare");

        if !cfg.lan_exposed() {
            append(
                menu,
                MF_STRING | MF_GRAYED,
                0,
                &self.strings.get("tray.share.noLan"),
            );
            return;
        }
        if cfg.share_token().is_none() {
            append(
                menu,
                MF_STRING | MF_GRAYED,
                0,
                &self.strings.get("tray.share.noToken"),
            );
            return;
        }
        match self.shares.len() {
            0 => append(
                menu,
                MF_STRING | MF_GRAYED,
                0,
                &self.strings.get("tray.share.noNic"),
            ),
            1 => append(menu, MF_STRING, ID_SHARE_BASE, &label),
            _ => {
                let Ok(sub) = CreatePopupMenu() else { return };
                for (i, entry) in self.shares.iter().enumerate() {
                    // Interface name and address only. The token lives in the
                    // URL fragment and must not be readable over someone's
                    // shoulder — the clipboard is the only place it goes.
                    let text = format!("{} — {}", entry.interface, address_of(&entry.url));
                    append(sub, MF_STRING, ID_SHARE_BASE + i, &text);
                }
                append(menu, MF_POPUP, sub.0 as usize, &label);
            }
        }
    }

    /// A desktop shortcut contains the same live token as the copy action, so
    /// it follows the same NIC selection and availability rules. The actual
    /// write is still guarded by a confirmation dialog in `create_shortcut`.
    unsafe fn append_shortcut_item(&self, menu: HMENU, cfg: &Config) {
        let label = self.strings.get("tray.menu.createShortcut");

        if !cfg.lan_exposed() {
            append(
                menu,
                MF_STRING | MF_GRAYED,
                0,
                &self.strings.get("tray.share.noLan"),
            );
            return;
        }
        if cfg.share_token().is_none() {
            append(
                menu,
                MF_STRING | MF_GRAYED,
                0,
                &self.strings.get("tray.share.noToken"),
            );
            return;
        }
        match self.shares.len() {
            0 => append(
                menu,
                MF_STRING | MF_GRAYED,
                0,
                &self.strings.get("tray.share.noNic"),
            ),
            1 => append(menu, MF_STRING, ID_SHORTCUT_BASE, &label),
            _ => {
                let Ok(sub) = CreatePopupMenu() else { return };
                for (i, entry) in self.shares.iter().enumerate() {
                    let text = format!("{} — {}", entry.interface, address_of(&entry.url));
                    append(sub, MF_STRING, ID_SHORTCUT_BASE + i, &text);
                }
                append(menu, MF_POPUP, sub.0 as usize, &label);
            }
        }
    }

    fn on_command(&mut self, id: usize) {
        match id {
            // 0 = the menu was dismissed without a choice.
            0 => {}
            ID_OPEN => self.open_ui(),
            ID_RESTART => self.supervisor.send(Request::Restart),
            ID_TOGGLE => {
                let request = if self.supervisor.state() == SupervisorState::Running {
                    Request::Stop
                } else {
                    Request::Start
                };
                self.supervisor.send(request);
            }
            ID_QUIT => unsafe {
                // Posted rather than destroyed inline: we are still inside the
                // window procedure that owns this window.
                let _ = PostMessageW(self.hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            id if id >= ID_SHORTCUT_BASE => {
                if let Some(entry) = self.shares.get(id - ID_SHORTCUT_BASE).cloned() {
                    self.create_shortcut(&entry);
                }
            }
            id if id >= ID_SHARE_BASE => {
                if let Some(entry) = self.shares.get(id - ID_SHARE_BASE) {
                    let url = entry.url.clone();
                    self.copy(&url);
                }
            }
            _ => {}
        }
    }

    fn open_ui(&self) {
        let cfg = load_config(&self.config_path);
        crate::open_url(&net::build_host_url(&cfg));
    }

    fn current_share_entries(&self) -> Vec<ShareEntry> {
        let cfg = load_config(&self.config_path);
        net::build_share_entries(&cfg, cfg.share_token())
    }

    /// The welcome window uses the same physical-NIC-first default as the
    /// one-click setup. The tray menu remains available when a different NIC
    /// must be selected explicitly.
    fn copy_first_share(&self) {
        let body = match self.current_share_entries().first() {
            Some(entry) if self.copy(&entry.url) => "tray.share.copiedBody",
            Some(_) => "tray.share.copyFailedBody",
            None => "tray.share.noShareBody",
        };
        welcome::report_with_title(&self.strings, "tray.share.title", body);
    }

    /// Put `text` on the clipboard. Returns whether it got there — the caller
    /// tells the user, because a silent no-op after "Copy" is indistinguishable
    /// from a successful copy of nothing.
    fn copy(&self, text: &str) -> bool {
        unsafe {
            if OpenClipboard(self.hwnd).is_err() {
                return false;
            }
            let result = write_clipboard(text);
            let _ = CloseClipboard();
            result
        }
    }

    fn create_shortcut(&self, entry: &ShareEntry) {
        if !welcome::confirm_shortcut(&self.strings) {
            return;
        }
        let result = write_share_shortcut(entry);
        let body = if result.is_ok() {
            "tray.shortcut.createdBody"
        } else {
            "tray.shortcut.failedBody"
        };
        welcome::report_with_title(&self.strings, "tray.shortcut.title", body);
    }

    fn create_first_shortcut(&self) {
        match self.current_share_entries().first() {
            Some(entry) => self.create_shortcut(entry),
            None => welcome::report_with_title(
                &self.strings,
                "tray.shortcut.title",
                "tray.shortcut.noShareBody",
            ),
        }
    }
}

/// Write a Windows Internet Shortcut on the user's actual Desktop. The known
/// folder API matters because Desktop may be redirected into OneDrive.
fn write_share_shortcut(entry: &ShareEntry) -> std::io::Result<PathBuf> {
    let desktop = desktop_directory()?;
    let path = shortcut_path(&desktop, entry)?;
    let mut contents = format!(
        "[InternetShortcut]\r\nURL={}\r\nComment=Remote Audio share shortcut\r\n",
        entry.url
    );
    if let Ok(exe) = std::env::current_exe() {
        contents.push_str(&format!("IconFile={}\r\nIconIndex=0\r\n", exe.display()));
    }
    fs::write(&path, contents)?;
    Ok(path)
}

fn desktop_directory() -> std::io::Result<PathBuf> {
    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None::<&HANDLE>)
            .map_err(to_io)?;
        let path = raw
            .to_string()
            .map_err(|e| std::io::Error::other(e.to_string()));
        CoTaskMemFree(Some(raw.0.cast()));
        path.map(PathBuf::from)
    }
}

/// Use an address in the filename so multiple NIC choices produce separate
/// shortcuts. Invalid Windows filename characters are replaced, and the token
/// never participates in the name.
fn shortcut_path(desktop: &Path, entry: &ShareEntry) -> std::io::Result<PathBuf> {
    let address = address_of(&entry.url);
    let safe_address: String = address
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let stem = format!("Remote Audio - {safe_address}");

    for suffix in 0..1000 {
        let name = if suffix == 0 {
            format!("{stem}.url")
        } else {
            format!("{stem} ({suffix}).url")
        };
        let candidate = desktop.join(name);
        if !candidate.exists() || is_audio_remote_shortcut(&candidate) {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not find a free desktop shortcut name",
    ))
}

fn is_audio_remote_shortcut(path: &Path) -> bool {
    fs::read_to_string(path)
        .map(|contents| {
            contents.contains("Comment=Remote Audio share shortcut")
                || contents.contains("Comment=AudioRemote share shortcut")
        })
        .unwrap_or(false)
}

/// Everything the window procedure has to do, once the state pointer is known.
unsafe fn handle(tray: &mut Tray, msg: u32, lparam: LPARAM) {
    let taskbar_created = WM_TASKBAR_CREATED.load(Ordering::Relaxed);
    if taskbar_created != 0 && msg == taskbar_created {
        // Explorer is back and took the icon with it. Re-add it — this is the
        // one message where NIM_ADD after a successful add is expected rather
        // than an error. The fresh icon carries no tooltip, so refresh right
        // away instead of waiting out the timer tick.
        if let Err(e) = tray.add_icon() {
            eprintln!("[tray] re-adding the icon after a shell restart failed: {e}");
        }
        tray.refresh_tooltip();
        return;
    }
    match msg {
        WM_TRAY => match lparam.0 as u32 {
            WM_RBUTTONUP | WM_CONTEXTMENU => tray.show_menu(),
            WM_LBUTTONUP | WM_LBUTTONDBLCLK => tray.open_ui(),
            _ => {}
        },
        WM_TIMER => tray.refresh_tooltip(),
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            return LRESULT(0);
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        _ => {}
    }

    // Null until `run` has stored it — the class receives creation messages
    // before that, and dereferencing here would be the classic tray crash.
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<Tray>;
    if !state.is_null() {
        // Busy means we were re-entered from a modal loop (menu open, UAC
        // prompt). Dropping the message is correct: whatever it was, it will be
        // true again a second later when the timer next fires.
        if let Ok(mut tray) = (*state).try_borrow_mut() {
            handle(&mut tray, msg, lparam);
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

// ---- helpers ----------------------------------------------------------------

/// Which sentence the setup should end on.
///
/// The most important shortfall, not a list: the user needs the one thing that
/// still needs doing, and a dialog they have to read twice is a dialog they
/// dismiss without reading once.
fn setup_outcome(
    packaged: bool,
    autostart_ok: bool,
    firewall_ok: bool,
    copied: bool,
) -> &'static str {
    if packaged {
        // Neither of the other two shortfalls can occur here — the Store build
        // never touches the registry or the firewall — so the only thing left to
        // report on is whether there was a URL to hand over.
        return if copied {
            "tray.setup.packagedBody"
        } else {
            "tray.setup.packagedNoShareBody"
        };
    }
    if !autostart_ok {
        "tray.setup.failedBody"
    } else if !copied {
        "tray.setup.noShareBody"
    } else if !firewall_ok {
        "tray.setup.noFirewallBody"
    } else {
        "tray.setup.doneBody"
    }
}

unsafe fn append(menu: HMENU, flags: MENU_ITEM_FLAGS, id: usize, text: &str) {
    let wide_text = wide(text);
    let _ = AppendMenuW(menu, flags, id, PCWSTR(wide_text.as_ptr()));
}

unsafe fn write_clipboard(text: &str) -> bool {
    // Spelled out rather than imported: windows-rs only exports the constant
    // from `Win32_System_Ole`, and pulling that whole feature in for one number
    // that has been 13 since Windows NT is not a trade worth making.
    const CF_UNICODETEXT: u32 = 13;

    if EmptyClipboard().is_err() {
        return false;
    }
    let units = wide(text);
    let bytes = std::mem::size_of_val(units.as_slice());
    let Ok(handle) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
        return false;
    };
    let target = GlobalLock(handle);
    if target.is_null() {
        let _ = GlobalFree(handle);
        return false;
    }
    std::ptr::copy_nonoverlapping(units.as_ptr(), target.cast::<u16>(), units.len());
    let _ = GlobalUnlock(handle);

    // Ownership transfers on success, so the block is only ours to free when the
    // handover failed. Freeing it after a successful call would be a
    // use-after-free the moment anything pasted.
    if SetClipboardData(CF_UNICODETEXT, HANDLE(handle.0)).is_ok() {
        true
    } else {
        let _ = GlobalFree(handle);
        false
    }
}

/// Copy a string into a fixed-size wide buffer, NUL-terminated and truncated to
/// fit. Never splits a surrogate pair — a lone half renders as a replacement
/// glyph in the notification area.
fn fill(dst: &mut [u16], text: &str) {
    let mut units: Vec<u16> = text.encode_utf16().take(dst.len() - 1).collect();
    if matches!(units.last(), Some(&u) if (0xd800..0xdc00).contains(&u)) {
        units.pop();
    }
    units.push(0);
    dst[..units.len()].copy_from_slice(&units);
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `http://192.0.2.10:17650/#t=<token>` → `192.0.2.10:17650`.
/// The fragment is dropped deliberately: this string is drawn on screen.
fn address_of(url: &str) -> &str {
    url.strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
}

/// What to put in the tooltip after the state.
///
/// The address a person could actually type, not the literal `bind`: a tooltip
/// reading `0.0.0.0:17650` tells nobody anything. When LAN is on, that is the
/// physical NIC (the same one the share menu lists first); when it is off, or
/// there is no NIC yet, it falls back to the host's own URL. No token — a
/// tooltip is on screen whenever the pointer drifts past.
fn listening_address(cfg: &Config) -> String {
    if cfg.lan_exposed() {
        let mut nics = net::list_lan_ipv4();
        nics.sort_by_key(|(iface, _)| net::is_virtual_iface(iface));
        if let Some((_, ip)) = nics.first() {
            return format!("{ip}:{}", cfg.server.port);
        }
    }
    net::build_host_url(cfg)
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

fn load_config(path: &Path) -> Config {
    config::load_or_init(path)
        .map(|(cfg, _)| cfg)
        .unwrap_or_default()
}

fn to_io(e: windows::core::Error) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::{address_of, fill, listening_address, shortcut_path};
    use crate::config::Config;
    use crate::net::ShareEntry;
    use std::path::Path;

    #[test]
    fn a_host_only_bind_shows_the_loopback_address() {
        let mut cfg = Config::default();
        cfg.server.bind = "127.0.0.1".to_string();
        cfg.server.port = 17650;
        assert_eq!(listening_address(&cfg), "127.0.0.1:17650");
    }

    #[test]
    fn the_tooltip_address_never_carries_a_token() {
        // Whatever the host's NICs look like on the machine running this test,
        // the tooltip is a bare address.
        let cfg = Config::default();
        let shown = listening_address(&cfg);
        assert!(!shown.contains("#t="), "{shown}");
        assert!(!shown.contains("http"), "{shown}");
        assert!(
            shown.ends_with(&format!(":{}", cfg.server.port)),
            "{shown} should end with the configured port"
        );
    }

    #[test]
    fn the_displayed_address_never_carries_the_token() {
        let url = "http://192.0.2.10:17650/#t=ar_live_0123456789abcdef";
        assert_eq!(address_of(url), "192.0.2.10:17650");
        assert!(!address_of(url).contains("ar_live"));
    }

    #[test]
    fn desktop_shortcut_name_contains_address_but_not_token() {
        let entry = ShareEntry {
            url: "http://192.0.2.10:17650/#t=ar_live_secret".to_string(),
            interface: "Ethernet".to_string(),
            virtual_iface: false,
        };
        let path = shortcut_path(Path::new("Desktop"), &entry).unwrap();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("Remote Audio - 192.0.2.10-17650.url")
        );
        assert!(!path.to_string_lossy().contains("ar_live"));
    }

    #[test]
    fn fill_truncates_and_terminates() {
        let mut buf = [0u16; 5];
        fill(&mut buf, "abcdefgh");
        assert_eq!(String::from_utf16_lossy(&buf[..4]), "abcd");
        assert_eq!(buf[4], 0, "the buffer must stay NUL-terminated");

        let mut short = [0u16; 8];
        fill(&mut short, "ab");
        assert_eq!(short[2], 0);
    }

    #[test]
    fn fill_does_not_leave_half_a_surrogate_pair() {
        // A buffer that can hold exactly one unit plus the terminator, given a
        // character that needs two. Splitting it renders as a stray glyph.
        let mut buf = [0u16; 2];
        fill(&mut buf, "\u{1f50a}");
        assert_eq!(buf[0], 0, "a lone surrogate was written: {buf:?}");
    }
}
