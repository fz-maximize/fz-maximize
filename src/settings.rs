//! Settings window: a small native Win32 window (no UI framework) that edits every
//! config option and applies it immediately.
//!
//! Opened from the tray menu or `fz-maximize --settings`. It runs on the main UI thread
//! (the main message loop forwards keyboard navigation via [`dialog_message`]). Saving
//! goes through a handler registered by the app ([`set_apply_handler`]), which writes
//! `config.toml` (see `config::render`) and applies it live.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, COLOR_GRAYTEXT, CreateFontIndirectW, DeleteObject, GetMonitorInfoW, GetSysColor, GetSysColorBrush,
    HDC, HFONT, HGDIOBJ, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{BST_CHECKED, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    BM_GETCHECK, BM_SETCHECK, BN_CLICKED, BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON, BS_GROUPBOX, BS_PUSHBUTTON, CB_ADDSTRING,
    CB_GETCURSEL, CB_SETCURSEL, CBN_SELCHANGE, CBS_DROPDOWNLIST, CreateWindowExW, DefWindowProcW, DestroyWindow,
    ES_AUTOHSCROLL, ES_AUTOVSCROLL, ES_MULTILINE, ES_WANTRETURN, GetCursorPos, GetDlgItem, GetWindowTextLengthW,
    GetWindowTextW, HICON, HMENU, ICON_BIG, ICON_SMALL, IDCANCEL, IDOK, IsDialogMessageW, MB_ICONWARNING, MB_OK, MSG,
    MessageBoxW, NONCLIENTMETRICSW, RegisterClassExW, SPI_GETNONCLIENTMETRICS, SW_SHOW, SWP_NOACTIVATE, SWP_NOZORDER,
    SendMessageW, SetForegroundWindow, SetWindowPos, SetWindowTextW, ShowWindow, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_CLOSE, WM_COMMAND, WM_CTLCOLORSTATIC, WM_DESTROY, WM_DPICHANGED, WM_SETFONT, WM_SETICON, WNDCLASSEXW,
    WS_CAPTION, WS_CHILD, WS_EX_CLIENTEDGE, WS_EX_CONTROLPARENT, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP,
    WS_VISIBLE, WS_VSCROLL,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::config::{self, AlreadyInZone, AnimationStyle, BypassModifier, Config, MismatchAction};

pub const SETTINGS_CLASS: PCWSTR = w!("fz-maximize.settings");

/// Everything the window shows that doesn't live in `Config`.
#[derive(Clone, Debug)]
pub struct Context {
    pub config: Config,
    pub config_path: PathBuf,
    pub autostart: bool,
    /// "Connected: BOE0A52 (\\.\DISPLAY1) ..." under the excluded-monitors box.
    pub monitors_hint: String,
    /// FancyZones agreement summary.
    pub check_status: String,
}

/// Saves and applies settings; returns a user-facing error message on failure.
pub type ApplyFn = fn(Config, bool) -> Result<(), String>;

static APPLY: OnceLock<ApplyFn> = OnceLock::new();
static HWND_OPEN: AtomicIsize = AtomicIsize::new(0);

pub fn set_apply_handler(f: ApplyFn) {
    let _ = APPLY.set(f);
}

fn open_hwnd() -> Option<HWND> {
    let raw = HWND_OPEN.load(Ordering::Relaxed);
    (raw != 0).then_some(HWND(raw as *mut core::ffi::c_void))
}

/// Keyboard navigation (Tab, Enter = OK, Esc = Cancel) for the settings window. Call
/// from the message loop; returns true when the message was handled.
pub fn dialog_message(msg: &MSG) -> bool {
    match open_hwnd() {
        Some(h) => unsafe { IsDialogMessageW(h, msg) }.as_bool(),
        None => false,
    }
}

// ---------------------------------------------------------------------------------------
// Option tables (index in combo box <-> value)
// ---------------------------------------------------------------------------------------

const MODIFIERS: [(&str, BypassModifier); 4] = [
    ("Shift", BypassModifier::Shift),
    ("Ctrl", BypassModifier::Ctrl),
    ("Alt", BypassModifier::Alt),
    ("Nothing (no full-screen bypass)", BypassModifier::None),
];
const ALREADY: [(&str, AlreadyInZone); 2] = [
    ("Restore its previous size", AlreadyInZone::Restore),
    ("Maximize to full screen", AlreadyInZone::Maximize),
];
const MISMATCH: [(&str, MismatchAction); 2] = [
    ("Warn me", MismatchAction::Warn),
    ("Warn, and use normal maximize on that monitor", MismatchAction::Native),
];
const STYLES: [(&str, AnimationStyle); 3] = [
    ("Match this Windows", AnimationStyle::Auto),
    ("Zoom into place (Windows 10)", AnimationStyle::Zoom),
    ("Move into the zone (Windows 11)", AnimationStyle::Morph),
];
const LOG_LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];

// Control ids. OK/Cancel use IDOK/IDCANCEL so Enter/Esc work via IsDialogMessage.
const ID_OK: i32 = IDOK.0;
const ID_CANCEL: i32 = IDCANCEL.0;
const ID_APPLY: i32 = 100;
const ID_OPEN_FILE: i32 = 101;
const ID_MODIFIER: i32 = 110;
const ID_ALREADY: i32 = 111;
const ID_DRAG: i32 = 112;
const ID_LOCK: i32 = 115;
const ID_PRE_MOUSE: i32 = 120;
const ID_PRE_WINUP: i32 = 121;
const ID_WINUP_BYPASS: i32 = 122;
const ID_ANIMATE: i32 = 123;
const ID_ANIM_MS: i32 = 124;
const ID_ANIM_MATCH: i32 = 125;
const ID_ANIM_STYLE: i32 = 126;
const ID_ASPECT: i32 = 130;
const ID_MONITORS: i32 = 131;
const ID_APPS: i32 = 132;
const ID_FZ_EXCLUDED: i32 = 133;
const ID_FZ_DEFAULT: i32 = 134;
const ID_FZ_REQUIRE: i32 = 135;
const ID_CHECK: i32 = 140;
const ID_MISMATCH: i32 = 141;
const ID_AUTOSTART: i32 = 150;
const ID_LOGLEVEL: i32 = 151;
const ID_UPDATES: i32 = 152;
/// Hint labels (drawn in grey) use ids in this range.
const ID_HINT_FIRST: i32 = 900;

// ---------------------------------------------------------------------------------------
// Layout (device-independent pixels; scaled by DPI)
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ctl {
    Group,
    Label,
    Hint,
    Check,
    Combo,
    Edit,
    MultiEdit,
    Button,
    DefaultButton,
}

#[derive(Clone, Debug)]
struct Spec {
    id: i32,
    ctl: Ctl,
    text: String,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// Two columns, so the window fits a 1080p laptop screen even at 150% scaling.
const COL_W: i32 = 560;
const PAD: i32 = 12;
const WIDTH: i32 = 2 * COL_W + 3 * PAD;
const ROW: i32 = 26;

struct Builder {
    specs: Vec<Spec>,
    /// Left edge and width of the current column.
    x0: i32,
    y: i32,
    group_top: i32,
    next_hint: i32,
    /// Extra left margin for options nested under a heading or checkbox.
    indent: i32,
}

/// How far nested options are indented.
const INDENT: i32 = 22;

impl Builder {
    #[allow(clippy::too_many_arguments)]
    fn push(&mut self, id: i32, ctl: Ctl, text: &str, x: i32, y: i32, w: i32, h: i32) {
        self.specs.push(Spec {
            id,
            ctl,
            text: text.to_string(),
            x,
            y,
            w,
            h,
        });
    }
    /// Start a new column at the top.
    fn column(&mut self, index: i32) {
        self.x0 = PAD + index * (COL_W + PAD);
        self.y = PAD;
    }
    /// Inner left edge / width (inside a group box).
    fn ix(&self) -> i32 {
        self.x0 + PAD + self.indent
    }
    fn iw(&self) -> i32 {
        COL_W - 2 * PAD - self.indent
    }
    /// Nest the following options (`true`) under the heading or checkbox above them,
    /// or stop nesting (`false`).
    fn nested(&mut self, on: bool) {
        self.indent = if on { INDENT } else { 0 };
    }
    fn group(&mut self, title: &str) {
        self.group_top = self.y;
        self.push(0, Ctl::Group, title, self.x0, self.y, COL_W, 0);
        self.y += 24;
    }
    fn end_group(&mut self) {
        let h = self.y - self.group_top + 4;
        if let Some(g) = self.specs.iter_mut().rev().find(|s| s.ctl == Ctl::Group) {
            g.h = h;
        }
        self.y += 12;
    }
    fn check(&mut self, id: i32, text: &str) {
        let (x, w) = (self.ix(), self.iw());
        self.push(id, Ctl::Check, text, x, self.y, w, 20);
        self.y += ROW - 2;
    }
    fn labeled(&mut self, label: &str, id: i32, ctl: Ctl, w: i32) {
        let (x, iw) = (self.ix(), self.iw());
        self.push(0, Ctl::Label, label, x, self.y + 3, iw - w - 8, 18);
        let h = if ctl == Ctl::Combo { 200 } else { 22 };
        self.push(id, ctl, "", x + iw - w, self.y, w, h);
        self.y += ROW + 2;
    }
    fn hint(&mut self, text: &str) {
        let id = self.next_hint;
        self.next_hint += 1;
        self.hint_with_id(id, text, 1);
    }
    fn hint_with_id(&mut self, id: i32, text: &str, lines: i32) {
        let (x, w) = (self.ix() + 2, self.iw());
        self.push(id, Ctl::Hint, text, x, self.y - 3, w, 16 * lines);
        self.y += 16 * lines + 2;
    }
    fn label(&mut self, text: &str) {
        let (x, w) = (self.ix(), self.iw());
        self.push(0, Ctl::Label, text, x, self.y, w, 18);
        self.y += 20;
    }
    fn multi(&mut self, id: i32, h: i32) {
        let (x, w) = (self.ix(), self.iw());
        self.push(id, Ctl::MultiEdit, "", x, self.y, w, h);
        self.y += h + 6;
    }
}

/// The whole window as data: (controls, client height). Ids of hint labels are
/// assigned from `ID_HINT_FIRST`; the monitors and status hints are the first two.
fn layout(ctx: &Context) -> (Vec<Spec>, i32) {
    let mut b = Builder {
        specs: Vec::new(),
        x0: PAD,
        y: PAD,
        group_top: 0,
        next_hint: ID_HINT_FIRST + 2,
        indent: 0,
    };

    // ---- left column ----
    b.column(0);
    b.group("Behavior");
    b.labeled(
        "Maximizing a window that already fills its zone:",
        ID_ALREADY,
        Ctl::Combo,
        230,
    );
    b.check(
        ID_DRAG,
        "Dragging a window to the top edge snaps it into the zone under the cursor",
    );
    b.check(
        ID_LOCK,
        "Lock zone-maximized windows (no resizing; dragging the title bar restores them)",
    );
    b.label("Full-screen maximize (instead of the zone):");
    b.nested(true);
    b.labeled("Hold this key:", ID_MODIFIER, Ctl::Combo, 230);
    b.hint("While clicking Maximize or double-clicking the title bar. It also unlocks a locked window.");
    // The text follows the chosen key ("Win + Shift + Up"); see update_dependent_controls.
    b.check(ID_WINUP_BYPASS, "Win + Shift + Up");
    b.hint("Replaces Windows' own use of that shortcut. Needs Win+Up flash prevention.");
    b.nested(false);
    b.end_group();

    b.group("Appearance");
    b.label("Prevent the maximize flash for:");
    b.nested(true);
    b.check(ID_PRE_MOUSE, "Maximize button and title-bar double-click");
    b.check(ID_PRE_WINUP, "Win+Up");
    b.hint("Caught before Windows maximizes, these go straight to the zone. Others are corrected after.");
    b.nested(false);
    b.label("Animation:");
    b.nested(true);
    b.check(
        ID_ANIMATE,
        "Animate the window into its zone (like Windows' own maximize)",
    );
    b.labeled("Style:", ID_ANIM_STYLE, Ctl::Combo, 260);
    b.check(ID_ANIM_MATCH, "Match Windows' animation speed");
    b.labeled("Otherwise, length (milliseconds, 50-1000):", ID_ANIM_MS, Ctl::Edit, 70);
    b.hint("Animates the gestures above. Off while Windows' animation effects are off.");
    b.nested(false);
    b.end_group();
    let left_bottom = b.y;

    // ---- right column ----
    b.column(1);
    b.group("General");
    b.check(ID_AUTOSTART, "Start with Windows");
    b.check(ID_UPDATES, "Check for updates once a day");
    b.labeled("Log detail:", ID_LOGLEVEL, Ctl::Combo, 120);
    b.end_group();

    b.group("Exclusions");
    b.labeled(
        "Only monitors at least this wide (width / height, 0 = all):",
        ID_ASPECT,
        Ctl::Edit,
        70,
    );
    b.label("Leave these monitors alone (one per line):");
    b.multi(ID_MONITORS, 44);
    b.hint_with_id(ID_HINT_FIRST, &ctx.monitors_hint, 2);
    b.label("Leave these apps alone (exe name or window-title text, one per line):");
    b.multi(ID_APPS, 44);
    b.check(ID_FZ_EXCLUDED, "Also leave FancyZones' excluded apps alone");
    b.end_group();

    b.group("FancyZones integration");
    b.check(ID_FZ_REQUIRE, "Do nothing while FancyZones is turned off in PowerToys");
    b.check(
        ID_FZ_DEFAULT,
        "Use FancyZones' default layout on monitors with no saved layout",
    );
    b.check(
        ID_CHECK,
        "Compare with FancyZones' own snaps (spots changes after updates)",
    );
    b.labeled("When they disagree:", ID_MISMATCH, Ctl::Combo, 300);
    b.hint_with_id(ID_HINT_FIRST + 1, &ctx.check_status, 1);
    b.end_group();
    let right_bottom = b.y;

    // ---- buttons ----
    let y = left_bottom.max(right_bottom);
    b.push(ID_OPEN_FILE, Ctl::Button, "Open config file", PAD, y, 130, 28);
    b.push(ID_OK, Ctl::DefaultButton, "OK", WIDTH - PAD - 3 * 90 - 2 * 8, y, 90, 28);
    b.push(ID_CANCEL, Ctl::Button, "Cancel", WIDTH - PAD - 2 * 90 - 8, y, 90, 28);
    b.push(ID_APPLY, Ctl::Button, "Apply", WIDTH - PAD - 90, y, 90, 28);
    (b.specs, y + 28 + PAD)
}

// ---------------------------------------------------------------------------------------
// Pure conversions (unit tested)
// ---------------------------------------------------------------------------------------

pub fn lines_to_list(text: &str) -> Vec<String> {
    text.lines()
        .flat_map(|l| l.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn list_to_lines(items: &[String]) -> String {
    items.join("\r\n")
}

fn index_of<T: PartialEq + Copy>(table: &[(&str, T)], value: T) -> usize {
    table.iter().position(|(_, v)| *v == value).unwrap_or(0)
}

// ---------------------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------------------

struct State {
    specs: Vec<Spec>,
    font: HFONT,
    ctx: Context,
    icons: Vec<HICON>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

pub(crate) fn scale(v: i32, dpi: u32) -> i32 {
    (v as i64 * dpi as i64 / 96) as i32
}

pub(crate) fn make_font(dpi: u32) -> HFONT {
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
        CreateFontIndirectW(&ncm.lfMessageFont)
    }
}

fn item(hwnd: HWND, id: i32) -> HWND {
    unsafe { GetDlgItem(Some(hwnd), id) }.unwrap_or_default()
}

fn send(h: HWND, msg: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(h, msg, Some(WPARAM(w)), Some(LPARAM(l))) }.0
}

/// The speed options only matter when animating; the length box only when not matching
/// Windows.
/// "Win + Shift + Up" for the chosen full-screen key.
pub fn win_up_label(key: BypassModifier) -> String {
    match key {
        BypassModifier::Shift => "Win + Shift + Up".into(),
        BypassModifier::Ctrl => "Win + Ctrl + Up".into(),
        BypassModifier::Alt => "Win + Alt + Up".into(),
        BypassModifier::None => "Win + (the key above) + Up".into(),
    }
}

fn update_dependent_controls(hwnd: HWND) {
    // Win + bypass + Up is handled by the Win+Up interception, with the bypass key.
    let key = MODIFIERS[combo_index(hwnd, ID_MODIFIER).min(MODIFIERS.len() - 1)].1;
    let bypass_key = key != BypassModifier::None;
    set_text(hwnd, ID_WINUP_BYPASS, &win_up_label(key));
    let win_up_bypass = checked(hwnd, ID_PRE_WINUP) && bypass_key;
    // Only the gestures caught early (no flash) are animated.
    let can_animate = checked(hwnd, ID_PRE_MOUSE) || checked(hwnd, ID_PRE_WINUP);
    let animate = can_animate && checked(hwnd, ID_ANIMATE);
    let custom = animate && !checked(hwnd, ID_ANIM_MATCH);
    unsafe {
        let _ = EnableWindow(item(hwnd, ID_WINUP_BYPASS), win_up_bypass);
        let _ = EnableWindow(item(hwnd, ID_ANIMATE), can_animate);
        let _ = EnableWindow(item(hwnd, ID_ANIM_STYLE), animate);
        let _ = EnableWindow(item(hwnd, ID_ANIM_MATCH), animate);
        let _ = EnableWindow(item(hwnd, ID_ANIM_MS), custom);
    }
}

fn set_check(hwnd: HWND, id: i32, on: bool) {
    send(item(hwnd, id), BM_SETCHECK, usize::from(on), 0);
}

fn checked(hwnd: HWND, id: i32) -> bool {
    send(item(hwnd, id), BM_GETCHECK, 0, 0) == BST_CHECKED.0 as isize
}

fn set_text(hwnd: HWND, id: i32, text: &str) {
    unsafe {
        let _ = SetWindowTextW(item(hwnd, id), &HSTRING::from(text));
    }
}

fn text(hwnd: HWND, id: i32) -> String {
    let h = item(hwnd, id);
    let len = unsafe { GetWindowTextLengthW(h) }.max(0) as usize;
    let mut buf = vec![0u16; len + 1];
    let n = unsafe { GetWindowTextW(h, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

fn fill_combo(hwnd: HWND, id: i32, items: &[&str], selected: usize) {
    let h = item(hwnd, id);
    for s in items {
        let w = HSTRING::from(*s);
        send(h, CB_ADDSTRING, 0, w.as_ptr() as isize);
    }
    send(h, CB_SETCURSEL, selected, 0);
}

fn combo_index(hwnd: HWND, id: i32) -> usize {
    send(item(hwnd, id), CB_GETCURSEL, 0, 0).max(0) as usize
}

fn create_controls(hwnd: HWND, specs: &[Spec]) {
    let hinstance = unsafe { GetModuleHandleW(None) }.ok().map(|m| m.into());
    for s in specs {
        let (class, style, ex): (PCWSTR, u32, u32) = match s.ctl {
            Ctl::Group => (w!("BUTTON"), BS_GROUPBOX as u32, 0),
            Ctl::Label | Ctl::Hint => (w!("STATIC"), 0, 0),
            Ctl::Check => (w!("BUTTON"), BS_AUTOCHECKBOX as u32 | WS_TABSTOP.0, 0),
            Ctl::Combo => (w!("COMBOBOX"), CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0 | WS_TABSTOP.0, 0),
            Ctl::Edit => (w!("EDIT"), ES_AUTOHSCROLL as u32 | WS_TABSTOP.0, WS_EX_CLIENTEDGE.0),
            Ctl::MultiEdit => (
                w!("EDIT"),
                (ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32 | WS_VSCROLL.0 | WS_TABSTOP.0,
                WS_EX_CLIENTEDGE.0,
            ),
            Ctl::Button => (w!("BUTTON"), BS_PUSHBUTTON as u32 | WS_TABSTOP.0, 0),
            Ctl::DefaultButton => (w!("BUTTON"), BS_DEFPUSHBUTTON as u32 | WS_TABSTOP.0, 0),
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

/// Position every control for `dpi` and give it `font`.
fn relayout(hwnd: HWND, specs: &[Spec], dpi: u32, font: HFONT) {
    // Controls with id 0 (labels, groups) can't be found by id; walk in creation order.
    let mut child = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetWindow(hwnd, windows::Win32::UI::WindowsAndMessaging::GW_CHILD)
    }
    .ok();
    for s in specs {
        let Some(h) = child else { break };
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
        child = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetWindow(h, windows::Win32::UI::WindowsAndMessaging::GW_HWNDNEXT)
        }
        .ok();
    }
}

fn fill(hwnd: HWND, ctx: &Context) {
    let c = &ctx.config;
    fill_combo(
        hwnd,
        ID_ANIM_STYLE,
        &STYLES.map(|s| s.0),
        index_of(&STYLES, c.animation_style),
    );
    fill_combo(
        hwnd,
        ID_MODIFIER,
        &MODIFIERS.map(|m| m.0),
        index_of(&MODIFIERS, c.bypass_modifier),
    );
    fill_combo(
        hwnd,
        ID_ALREADY,
        &ALREADY.map(|m| m.0),
        index_of(&ALREADY, c.already_in_zone),
    );
    fill_combo(
        hwnd,
        ID_MISMATCH,
        &MISMATCH.map(|m| m.0),
        index_of(&MISMATCH, c.on_fancyzones_mismatch),
    );
    let level = LOG_LEVELS
        .iter()
        .position(|l| l.eq_ignore_ascii_case(&c.log_level))
        .unwrap_or(2);
    fill_combo(hwnd, ID_LOGLEVEL, &LOG_LEVELS, level);
    set_check(hwnd, ID_DRAG, c.snap_on_drag_maximize);
    set_check(hwnd, ID_LOCK, c.lock_zone_maximized);
    set_check(hwnd, ID_PRE_MOUSE, c.preempt_mouse_maximize);
    set_check(hwnd, ID_PRE_WINUP, c.preempt_win_up);
    set_check(hwnd, ID_WINUP_BYPASS, c.win_up_bypass);
    set_check(hwnd, ID_ANIMATE, c.animate_maximize);
    set_check(hwnd, ID_ANIM_MATCH, c.animation_match_windows);
    set_text(hwnd, ID_ANIM_MS, &c.animation_ms.to_string());
    update_dependent_controls(hwnd);
    set_check(hwnd, ID_FZ_EXCLUDED, c.respect_fancyzones_excluded_apps);
    set_check(hwnd, ID_FZ_DEFAULT, c.use_fancyzones_default_layout);
    set_check(hwnd, ID_FZ_REQUIRE, c.require_fancyzones_enabled);
    set_check(hwnd, ID_CHECK, c.fancyzones_check);
    set_check(hwnd, ID_AUTOSTART, ctx.autostart);
    set_check(hwnd, ID_UPDATES, c.check_for_updates);
    set_text(hwnd, ID_ASPECT, &format!("{}", c.min_aspect_ratio));
    set_text(hwnd, ID_MONITORS, &list_to_lines(&c.excluded_monitors));
    set_text(hwnd, ID_APPS, &list_to_lines(&c.excluded_apps));
}

/// Read the controls back into a `Config` (plus the autostart checkbox).
fn read(hwnd: HWND) -> Result<(Config, bool), String> {
    let aspect_text = text(hwnd, ID_ASPECT);
    let min_aspect_ratio: f64 = if aspect_text.trim().is_empty() {
        0.0
    } else {
        aspect_text
            .trim()
            .parse()
            .map_err(|_| format!("'{aspect_text}' is not a number (monitor width / height)."))?
    };
    let match_windows = checked(hwnd, ID_ANIM_MATCH);
    let ms_text = text(hwnd, ID_ANIM_MS);
    let animation_ms: u32 = match ms_text.trim().parse() {
        Ok(ms) => ms,
        // The box is greyed out while matching Windows: don't complain about it then.
        Err(_) if match_windows => Config::default().animation_ms,
        Err(_) => return Err(format!("'{ms_text}' is not a whole number of milliseconds (50-1000).")),
    };
    let cfg = Config {
        bypass_modifier: MODIFIERS[combo_index(hwnd, ID_MODIFIER).min(MODIFIERS.len() - 1)].1,
        already_in_zone: ALREADY[combo_index(hwnd, ID_ALREADY).min(ALREADY.len() - 1)].1,
        snap_on_drag_maximize: checked(hwnd, ID_DRAG),
        lock_zone_maximized: checked(hwnd, ID_LOCK),
        preempt_mouse_maximize: checked(hwnd, ID_PRE_MOUSE),
        preempt_win_up: checked(hwnd, ID_PRE_WINUP),
        win_up_bypass: checked(hwnd, ID_WINUP_BYPASS),
        animate_maximize: checked(hwnd, ID_ANIMATE),
        legacy_hotkey: None,
        animation_style: STYLES[combo_index(hwnd, ID_ANIM_STYLE).min(STYLES.len() - 1)].1,
        animation_match_windows: match_windows,
        animation_ms,
        min_aspect_ratio,
        excluded_monitors: lines_to_list(&text(hwnd, ID_MONITORS)),
        excluded_apps: lines_to_list(&text(hwnd, ID_APPS)),
        respect_fancyzones_excluded_apps: checked(hwnd, ID_FZ_EXCLUDED),
        use_fancyzones_default_layout: checked(hwnd, ID_FZ_DEFAULT),
        require_fancyzones_enabled: checked(hwnd, ID_FZ_REQUIRE),
        fancyzones_check: checked(hwnd, ID_CHECK),
        on_fancyzones_mismatch: MISMATCH[combo_index(hwnd, ID_MISMATCH).min(MISMATCH.len() - 1)].1,
        log_level: LOG_LEVELS[combo_index(hwnd, ID_LOGLEVEL).min(LOG_LEVELS.len() - 1)].to_string(),
        check_for_updates: checked(hwnd, ID_UPDATES),
    };
    cfg.validate()?;
    Ok((cfg, checked(hwnd, ID_AUTOSTART)))
}

/// Validate + save + apply. Shows an error box and returns false on failure.
fn apply(hwnd: HWND) -> bool {
    let result = read(hwnd).and_then(|(cfg, autostart)| match APPLY.get() {
        Some(f) => f(cfg, autostart),
        None => Err("settings handler not registered".into()),
    });
    match result {
        Ok(()) => true,
        Err(e) => {
            unsafe {
                MessageBoxW(
                    Some(hwnd),
                    &HSTRING::from(e),
                    w!("fz-maximize settings"),
                    MB_OK | MB_ICONWARNING,
                );
            }
            false
        }
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
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            false,
            WS_EX_CONTROLPARENT,
            dpi,
        );
    }
    (r.right - r.left, r.bottom - r.top)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
            tracing::trace!(id, code, "settings WM_COMMAND");
            if code == CBN_SELCHANGE && id == ID_MODIFIER {
                update_dependent_controls(hwnd);
            }
            if code == BN_CLICKED {
                match id {
                    ID_OK => {
                        if apply(hwnd) {
                            unsafe {
                                let _ = DestroyWindow(hwnd);
                            }
                        }
                    }
                    ID_CANCEL => unsafe {
                        let _ = DestroyWindow(hwnd);
                    },
                    ID_APPLY => {
                        apply(hwnd);
                    }
                    ID_ANIMATE | ID_ANIM_MATCH | ID_PRE_MOUSE | ID_PRE_WINUP => update_dependent_controls(hwnd),
                    ID_OPEN_FILE => {
                        let path = STATE.with(|s| s.borrow().as_ref().map(|s| s.ctx.config_path.clone()));
                        if let Some(p) = path {
                            let _ = config::ensure_exists(&p);
                            let _ = std::process::Command::new("notepad.exe").arg(&p).spawn();
                        }
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            // Hint labels in grey; everything else on the dialog background.
            let child = HWND(lparam.0 as *mut core::ffi::c_void);
            let id = unsafe { windows::Win32::UI::WindowsAndMessaging::GetDlgCtrlID(child) };
            let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
            unsafe {
                if id >= ID_HINT_FIRST {
                    SetTextColor(hdc, windows::Win32::Foundation::COLORREF(GetSysColor(COLOR_GRAYTEXT)));
                }
                SetBkMode(hdc, TRANSPARENT);
                LRESULT(GetSysColorBrush(COLOR_BTNFACE).0 as isize)
            }
        }
        WM_DPICHANGED => {
            let dpi = ((wparam.0 >> 16) & 0xFFFF) as u32;
            let suggested = unsafe { &*(lparam.0 as *const RECT) };
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
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
            tracing::debug!("settings window destroyed");
            HWND_OPEN.store(0, Ordering::Relaxed);
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().take() {
                    unsafe {
                        let _ = DeleteObject(HGDIOBJ(st.font.0));
                        for i in st.icons {
                            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(i);
                        }
                    }
                }
            });
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn register_class() -> bool {
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    *REGISTERED.get_or_init(|| unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: SETTINGS_CLASS,
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

/// Open the settings window (or bring the open one to the front).
pub fn open(ctx: Context) {
    if let Some(h) = open_hwnd() {
        unsafe {
            let _ = ShowWindow(h, SW_SHOW);
            let _ = SetForegroundWindow(h);
        }
        return;
    }
    if !register_class() {
        tracing::error!("could not register the settings window class");
        return;
    }

    // Place it on the monitor under the cursor, centered.
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let mon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = GetMonitorInfoW(mon, &mut mi);
    }
    let mut dpi_x = 96u32;
    let mut dpi_y = 96u32;
    unsafe {
        let _ = windows::Win32::UI::HiDpi::GetDpiForMonitor(
            mon,
            windows::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI,
            &mut dpi_x,
            &mut dpi_y,
        );
    }
    let (specs, height) = layout(&ctx);
    let (w, h) = window_size(dpi_x, height);
    let work = mi.rcWork;
    let x = work.left + ((work.right - work.left) - w).max(0) / 2;
    let y = work.top + ((work.bottom - work.top) - h).max(0) / 2;

    let hinstance = unsafe { GetModuleHandleW(None) }.ok().map(|m| m.into());
    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_CONTROLPARENT,
            SETTINGS_CLASS,
            w!("fz-maximize settings"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
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
            tracing::error!(error = %e, "could not create the settings window");
            return;
        }
    };
    HWND_OPEN.store(hwnd.0 as isize, Ordering::Relaxed);
    tracing::debug!(hwnd = hwnd.0 as isize, w, h, "settings window created");

    // The window may have landed on a different DPI than we sized for.
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
    fill(hwnd, &ctx);

    let mut icons = Vec::new();
    for (which, size) in [(ICON_SMALL, 16), (ICON_BIG, 32)] {
        if let Some(icon) = crate::tray::create_icon(scale(size, dpi), false) {
            send(hwnd, WM_SETICON, which as usize, icon.0 as isize);
            icons.push(icon);
        }
    }
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            specs,
            font,
            ctx,
            icons,
        })
    });
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(item(hwnd, ID_MODIFIER)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context {
            config: Config::default(),
            config_path: PathBuf::from("config.toml"),
            autostart: false,
            monitors_hint: "Connected: BOE0A52".into(),
            check_status: "ok".into(),
        }
    }

    #[test]
    fn lists() {
        assert_eq!(lines_to_list(" a \r\n\r\nb, c\n"), vec!["a", "b", "c"]);
        assert_eq!(list_to_lines(&["x".into(), "y".into()]), "x\r\ny");
        assert!(lines_to_list("").is_empty());
    }

    #[test]
    fn combo_tables_cover_every_value() {
        for m in [
            BypassModifier::Shift,
            BypassModifier::Ctrl,
            BypassModifier::Alt,
            BypassModifier::None,
        ] {
            assert_eq!(MODIFIERS[index_of(&MODIFIERS, m)].1, m);
        }
        for a in [AlreadyInZone::Restore, AlreadyInZone::Maximize] {
            assert_eq!(ALREADY[index_of(&ALREADY, a)].1, a);
        }
        for s in [AnimationStyle::Auto, AnimationStyle::Zoom, AnimationStyle::Morph] {
            assert_eq!(STYLES[index_of(&STYLES, s)].1, s);
        }
        for a in [MismatchAction::Warn, MismatchAction::Native] {
            assert_eq!(MISMATCH[index_of(&MISMATCH, a)].1, a);
        }
        for l in ["error", "warn", "info", "debug", "trace"] {
            assert!(LOG_LEVELS.contains(&l));
        }
    }

    #[test]
    fn layout_is_sane() {
        let (specs, height) = layout(&ctx());
        // Must fit a 1080p laptop at 150% scaling: about 690 DIPs of work area.
        assert!(height > 300 && height < 640, "height {height}");
        // Every interactive control has a unique id; everything fits horizontally.
        let mut ids: Vec<i32> = specs.iter().filter(|s| s.id != 0).map(|s| s.id).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate control ids");
        for s in &specs {
            assert!(s.x >= 0 && s.x + s.w <= WIDTH, "{:?} overflows", s.text);
            assert!(s.y + s.h.min(40) <= height, "{:?} below the window", s.text);
        }
        for id in [ID_OK, ID_CANCEL, ID_APPLY, ID_MODIFIER, ID_AUTOSTART, ID_MONITORS] {
            assert!(specs.iter().any(|s| s.id == id), "missing control {id}");
        }
    }
}
