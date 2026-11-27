//! The About window: which fz-maximize this is (version, architecture, how it was
//! installed), what's new in it, a button to check for updates (and install one), and
//! links. A native Win32 window like the settings and update windows.

use std::cell::RefCell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, COLOR_GRAYTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT, CreateFontIndirectW, DeleteObject, FW_SEMIBOLD,
    GetMonitorInfoW, GetSysColor, GetSysColorBrush, HDC, HFONT, HGDIOBJ, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromPoint, SetBkColor, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    ICC_LINK_CLASS, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, NM_CLICK, NM_RETURN, NMHDR,
    NMLINK, WC_LINK,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    BN_CLICKED, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow,
    ES_AUTOVSCROLL, ES_MULTILINE, ES_READONLY, GetDlgCtrlID, GetDlgItem, HICON, HMENU, ICON_BIG, ICON_SMALL,
    IsDialogMessageW, IsWindow, MSG, NONCLIENTMETRICSW, RegisterClassExW, SPI_GETNONCLIENTMETRICS, SW_SHOW,
    SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW, SetForegroundWindow, SetWindowPos, SetWindowTextW, ShowWindow,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CTLCOLORSTATIC, WM_DESTROY, WM_DPICHANGED, WM_NOTIFY,
    WM_SETFONT, WM_SETICON, WNDCLASSEXW, WS_CAPTION, WS_CHILD, WS_EX_CLIENTEDGE, WS_EX_CONTROLPARENT, WS_OVERLAPPED,
    WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::settings::{make_font, scale};

pub const ABOUT_CLASS: PCWSTR = w!("fz-maximize.about");

pub const WEBSITE: &str = "https://fz-maximize.github.io/fz-maximize/";
const PRIVACY: &str = "https://fz-maximize.github.io/fz-maximize/privacy/";
const LICENSE: &str = "https://github.com/fz-maximize/fz-maximize/blob/main/LICENSE";
const RELEASES: &str = "https://github.com/fz-maximize/fz-maximize/releases";

/// Where the update check stands, as the window shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Nothing checked since the window opened. `auto`: daily checks are on.
    Idle {
        auto: bool,
    },
    Checking,
    UpToDate,
    Available {
        version: String,
        notes_url: String,
    },
    Failed(String),
    Installing(String),
    InstallFailed(String),
}

/// What the window shows.
pub struct Info {
    pub version: String,
    /// "x64, installed for this user", "ARM64, portable", ...
    pub build: String,
    pub notes: Option<String>,
    /// False for development builds: they can only be told about new versions.
    pub can_install: bool,
    pub status: Status,
}

/// What the buttons do (called from the window's message handling, no app borrow held).
pub struct Handlers {
    pub check: fn(),
    pub install: fn(&str),
}

static HANDLERS: OnceLock<Handlers> = OnceLock::new();
static HWND_OPEN: AtomicIsize = AtomicIsize::new(0);

pub fn set_handlers(h: Handlers) {
    let _ = HANDLERS.set(h);
}

fn open_hwnd() -> Option<HWND> {
    let h = HWND(HWND_OPEN.load(Ordering::Relaxed) as *mut core::ffi::c_void);
    (!h.is_invalid() && unsafe { IsWindow(Some(h)) }.as_bool()).then_some(h)
}

/// Keyboard navigation (Tab, Enter, Esc) for the window, from the main message loop.
pub fn dialog_message(msg: &MSG) -> bool {
    match open_hwnd() {
        Some(h) => unsafe { IsDialogMessageW(h, msg) }.as_bool(),
        None => false,
    }
}

const ID_CLOSE: i32 = 2; // IDCANCEL: Esc and the close box
const ID_TITLE: i32 = 10;
const ID_BUILD: i32 = 11;
const ID_NOTES_LABEL: i32 = 12;
const ID_NOTES: i32 = 13;
const ID_UPDATE: i32 = 14;
const ID_STATUS: i32 = 15;
const ID_LINKS: i32 = 16;

const WIDTH: i32 = 460;
const PAD: i32 = 14;
const UPDATE_W: i32 = 170;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctl {
    Title,
    Label,
    Hint,
    Notes,
    Button,
    DefaultButton,
    Link,
}

struct Spec {
    id: i32,
    ctl: Ctl,
    text: String,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

fn layout(info: &Info) -> (Vec<Spec>, i32) {
    let mut specs = Vec::new();
    let mut add = |id, ctl, text: &str, x, y, w, h| {
        specs.push(Spec {
            id,
            ctl,
            text: text.to_string(),
            x,
            y,
            w,
            h,
        })
    };
    let inner = WIDTH - 2 * PAD;
    let mut y = PAD;
    add(
        ID_TITLE,
        Ctl::Title,
        &format!("fz-maximize {}", info.version),
        PAD,
        y,
        inner,
        30,
    );
    y += 32;
    add(ID_BUILD, Ctl::Hint, &info.build, PAD, y, inner, 20);
    y += 30;
    add(
        ID_NOTES_LABEL,
        Ctl::Label,
        "What's new in this version:",
        PAD,
        y,
        inner,
        20,
    );
    y += 22;
    let notes = info
        .notes
        .clone()
        .unwrap_or_else(|| "No notes for this version.".into());
    add(ID_NOTES, Ctl::Notes, &notes, PAD, y, inner, 120);
    y += 132;
    let (button, status) = status_texts(&info.status, info.can_install);
    add(ID_UPDATE, Ctl::Button, &button, PAD, y, UPDATE_W, 28);
    add(
        ID_STATUS,
        Ctl::Link,
        &status,
        PAD + UPDATE_W + 10,
        y - 2,
        inner - UPDATE_W - 10,
        34,
    );
    y += 44;
    let links = format!(
        r#"<a href="{WEBSITE}">Website and help</a>    <a href="{RELEASES}/tag/v{v}">Release notes</a>    <a href="{PRIVACY}">Privacy</a>    <a href="{LICENSE}">License (MIT)</a>"#,
        v = info.version
    );
    add(ID_LINKS, Ctl::Link, &links, PAD, y, inner, 20);
    y += 32;
    add(ID_CLOSE, Ctl::DefaultButton, "Close", WIDTH - PAD - 100, y, 100, 28);
    y += 28;
    (specs, y + PAD)
}

/// SysLink text treats `<` as markup.
fn link_safe(s: &str) -> String {
    s.replace('<', "\u{2039}").replace('>', "\u{203A}")
}

/// The update button's label and the status line (SysLink markup) for `status`.
fn status_texts(status: &Status, can_install: bool) -> (String, String) {
    match status {
        Status::Idle { auto: true } => ("Check for updates".into(), "Checks automatically once a day.".into()),
        Status::Idle { auto: false } => (
            "Check for updates".into(),
            "Automatic checks are off (Settings > General).".into(),
        ),
        Status::Checking => ("Checking...".into(), "Asking GitHub for the latest version...".into()),
        Status::UpToDate => ("Check for updates".into(), "You have the latest version.".into()),
        Status::Available { version, notes_url } => {
            let v = link_safe(version);
            let notes = if notes_url.starts_with("https://") {
                format!(r#" <a href="{}">What's new</a>"#, link_safe(notes_url))
            } else {
                String::new()
            };
            if can_install {
                (
                    format!("Install {v} now"),
                    format!("fz-maximize {v} is available.{notes}"),
                )
            } else {
                (
                    "Check for updates".into(),
                    format!("fz-maximize {v} is available (development build: update from source).{notes}"),
                )
            }
        }
        Status::Failed(e) => ("Check for updates".into(), format!("Couldn't check: {}", link_safe(e))),
        Status::InstallFailed(e) => (
            "Check for updates".into(),
            format!("Couldn't install: {}", link_safe(e)),
        ),
        Status::Installing(v) => (
            "Installing...".into(),
            format!("Downloading {}. fz-maximize restarts when it's done.", link_safe(v)),
        ),
    }
}

struct State {
    font: HFONT,
    title_font: HFONT,
    icons: Vec<HICON>,
    specs: Vec<Spec>,
    can_install: bool,
    status: Status,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn item(hwnd: HWND, id: i32) -> HWND {
    unsafe { GetDlgItem(Some(hwnd), id) }.unwrap_or_default()
}

fn send(h: HWND, msg: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(h, msg, Some(WPARAM(w)), Some(LPARAM(l))) }.0
}

/// The message font, larger and semibold, for the title.
fn make_title_font(dpi: u32) -> HFONT {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some((&mut ncm as *mut NONCLIENTMETRICSW).cast()),
            0,
            dpi,
        );
        let mut lf = ncm.lfMessageFont;
        lf.lfHeight = lf.lfHeight * 3 / 2;
        lf.lfWeight = FW_SEMIBOLD.0 as i32;
        CreateFontIndirectW(&lf)
    }
}

fn create_controls(hwnd: HWND, specs: &[Spec]) {
    let hinstance = unsafe { GetModuleHandleW(None) }.ok().map(|m| m.into());
    for s in specs {
        const LWS_TRANSPARENT: u32 = 0x0002;
        let (class, style, ex): (PCWSTR, u32, u32) = match s.ctl {
            Ctl::Title | Ctl::Label | Ctl::Hint => (w!("STATIC"), 0, 0),
            Ctl::Notes => (
                w!("EDIT"),
                (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32 | WS_VSCROLL.0 | WS_TABSTOP.0,
                WS_EX_CLIENTEDGE.0,
            ),
            Ctl::Button => (w!("BUTTON"), BS_PUSHBUTTON as u32 | WS_TABSTOP.0, 0),
            Ctl::DefaultButton => (w!("BUTTON"), BS_DEFPUSHBUTTON as u32 | WS_TABSTOP.0, 0),
            Ctl::Link => (WC_LINK, LWS_TRANSPARENT | WS_TABSTOP.0, 0),
        };
        unsafe {
            let _ = CreateWindowExW(
                WINDOW_EX_STYLE(ex),
                class,
                &HSTRING::from(s.text.as_str()),
                WINDOW_STYLE(style | WS_CHILD.0 | WS_VISIBLE.0),
                0,
                0,
                0,
                0,
                Some(hwnd),
                Some(HMENU(s.id as isize as *mut core::ffi::c_void)),
                hinstance,
                None,
            );
        }
    }
}

fn relayout(hwnd: HWND, specs: &[Spec], dpi: u32, font: HFONT, title_font: HFONT) {
    for s in specs {
        let h = item(hwnd, s.id);
        if h.is_invalid() {
            continue;
        }
        unsafe {
            let _ = SetWindowPos(
                h,
                None,
                scale(s.x, dpi),
                scale(s.y, dpi),
                scale(s.w, dpi),
                scale(s.h, dpi),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        let f = if s.ctl == Ctl::Title { title_font } else { font };
        send(h, WM_SETFONT, f.0 as usize, 1);
    }
}

fn window_size(dpi: u32, client_h: i32) -> (i32, i32) {
    let mut r = RECT {
        left: 0,
        top: 0,
        right: scale(WIDTH, dpi),
        bottom: scale(client_h, dpi),
    };
    unsafe {
        let _ = AdjustWindowRectExForDpi(
            &mut r,
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            false,
            WS_EX_CONTROLPARENT,
            dpi,
        );
    }
    (r.right - r.left, r.bottom - r.top)
}

/// Show the update check's progress or result in the window, if it's open.
pub fn set_status(status: Status) {
    let Some(hwnd) = open_hwnd() else {
        return;
    };
    let can_install = STATE.with_borrow_mut(|s| {
        s.as_mut().map(|st| {
            st.status = status.clone();
            st.can_install
        })
    });
    let Some(can_install) = can_install else {
        return;
    };
    let (button, text) = status_texts(&status, can_install);
    let busy = matches!(status, Status::Checking | Status::Installing(_));
    unsafe {
        let b = item(hwnd, ID_UPDATE);
        let _ = SetWindowTextW(b, &HSTRING::from(button));
        let _ = EnableWindow(b, !busy);
        let _ = SetWindowTextW(item(hwnd, ID_STATUS), &HSTRING::from(text));
    }
}

fn update_clicked() {
    let status = STATE.with_borrow(|s| s.as_ref().map(|st| (st.status.clone(), st.can_install)));
    let Some(handlers) = HANDLERS.get() else {
        return;
    };
    match status {
        Some((Status::Available { version, .. }, true)) => (handlers.install)(&version),
        Some(_) => (handlers.check)(),
        None => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND if ((wparam.0 >> 16) & 0xFFFF) as u32 == BN_CLICKED => {
            match (wparam.0 & 0xFFFF) as i32 {
                ID_UPDATE => update_clicked(),
                ID_CLOSE => unsafe {
                    let _ = DestroyWindow(hwnd);
                },
                _ => {}
            }
            LRESULT(0)
        }
        WM_NOTIFY => {
            let hdr = unsafe { &*(lparam.0 as *const NMHDR) };
            if (hdr.code == NM_CLICK || hdr.code == NM_RETURN) && matches!(hdr.idFrom as i32, ID_LINKS | ID_STATUS) {
                let link = unsafe { &*(lparam.0 as *const NMLINK) };
                let url = &link.item.szUrl;
                let n = url.iter().position(|&c| c == 0).unwrap_or(url.len());
                crate::app::open_url(&String::from_utf16_lossy(&url[..n]));
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            let child = HWND(lparam.0 as *mut core::ffi::c_void);
            let id = unsafe { GetDlgCtrlID(child) };
            let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
            unsafe {
                if id == ID_NOTES {
                    // The read-only notes box: like a page of text, not the dialog.
                    SetTextColor(hdc, COLORREF(GetSysColor(COLOR_WINDOWTEXT)));
                    SetBkColor(hdc, COLORREF(GetSysColor(COLOR_WINDOW)));
                    return LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize);
                }
                if id == ID_BUILD {
                    SetTextColor(hdc, COLORREF(GetSysColor(COLOR_GRAYTEXT)));
                }
                SetBkMode(hdc, TRANSPARENT);
                LRESULT(GetSysColorBrush(COLOR_BTNFACE).0 as isize)
            }
        }
        WM_DPICHANGED => {
            let dpi = ((wparam.0 >> 16) & 0xFFFF) as u32;
            let suggested = unsafe { &*(lparam.0 as *const RECT) };
            STATE.with_borrow_mut(|s| {
                if let Some(st) = s.as_mut() {
                    let old = (st.font, st.title_font);
                    st.font = make_font(dpi);
                    st.title_font = make_title_font(dpi);
                    relayout(hwnd, &st.specs, dpi, st.font, st.title_font);
                    unsafe {
                        let _ = DeleteObject(HGDIOBJ(old.0.0));
                        let _ = DeleteObject(HGDIOBJ(old.1.0));
                    }
                }
            });
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            STATE.with_borrow_mut(|s| {
                if let Some(st) = s.take() {
                    unsafe {
                        let _ = DeleteObject(HGDIOBJ(st.font.0));
                        let _ = DeleteObject(HGDIOBJ(st.title_font.0));
                        for icon in st.icons {
                            let _ = DestroyIcon(icon);
                        }
                    }
                }
            });
            HWND_OPEN.store(0, Ordering::Relaxed);
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn register_class() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES | ICC_LINK_CLASS,
        };
        let _ = InitCommonControlsEx(&icc);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
            lpszClassName: ABOUT_CLASS,
            hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
            hCursor: windows::Win32::UI::WindowsAndMessaging::LoadCursorW(
                None,
                windows::Win32::UI::WindowsAndMessaging::IDC_ARROW,
            )
            .unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassExW(&wc) != 0
    })
}

/// Show the About window (or bring the open one to the front).
pub fn open(info: Info) {
    if let Some(h) = open_hwnd() {
        unsafe {
            let _ = SetForegroundWindow(h);
        }
        return;
    }
    if !register_class() {
        return;
    }
    // Centre on the primary monitor's work area.
    let mon = unsafe { MonitorFromPoint(windows::Win32::Foundation::POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = GetMonitorInfoW(mon, &mut mi);
    }
    let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
    unsafe {
        let _ = windows::Win32::UI::HiDpi::GetDpiForMonitor(
            mon,
            windows::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI,
            &mut dpi_x,
            &mut dpi_y,
        );
    }
    let (specs, height) = layout(&info);
    let (w, h) = window_size(dpi_x, height);
    let work = mi.rcWork;
    let x = work.left + ((work.right - work.left) - w).max(0) / 2;
    let y = work.top + ((work.bottom - work.top) - h).max(0) / 2;
    let hinstance = unsafe { GetModuleHandleW(None) }.ok().map(|m| m.into());
    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_CONTROLPARENT,
            ABOUT_CLASS,
            w!("About fz-maximize"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            w,
            h,
            None,
            None,
            hinstance,
            None,
        )
    } {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "could not create the About window");
            return;
        }
    };
    HWND_OPEN.store(hwnd.0 as isize, Ordering::Relaxed);
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    if dpi != dpi_x {
        let (w, h) = window_size(dpi, height);
        unsafe {
            let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
    create_controls(hwnd, &specs);
    let font = make_font(dpi);
    let title_font = make_title_font(dpi);
    relayout(hwnd, &specs, dpi, font, title_font);
    let mut icons = Vec::new();
    for (which, size) in [(ICON_SMALL, 16), (ICON_BIG, 32)] {
        if let Some(icon) = crate::tray::create_icon(scale(size, dpi), false) {
            send(hwnd, WM_SETICON, which as usize, icon.0 as isize);
            icons.push(icon);
        }
    }
    let busy = matches!(info.status, Status::Checking | Status::Installing(_));
    unsafe {
        let _ = EnableWindow(item(hwnd, ID_UPDATE), !busy);
    }
    STATE.with_borrow_mut(|s| {
        *s = Some(State {
            font,
            title_font,
            icons,
            specs,
            can_install: info.can_install,
            status: info.status,
        })
    });
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        if !SetForegroundWindow(hwnd).as_bool() {
            // See update_dialog::open: at least don't open behind other windows.
            use windows::Win32::UI::WindowsAndMessaging::{HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE};
            let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
            let _ = SetWindowPos(hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, flags);
        }
    }
}

/// "x64, installed for this user" and the like.
pub fn build_description(kind: &crate::update::InstallKind) -> String {
    use crate::update::InstallKind;
    let arch = if cfg!(target_arch = "aarch64") { "ARM64" } else { "x64" };
    let how = match kind {
        InstallKind::Installer { all_users: false } => "installed for this user",
        InstallKind::Installer { all_users: true } => "installed for all users",
        InstallKind::Portable { .. } => "portable",
        InstallKind::Dev => "development build",
    };
    format!("{arch}, {how}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(status: Status) -> Info {
        Info {
            version: "0.4.2".into(),
            build: "x64, installed for this user".into(),
            notes: Some("\u{2022} One".into()),
            can_install: true,
            status,
        }
    }

    #[test]
    fn layout_fits_and_ids_are_unique() {
        let (specs, height) = layout(&info(Status::Idle { auto: true }));
        assert!(height > 250 && height < 450, "height {height}");
        let mut ids: Vec<i32> = specs.iter().map(|s| s.id).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate control ids");
        for s in &specs {
            assert!(s.x >= 0 && s.x + s.w <= WIDTH, "{:?} overflows", s.text);
        }
        for id in [ID_TITLE, ID_NOTES, ID_UPDATE, ID_STATUS, ID_LINKS, ID_CLOSE] {
            assert!(specs.iter().any(|s| s.id == id), "missing control {id}");
        }
    }

    #[test]
    fn button_installs_only_when_it_can() {
        let available = Status::Available {
            version: "0.4.3".into(),
            notes_url: "https://github.com/fz-maximize/fz-maximize/releases/tag/v0.4.3".into(),
        };
        let (button, text) = status_texts(&available, true);
        assert_eq!(button, "Install 0.4.3 now");
        assert!(text.contains(r#"<a href="https://github.com/"#), "{text}");
        let (button, text) = status_texts(&available, false);
        assert_eq!(button, "Check for updates");
        assert!(text.contains("development build"), "{text}");
    }

    #[test]
    fn errors_cant_inject_links() {
        let (_, text) = status_texts(&Status::Failed(r#"<a href="https://evil.invalid">x</a>"#.into()), true);
        assert!(!text.contains("<a"), "{text}");
    }
}
