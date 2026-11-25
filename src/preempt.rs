//! Pre-emptive maximize interception: catch the most common maximize gestures *before*
//! Windows maximizes the window, so there is no maximize animation to flash.
//!
//! * Maximize-button click: the press on `HTMAXBUTTON` and its release are swallowed. If
//!   the release is still over the button, the main thread places the window (a press
//!   that slides off the button cancels, as normal).
//! * Title-bar double-click: low-level hooks never see `WM_LBUTTONDBLCLK` (Windows
//!   synthesizes it later, per window), so double-clicks are recognized here from two
//!   presses on the same caption within the system double-click time and rectangle. The
//!   second press and its release are swallowed, so Windows never sees a double-click.
//! * Win+Up: the Up key is swallowed while a Win key is held, and a no-op "mask" key is
//!   injected so releasing Win does not open the Start menu.
//!
//! Everything else (drag to the top edge, apps whose title bars don't answer
//! `WM_NCHITTEST`, programmatic maximizes) still goes through the WinEvent path, which
//! corrects the window after the fact.
//!
//! The hooks run on a dedicated thread that does nothing else: Windows holds every
//! mouse/keyboard event system-wide until a low-level hook returns, and silently
//! removes hooks that are slow. The only blocking call there is a `WM_NCHITTEST`
//! (on button presses only) with a 40 ms timeout, and it aborts if the target hangs.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::PhysicalToLogicalPointForPerMonitorDPI;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetDoubleClickTime, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
    VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GA_ROOT, GetAncestor, GetForegroundWindow, GetMessageW, GetSystemMetrics,
    HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, PostMessageW, PostThreadMessageW, SM_CXDOUBLECLK,
    SM_CYDOUBLECLK, SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW, SetWindowsHookExW, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_NCHITTEST, WM_QUIT,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WindowFromPoint,
};

use crate::config::BypassModifier;
use crate::engine::Hwnd;
use crate::geometry::Point;
use crate::win::{hwnd_from, hwnd_key};
use crate::window;

/// Posted to the main window: `wParam` = target HWND, `lParam` = [`Kind`].
pub const WM_APP_PREEMPT: u32 = WM_APP + 2;
/// Posted to the hook thread itself: inject the Start-menu mask key.
const WM_APP_MASK: u32 = WM_APP + 3;
/// Unassigned virtual key used to "mask" a Win press (same trick AutoHotkey uses).
const VK_MASK: u16 = 0xE8;
const HT_MAXBUTTON: u32 = 9;
const HT_CAPTION: u32 = 2;
const HIT_TEST_TIMEOUT_MS: u32 = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(isize)]
pub enum Kind {
    MaxButton = 1,
    CaptionDoubleClick = 2,
    WinUp = 3,
    /// Win + bypass modifier (Shift by default) + Up: normal full-monitor maximize.
    WinUpFull = 4,
    /// Maximize button / title-bar double-click with the bypass modifier held: not
    /// swallowed (Windows maximizes natively), but the main thread is told right away so
    /// the result isn't redirected even if the modifier is released before Windows acts.
    Bypassed = 5,
}

impl Kind {
    pub fn from_raw(v: isize) -> Option<Self> {
        Some(match v {
            1 => Kind::MaxButton,
            2 => Kind::CaptionDoubleClick,
            3 => Kind::WinUp,
            4 => Kind::WinUpFull,
            5 => Kind::Bypassed,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::MaxButton => "maximize button",
            Kind::CaptionDoubleClick => "title-bar double-click",
            Kind::WinUp => "Win+Up",
            Kind::WinUpFull => "Win+bypass+Up",
            Kind::Bypassed => "bypass-modifier click",
        }
    }
}

// ---------------------------------------------------------------------------------------
// Pure gesture recognition
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DoubleClick {
    pub time_ms: u32,
    pub cx: i32,
    pub cy: i32,
}

/// A left-button press, already hit-tested by the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Press {
    pub root: Hwnd,
    pub hit: u32,
    pub pt: Point,
    pub time: u32,
    /// Interception allowed for this press: window is a normal (not maximized) candidate,
    /// interception is enabled, not paused.
    pub eligible: bool,
    /// The bypass modifier is held: recognize the gesture but let Windows do it natively.
    pub bypass: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Swallow,
    /// Swallow and ask the main thread to place this window.
    Fire(Hwnd, Kind),
    /// Let the event through, but tell the main thread (bypass gesture).
    Notice(Hwnd),
    /// Swallow this Win-key release and replay it after a mask key, so the shell sees
    /// "Win + some key" instead of a lone Win press (which opens the Start menu).
    ReplayWinRelease(u16),
}

impl Verdict {
    pub fn swallows(self) -> bool {
        !matches!(self, Verdict::Pass | Verdict::Notice(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    MaxButton(Hwnd),
    CaptionDouble(Hwnd),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LastCaptionClick {
    root: Hwnd,
    pt: Point,
    time: u32,
}

#[derive(Debug, Default)]
pub struct Recognizer {
    pending: Option<Pending>,
    last_caption: Option<LastCaptionClick>,
    lwin: bool,
    rwin: bool,
    swallowing_up_key: bool,
    /// We swallowed Up during this Win press: the Win release must be masked.
    replay_win_release: bool,
}

impl Recognizer {
    /// Left button pressed.
    pub fn press(&mut self, p: Press, dbl: DoubleClick) -> Verdict {
        // A new press always resolves any half-finished gesture.
        self.pending = None;
        if !p.eligible {
            self.last_caption = None;
            return Verdict::Pass;
        }
        match p.hit {
            HT_MAXBUTTON => {
                self.last_caption = None;
                if p.bypass {
                    return Verdict::Notice(p.root);
                }
                self.pending = Some(Pending::MaxButton(p.root));
                Verdict::Swallow
            }
            HT_CAPTION => {
                let is_double = self.last_caption.is_some_and(|prev| {
                    prev.root == p.root
                        && p.time.wrapping_sub(prev.time) <= dbl.time_ms
                        && (p.pt.x - prev.pt.x).abs() <= dbl.cx / 2
                        && (p.pt.y - prev.pt.y).abs() <= dbl.cy / 2
                });
                if is_double {
                    self.last_caption = None;
                    if p.bypass {
                        return Verdict::Notice(p.root);
                    }
                    self.pending = Some(Pending::CaptionDouble(p.root));
                    Verdict::Swallow
                } else {
                    self.last_caption = Some(LastCaptionClick {
                        root: p.root,
                        pt: p.pt,
                        time: p.time,
                    });
                    Verdict::Pass
                }
            }
            _ => {
                self.last_caption = None;
                Verdict::Pass
            }
        }
    }

    /// The window and hit-test result needed to finish a maximize-button gesture.
    #[cfg(test)]
    pub fn release_needs_hit_test(&self) -> bool {
        matches!(self.pending, Some(Pending::MaxButton(_)))
    }

    /// Left button released. `still_on_button` is the hit-test at release (only asked
    /// for when [`release_needs_hit_test`](Self::release_needs_hit_test) is true).
    pub fn release(&mut self, still_on_button: impl FnOnce(Hwnd) -> bool) -> Verdict {
        match self.pending.take() {
            Some(Pending::MaxButton(root)) => {
                if still_on_button(root) {
                    Verdict::Fire(root, Kind::MaxButton)
                } else {
                    Verdict::Swallow // slid off the button: cancelled, like a normal button
                }
            }
            Some(Pending::CaptionDouble(root)) => Verdict::Fire(root, Kind::CaptionDoubleClick),
            None => Verdict::Pass,
        }
    }

    /// A key event. `intercept` returns the foreground window to place when Win+Up may
    /// be intercepted now (enabled, eligible window, no Shift/Ctrl/Alt), else `None`.
    pub fn key(&mut self, vk: u16, down: bool, intercept: impl FnOnce() -> Option<(Hwnd, Kind)>) -> Verdict {
        match VIRTUAL_KEY(vk) {
            VK_LWIN | VK_RWIN => {
                if VIRTUAL_KEY(vk) == VK_LWIN {
                    self.lwin = down;
                } else {
                    self.rwin = down;
                }
                if !down && self.replay_win_release {
                    // The replayed (injected) release comes back through the hook with
                    // the flag cleared, so it passes.
                    self.replay_win_release = false;
                    return Verdict::ReplayWinRelease(vk);
                }
                Verdict::Pass
            }
            VK_UP => {
                if !down {
                    if self.swallowing_up_key {
                        self.swallowing_up_key = false;
                        return Verdict::Swallow;
                    }
                    return Verdict::Pass;
                }
                if self.swallowing_up_key {
                    return Verdict::Swallow; // auto-repeat while held
                }
                if !(self.lwin || self.rwin) {
                    return Verdict::Pass;
                }
                match intercept() {
                    Some((h, kind)) => {
                        self.swallowing_up_key = true;
                        self.replay_win_release = true;
                        Verdict::Fire(h, kind)
                    }
                    None => Verdict::Pass,
                }
            }
            _ => Verdict::Pass,
        }
    }
}

/// What Win+Up should do with the given modifiers held. `None` leaves the keystroke to
/// Windows (e.g. Win+Shift+Up "stretch vertically" when the bypass is disabled, or any
/// other combination).
pub fn win_up_action(shift: bool, ctrl: bool, alt: bool, bypass: BypassModifier, bypass_enabled: bool) -> Option<Kind> {
    let bypass_held = match bypass {
        BypassModifier::Shift => shift,
        BypassModifier::Ctrl => ctrl,
        BypassModifier::Alt => alt,
        BypassModifier::None => false,
    };
    let other_held = (shift && bypass != BypassModifier::Shift)
        || (ctrl && bypass != BypassModifier::Ctrl)
        || (alt && bypass != BypassModifier::Alt);
    if other_held {
        return None;
    }
    if bypass_held {
        return bypass_enabled.then_some(Kind::WinUpFull);
    }
    Some(Kind::WinUp)
}

// ---------------------------------------------------------------------------------------
// Shared switches (written by the main thread, read by the hook thread)
// ---------------------------------------------------------------------------------------

static TARGET: AtomicIsize = AtomicIsize::new(0);
static MOUSE_ON: AtomicBool = AtomicBool::new(false);
static WIN_UP_ON: AtomicBool = AtomicBool::new(false);
static WIN_UP_BYPASS_ON: AtomicBool = AtomicBool::new(false);
static PAUSED: AtomicBool = AtomicBool::new(false);
static BYPASS: AtomicU8 = AtomicU8::new(0);

pub fn configure(mouse: bool, win_up: bool, win_up_bypass: bool, bypass: BypassModifier) {
    MOUSE_ON.store(mouse, Ordering::Relaxed);
    WIN_UP_ON.store(win_up, Ordering::Relaxed);
    WIN_UP_BYPASS_ON.store(win_up_bypass, Ordering::Relaxed);
    BYPASS.store(
        match bypass {
            BypassModifier::None => 0,
            BypassModifier::Shift => 1,
            BypassModifier::Ctrl => 2,
            BypassModifier::Alt => 3,
        },
        Ordering::Relaxed,
    );
}

pub fn set_paused(paused: bool) {
    PAUSED.store(paused, Ordering::Relaxed);
}

fn key_held(vk: VIRTUAL_KEY) -> bool {
    (unsafe { GetAsyncKeyState(i32::from(vk.0)) } as u16 & 0x8000) != 0
}

fn bypass_modifier() -> BypassModifier {
    match BYPASS.load(Ordering::Relaxed) {
        1 => BypassModifier::Shift,
        2 => BypassModifier::Ctrl,
        3 => BypassModifier::Alt,
        _ => BypassModifier::None,
    }
}

fn bypass_held() -> bool {
    match bypass_modifier() {
        BypassModifier::Shift => key_held(VK_SHIFT),
        BypassModifier::Ctrl => key_held(VK_CONTROL),
        BypassModifier::Alt => key_held(VK_MENU),
        BypassModifier::None => false,
    }
}

// ---------------------------------------------------------------------------------------
// Hook thread
// ---------------------------------------------------------------------------------------

thread_local! {
    static RECOGNIZER: RefCell<Recognizer> = RefCell::new(Recognizer::default());
}

fn root_at(pt: POINT) -> Option<HWND> {
    let h = unsafe { WindowFromPoint(pt) };
    if h.is_invalid() {
        return None;
    }
    let root = unsafe { GetAncestor(h, GA_ROOT) };
    (!root.is_invalid()).then_some(root)
}

/// `WM_NCHITTEST` with a short timeout; 0 (HTNOWHERE) on failure or a hung target.
fn hit_test(root: HWND, pt: POINT) -> u32 {
    // Our process is per-monitor-DPI aware; convert for DPI-unaware/system-aware targets.
    let mut p = pt;
    unsafe {
        let _ = PhysicalToLogicalPointForPerMonitorDPI(Some(root), &mut p);
    }
    let lparam = ((p.y as u16 as u32) << 16) | (p.x as u16 as u32);
    let mut result: usize = 0;
    let ok = unsafe {
        SendMessageTimeoutW(
            root,
            WM_NCHITTEST,
            WPARAM(0),
            LPARAM(lparam as i32 as isize),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            HIT_TEST_TIMEOUT_MS,
            Some(&mut result),
        )
    };
    if ok.0 == 0 { 0 } else { result as u32 }
}

fn fire(hwnd: Hwnd, kind: Kind) {
    let target = TARGET.load(Ordering::Relaxed);
    if target != 0 {
        unsafe {
            let _ = PostMessageW(
                Some(hwnd_from(target)),
                WM_APP_PREEMPT,
                WPARAM(hwnd as usize),
                LPARAM(kind as isize),
            );
        }
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && MOUSE_ON.load(Ordering::Relaxed) {
        let msg = wparam.0 as u32;
        if msg == WM_LBUTTONDOWN || msg == WM_LBUTTONUP {
            // SAFETY: for WH_MOUSE_LL, lParam points to an MSLLHOOKSTRUCT.
            let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            let verdict = RECOGNIZER.with(|r| {
                let Ok(mut r) = r.try_borrow_mut() else {
                    return Verdict::Pass;
                };
                if msg == WM_LBUTTONDOWN {
                    let press = match root_at(info.pt) {
                        Some(root) => {
                            let eligible = !PAUSED.load(Ordering::Relaxed)
                                && window::is_trackable(root)
                                && !window::is_zoomed(root);
                            Press {
                                root: hwnd_key(root),
                                hit: if eligible { hit_test(root, info.pt) } else { 0 },
                                bypass: bypass_held(),
                                pt: Point::new(info.pt.x, info.pt.y),
                                time: info.time,
                                eligible,
                            }
                        }
                        None => Press {
                            root: 0,
                            hit: 0,
                            pt: Point::new(info.pt.x, info.pt.y),
                            time: info.time,
                            eligible: false,
                            bypass: false,
                        },
                    };
                    let dbl = DoubleClick {
                        time_ms: unsafe { GetDoubleClickTime() },
                        cx: unsafe { GetSystemMetrics(SM_CXDOUBLECLK) },
                        cy: unsafe { GetSystemMetrics(SM_CYDOUBLECLK) },
                    };
                    r.press(press, dbl)
                } else {
                    r.release(|root| {
                        root_at(info.pt).is_some_and(|h| hwnd_key(h) == root)
                            && hit_test(hwnd_from(root), info.pt) == HT_MAXBUTTON
                    })
                }
            });
            match verdict {
                Verdict::Fire(h, kind) => fire(h, kind),
                Verdict::Notice(h) => fire(h, Kind::Bypassed),
                _ => {}
            }
            if verdict.swallows() {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && WIN_UP_ON.load(Ordering::Relaxed) {
        let msg = wparam.0 as u32;
        let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        if down || up {
            // SAFETY: for WH_KEYBOARD_LL, lParam points to a KBDLLHOOKSTRUCT.
            let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let vk = info.vkCode as u16;
            let verdict = RECOGNIZER.with(|r| {
                let Ok(mut r) = r.try_borrow_mut() else {
                    return Verdict::Pass;
                };
                r.key(vk, down, || {
                    if PAUSED.load(Ordering::Relaxed) {
                        return None;
                    }
                    let kind = win_up_action(
                        key_held(VK_SHIFT),
                        key_held(VK_CONTROL),
                        key_held(VK_MENU),
                        bypass_modifier(),
                        WIN_UP_BYPASS_ON.load(Ordering::Relaxed),
                    )?;
                    let fg = unsafe { GetForegroundWindow() };
                    if fg.is_invalid() {
                        return None;
                    }
                    let root = unsafe { GetAncestor(fg, GA_ROOT) };
                    (window::is_trackable(root) && !window::is_zoomed(root) && !window::is_iconic(root))
                        .then(|| (hwnd_key(root), kind))
                })
            });
            match verdict {
                Verdict::Fire(h, kind) => {
                    fire(h, kind);
                    // Tap the mask key right away: apps watching for a lone, held Win key
                    // (PowerToys Shortcut Guide, the Start menu) must see that Win was
                    // combined with another key, as they would with a native Win+Up.
                    unsafe {
                        let _ = PostThreadMessageW(GetCurrentThreadId(), WM_APP_MASK, WPARAM(0), LPARAM(0));
                    }
                }
                // Inject after returning from the hook, not from inside it.
                Verdict::ReplayWinRelease(win_vk) => unsafe {
                    let _ = PostThreadMessageW(GetCurrentThreadId(), WM_APP_MASK, WPARAM(win_vk as usize), LPARAM(0));
                },
                _ => {}
            }
            if verdict.swallows() {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Tap an unassigned key, then (when `win_vk` is non-zero) release the Win key we
/// swallowed. Anything watching Win sees it combined with another key, so neither the
/// Start menu nor "hold Win" features (PowerToys Shortcut Guide) trigger.
fn inject_mask_and_win_release(win_vk: u16) {
    let key = |vk: u16, flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let mut inputs = vec![key(VK_MASK, KEYBD_EVENT_FLAGS(0)), key(VK_MASK, KEYEVENTF_KEYUP)];
    if win_vk != 0 {
        inputs.push(key(win_vk, KEYEVENTF_KEYUP | KEYEVENTF_EXTENDEDKEY));
    }
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// The running hook thread; stops (and unhooks) on drop.
pub struct InputHooks {
    thread_id: u32,
    handle: Option<JoinHandle<()>>,
}

impl InputHooks {
    /// Start the hook thread. `target` receives [`WM_APP_PREEMPT`].
    pub fn start(target: HWND) -> Option<Self> {
        TARGET.store(hwnd_key(target), Ordering::Relaxed);
        let (tx, rx) = mpsc::channel::<Result<u32, String>>();
        let handle = std::thread::Builder::new()
            .name("fzm-input-hooks".into())
            .spawn(move || {
                let hinstance = unsafe { GetModuleHandleW(None) }.ok().map(|m| m.into());
                let hooks: Result<Vec<HHOOK>, String> = (|| {
                    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinstance, 0) }
                        .map_err(|e| format!("WH_MOUSE_LL: {e}"))?;
                    match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), hinstance, 0) } {
                        Ok(kb) => Ok(vec![mouse, kb]),
                        Err(e) => {
                            unsafe {
                                let _ = UnhookWindowsHookEx(mouse);
                            }
                            Err(format!("WH_KEYBOARD_LL: {e}"))
                        }
                    }
                })();
                let hooks = match hooks {
                    Ok(h) => {
                        let _ = tx.send(Ok(unsafe { GetCurrentThreadId() }));
                        h
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                let mut msg = MSG::default();
                while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
                    if msg.message == WM_APP_MASK {
                        inject_mask_and_win_release(msg.wParam.0 as u16);
                        continue;
                    }
                    unsafe {
                        DispatchMessageW(&msg);
                    }
                }
                for h in hooks {
                    unsafe {
                        let _ = UnhookWindowsHookEx(h);
                    }
                }
            })
            .ok()?;
        match rx.recv() {
            Ok(Ok(thread_id)) => Some(Self {
                thread_id,
                handle: Some(handle),
            }),
            Ok(Err(e)) => {
                tracing::warn!(
                    error = e,
                    "could not install input hooks; maximize flash prevention disabled"
                );
                let _ = handle.join();
                None
            }
            Err(_) => None,
        }
    }
}

impl Drop for InputHooks {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DBL: DoubleClick = DoubleClick {
        time_ms: 500,
        cx: 4,
        cy: 4,
    };
    const W: Hwnd = 0x100;

    fn press(hit: u32, x: i32, time: u32) -> Press {
        Press {
            root: W,
            hit,
            pt: Point::new(x, 10),
            time,
            eligible: true,
            bypass: false,
        }
    }

    #[test]
    fn max_button_click_fires_on_release_over_button() {
        let mut r = Recognizer::default();
        assert_eq!(r.press(press(HT_MAXBUTTON, 50, 0), DBL), Verdict::Swallow);
        assert!(r.release_needs_hit_test());
        assert_eq!(r.release(|h| h == W), Verdict::Fire(W, Kind::MaxButton));
        // Next release has nothing pending.
        assert_eq!(r.release(|_| true), Verdict::Pass);
    }

    #[test]
    fn max_button_release_elsewhere_cancels() {
        let mut r = Recognizer::default();
        r.press(press(HT_MAXBUTTON, 50, 0), DBL);
        assert_eq!(
            r.release(|_| false),
            Verdict::Swallow,
            "release still swallowed (its press was)"
        );
    }

    #[test]
    fn caption_double_click_detection() {
        let mut r = Recognizer::default();
        assert_eq!(r.press(press(HT_CAPTION, 50, 1000), DBL), Verdict::Pass);
        assert_eq!(r.release(|_| unreachable!()), Verdict::Pass);
        assert_eq!(r.press(press(HT_CAPTION, 51, 1200), DBL), Verdict::Swallow);
        assert!(!r.release_needs_hit_test());
        assert_eq!(
            r.release(|_| unreachable!()),
            Verdict::Fire(W, Kind::CaptionDoubleClick)
        );
        // A third click starts over (not a second double-click).
        assert_eq!(r.press(press(HT_CAPTION, 51, 1300), DBL), Verdict::Pass);
    }

    #[test]
    fn caption_clicks_too_slow_far_or_different_window_are_not_double() {
        let mut r = Recognizer::default();
        r.press(press(HT_CAPTION, 50, 1000), DBL);
        assert_eq!(r.press(press(HT_CAPTION, 50, 1600), DBL), Verdict::Pass, "too slow");
        assert_eq!(r.press(press(HT_CAPTION, 60, 1700), DBL), Verdict::Pass, "too far");
        let mut other = press(HT_CAPTION, 60, 1800);
        other.root = 0x200;
        assert_eq!(r.press(other, DBL), Verdict::Pass, "different window");
        // Tick-count wrap-around is handled.
        let mut r = Recognizer::default();
        r.press(press(HT_CAPTION, 50, u32::MAX - 100), DBL);
        assert_eq!(r.press(press(HT_CAPTION, 50, 100), DBL), Verdict::Swallow);
    }

    #[test]
    fn click_elsewhere_between_breaks_double_click() {
        let mut r = Recognizer::default();
        r.press(press(HT_CAPTION, 50, 1000), DBL);
        r.press(press(1 /* HTCLIENT */, 50, 1100), DBL);
        assert_eq!(r.press(press(HT_CAPTION, 50, 1200), DBL), Verdict::Pass);
    }

    #[test]
    fn ineligible_presses_pass_through() {
        let mut r = Recognizer::default();
        let mut p = press(HT_MAXBUTTON, 50, 0);
        p.eligible = false; // maximized window, Shift held, paused...
        assert_eq!(r.press(p, DBL), Verdict::Pass);
        assert_eq!(r.release(|_| true), Verdict::Pass);
        // An ineligible caption press also resets double-click tracking.
        r.press(press(HT_CAPTION, 50, 1000), DBL);
        let mut p = press(HT_CAPTION, 50, 1100);
        p.eligible = false;
        assert_eq!(r.press(p, DBL), Verdict::Pass);
        assert_eq!(r.press(press(HT_CAPTION, 50, 1200), DBL), Verdict::Pass);
    }

    #[test]
    fn win_up_is_swallowed_with_repeats_and_release() {
        let mut r = Recognizer::default();
        assert_eq!(r.key(VK_UP.0, true, || panic!("no Win held")), Verdict::Pass);
        assert_eq!(r.key(VK_UP.0, false, || None), Verdict::Pass);
        assert_eq!(r.key(VK_LWIN.0, true, || None), Verdict::Pass);
        assert_eq!(
            r.key(VK_UP.0, true, || Some((W, Kind::WinUp))),
            Verdict::Fire(W, Kind::WinUp)
        );
        assert_eq!(r.key(VK_UP.0, true, || panic!("repeat")), Verdict::Swallow);
        assert_eq!(r.key(VK_UP.0, false, || None), Verdict::Swallow);
        // The Win release is swallowed for replay (behind a mask key) exactly once...
        assert_eq!(r.key(VK_LWIN.0, false, || None), Verdict::ReplayWinRelease(VK_LWIN.0));
        // ...and the replayed release passes.
        assert_eq!(r.key(VK_LWIN.0, false, || None), Verdict::Pass);
        assert_eq!(r.key(VK_UP.0, true, || panic!("Win released")), Verdict::Pass);
    }

    #[test]
    fn win_release_is_masked_even_when_keys_arrive_in_one_burst() {
        // Win down, Up down, Up up, Win up with no gap (fast typists, injected input).
        let mut r = Recognizer::default();
        assert_eq!(r.key(VK_RWIN.0, true, || None), Verdict::Pass);
        assert_eq!(
            r.key(VK_UP.0, true, || Some((W, Kind::WinUp))),
            Verdict::Fire(W, Kind::WinUp)
        );
        assert_eq!(r.key(VK_UP.0, false, || None), Verdict::Swallow);
        assert_eq!(r.key(VK_RWIN.0, false, || None), Verdict::ReplayWinRelease(VK_RWIN.0));
        // A plain Win tap afterwards is untouched (Start menu works normally).
        assert_eq!(r.key(VK_LWIN.0, true, || None), Verdict::Pass);
        assert_eq!(r.key(VK_LWIN.0, false, || None), Verdict::Pass);
    }

    #[test]
    fn win_up_not_intercepted_when_declined() {
        let mut r = Recognizer::default();
        r.key(VK_RWIN.0, true, || None);
        assert_eq!(r.key(VK_UP.0, true, || None), Verdict::Pass);
        assert_eq!(r.key(VK_UP.0, false, || None), Verdict::Pass);
        assert_eq!(r.key(0x41, true, || panic!()), Verdict::Pass);
    }

    #[test]
    fn bypass_modifier_clicks_pass_through_with_a_notice() {
        let mut r = Recognizer::default();
        let mut p = press(HT_MAXBUTTON, 50, 0);
        p.bypass = true;
        assert_eq!(r.press(p, DBL), Verdict::Notice(W));
        assert!(!Verdict::Notice(W).swallows());
        assert_eq!(r.release(|_| unreachable!()), Verdict::Pass, "nothing pending");
        // Double-click: first press records, second (bypass) press notifies.
        let mut first = press(HT_CAPTION, 50, 1000);
        first.bypass = true;
        assert_eq!(r.press(first, DBL), Verdict::Pass);
        let mut second = press(HT_CAPTION, 50, 1100);
        second.bypass = true;
        assert_eq!(r.press(second, DBL), Verdict::Notice(W));
        assert_eq!(r.release(|_| unreachable!()), Verdict::Pass);
    }

    #[test]
    fn win_up_modifier_matrix() {
        use BypassModifier::*;
        let none = |b| win_up_action(false, false, false, b, true);
        assert_eq!(none(Shift), Some(Kind::WinUp));
        assert_eq!(none(None), Some(Kind::WinUp));
        // Win+Shift+Up with the default bypass -> full maximize.
        assert_eq!(win_up_action(true, false, false, Shift, true), Some(Kind::WinUpFull));
        // ...or left to Windows (stretch vertically) when the bypass is disabled.
        assert_eq!(win_up_action(true, false, false, Shift, false), Option::None);
        // Bypass on Ctrl: Win+Ctrl+Up is the bypass; Win+Shift+Up is left to Windows.
        assert_eq!(win_up_action(false, true, false, Ctrl, true), Some(Kind::WinUpFull));
        assert_eq!(win_up_action(true, false, false, Ctrl, true), Option::None);
        // Bypass "none": any modifier means "not ours".
        assert_eq!(win_up_action(true, false, false, None, true), Option::None);
        // Extra modifiers on top of the bypass: not ours.
        assert_eq!(win_up_action(true, true, false, Shift, true), Option::None);
        assert_eq!(win_up_action(true, false, true, Alt, true), Option::None);
        assert_eq!(win_up_action(false, false, true, Alt, true), Some(Kind::WinUpFull));
    }

    #[test]
    fn win_bypass_up_is_swallowed_like_win_up() {
        let mut r = Recognizer::default();
        r.key(VK_LWIN.0, true, || Option::None);
        assert_eq!(
            r.key(VK_UP.0, true, || Some((W, Kind::WinUpFull))),
            Verdict::Fire(W, Kind::WinUpFull)
        );
        assert_eq!(r.key(VK_UP.0, false, || Option::None), Verdict::Swallow);
        assert_eq!(
            r.key(VK_LWIN.0, false, || Option::None),
            Verdict::ReplayWinRelease(VK_LWIN.0)
        );
    }

    #[test]
    fn kind_roundtrip() {
        for k in [
            Kind::MaxButton,
            Kind::CaptionDoubleClick,
            Kind::WinUp,
            Kind::WinUpFull,
            Kind::Bypassed,
        ] {
            assert_eq!(Kind::from_raw(k as isize), Some(k));
        }
        assert_eq!(Kind::from_raw(0), None);
    }
}
