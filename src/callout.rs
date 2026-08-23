//! A short-lived arrow that points at the app's own notification-area icon.
//!
//! The problem it solves: after the setup dialog says "done", the app has no
//! window and the user has no idea where it went. Telling them in prose ("next
//! to the clock") is weak, and drawing a picture of a taskbar is worse — the
//! drawing would show *a* taskbar, not *theirs*, and a picture that disagrees
//! with the screen underneath it misleads more than it helps.
//!
//! So we ask Windows where our icon actually is (`Shell_NotifyIconGetRect`) and
//! point at that. Two things fall out of this for free:
//!
//! * Taskbar at the top, on the side, on a second monitor, icons reordered —
//!   all handled, because we never assumed where it would be.
//! * Windows 11 hides new notification icons by default, and in that case the
//!   API hands back the rectangle of the "show hidden icons" chevron instead.
//!   Pointing there is exactly right: that is where the user has to click.
//!
//! Sizes are derived from the message font's height rather than written as
//! pixel constants, so the callout scales with the display without a single
//! DPI calculation.

use std::cell::RefCell;

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CombineRgn, CreateFontIndirectW, CreatePolygonRgn, CreateRoundRectRgn,
    CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRgn, FrameRgn, GetMonitorInfoW,
    GetSysColor, MonitorFromRect, SelectObject, SetBkMode, SetTextColor, SetWindowRgn,
    COLOR_INFOBK, COLOR_INFOTEXT, DT_CALCRECT, DT_CENTER, DT_NOPREFIX, DT_WORDBREAK, HFONT, HRGN,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, RGN_OR, TRANSPARENT, WINDING,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{Shell_NotifyIconGetRect, NOTIFYICONIDENTIFIER};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW, KillTimer, RegisterClassW,
    SetTimer, SetWindowLongPtrW, ShowWindow, SystemParametersInfoW, GWLP_USERDATA,
    NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, SW_SHOWNOACTIVATE,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_DESTROY, WM_LBUTTONDOWN, WM_PAINT, WM_TIMER, WNDCLASSW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::lang::Strings;

/// How long the arrow stays up.
///
/// Long enough to follow it with your eyes and find the icon, short enough that
/// it never becomes something to dismiss. It also disappears on a click.
const VISIBLE_MS: u32 = 7000;
const TIMER_ID: usize = 1;

/// Widest the bubble may get, as a multiple of the font height. Keeps a long
/// translation from stretching into a single unreadable line.
const MAX_WIDTH_IN_LINES: i32 = 18;

/// Point at our notification-area icon, if Windows will say where it is.
///
/// Creates the window and returns immediately — it lives on the caller's
/// message loop and tears itself down. Silently does nothing when the icon has
/// no rectangle (the shell can refuse, e.g. while the taskbar is restarting),
/// because a missing arrow is a smaller failure than a stray one.
pub fn point_at_tray_icon(strings: &Strings, owner: HWND, icon_id: u32) {
    unsafe {
        let identifier = NOTIFYICONIDENTIFIER {
            cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: owner,
            uID: icon_id,
            ..Default::default()
        };
        let Ok(icon) = Shell_NotifyIconGetRect(&identifier) else {
            return;
        };
        // A zero-sized rectangle means the shell answered but has nothing to
        // show us. Pointing at a point is worse than not pointing.
        if icon.right <= icon.left || icon.bottom <= icon.top {
            return;
        }

        let Some(font) = message_font() else { return };
        let text = strings.get("tray.callout.here");
        let Some(callout) = Callout::build(&text, font, icon) else {
            let _ = DeleteObject(font);
            return;
        };
        callout.show();
    }
}

/// The system's message-box font, at this display's scale.
unsafe fn message_font() -> Option<HFONT> {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    SystemParametersInfoW(
        SPI_GETNONCLIENTMETRICS,
        metrics.cbSize,
        Some(std::ptr::addr_of_mut!(metrics).cast()),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .ok()?;
    let font = CreateFontIndirectW(&metrics.lfMessageFont);
    (!font.is_invalid()).then_some(font)
}

/// Everything the window needs after it exists.
struct Callout {
    text: Vec<u16>,
    font: HFONT,
    /// Where the text goes, in client coordinates.
    text_area: RECT,
    /// The bubble, in client coordinates — the arrow sits outside it.
    bubble: RECT,
    /// The three corners of the arrow, in client coordinates.
    arrow: [POINT; 3],
    /// Corner rounding, in pixels.
    radius: i32,
    /// Where the whole thing goes, in screen coordinates.
    placement: RECT,
}

impl Callout {
    /// Lay the callout out around `icon`.
    unsafe fn build(text: &str, font: HFONT, icon: RECT) -> Option<Self> {
        let wide: Vec<u16> = text.encode_utf16().collect();

        // Measure against a screen DC through a throwaway window, because
        // `DrawText` needs a device context that has the font selected.
        let (line_height, mut text_size) = measure(&wide, font)?;
        let pad = line_height * 2 / 3;
        let arrow_h = line_height * 2 / 3;
        let arrow_w = arrow_h * 2;
        let radius = line_height / 2;

        text_size.right = text_size.right.max(arrow_w * 2);
        let bubble_w = text_size.right + pad * 2;
        let bubble_h = text_size.bottom + pad * 2;

        // The icon usually sits at the bottom of the screen, so the bubble goes
        // above it and the arrow points down. A taskbar at the top flips both.
        let work = work_area(icon);
        let icon_cx = (icon.left + icon.right) / 2;
        let icon_cy = (icon.top + icon.bottom) / 2;
        let below = icon_cy < (work.top + work.bottom) / 2;

        let total_h = bubble_h + arrow_h;
        let top = if below {
            icon.bottom
        } else {
            icon.top - total_h
        };
        // Centred on the icon, then pulled back inside the monitor: the icon is
        // usually near a corner, so the bubble would otherwise hang off-screen.
        let left =
            (icon_cx - bubble_w / 2).clamp(work.left, (work.right - bubble_w).max(work.left));

        let (bubble_top, arrow_tip_y, arrow_base_y) = if below {
            (arrow_h, 0, arrow_h)
        } else {
            (0, total_h, bubble_h)
        };
        let bubble = RECT {
            left: 0,
            top: bubble_top,
            right: bubble_w,
            bottom: bubble_top + bubble_h,
        };
        // Anchored under the icon's real centre, not the bubble's, so the arrow
        // still points at the icon after the clamp above moved the bubble.
        let tip_x = (icon_cx - left).clamp(arrow_w, (bubble_w - arrow_w).max(arrow_w));
        let arrow = [
            POINT {
                x: tip_x,
                y: arrow_tip_y,
            },
            POINT {
                x: tip_x - arrow_w / 2,
                y: arrow_base_y,
            },
            POINT {
                x: tip_x + arrow_w / 2,
                y: arrow_base_y,
            },
        ];

        Some(Self {
            text: wide,
            font,
            text_area: RECT {
                left: pad,
                top: bubble.top + pad,
                right: bubble_w - pad,
                bottom: bubble.bottom - pad,
            },
            bubble,
            arrow,
            radius,
            placement: RECT {
                left,
                top,
                right: left + bubble_w,
                bottom: top + total_h,
            },
        })
    }

    unsafe fn show(self) {
        let Ok(instance) = GetModuleHandleW(None) else {
            return;
        };
        let class_name = w!("AudioRemoteCallout");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        // Zero also means "already registered", which is the normal case from
        // the second call on. Registering is idempotent for our purposes, so a
        // failure here only shows up as the window creation failing below.
        RegisterClassW(&class);

        let width = self.placement.right - self.placement.left;
        let height = self.placement.bottom - self.placement.top;
        let hwnd = CreateWindowExW(
            // Topmost to clear the taskbar, tool-window to stay out of Alt-Tab,
            // no-activate so it never steals the caret from whatever the user
            // is typing in.
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name,
            w!("Remote Audio"),
            WS_POPUP,
            self.placement.left,
            self.placement.top,
            width,
            height,
            None,
            None,
            instance,
            None,
        );
        let Ok(hwnd) = hwnd else {
            return;
        };

        // Clip the window to the bubble plus the arrow, so the corners outside
        // them are not painted and the shape reads as a speech bubble.
        // Ownership passes to the window here; Windows frees it with the window.
        let _ = SetWindowRgn(hwnd, self.shape(), false);

        // Handed to the window; the window procedure reclaims it on
        // `WM_DESTROY`, and `Drop` releases the font with it.
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            Box::into_raw(Box::new(RefCell::new(self))) as isize,
        );
        SetTimer(hwnd, TIMER_ID, VISIBLE_MS, None);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }

    /// The bubble and its arrow as one region.
    ///
    /// One shape and not two: drawing a bordered rectangle and a bordered
    /// triangle that share an edge puts the border down twice along that edge,
    /// which reads on screen as a gap between the bubble and its arrow.
    unsafe fn shape(&self) -> HRGN {
        // `+1` on the far edges: region boundaries are exclusive, so without it
        // the last row and column fall outside the shape.
        let bubble = CreateRoundRectRgn(
            self.bubble.left,
            self.bubble.top,
            self.bubble.right + 1,
            self.bubble.bottom + 1,
            self.radius,
            self.radius,
        );
        let arrow = CreatePolygonRgn(&self.arrow, WINDING);
        CombineRgn(bubble, bubble, arrow, RGN_OR);
        let _ = DeleteObject(arrow);
        bubble
    }

    unsafe fn paint(&self, hwnd: HWND) {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);

        let fill = CreateSolidBrush(COLORREF(GetSysColor(COLOR_INFOBK)));
        let border = CreateSolidBrush(COLORREF(GetSysColor(COLOR_INFOTEXT)));
        let shape = self.shape();
        let _ = FillRgn(hdc, shape, fill);
        let _ = FrameRgn(hdc, shape, border, 1, 1);
        let _ = DeleteObject(shape);

        let old_font = SelectObject(hdc, self.font);
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(GetSysColor(COLOR_INFOTEXT)));
        let mut area = self.text_area;
        let mut text = self.text.clone();
        DrawTextW(
            hdc,
            &mut text,
            &mut area,
            DT_CENTER | DT_WORDBREAK | DT_NOPREFIX,
        );

        SelectObject(hdc, old_font);
        let _ = DeleteObject(fill);
        let _ = DeleteObject(border);
        let _ = EndPaint(hwnd, &ps);
    }
}

impl Drop for Callout {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RefCell<Callout>;

    match msg {
        WM_PAINT if !state.is_null() => {
            if let Ok(callout) = (*state).try_borrow() {
                callout.paint(hwnd);
            }
            return LRESULT(0);
        }
        // Either the time ran out or the user swatted it away. Both mean the
        // same thing, and neither needs a confirmation.
        WM_TIMER | WM_LBUTTONDOWN => {
            let _ = DestroyWindow(hwnd);
            return LRESULT(0);
        }
        WM_DESTROY => {
            KillTimer(hwnd, TIMER_ID).ok();
            if !state.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                // Reclaimed here and nowhere else; `Drop` releases the font.
                drop(Box::from_raw(state));
            }
            // Deliberately no `PostQuitMessage`: this window is a guest on the
            // tray's message loop, and ending that loop would close the app.
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Text extent for `text`, plus one line's height, in pixels at this scale.
unsafe fn measure(text: &[u16], font: HFONT) -> Option<(i32, RECT)> {
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, GetTextMetricsW, TEXTMETRICW,
    };

    let hdc = CreateCompatibleDC(None);
    if hdc.is_invalid() {
        return None;
    }
    let old = SelectObject(hdc, font);

    let mut tm = TEXTMETRICW::default();
    let line_height = if GetTextMetricsW(hdc, &mut tm).as_bool() {
        tm.tmHeight
    } else {
        16
    };

    let mut area = RECT {
        left: 0,
        top: 0,
        right: line_height * MAX_WIDTH_IN_LINES,
        bottom: 0,
    };
    let mut copy = text.to_vec();
    DrawTextW(
        hdc,
        &mut copy,
        &mut area,
        DT_CALCRECT | DT_WORDBREAK | DT_CENTER | DT_NOPREFIX,
    );

    SelectObject(hdc, old);
    let _ = DeleteDC(hdc);
    Some((line_height.max(1), area))
}

/// Work area of the monitor `rect` sits on, falling back to the rect itself so
/// a failure here cannot move the callout somewhere absurd.
unsafe fn work_area(rect: RECT) -> RECT {
    let monitor = MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        info.rcWork
    } else {
        rect
    }
}
