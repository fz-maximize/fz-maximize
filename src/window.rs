//! Window inspection and placement (Win32). Mirrors FancyZones' `WindowUtils.cpp` /
//! `FancyZonesWindowProcessing.cpp` where behavior matters.

use windows::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromRect};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_PER_MONITOR_AWARE, GetAwarenessFromDpiAwarenessContext, GetDpiForWindow, GetSystemMetricsForDpi,
    GetWindowDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetAncestor, GetClassNameW, GetDesktopWindow,
    GetLayeredWindowAttributes, GetPropW, GetShellWindow, GetWindow, GetWindowLongPtrW, GetWindowPlacement,
    GetWindowRect, GetWindowThreadProcessId, InternalGetWindowText, IsIconic, IsWindowVisible, IsZoomed,
    LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA, SM_CXPADDEDBORDER, SM_CXSIZEFRAME, SM_CYSIZEFRAME, SW_RESTORE,
    SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SetWindowPlacement, SetWindowPos,
    WINDOWPLACEMENT, WPF_ASYNCWINDOWPLACEMENT, WPF_RESTORETOMAXIMIZED, WS_CAPTION, WS_CHILD, WS_EX_LAYERED,
    WS_EX_TOOLWINDOW, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_THICKFRAME, WS_VISIBLE,
};

use crate::geometry::{Margins, Rect};
use crate::win::{from_wide, rect_from, rect_to};

pub fn style(hwnd: HWND) -> u32 {
    unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 }
}

pub fn ex_style(hwnd: HWND) -> u32 {
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 }
}

fn has(bits: u32, flag: u32) -> bool {
    bits & flag != 0
}

/// Cheap filter for windows worth tracking: visible, top-level, resizable, has a
/// maximize box, not a tool window. Run on every location change, so no syscalls
/// beyond two `GetWindowLongPtr`.
pub fn is_trackable(hwnd: HWND) -> bool {
    let s = style(hwnd);
    let ex = ex_style(hwnd);
    has(s, WS_VISIBLE.0)
        && !has(s, WS_CHILD.0)
        && has(s, WS_MAXIMIZEBOX.0)
        && has(s, WS_THICKFRAME.0)
        && !has(ex, WS_EX_TOOLWINDOW.0)
}

pub fn is_iconic(hwnd: HWND) -> bool {
    unsafe { IsIconic(hwnd).as_bool() }
}

pub fn is_zoomed(hwnd: HWND) -> bool {
    unsafe { IsZoomed(hwnd).as_bool() }
}

/// Zoomed state for seeding: a minimized window counts as zoomed when it will restore
/// to maximized, so un-minimizing it is not mistaken for a fresh maximize.
pub fn zoomed_for_seed(hwnd: HWND) -> bool {
    if is_iconic(hwnd) {
        return placement(hwnd).is_some_and(|p| p.flags.contains(WPF_RESTORETOMAXIMIZED));
    }
    is_zoomed(hwnd)
}

pub fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    let r = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    r.is_ok() && cloaked != 0
}

/// The opacity a layered window is shown with as a whole (`SetLayeredWindowAttributes`
/// with LWA_ALPHA; 0 is invisible), or 255 for any other window, including layered
/// windows drawn with per-pixel alpha (`UpdateLayeredWindow`). DWM thumbnails leave this
/// out: they draw such a window opaque (an invisible full-screen overlay, such as the
/// NVIDIA App's, as a black sheet), while per-pixel alpha and colour keys are kept.
pub fn constant_alpha(hwnd: HWND) -> u8 {
    if !has(ex_style(hwnd), WS_EX_LAYERED.0) {
        return 255;
    }
    let (mut alpha, mut flags) = (255u8, LAYERED_WINDOW_ATTRIBUTES_FLAGS(0));
    // Fails for UpdateLayeredWindow windows, which have no constant alpha.
    let ok = unsafe { GetLayeredWindowAttributes(hwnd, None, Some(&mut alpha), Some(&mut flags)) }.is_ok();
    if ok && flags.contains(LWA_ALPHA) { alpha } else { 255 }
}

pub fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    from_wide(&buf[..n.max(0) as usize])
}

/// Window title without sending `WM_GETTEXT` (which could block on a hung window and
/// re-enter our message loop).
pub fn title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { InternalGetWindowText(hwnd, &mut buf) };
    from_wide(&buf[..n.max(0) as usize])
}

/// Full path of the process owning `hwnd`, or empty if inaccessible.
pub fn process_path(hwnd: HWND) -> String {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return String::new();
    }
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return String::new();
    };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    };
    unsafe {
        let _ = CloseHandle(handle);
    }
    if ok.is_err() {
        return String::new();
    }
    from_wide(&buf[..len as usize])
}

/// `FancyZonesWindowUtils::HasVisibleOwner`
fn has_visible_owner(hwnd: HWND) -> bool {
    let Ok(owner) = (unsafe { GetWindow(hwnd, GW_OWNER) }) else {
        return false;
    };
    if owner.is_invalid() || !unsafe { IsWindowVisible(owner) }.as_bool() {
        return false;
    }
    let mut r = RECT::default();
    if unsafe { GetWindowRect(owner, &mut r) }.is_err() {
        return true;
    }
    r.top != r.bottom && r.left != r.right
}

const SYSTEM_CLASSES: [&str; 6] = [
    "SysListView32",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "Progman",
    "MsoSplash", // FancyZones' SplashClassName
];

/// Upper-cased entries FancyZones always excludes (`IsExcludedByDefault`).
pub const DEFAULT_EXCLUDED_APPS: [&str; 3] = [
    "POWERTOYS.FANCYZONESEDITOR.EXE",
    "WINDOWS.UI.CORE.COREWINDOW",
    "SEARCHUI.EXE",
];

/// Full eligibility check, run once per maximize. App-exclusion lists are checked by
/// the caller (they need configuration).
pub fn check_eligible(hwnd: HWND, allow_child_windows: bool) -> Result<(), &'static str> {
    if !is_trackable(hwnd) {
        return Err("not a visible, resizable top-level window with a maximize box");
    }
    if unsafe { GetAncestor(hwnd, GA_ROOT) } != hwnd {
        return Err("not a root window");
    }
    let s = style(hwnd);
    // FancyZones' popup rule (always satisfied given is_trackable, kept for parity).
    if has(s, WS_POPUP.0)
        && !(has(s, WS_THICKFRAME.0) && (has(s, WS_CAPTION.0) || has(s, WS_MINIMIZEBOX.0) || has(s, WS_MAXIMIZEBOX.0)))
    {
        return Err("popup window");
    }
    if !allow_child_windows && has_visible_owner(hwnd) {
        return Err("owned (child/dialog) window");
    }
    if is_cloaked(hwnd) {
        return Err("cloaked");
    }
    if hwnd == unsafe { GetDesktopWindow() } || hwnd == unsafe { GetShellWindow() } {
        return Err("shell window");
    }
    let class = class_name(hwnd);
    if SYSTEM_CLASSES.contains(&class.as_str()) {
        return Err("system window class");
    }
    Ok(())
}

/// FancyZones' zone stamp on a window it snapped (`FancyZones_zones` /
/// `FancyZones_zones_max128` properties): two 64-bit halves of the zone-index bitmask.
/// `(0, 0)` when FancyZones hasn't snapped the window. Reading a property neither sends
/// a message nor needs access to the other process.
pub fn fancyzones_stamp(hwnd: HWND) -> (u64, u64) {
    let read = |name: windows::core::PCWSTR| unsafe { GetPropW(hwnd, name) }.0 as usize as u64;
    (
        read(windows::core::w!("FancyZones_zones")),
        read(windows::core::w!("FancyZones_zones_max128")),
    )
}

/// True for processes under `C:\Windows\SystemApps` (FancyZones excludes these).
pub fn is_system_app(path_upper: &str) -> bool {
    path_upper.contains("SYSTEMAPPS")
}

pub fn placement(hwnd: HWND) -> Option<WINDOWPLACEMENT> {
    let mut p = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    unsafe { GetWindowPlacement(hwnd, &mut p) }.ok().map(|_| p)
}

pub fn window_rect(hwnd: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut r) }.ok().map(|_| rect_from(r))
}

/// The visible frame (`DWMWA_EXTENDED_FRAME_BOUNDS`), always in physical pixels.
pub fn frame_rect(hwnd: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut r as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .ok()
    .map(|_| rect_from(r))
}

/// Measure the invisible borders. Only meaningful in the normal (restored) state.
pub fn measure_margins(hwnd: HWND) -> Option<Margins> {
    if is_zoomed(hwnd) || is_iconic(hwnd) {
        return None;
    }
    let m = Margins::between(window_rect(hwnd)?, frame_rect(hwnd)?);
    m.is_plausible().then_some(m)
}

/// Best guess when a window's borders were never measured in the normal state: the
/// standard resize-frame thickness at the window's DPI, minus the 1 px visible border,
/// on left/right/bottom (the top edge has no invisible border on Windows 10/11).
pub fn estimate_margins(hwnd: HWND) -> Margins {
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let cx = unsafe { GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi) };
    let cy = unsafe { GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi) };
    let side = (cx - 1).max(0);
    let bottom = (cy - 1).max(0);
    Margins::new(side, 0, side, bottom)
}

/// Monitor + work area containing `r` (nearest).
fn monitor_and_work(r: &Rect) -> (Rect, Rect) {
    let raw = rect_to(*r);
    let h = unsafe { MonitorFromRect(&raw, MONITOR_DEFAULTTONEAREST) };
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(h, &mut mi) }.as_bool() {
        (rect_from(mi.rcMonitor), rect_from(mi.rcWork))
    } else {
        (*r, *r)
    }
}

/// `ScreenToWorkAreaCoords` from FancyZones' WindowUtils.cpp: workspace coordinates are
/// screen coordinates shifted by the work area's offset inside its monitor (non-zero
/// only with a top/left taskbar). The monitor is found twice: first from the rect,
/// then from the shifted rect, to handle rects straddling monitors.
pub fn screen_to_workspace_with(rect: Rect, lookup: impl Fn(&Rect) -> (Rect, Rect)) -> Rect {
    let (m, w) = lookup(&rect);
    let reference = rect.offset(-(w.left - m.left), -(w.top - m.top));
    let (m, w) = lookup(&reference);
    rect.offset(-(w.left - m.left), -(w.top - m.top))
}

/// Inverse of [`screen_to_workspace_with`].
pub fn workspace_to_screen_with(rect: Rect, lookup: impl Fn(&Rect) -> (Rect, Rect)) -> Rect {
    let (m, w) = lookup(&rect);
    let shifted = rect.offset(w.left - m.left, w.top - m.top);
    let (m, w) = lookup(&shifted);
    rect.offset(w.left - m.left, w.top - m.top)
}

/// The window's restore rect (`rcNormalPosition`) in screen coordinates.
pub fn normal_rect_screen(hwnd: HWND) -> Option<Rect> {
    let p = placement(hwnd)?;
    Some(workspace_to_screen_with(
        rect_from(p.rcNormalPosition),
        monitor_and_work,
    ))
}

fn is_dpi_unaware(hwnd: HWND) -> bool {
    unsafe {
        let ctx = GetWindowDpiAwarenessContext(hwnd);
        GetAwarenessFromDpiAwarenessContext(ctx).0 < DPI_AWARENESS_PER_MONITOR_AWARE.0
    }
}

/// Restore `hwnd` (if maximized) into `target` (window rect, screen coordinates) with a
/// single `SetWindowPlacement`, like FancyZones' `SizeWindowToRect`. Asynchronous, so a
/// hung target can never block us.
pub fn restore_to(hwnd: HWND, target: Rect, all_monitors_same_dpi: bool) -> windows::core::Result<()> {
    let mut p = placement(hwnd).ok_or_else(windows::core::Error::from_thread)?;
    p.showCmd = SW_RESTORE.0 as u32;
    p.flags.0 &= !WPF_RESTORETOMAXIMIZED.0;
    p.flags.0 |= WPF_ASYNCWINDOWPLACEMENT.0;

    let mut ws = screen_to_workspace_with(target, monitor_and_work);
    if !all_monitors_same_dpi && is_dpi_unaware(hwnd) {
        // FancyZones clamps for DPI-unaware windows on mixed-DPI setups, where Windows'
        // scaling of the placement can otherwise push the window off its monitor.
        let (m, w) = monitor_and_work(&target);
        let (dx, dy) = (w.left - m.left, w.top - m.top);
        ws = Rect::new(
            ws.left.max(m.left),
            ws.top.max(m.top),
            ws.right.min(m.right - dx),
            ws.bottom.min(m.bottom - dy),
        );
    }
    p.rcNormalPosition = rect_to(ws);
    unsafe { SetWindowPlacement(hwnd, &p) }
}

/// Whether we're allowed to move `hwnd`: re-applies its current placement unchanged.
/// UIPI rejects this for windows of elevated processes, which is exactly the case to
/// detect (an animation into a zone the window then can't move to would look broken).
pub fn can_place(hwnd: HWND) -> bool {
    let Some(mut p) = placement(hwnd) else {
        return false;
    };
    p.flags.0 |= WPF_ASYNCWINDOWPLACEMENT.0;
    unsafe { SetWindowPlacement(hwnd, &p) }.is_ok()
}

/// Bring `hwnd` to the foreground after we swallowed the click that would have activated
/// it. Same trick as FancyZones' `SwitchToWindow`: a zero-length synthetic mouse input
/// makes us the process that "received the last input", which permits
/// `SetForegroundWindow`.
pub fn activate(hwnd: HWND) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_MOUSE, SendInput};
    use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
    let input = INPUT {
        r#type: INPUT_MOUSE,
        ..Default::default()
    };
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Ask the window to maximize itself exactly as its maximize button would
/// (`WM_SYSCOMMAND` / `SC_MAXIMIZE`), asynchronously.
pub fn post_sc_maximize(hwnd: HWND) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, SC_MAXIMIZE, WM_SYSCOMMAND};
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_SYSCOMMAND, WPARAM(SC_MAXIMIZE as usize), LPARAM(0));
    }
}

/// Move/resize a window in its normal state (screen coordinates), asynchronously.
pub fn move_to(hwnd: HWND, r: Rect) -> windows::core::Result<()> {
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            r.left,
            r.top,
            r.width(),
            r.height(),
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two monitors side by side; the left one has a 40px taskbar on the left edge,
    /// the right one a 48px taskbar on top.
    fn lookup(r: &Rect) -> (Rect, Rect) {
        let left_mon = Rect::new(0, 0, 1920, 1080);
        let right_mon = Rect::new(1920, 0, 1920 + 2560, 1440);
        let c = r.center();
        if c.x < 1920 {
            (left_mon, Rect::new(40, 0, 1920, 1080))
        } else {
            (right_mon, Rect::new(1920, 48, 1920 + 2560, 1440))
        }
    }

    #[test]
    fn workspace_conversion_roundtrips() {
        for r in [
            Rect::new(100, 100, 900, 700),
            Rect::new(2100, 300, 3500, 1300),
            Rect::new(1500, 100, 2600, 800), // straddles both
        ] {
            let ws = screen_to_workspace_with(r, lookup);
            assert_eq!(workspace_to_screen_with(ws, lookup), r, "{r}");
        }
    }

    #[test]
    fn workspace_offsets_follow_the_taskbar() {
        let ws = screen_to_workspace_with(Rect::new(100, 100, 900, 700), lookup);
        assert_eq!(ws, Rect::new(60, 100, 860, 700)); // left taskbar: x shifted by 40
        let ws = screen_to_workspace_with(Rect::new(2100, 300, 3500, 1300), lookup);
        assert_eq!(ws, Rect::new(2100, 252, 3500, 1252)); // top taskbar: y shifted by 48
    }

    #[test]
    fn no_offset_with_bottom_taskbar() {
        let flat = |_: &Rect| (Rect::new(0, 0, 1920, 1080), Rect::new(0, 0, 1920, 1040));
        let r = Rect::new(10, 20, 30, 40);
        assert_eq!(screen_to_workspace_with(r, flat), r);
        assert_eq!(workspace_to_screen_with(r, flat), r);
    }

    #[test]
    fn system_apps_path() {
        assert!(is_system_app(
            r"C:\WINDOWS\SYSTEMAPPS\MICROSOFT.WINDOWS.SEARCH\SEARCHAPP.EXE"
        ));
        assert!(!is_system_app(r"C:\WINDOWS\SYSTEM32\NOTEPAD.EXE"));
    }
}
