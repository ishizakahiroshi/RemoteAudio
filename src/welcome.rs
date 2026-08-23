//! The window that tells a first-time user where the app went.
//!
//! Why a window and not a notification: since Windows 10, `Shell_NotifyIcon`
//! balloons are delivered through the toast system, where focus assist, quiet
//! hours and a per-app notification switch can each drop them — silently, with
//! no way for the app to learn that nothing was shown. It was measured doing
//! exactly that on the target host (2026-08-04). An app whose entire UI lives
//! in the notification area cannot make its one "here is where I am" message
//! depend on a channel that fails invisibly.
//!
//! Why `TaskDialogIndirect` and not a hand-built window: it carries the
//! "don't show this again" checkbox as a first-class field, and inherits the
//! system font, DPI scaling, dark mode and command-link styling for free. The
//! cost is one comctl32 v6 dependency in `packaging/audioremote.manifest`.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::UI::Controls::{
    TaskDialogIndirect, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TASKDIALOG_BUTTON,
    TDCBF_CLOSE_BUTTON, TDF_ALLOW_DIALOG_CANCELLATION, TDF_USE_COMMAND_LINKS, TDF_USE_HICON_MAIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, GetSystemMetrics, MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_YESNO,
    SM_CXICON, SM_CYICON,
};

use crate::assets;
use crate::lang::Strings;

/// Command ids for the welcome window's command links. Anything outside the
/// `IDOK`…`IDCONTINUE` range is safe; keeping them together makes the mapping
/// back to [`WelcomeAction`] explicit.
const ID_SETUP: i32 = 100;
const ID_OPEN: i32 = 101;
const ID_COPY_SHARE: i32 = 102;
const ID_CREATE_SHORTCUT: i32 = 103;

struct DialogAction {
    id: i32,
    text: String,
}

/// What the user selected in the welcome window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WelcomeAction {
    None,
    Setup,
    Open,
    CopyShare,
    CreateShortcut,
}

/// What the user did with the welcome window.
pub struct Choice {
    /// The command link they pressed, or [`WelcomeAction::None`] if they
    /// closed the window.
    pub action: WelcomeAction,
    /// They left "don't show this again" unticked, so it should appear next
    /// time too.
    pub keep_showing: bool,
}

/// Show the welcome window and wait for it to be dismissed.
///
/// Blocks the calling thread, which must be the one owning the tray's message
/// loop: the dialog pumps that thread's queue while it is up, so the tray icon
/// stays alive and its menu keeps working underneath.
///
/// `setup_done` switches the action button between "finish setting up" and
/// "open" — offering the setup again to someone who already ran it is how you
/// get a UAC prompt nobody asked for.
///
/// `packaged` only changes the small print under the button: the Store build's
/// setup raises no UAC prompt, so promising one would be a lie in the one place
/// the promise exists to stop a nasty surprise.
pub fn ask(strings: &Strings, setup_done: bool, packaged: bool) -> Choice {
    // A command link renders the text before the first newline as the headline
    // and the rest as the smaller line beneath it. Once setup is complete,
    // this same welcome window is also the convenient home for the two common
    // hand-off actions: copy the URL or make a desktop shortcut.
    let actions = if setup_done {
        vec![
            DialogAction {
                id: ID_OPEN,
                text: format!(
                    "{}\n{}",
                    strings.get("tray.welcome.openButton"),
                    strings.get("tray.welcome.openNote")
                ),
            },
            DialogAction {
                id: ID_COPY_SHARE,
                text: format!(
                    "{}\n{}",
                    strings.get("tray.menu.copyShare"),
                    strings.get("tray.welcome.copyNote")
                ),
            },
            DialogAction {
                id: ID_CREATE_SHORTCUT,
                text: format!(
                    "{}\n{}",
                    strings.get("tray.menu.createShortcut"),
                    strings.get("tray.welcome.shortcutNote")
                ),
            },
        ]
    } else {
        let note_key = if packaged {
            "tray.welcome.setupNotePackaged"
        } else {
            "tray.welcome.setupNote"
        };
        vec![DialogAction {
            id: ID_SETUP,
            text: format!(
                "{}\n{}",
                strings.get("tray.welcome.setupButton"),
                strings.get(note_key)
            ),
        }]
    };

    let mut pressed = 0i32;
    let mut checked = BOOL(0);
    let dont_show_again = strings.get("tray.welcome.dontShowAgain");
    let shown = show(
        strings,
        &strings.get("tray.welcome.title"),
        &strings.get("tray.welcome.body"),
        &actions,
        Some(&dont_show_again),
        Some((&mut pressed, &mut checked)),
    );

    if !shown {
        // The dialog could not be created at all. "Keep showing" is the safe
        // answer: the alternative silently switches off an announcement that
        // was never made in the first place.
        return Choice {
            action: WelcomeAction::None,
            keep_showing: true,
        };
    }
    let action = match pressed {
        ID_SETUP => WelcomeAction::Setup,
        ID_OPEN => WelcomeAction::Open,
        ID_COPY_SHARE => WelcomeAction::CopyShare,
        ID_CREATE_SHORTCUT => WelcomeAction::CreateShortcut,
        _ => WelcomeAction::None,
    };
    Choice {
        action,
        keep_showing: !checked.as_bool(),
    }
}

/// Report the outcome of the one-click setup.
///
/// Separate from [`ask`] because it carries exactly what the balloon used to,
/// and the point of this module is that it can no longer be dropped on the way.
pub fn report(strings: &Strings, body_key: &str) {
    report_with_title(strings, "tray.setup.doneTitle", body_key);
}

/// Show a confirmation before writing a share URL (and its live token) to a
/// desktop shortcut. The default is No so an accidental menu click cannot
/// create a credential-bearing file.
pub fn confirm_shortcut(strings: &Strings) -> bool {
    let title = wide(&strings.get("tray.shortcut.title"));
    let body = wide(&strings.get("tray.shortcut.confirmBody"));
    unsafe {
        MessageBoxW(
            None::<&HWND>,
            PCWSTR(body.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
        ) == IDYES
    }
}

/// Show a result dialog with a caller-selected title.
pub fn report_with_title(strings: &Strings, title_key: &str, body_key: &str) {
    show(
        strings,
        &strings.get(title_key),
        &strings.get(body_key),
        &[],
        None,
        None,
    );
}

/// Tell a second launch that the first one is already there.
///
/// A window rather than silence: somebody just double-clicked the app, and an
/// app that visibly does nothing reads as broken — which is how they end up
/// double-clicking it again.
pub fn already_running(strings: &Strings) {
    show(
        strings,
        &strings.get("tray.alreadyRunning.title"),
        &strings.get("tray.alreadyRunning.body"),
        &[],
        None,
        None,
    );
}

/// The one call to `TaskDialogIndirect`. Returns whether the dialog appeared.
fn show(
    strings: &Strings,
    instruction: &str,
    content: &str,
    actions: &[DialogAction],
    verification_text: Option<&str>,
    out: Option<(&mut i32, &mut BOOL)>,
) -> bool {
    let title = wide(&strings.get("brand.name"));
    let instruction = wide(instruction);
    let content = wide(content);

    // Held for the whole call: the config only stores pointers into these.
    let action_texts: Vec<Vec<u16>> = actions.iter().map(|action| wide(&action.text)).collect();
    let buttons: Vec<TASKDIALOG_BUTTON> = actions
        .iter()
        .zip(action_texts.iter())
        .map(|(action, text)| TASKDIALOG_BUTTON {
            nButtonID: action.id,
            pszButtonText: PCWSTR(text.as_ptr()),
        })
        .collect();
    let verification = verification_text.map(wide);

    // Sized from the *system* metric rather than a constant: the process is
    // per-monitor DPI aware (see the manifest), so this is already the right
    // number of real pixels for a large icon on this display.
    let icon =
        unsafe { assets::load_icon(GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON)) };

    let mut flags = TDF_ALLOW_DIALOG_CANCELLATION;
    if !icon.is_invalid() {
        flags |= TDF_USE_HICON_MAIN;
    }
    if !actions.is_empty() {
        flags |= TDF_USE_COMMAND_LINKS;
    }

    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        // No owner. The tray's window is a never-shown 0x0 rectangle at the
        // top-left corner, and a task dialog centres on its owner — handing it
        // that window would pin the welcome to the corner of the screen. A null
        // owner centres on the monitor instead.
        hwndParent: HWND::default(),
        dwFlags: flags,
        // Windows supplies this button's label in the user's own Windows
        // language, which is the right call even when our packs disagree with
        // it: it is a system button, and it always says the same thing.
        dwCommonButtons: TDCBF_CLOSE_BUTTON,
        pszWindowTitle: PCWSTR(title.as_ptr()),
        Anonymous1: TASKDIALOGCONFIG_0 { hMainIcon: icon },
        pszMainInstruction: PCWSTR(instruction.as_ptr()),
        pszContent: PCWSTR(content.as_ptr()),
        cButtons: actions.len() as u32,
        pButtons: if !actions.is_empty() {
            buttons.as_ptr()
        } else {
            std::ptr::null()
        },
        nDefaultButton: actions.first().map_or(0, |action| action.id),
        pszVerificationText: PCWSTR(
            verification
                .as_ref()
                .map_or(std::ptr::null(), |v| v.as_ptr()),
        ),
        ..Default::default()
    };

    let (button_out, checked_out) = match out {
        Some((b, c)) => (Some(b as *mut i32), Some(c as *mut BOOL)),
        None => (None, None),
    };
    let result = unsafe { TaskDialogIndirect(&config, button_out, None, checked_out) };

    if !icon.is_invalid() {
        unsafe {
            let _ = DestroyIcon(icon);
        }
    }
    result.is_ok()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
