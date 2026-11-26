//! The update window: a new version is available. Install it now, at a chosen time,
//! remind later (1 day, 3 days or a week), or skip that version. A native Win32
//! window like the settings window (DPI-aware, system font).

use std::cell::RefCell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, SYSTEMTIME, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, COLOR_GRAYTEXT, DeleteObject, GetMonitorInfoW, GetSysColor, GetSysColorBrush, HDC, HFONT, HGDIOBJ,
    MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    ICC_DATE_CLASSES, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    BN_CLICKED, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CBS_DROPDOWNLIST,
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetDlgItem, HICON, HMENU, ICON_BIG, ICON_SMALL, IsDialogMessageW,
    IsWindow, MSG, RegisterClassExW, SW_SHOW, SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW, SetForegroundWindow,
    SetWindowPos, ShowWindow, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CTLCOLORSTATIC, WM_DESTROY,
    WM_DPICHANGED, WM_SETFONT, WM_SETICON, WNDCLASSEXW, WS_CAPTION, WS_CHILD, WS_DISABLED, WS_EX_CONTROLPARENT,
    WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::settings::{make_font, scale};

pub const UPDATE_CLASS: PCWSTR = w!("fz-maximize.update");

/// What the user chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    InstallNow,
    /// Install at this local time of day (minutes after midnight), the next time it comes.
    InstallAt {
        minute_of_day: u32,
    },
    RemindIn {
        days: u64,
    },
    Skip,
}

/// What the window offers.
#[derive(Clone, Debug)]
pub struct Offer {
    pub version: String,
    pub current: String,
    pub notes_url: String,
    /// False for development builds: they can only be told about new versions.
    pub can_install: bool,
    /// An install already scheduled ("at 3:00 AM"), shown so it can be changed.
    pub scheduled: Option<String>,
}

type Handler = fn(&str, Choice);
static HANDLER: OnceLock<Handler> = OnceLock::new();
static HWND_OPEN: AtomicIsize = AtomicIsize::new(0);

pub fn set_handler(f: Handler) {
    let _ = HANDLER.set(f);
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

const ID_INSTALL: i32 = 1;
const ID_CLOSE: i32 = 2; // IDCANCEL: Esc and the close box
const ID_AT: i32 = 10;
const ID_TIME: i32 = 11;
const ID_REMIND: i32 = 12;
const ID_DAYS: i32 = 13;
const ID_SKIP: i32 = 14;
const ID_NOTES: i32 = 15;
const ID_HINT_FIRST: i32 = 900;

const REMIND_DAYS: [(&str, u64); 3] = [("1 day", 1), ("3 days", 3), ("1 week", 7)];
/// The suggested install time: 3:00 AM.
const DEFAULT_MINUTE: u32 = 3 * 60;

const WIDTH: i32 = 470;
const PAD: i32 = 14;
const BUTTON_W: i32 = 150;

#[derive(Clone, Copy)]
enum Ctl {
    Label,
    Hint,
    Button,
    DefaultButton,
    Combo,
    Time,
}

struct Spec {
    id: i32,
    ctl: Ctl,
    text: String,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    enabled: bool,
}

fn layout(offer: &Offer) -> (Vec<Spec>, i32) {
    let mut specs = Vec::new();
    let mut hint_id = ID_HINT_FIRST;
    let mut add = |id: i32, ctl: Ctl, text: &str, x: i32, y: i32, w: i32, h: i32, enabled: bool| {
        let id = if matches!(ctl, Ctl::Hint) {
            hint_id += 1;
            hint_id
        } else {
            id
        };
        specs.push(Spec {
            id,
            ctl,
            text: text.to_string(),
            x,
            y,
            w,
            h,
            enabled,
        });
    };
    let inner = WIDTH - 2 * PAD;
    let mut y = PAD;
    add(
        0,
        Ctl::Label,
        &format!(
            "fz-maximize {} is available. You have {}.",
            offer.version, offer.current
        ),
        PAD,
        y,
        inner,
        20,
        true,
    );
    y += 26;
    add(
        ID_NOTES,
        Ctl::Button,
        &format!("What's new in {}", offer.version),
        PAD,
        y,
        190,
        28,
        true,
    );
    y += 42;
    let can = offer.can_install;
    let right = PAD + BUTTON_W + 10;
    add(ID_INSTALL, Ctl::DefaultButton, "Install now", PAD, y, BUTTON_W, 28, can);
    add(
        0,
        Ctl::Hint,
        "Restarts fz-maximize when it's done",
        right,
        y + 5,
        inner - BUTTON_W - 10,
        20,
        true,
    );
    y += 36;
    add(ID_AT, Ctl::Button, "Install at", PAD, y, BUTTON_W, 28, can);
    add(ID_TIME, Ctl::Time, "", right, y + 1, 120, 26, can);
    add(
        0,
        Ctl::Hint,
        "or the next time it's on",
        right + 130,
        y + 5,
        inner - BUTTON_W - 140,
        20,
        true,
    );
    y += 36;
    add(ID_REMIND, Ctl::Button, "Remind me in", PAD, y, BUTTON_W, 28, true);
    add(ID_DAYS, Ctl::Combo, "", right, y + 1, 120, 200, true);
    y += 36;
    add(ID_SKIP, Ctl::Button, "Skip this version", PAD, y, BUTTON_W, 28, true);
    y += 36;
    let note = if !can {
        "This is a development build: update it from source.".to_string()
    } else if let Some(s) = &offer.scheduled {
        format!("Currently scheduled {s}.")
    } else {
        "Downloads only from this project's GitHub releases, and checks the file first.".into()
    };
    add(0, Ctl::Hint, &note, PAD, y + 4, inner, 20, true);
    y += 30;
    (specs, y + PAD / 2)
}

struct State {
    specs: Vec<Spec>,
    font: HFONT,
    icons: Vec<HICON>,
    version: String,
    notes_url: String,
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

// Date and time picker messages and styles (commctrl.h).
const DTM_GETSYSTEMTIME: u32 = 0x1001;
const DTM_SETSYSTEMTIME: u32 = 0x1002;
const DTM_SETFORMATW: u32 = 0x1032;
const DTS_UPDOWN: u32 = 0x0001;
const DTS_TIMEFORMAT: u32 = 0x0009;
const GDT_VALID: usize = 0;

/// The user's short time format ("h:mm tt", "HH:mm"...).
fn short_time_format() -> HSTRING {
    use windows::Win32::Globalization::{GetLocaleInfoEx, LOCALE_SSHORTTIME};
    let mut buf = [0u16; 80];
    let n = unsafe { GetLocaleInfoEx(PCWSTR::null(), LOCALE_SSHORTTIME, Some(&mut buf)) };
    if n > 1 {
        HSTRING::from_wide(&buf[..(n as usize - 1)])
    } else {
        HSTRING::from("h:mm tt")
    }
}

fn create_controls(hwnd: HWND, specs: &[Spec]) {
    let hinstance = unsafe { GetModuleHandleW(None) }.ok().map(|m| m.into());
    for s in specs {
        let (class, style): (PCWSTR, u32) = match s.ctl {
            Ctl::Label | Ctl::Hint => (w!("STATIC"), 0),
            Ctl::Button => (w!("BUTTON"), BS_PUSHBUTTON as u32 | WS_TABSTOP.0),
            Ctl::DefaultButton => (w!("BUTTON"), BS_DEFPUSHBUTTON as u32 | WS_TABSTOP.0),
            Ctl::Combo => (w!("COMBOBOX"), CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0 | WS_TABSTOP.0),
            Ctl::Time => (w!("SysDateTimePick32"), DTS_TIMEFORMAT | DTS_UPDOWN | WS_TABSTOP.0),
        };
        let disabled = if s.enabled { 0 } else { WS_DISABLED.0 };
        unsafe {
            let _ = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                &HSTRING::from(s.text.as_str()),
                WINDOW_STYLE(style | disabled | WS_CHILD.0 | WS_VISIBLE.0),
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
    // Remind: 1 day by default.
    let days = item(hwnd, ID_DAYS);
    for (label, _) in REMIND_DAYS {
        let w = HSTRING::from(label);
        send(days, CB_ADDSTRING, 0, w.as_ptr() as isize);
    }
    send(days, CB_SETCURSEL, 0, 0);
    // Install time: 3:00 AM, in the user's time format.
    let time = item(hwnd, ID_TIME);
    let format = short_time_format();
    send(time, DTM_SETFORMATW, 0, format.as_ptr() as isize);
    let mut st = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    st.wHour = (DEFAULT_MINUTE / 60) as u16;
    st.wMinute = (DEFAULT_MINUTE % 60) as u16;
    st.wSecond = 0;
    st.wMilliseconds = 0;
    send(time, DTM_SETSYSTEMTIME, GDT_VALID, &st as *const SYSTEMTIME as isize);
}

fn relayout(hwnd: HWND, specs: &[Spec], dpi: u32, font: HFONT) {
    for s in specs {
        // Labels have id 0 and hints are numbered: find every control by its spec order.
        let h = if s.id == 0 { None } else { Some(item(hwnd, s.id)) };
        let Some(h) = h.filter(|h| !h.is_invalid()) else {
            continue;
        };
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
        send(h, WM_SETFONT, font.0 as usize, 1);
    }
    // The title label (id 0) is the first child.
    if let Ok(first) = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetWindow(hwnd, windows::Win32::UI::WindowsAndMessaging::GW_CHILD)
    } && let Some(s) = specs.first()
    {
        unsafe {
            let _ = SetWindowPos(
                first,
                None,
                scale(s.x, dpi),
                scale(s.y, dpi),
                scale(s.w, dpi),
                scale(s.h, dpi),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        send(first, WM_SETFONT, font.0 as usize, 1);
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

fn choose(hwnd: HWND, choice: Choice) {
    let version = STATE.with_borrow(|s| s.as_ref().map(|s| s.version.clone()));
    if let (Some(v), Some(f)) = (version, HANDLER.get()) {
        f(&v, choice);
    }
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND if ((wparam.0 >> 16) & 0xFFFF) as u32 == BN_CLICKED => {
            match (wparam.0 & 0xFFFF) as i32 {
                // Enter presses the default button even when it's disabled (dev builds).
                ID_INSTALL
                    if unsafe {
                        windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(item(hwnd, ID_INSTALL))
                    }
                    .as_bool() =>
                {
                    choose(hwnd, Choice::InstallNow)
                }
                ID_AT => {
                    let mut st = SYSTEMTIME::default();
                    let ok = send(
                        item(hwnd, ID_TIME),
                        DTM_GETSYSTEMTIME,
                        0,
                        &mut st as *mut SYSTEMTIME as isize,
                    );
                    let minute = if ok == GDT_VALID as isize {
                        u32::from(st.wHour) * 60 + u32::from(st.wMinute)
                    } else {
                        DEFAULT_MINUTE
                    };
                    choose(hwnd, Choice::InstallAt { minute_of_day: minute });
                }
                ID_REMIND => {
                    let i = send(item(hwnd, ID_DAYS), CB_GETCURSEL, 0, 0).max(0) as usize;
                    choose(
                        hwnd,
                        Choice::RemindIn {
                            days: REMIND_DAYS[i.min(REMIND_DAYS.len() - 1)].1,
                        },
                    );
                }
                ID_SKIP => choose(hwnd, Choice::Skip),
                ID_NOTES => {
                    if let Some(url) = STATE.with_borrow(|s| s.as_ref().map(|s| s.notes_url.clone())) {
                        crate::app::open_url(&url);
                    }
                }
                ID_CLOSE => unsafe {
                    let _ = DestroyWindow(hwnd);
                },
                _ => {}
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            let child = HWND(lparam.0 as *mut core::ffi::c_void);
            let id = unsafe { windows::Win32::UI::WindowsAndMessaging::GetDlgCtrlID(child) };
            let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
            unsafe {
                if id > ID_HINT_FIRST {
                    SetTextColor(hdc, windows::Win32::Foundation::COLORREF(GetSysColor(COLOR_GRAYTEXT)));
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
                    let old = st.font;
                    st.font = make_font(dpi);
                    relayout(hwnd, &st.specs, dpi, st.font);
                    unsafe {
                        let _ = DeleteObject(HGDIOBJ(old.0));
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
                        for icon in st.icons {
                            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(icon);
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
            dwICC: ICC_STANDARD_CLASSES | ICC_DATE_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
            lpszClassName: UPDATE_CLASS,
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

/// Show the update window (or bring the open one to the front).
pub fn open(offer: Offer) {
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
    let (specs, height) = layout(&offer);
    let (w, h) = window_size(dpi_x, height);
    let work = mi.rcWork;
    let x = work.left + ((work.right - work.left) - w).max(0) / 2;
    let y = work.top + ((work.bottom - work.top) - h).max(0) / 2;
    let hinstance = unsafe { GetModuleHandleW(None) }.ok().map(|m| m.into());
    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_CONTROLPARENT,
            UPDATE_CLASS,
            w!("fz-maximize update"),
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
            tracing::error!(error = %e, "could not create the update window");
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
    relayout(hwnd, &specs, dpi, font);
    let mut icons = Vec::new();
    for (which, size) in [(ICON_SMALL, 16), (ICON_BIG, 32)] {
        if let Some(icon) = crate::tray::create_icon(scale(size, dpi), false) {
            send(hwnd, WM_SETICON, which as usize, icon.0 as isize);
            icons.push(icon);
        }
    }
    STATE.with_borrow_mut(|s| {
        *s = Some(State {
            specs,
            font,
            icons,
            version: offer.version.clone(),
            notes_url: offer.notes_url.clone(),
        })
    });
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        if !SetForegroundWindow(hwnd).as_bool() {
            // Windows didn't let us take the foreground (it only allows that right after
            // the user interacted with us): at least put the window on top of the others
            // instead of behind them.
            use windows::Win32::UI::WindowsAndMessaging::{HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE};
            let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
            let _ = SetWindowPos(hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, flags);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(can_install: bool) -> Offer {
        Offer {
            version: "0.3.5".into(),
            current: "0.3.4".into(),
            notes_url: "https://example.invalid".into(),
            can_install,
            scheduled: None,
        }
    }

    #[test]
    fn layout_fits_and_ids_are_unique() {
        for can in [true, false] {
            let (specs, height) = layout(&offer(can));
            assert!(height > 150 && height < 400, "height {height}");
            let ids: Vec<i32> = specs.iter().map(|s| s.id).filter(|&i| i != 0).collect();
            let mut unique = ids.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(unique.len(), ids.len(), "duplicate control ids");
            for s in &specs {
                assert!(s.x >= 0 && s.x + s.w <= WIDTH, "{:?} overflows", s.text);
            }
            for id in [ID_INSTALL, ID_AT, ID_TIME, ID_REMIND, ID_DAYS, ID_SKIP, ID_NOTES] {
                assert!(specs.iter().any(|s| s.id == id), "missing control {id}");
            }
            let install = specs.iter().find(|s| s.id == ID_INSTALL).unwrap();
            assert_eq!(install.enabled, can, "dev builds can't install");
        }
    }
}
