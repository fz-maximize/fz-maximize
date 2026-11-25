//! Live end-to-end test against the real desktop. Ignored by default because it opens
//! Notepad and moves it around; run with `task e2e` (or
//! `cargo test --test e2e -- --ignored --nocapture --test-threads=1`).
//!
//! It starts its own fz-maximize, opens Notepad, maximizes it through the Win32 API and
//! checks that the visible frame lands exactly on the zone reported by `--dump --json`.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_CONTROL, VK_DOWN, VK_LEFT, VK_LWIN, VK_MENU,
    VK_RIGHT, VK_SHIFT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GA_ROOT, GetAncestor, GetClassNameW, GetCursorPos, GetForegroundWindow, GetSystemMetrics,
    GetTitleBarInfo, GetWindowRect, GetWindowThreadProcessId, HWND_TOPMOST, IsWindowVisible, IsZoomed, PostMessageW,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_MAXIMIZE, SW_RESTORE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SendMessageW, SetCursorPos, SetForegroundWindow,
    SetWindowPos, ShowWindow, TITLEBARINFO, TITLEBARINFOEX, WM_CLOSE, WM_GETTITLEBARINFOEX, WindowFromPoint,
};
use windows::core::{BOOL, w};

const EXE: &str = env!("CARGO_BIN_EXE_fz-maximize");

#[derive(Clone, Copy, Debug, PartialEq)]
struct R {
    l: i32,
    t: i32,
    r: i32,
    b: i32,
}

impl R {
    fn from(rc: RECT) -> Self {
        R {
            l: rc.left,
            t: rc.top,
            r: rc.right,
            b: rc.bottom,
        }
    }
    fn near(&self, o: &R, tol: i32) -> bool {
        (self.l - o.l).abs() <= tol
            && (self.t - o.t).abs() <= tol
            && (self.r - o.r).abs() <= tol
            && (self.b - o.b).abs() <= tol
    }
}

fn frame(h: HWND) -> R {
    let mut rc = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            h,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut rc as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
        .unwrap();
    }
    R::from(rc)
}

fn window_rect(h: HWND) -> R {
    let mut rc = RECT::default();
    unsafe { GetWindowRect(h, &mut rc).unwrap() };
    R::from(rc)
}

fn zoomed(h: HWND) -> bool {
    unsafe { IsZoomed(h).as_bool() }
}

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}

fn find_notepad(pid: u32) -> Option<HWND> {
    struct Ctx {
        pid: u32,
        found: Option<HWND>,
    }
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(lp.0 as *mut Ctx) };
        let mut p = 0;
        unsafe { GetWindowThreadProcessId(h, Some(&mut p)) };
        let mut cls = [0u16; 64];
        let n = unsafe { GetClassNameW(h, &mut cls) } as usize;
        if p == ctx.pid && String::from_utf16_lossy(&cls[..n]) == "Notepad" && unsafe { IsWindowVisible(h) }.as_bool() {
            ctx.found = Some(h);
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut ctx = Ctx { pid, found: None };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.found
}

struct Guard {
    fzm: Child,
    notepad: Option<(Child, HWND)>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some((mut child, h)) = self.notepad.take() {
            unsafe {
                let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            if !wait_until(Duration::from_secs(2), || child.try_wait().ok().flatten().is_some()) {
                let _ = child.kill();
            }
        }
        let _ = Command::new(EXE).arg("--quit").output();
        if !wait_until(Duration::from_secs(3), || self.fzm.try_wait().ok().flatten().is_some()) {
            let _ = self.fzm.kill();
        }
    }
}

fn zones_of_primary() -> Option<Vec<R>> {
    let out = Command::new(EXE)
        .args(["--dump", "--json"])
        .output()
        .expect("run --dump");
    assert!(
        out.status.success(),
        "--dump failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("dump JSON");
    let m = v["monitors"]
        .as_array()?
        .iter()
        .find(|m| m["primary"].as_bool() == Some(true) && m["intercept"].as_bool() == Some(true))?;
    let zones = m["layout"]["zones"].as_array()?;
    Some(
        zones
            .iter()
            .map(|z| R {
                l: z["left"].as_i64().unwrap() as i32,
                t: z["top"].as_i64().unwrap() as i32,
                r: z["right"].as_i64().unwrap() as i32,
                b: z["bottom"].as_i64().unwrap() as i32,
            })
            .collect(),
    )
}

fn place_inside(h: HWND, zone: &R) -> R {
    let w = ((zone.r - zone.l) * 2 / 3).max(300);
    let hgt = ((zone.b - zone.t) / 2).max(300);
    let (x, y) = (zone.l + 40, zone.t + 60);
    unsafe { SetWindowPos(h, None, x, y, w, hgt, SWP_NOZORDER).unwrap() };
    std::thread::sleep(Duration::from_millis(300));
    window_rect(h)
}

/// Start fz-maximize + Notepad. `None` means the test should be skipped.
fn setup(log_name: &str) -> Option<(Guard, HWND, Vec<R>, std::path::PathBuf)> {
    setup_with(log_name, None)
}

/// Like [`setup`], with an fz-maximize config file containing `config` (TOML).
fn setup_with(log_name: &str, config: Option<&str>) -> Option<(Guard, HWND, Vec<R>, std::path::PathBuf)> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    if unsafe { FindWindowW(w!("fz-maximize.hidden-window"), None) }.is_ok() {
        eprintln!("SKIP: an fz-maximize instance is already running; quit it first (task stop)");
        return None;
    }
    let Some(zones) = zones_of_primary() else {
        eprintln!("SKIP: primary monitor has no multi-zone FancyZones layout (see `fz-maximize --dump`)");
        return None;
    };
    assert!(zones.len() >= 2);
    eprintln!("zones on primary monitor: {zones:?}");

    let log = std::env::temp_dir().join(log_name);
    let mut cmd = Command::new(EXE);
    cmd.arg("--console");
    // Optional config override for experiments, e.g. with interception disabled.
    if let Some(text) = config {
        let path = std::env::temp_dir().join(format!("{log_name}.config.toml"));
        std::fs::write(&path, text).unwrap();
        cmd.arg("--config").arg(&path);
    } else if let Ok(cfg) = std::env::var("FZM_E2E_CONFIG") {
        cmd.args(["--config", &cfg]);
    }
    let fzm = cmd
        .env("FZM_LOG", "debug")
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .expect("start fz-maximize");
    let mut guard = Guard { fzm, notepad: None };
    assert!(
        wait_until(Duration::from_secs(5), || unsafe {
            FindWindowW(w!("fz-maximize.hidden-window"), None).is_ok()
        }),
        "fz-maximize did not start"
    );

    let notepad = Command::new("notepad.exe").spawn().expect("start notepad");
    let pid = notepad.id();
    let mut hwnd = None;
    assert!(
        wait_until(Duration::from_secs(5), || {
            hwnd = find_notepad(pid);
            hwnd.is_some()
        }),
        "notepad window not found"
    );
    let h = hwnd.unwrap();
    guard.notepad = Some((notepad, h));
    // Keep the test window above everything so synthetic clicks can never land on
    // another (real) window.
    unsafe {
        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    force_foreground(h);
    std::thread::sleep(Duration::from_millis(500));
    Some((guard, h, zones, log))
}

#[test]
#[ignore = "drives the live desktop; run with `task e2e`"]
fn maximize_snaps_into_zone_and_toggles_back() {
    let Some((_guard, h, zones, log)) = setup("fz-maximize-e2e.log") else {
        return;
    };

    for (i, zone) in [(0usize, zones[0]), (1, zones[1])] {
        eprintln!("--- zone {} {:?}", i + 1, zone);
        let before = place_inside(h, &zone);

        unsafe {
            let _ = ShowWindow(h, SW_MAXIMIZE);
        }
        let snapped = wait_until(Duration::from_secs(3), || !zoomed(h) && frame(h).near(&zone, 1));
        let f = frame(h);
        assert!(
            snapped,
            "zone {}: expected frame {zone:?}, got {f:?} (zoomed={}); see {}",
            i + 1,
            zoomed(h),
            log.display()
        );
        eprintln!("snapped: frame {f:?}");

        // Let the anti-fight window pass, then maximize again -> toggles back.
        std::thread::sleep(Duration::from_millis(1200));
        unsafe {
            let _ = ShowWindow(h, SW_MAXIMIZE);
        }
        let restored = wait_until(Duration::from_secs(3), || !zoomed(h) && window_rect(h).near(&before, 2));
        assert!(
            restored,
            "zone {}: expected toggle back to {before:?}, got {:?} (zoomed={})",
            i + 1,
            window_rect(h),
            zoomed(h)
        );
        eprintln!("toggled back: {:?}", window_rect(h));
        std::thread::sleep(Duration::from_millis(900));
    }
}

/// Keys pressed on construction and always released (in reverse order) on drop, so a
/// failing assertion can never leave a modifier stuck down.
struct Keys(Vec<VIRTUAL_KEY>);

fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    let extended = matches!(vk, VK_UP | VK_DOWN | VK_LEFT | VK_RIGHT | VK_LWIN);
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

impl Keys {
    fn down(keys: &[VIRTUAL_KEY]) -> Self {
        let inputs: Vec<INPUT> = keys.iter().map(|&k| key_input(k, false)).collect();
        unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        Keys(keys.to_vec())
    }
}

impl Drop for Keys {
    fn drop(&mut self) {
        let inputs: Vec<INPUT> = self.0.iter().rev().map(|&k| key_input(k, true)).collect();
        unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn chord(keys: &[VIRTUAL_KEY]) {
    drop(Keys::down(keys));
}

#[test]
#[ignore = "drives the live desktop and injects keystrokes; run with `task e2e`"]
fn bypass_modifier_hotkey_and_win_up() {
    let Some((_guard, h, zones, log)) = setup("fz-maximize-e2e-keys.log") else {
        return;
    };
    let zone = zones[1];
    place_inside(h, &zone);
    force_foreground(h);
    std::thread::sleep(Duration::from_millis(300));

    // 1. Shift held while maximizing -> native full-monitor maximize.
    {
        let _shift = Keys::down(&[VK_SHIFT]);
        std::thread::sleep(Duration::from_millis(100));
        unsafe {
            let _ = ShowWindow(h, SW_MAXIMIZE);
        }
        std::thread::sleep(Duration::from_millis(800));
    }
    assert!(
        zoomed(h),
        "Shift+maximize should stay natively maximized; see {}",
        log.display()
    );
    eprintln!("shift bypass: natively maximized, frame {:?}", frame(h));
    unsafe {
        let _ = ShowWindow(h, SW_RESTORE);
    }
    assert!(wait_until(Duration::from_secs(2), || !zoomed(h)));
    std::thread::sleep(Duration::from_millis(900));

    // 2. Hotkey (default Ctrl+Alt+Up) -> native maximize; again -> restore.
    force_foreground(h);
    std::thread::sleep(Duration::from_millis(200));
    chord(&[VK_CONTROL, VK_MENU, VK_UP]);
    assert!(
        wait_until(Duration::from_secs(2), || zoomed(h)),
        "hotkey should natively maximize the foreground window; see {}",
        log.display()
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(zoomed(h), "hotkey maximize must not be redirected into a zone");
    eprintln!("hotkey: natively maximized");
    chord(&[VK_CONTROL, VK_MENU, VK_UP]);
    assert!(
        wait_until(Duration::from_secs(2), || !zoomed(h)),
        "hotkey again should restore"
    );
    eprintln!("hotkey again: restored");
    std::thread::sleep(Duration::from_millis(900));

    // 3. Win+Up (the Windows maximize shortcut) -> redirected into the zone.
    place_inside(h, &zone);
    force_foreground(h);
    std::thread::sleep(Duration::from_millis(200));
    chord(&[VK_LWIN, VK_UP]);
    assert!(
        wait_until(Duration::from_secs(3), || !zoomed(h) && frame(h).near(&zone, 1)),
        "Win+Up: expected frame {zone:?}, got {:?} (zoomed={}); see {}",
        frame(h),
        zoomed(h),
        log.display()
    );
    eprintln!("win+up: snapped to {:?}", frame(h));
}

fn send_mouse(x: i32, y: i32, flags: MOUSE_EVENT_FLAGS) {
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let nx = ((x - vx) as i64 * 65535 / (vw - 1).max(1) as i64) as i32;
    let ny = ((y - vy) as i64 * 65535 / (vh - 1).max(1) as i64) as i32;
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: nx,
                dy: ny,
                mouseData: 0,
                dwFlags: flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
}

/// Releases the left button and restores the cursor even if the test fails mid-drag.
struct MouseGuard(POINT);
impl Drop for MouseGuard {
    fn drop(&mut self) {
        send_mouse(self.0.x, self.0.y, MOUSEEVENTF_LEFTUP);
        unsafe {
            let _ = SetCursorPos(self.0.x, self.0.y);
        }
    }
}

#[test]
#[ignore = "drives the live desktop, moves the mouse and injects Shift; run with `task e2e`"]
fn fancyzones_shift_drag_agrees_with_our_zones() {
    let Some((_guard, h, zones, log)) = setup("fz-maximize-e2e-drag.log") else {
        return;
    };
    // Start inside the middle zone, drop into the first zone.
    let target = zones[0];
    // Synthetic drags are timing-sensitive (e.g. right after another test released
    // injected keys), so the gesture gets up to 3 attempts. A real geometry mismatch
    // still fails: FancyZones would land on the same wrong rect every time.
    let mut snapped = false;
    for attempt in 1..=3 {
        if attempt > 1 {
            eprintln!(
                "drag attempt {} did not snap (window at {:?}); retrying",
                attempt - 1,
                frame(h)
            );
        }
        drag_with_shift(h, &zones[1], &target);
        if wait_until(Duration::from_secs(2), || frame(h).near(&target, 1)) {
            snapped = true;
            break;
        }
    }

    // FancyZones itself placed the window: its frame must equal *our* zone 1.
    assert!(
        snapped,
        "FancyZones placed the window at {:?}, fz-maximize computes zone 1 as {target:?} \n         (if the window didn't move at all, FancyZones didn't snap: is Shift-drag enabled?)",
        frame(h)
    );
    eprintln!("FancyZones snapped to {:?} == our zone 1", frame(h));
    check_logged_match(&log);
}

/// Place the window inside `start`, then Shift-drag it by its title bar into `target`.
fn drag_with_shift(h: HWND, start: &R, target: &R) {
    let held = |vk: VIRTUAL_KEY| {
        (unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(i32::from(vk.0)) } as u16 & 0x8000) != 0
    };
    eprintln!(
        "drag start: LWin={} Shift={} foreground={}",
        held(VK_LWIN),
        held(VK_SHIFT),
        foreground_class()
    );
    place_inside(h, start);
    force_foreground(h);
    std::thread::sleep(Duration::from_millis(800));

    let mut tbi = TITLEBARINFO {
        cbSize: std::mem::size_of::<TITLEBARINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetTitleBarInfo(h, &mut tbi).unwrap() };
    let tb = tbi.rcTitleBar;
    let grab = POINT {
        x: tb.left + (tb.right - tb.left) / 3,
        y: (tb.top + tb.bottom) / 2,
    };
    let drop_at = POINT {
        x: (target.l + target.r) / 2,
        y: (target.t + target.b) / 2,
    };

    let mut original = POINT::default();
    unsafe { GetCursorPos(&mut original).unwrap() };
    {
        let _mouse = MouseGuard(original);
        send_mouse(grab.x, grab.y, MOUSE_EVENT_FLAGS(0));
        std::thread::sleep(Duration::from_millis(150));
        assert_target_under(h, grab.x, grab.y);
        send_mouse(grab.x, grab.y, MOUSEEVENTF_LEFTDOWN);
        std::thread::sleep(Duration::from_millis(150));
        // Start the drag, then hold Shift so FancyZones shows and snaps to zones.
        send_mouse(grab.x + 20, grab.y + 10, MOUSE_EVENT_FLAGS(0));
        std::thread::sleep(Duration::from_millis(150));
        let _shift = Keys::down(&[VK_SHIFT]);
        let steps = 25;
        for i in 1..=steps {
            let x = grab.x + (drop_at.x - grab.x) * i / steps;
            let y = grab.y + (drop_at.y - grab.y) * i / steps;
            send_mouse(x, y, MOUSE_EVENT_FLAGS(0));
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(300));
        send_mouse(drop_at.x, drop_at.y, MOUSEEVENTF_LEFTUP);
        std::thread::sleep(Duration::from_millis(200));
        // _shift released, then the cursor is restored.
    }
}

/// The running fz-maximize must have logged a matching agreement check.
fn check_logged_match(log: &std::path::Path) {
    let mut text = String::new();
    let found = wait_until(Duration::from_secs(3), || {
        text = std::fs::read_to_string(log).unwrap_or_default();
        text.contains("FancyZones agreement check")
    });
    let line = text
        .lines()
        .find(|l| l.contains("FancyZones agreement check"))
        .unwrap_or_default()
        .to_string();
    assert!(found, "no agreement check logged; see {}", log.display());
    eprintln!("log: {line}");
    assert!(
        line.contains("match") && !line.contains("MISMATCH"),
        "check did not match: {line}"
    );
}

/// Watches a window on a background thread and records whether it was *ever*
/// maximized, even for a single frame. A pre-empted maximize must never be.
struct ZoomWatch {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    seen: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ZoomWatch {
    fn start(h: HWND) -> Self {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(AtomicBool::new(false));
        let (s, z) = (stop.clone(), seen.clone());
        let raw = h.0 as isize;
        let handle = std::thread::spawn(move || {
            let h = HWND(raw as *mut core::ffi::c_void);
            while !s.load(Ordering::Relaxed) {
                if unsafe { IsZoomed(h) }.as_bool() {
                    z.store(true, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        ZoomWatch {
            stop,
            seen,
            handle: Some(handle),
        }
    }

    /// Stop watching; true if the window was maximized at any point.
    fn finish(mut self) -> bool {
        use std::sync::atomic::Ordering;
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.seen.load(Ordering::Relaxed)
    }
}

/// Panics unless the window under `(x, y)` is the test window: synthetic input must never
/// reach anything else on the desktop.
fn assert_target_under(h: HWND, x: i32, y: i32) {
    let under = unsafe { WindowFromPoint(POINT { x, y }) };
    let root = unsafe { GetAncestor(under, GA_ROOT) };
    assert_eq!(
        root,
        h,
        "refusing to click: another window ({}) is under ({x}, {y})",
        class_of(root)
    );
}

fn class_of(h: HWND) -> String {
    let mut cls = [0u16; 128];
    let n = unsafe { GetClassNameW(h, &mut cls) } as usize;
    String::from_utf16_lossy(&cls[..n])
}

/// Make `h` the foreground window (same zero-input trick fz-maximize uses).
fn force_foreground(h: HWND) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        ..Default::default()
    };
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        let _ = SetForegroundWindow(h);
    }
}

fn click(h: HWND, x: i32, y: i32) {
    assert_target_under(h, x, y);
    send_mouse(x, y, MOUSE_EVENT_FLAGS(0));
    std::thread::sleep(Duration::from_millis(40));
    send_mouse(x, y, MOUSEEVENTF_LEFTDOWN);
    std::thread::sleep(Duration::from_millis(40));
    send_mouse(x, y, MOUSEEVENTF_LEFTUP);
}

fn title_bar_buttons(h: HWND) -> (POINT, POINT) {
    let mut info = TITLEBARINFOEX {
        cbSize: std::mem::size_of::<TITLEBARINFOEX>() as u32,
        ..Default::default()
    };
    unsafe {
        SendMessageW(
            h,
            WM_GETTITLEBARINFOEX,
            Some(WPARAM(0)),
            Some(LPARAM(&mut info as *mut _ as isize)),
        );
    }
    let max = info.rgrect[3]; // 3 = maximize button
    let bar = info.rcTitleBar;
    (
        POINT {
            x: (max.left + max.right) / 2,
            y: (max.top + max.bottom) / 2,
        },
        POINT {
            x: bar.left + (bar.right - bar.left) / 3,
            y: (bar.top + bar.bottom) / 2,
        },
    )
}

fn foreground_class() -> String {
    let fg = unsafe { GetForegroundWindow() };
    let mut cls = [0u16; 128];
    let n = unsafe { GetClassNameW(fg, &mut cls) } as usize;
    String::from_utf16_lossy(&cls[..n])
}

fn foreground_is(h: HWND) -> bool {
    let fg = unsafe { GetForegroundWindow() };
    fg == h
}

#[test]
#[ignore = "drives the live desktop with real clicks and keys; run with `task e2e`"]
fn intercepted_gestures_never_flash() {
    let Some((_guard, h, zones, log)) = setup("fz-maximize-e2e-preempt.log") else {
        return;
    };
    let zone = zones[1];
    let mut original = POINT::default();
    unsafe { GetCursorPos(&mut original).unwrap() };
    let _mouse = MouseGuard(original);

    // 1. Click the maximize button: straight into the zone, never maximized.
    let before = place_inside(h, &zone);
    force_foreground(h);
    std::thread::sleep(Duration::from_millis(500));
    let (max_btn, caption) = title_bar_buttons(h);
    eprintln!("before click: foreground {}", foreground_class());
    let watch = ZoomWatch::start(h);
    click(h, max_btn.x, max_btn.y);
    let mut seen_fg: Vec<String> = Vec::new();
    for _ in 0..30 {
        let c = foreground_class();
        if seen_fg.last() != Some(&c) {
            seen_fg.push(c);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("foreground after max-button click: {seen_fg:?}");
    let snapped = wait_until(Duration::from_secs(2), || frame(h).near(&zone, 1));
    std::thread::sleep(Duration::from_millis(300));
    let flashed = watch.finish();
    assert!(
        snapped,
        "max button: expected {zone:?}, got {:?}; see {}",
        frame(h),
        log.display()
    );
    assert!(!flashed, "max button: window was maximized at some point (flash)");
    assert!(
        foreground_is(h),
        "max button: window should be active after the snap (foreground: {})",
        foreground_class()
    );
    eprintln!("maximize button: zone {:?}, no flash", frame(h));
    std::thread::sleep(Duration::from_millis(900));

    // 2. Double-click the title bar: toggles back to the previous size, never maximized.
    let (_, caption_now) = title_bar_buttons(h);
    let watch = ZoomWatch::start(h);
    click(h, caption_now.x, caption_now.y);
    std::thread::sleep(Duration::from_millis(60));
    click(h, caption_now.x, caption_now.y);
    let restored = wait_until(Duration::from_secs(2), || window_rect(h).near(&before, 2));
    std::thread::sleep(Duration::from_millis(300));
    let flashed = watch.finish();
    assert!(
        restored,
        "double-click: expected toggle back to {before:?}, got {:?}",
        window_rect(h)
    );
    assert!(!flashed, "double-click (toggle): window was maximized at some point");
    eprintln!("title-bar double-click: toggled back, no flash");
    std::thread::sleep(Duration::from_millis(900));

    // 3. Double-click again: into the zone, never maximized.
    let watch = ZoomWatch::start(h);
    click(h, caption.x, caption.y);
    std::thread::sleep(Duration::from_millis(60));
    click(h, caption.x, caption.y);
    let snapped = wait_until(Duration::from_secs(2), || frame(h).near(&zone, 1));
    std::thread::sleep(Duration::from_millis(300));
    let flashed = watch.finish();
    assert!(snapped, "double-click: expected {zone:?}, got {:?}", frame(h));
    assert!(!flashed, "double-click: window was maximized at some point (flash)");
    eprintln!("title-bar double-click: zone, no flash");
    std::thread::sleep(Duration::from_millis(900));

    // 4. Win+Up from a floating window: zone, never maximized, and no Start menu.
    place_inside(h, &zone);
    force_foreground(h);
    std::thread::sleep(Duration::from_millis(400));
    let watch = ZoomWatch::start(h);
    chord(&[VK_LWIN, VK_UP]);
    let snapped = wait_until(Duration::from_secs(2), || frame(h).near(&zone, 1));
    std::thread::sleep(Duration::from_millis(600));
    let flashed = watch.finish();
    assert!(snapped, "Win+Up: expected {zone:?}, got {:?}", frame(h));
    assert!(!flashed, "Win+Up: window was maximized at some point (flash)");
    assert!(
        foreground_is(h),
        "Win+Up: focus left the window to {} (did the Start menu open?)",
        foreground_class()
    );
    eprintln!("Win+Up: zone, no flash, no Start menu");
    std::thread::sleep(Duration::from_millis(900));

    // 5. Shift + maximize button still gives the native full maximize.
    place_inside(h, &zone);
    force_foreground(h);
    std::thread::sleep(Duration::from_millis(400));
    let (max_btn, _) = title_bar_buttons(h);
    {
        let _shift = Keys::down(&[VK_SHIFT]);
        std::thread::sleep(Duration::from_millis(80));
        click(h, max_btn.x, max_btn.y);
        std::thread::sleep(Duration::from_millis(400));
    }
    assert!(
        wait_until(Duration::from_secs(2), || zoomed(h)),
        "Shift+max button should maximize natively"
    );
    eprintln!("Shift + maximize button: native maximize");

    let text = std::fs::read_to_string(&log).unwrap_or_default();
    for gesture in ["maximize button", "title-bar double-click", "Win+Up"] {
        assert!(
            text.contains(&format!("source=\"{gesture}\"")),
            "log has no intercepted '{gesture}' snap; see {}",
            log.display()
        );
    }
}

#[test]
#[ignore = "drives the live desktop and injects keystrokes; run with `task e2e`"]
fn win_held_repeated_up_keeps_toggling() {
    let Some((_guard, h, zones, log)) = setup("fz-maximize-e2e-winrepeat.log") else {
        return;
    };
    let zone = zones[1];
    let before = place_inside(h, &zone);
    force_foreground(h);
    std::thread::sleep(Duration::from_millis(400));

    let mut states = Vec::new();
    {
        let _win = Keys::down(&[VK_LWIN]);
        std::thread::sleep(Duration::from_millis(150));
        // Quick presses, then slow ones: holding Win alone for a second must not let
        // "hold Win" features (PowerToys Shortcut Guide) steal focus between presses.
        for i in 0..10 {
            chord(&[VK_UP]);
            std::thread::sleep(Duration::from_millis(if i < 7 { 450 } else { 1500 }));
            let f = frame(h);
            let state = if zoomed(h) {
                "MAXIMIZED"
            } else if f.near(&zone, 1) {
                "zone"
            } else if window_rect(h).near(&before, 2) {
                "restored"
            } else {
                "other"
            };
            eprintln!("press {}: {state} frame={f:?} foreground={}", i + 1, foreground_class());
            states.push(state);
        }
    }
    let expected: Vec<&str> = (0..10).map(|i| if i % 2 == 0 { "zone" } else { "restored" }).collect();
    assert_eq!(states, expected, "see {}", log.display());
}

/// Undo a native maximize and float the window inside `zone` again.
fn reset_window(h: HWND, zone: &R) {
    unsafe {
        let _ = ShowWindow(h, SW_RESTORE);
    }
    assert!(wait_until(Duration::from_secs(2), || !zoomed(h)), "could not restore");
    place_inside(h, zone);
    force_foreground(h);
    std::thread::sleep(Duration::from_millis(900)); // past the anti-fight window
}

fn double_click(h: HWND, p: POINT) {
    click(h, p.x, p.y);
    std::thread::sleep(Duration::from_millis(60));
    click(h, p.x, p.y);
}

#[test]
#[ignore = "drives the live desktop with real clicks and keys; run with `task e2e`"]
fn bypass_modifier_is_configurable() {
    let choices = [("shift", VK_SHIFT), ("ctrl", VK_CONTROL), ("alt", VK_MENU)];
    for (name, vk) in choices {
        let log_name = format!("fz-maximize-e2e-bypass-{name}.log");
        let config = format!("bypass_modifier = \"{name}\"\n");
        let Some((guard, h, zones, log)) = setup_with(&log_name, Some(&config)) else {
            return;
        };
        let zone = zones[1];
        let mut original = POINT::default();
        unsafe { GetCursorPos(&mut original).unwrap() };
        let _mouse = MouseGuard(original);
        reset_window(h, &zone);

        // modifier + maximize button -> native maximize
        let (max_btn, caption) = title_bar_buttons(h);
        {
            let _m = Keys::down(&[vk]);
            std::thread::sleep(Duration::from_millis(80));
            click(h, max_btn.x, max_btn.y);
            std::thread::sleep(Duration::from_millis(300));
        }
        assert!(
            wait_until(Duration::from_secs(2), || zoomed(h)),
            "[{name}] modifier + max button should maximize natively; see {}",
            log.display()
        );
        reset_window(h, &zone);

        // modifier + title-bar double-click -> native maximize
        {
            let _m = Keys::down(&[vk]);
            std::thread::sleep(Duration::from_millis(80));
            double_click(h, caption);
            std::thread::sleep(Duration::from_millis(300));
        }
        assert!(
            wait_until(Duration::from_secs(2), || zoomed(h)),
            "[{name}] modifier + double-click should maximize natively; see {}",
            log.display()
        );
        reset_window(h, &zone);

        // Win + modifier + Up -> native maximize, focus kept (no Start menu)
        chord(&[VK_LWIN, vk, VK_UP]);
        assert!(
            wait_until(Duration::from_secs(2), || zoomed(h)),
            "[{name}] Win+modifier+Up should maximize natively; see {}",
            log.display()
        );
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            foreground_is(h),
            "[{name}] Win+modifier+Up: focus went to {}",
            foreground_class()
        );
        reset_window(h, &zone);

        // A different modifier is not a bypass: the click still goes to the zone.
        let other = if vk == VK_CONTROL { VK_SHIFT } else { VK_CONTROL };
        let (max_btn, _) = title_bar_buttons(h);
        {
            let _m = Keys::down(&[other]);
            std::thread::sleep(Duration::from_millis(80));
            click(h, max_btn.x, max_btn.y);
            std::thread::sleep(Duration::from_millis(300));
        }
        assert!(
            wait_until(Duration::from_secs(2), || !zoomed(h) && frame(h).near(&zone, 1)),
            "[{name}] a non-bypass modifier must not bypass (zoomed={}, frame={:?})",
            zoomed(h),
            frame(h)
        );
        eprintln!(
            "bypass_modifier = \"{name}\": max button, double-click, Win+{name}+Up all native; other modifier snaps"
        );
        drop(guard);
        std::thread::sleep(Duration::from_millis(800));
    }
}

/// Control ids from src/settings.rs.
const SET_OK: i32 = 1;
const SET_MODIFIER: i32 = 110;
const SET_DRAG: i32 = 112;
const SET_ASPECT: i32 = 130;

fn find_top_level(class: windows::core::PCWSTR) -> Option<HWND> {
    unsafe { FindWindowW(class, None) }.ok().filter(|h| !h.is_invalid())
}

fn settings_item(s: HWND, id: i32) -> HWND {
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetDlgItem(Some(s), id) }.expect("settings control")
}

/// Set a control's text in another process (SetWindowText can't; WM_SETTEXT is marshaled).
fn set_control_text(h: HWND, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        SendMessageW(
            h,
            windows::Win32::UI::WindowsAndMessaging::WM_SETTEXT,
            Some(WPARAM(0)),
            Some(LPARAM(wide.as_ptr() as isize)),
        );
    }
}

/// Simulate clicking a settings button (WM_COMMAND/BN_CLICKED to the window).
fn press_settings_button(s: HWND, id: i32, post: bool) {
    let wparam = WPARAM(id as usize); // HIWORD = BN_CLICKED (0)
    let lparam = LPARAM(settings_item(s, id).0 as isize);
    unsafe {
        if post {
            let _ = PostMessageW(
                Some(s),
                windows::Win32::UI::WindowsAndMessaging::WM_COMMAND,
                wparam,
                lparam,
            );
        } else {
            SendMessageW(
                s,
                windows::Win32::UI::WindowsAndMessaging::WM_COMMAND,
                Some(wparam),
                Some(lparam),
            );
        }
    }
}

#[test]
#[ignore = "drives the live desktop; run with `task e2e`"]
fn settings_window_saves_and_applies() {
    use windows::Win32::UI::WindowsAndMessaging::{BM_SETCHECK, CB_SETCURSEL};
    let log_name = "fz-maximize-e2e-settings.log";
    let Some((_guard, h, zones, log)) = setup_with(log_name, Some("bypass_modifier = \"shift\"\n")) else {
        return;
    };
    let config_path = std::env::temp_dir().join(format!("{log_name}.config.toml"));
    let zone = zones[1];

    // Open the settings window in the running instance via the CLI.
    let out = Command::new(EXE).arg("--settings").output().unwrap();
    assert!(
        out.status.success(),
        "--settings: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let mut found = None;
    assert!(
        wait_until(Duration::from_secs(3), || {
            // Visible = fully built (it is shown after its controls are created).
            found = find_top_level(w!("fz-maximize.settings")).filter(|h| unsafe { IsWindowVisible(*h) }.as_bool());
            found.is_some()
        }),
        "settings window did not open"
    );
    let s = found.unwrap();

    // 1. An invalid value shows an error and writes nothing.
    let before = std::fs::read_to_string(&config_path).unwrap();
    set_control_text(settings_item(s, SET_ASPECT), "wide");
    press_settings_button(s, SET_OK, true); // posted: the error box is modal
    let mut error_box = None;
    assert!(
        wait_until(Duration::from_secs(3), || {
            error_box = unsafe { FindWindowW(w!("#32770"), w!("fz-maximize settings")) }
                .ok()
                .filter(|h| !h.is_invalid());
            error_box.is_some()
        }),
        "no error box for an invalid number"
    );
    unsafe {
        let _ = PostMessageW(
            error_box,
            windows::Win32::UI::WindowsAndMessaging::WM_COMMAND,
            WPARAM(1),
            LPARAM(0),
        );
    }
    let gone = || unsafe { FindWindowW(w!("#32770"), w!("fz-maximize settings")) }.is_err();
    if !wait_until(Duration::from_secs(1), gone) {
        eprintln!(
            "error box still open after IDOK (visible={}); sending WM_CLOSE",
            unsafe { IsWindowVisible(error_box.unwrap()) }.as_bool()
        );
        unsafe {
            let _ = PostMessageW(error_box, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    assert!(
        wait_until(Duration::from_secs(2), gone),
        "could not dismiss the error box"
    );
    assert_eq!(
        std::fs::read_to_string(&config_path).unwrap(),
        before,
        "invalid settings must not be saved"
    );
    assert!(
        find_top_level(w!("fz-maximize.settings")).is_some(),
        "window stays open after an error"
    );
    eprintln!("invalid value: error shown, nothing saved");

    // 2. Change the modifier to Ctrl, untick drag-snapping, OK.
    unsafe {
        set_control_text(settings_item(s, SET_ASPECT), "0");
        SendMessageW(
            settings_item(s, SET_MODIFIER),
            CB_SETCURSEL,
            Some(WPARAM(1)),
            Some(LPARAM(0)),
        );
        SendMessageW(
            settings_item(s, SET_DRAG),
            BM_SETCHECK,
            Some(WPARAM(0)),
            Some(LPARAM(0)),
        );
    }
    press_settings_button(s, SET_OK, false);
    assert!(
        wait_until(Duration::from_secs(2), || find_top_level(w!("fz-maximize.settings"))
            .is_none()),
        "OK should close the window"
    );
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("bypass_modifier = \"ctrl\""), "config not saved:\n{text}");
    assert!(
        text.contains("snap_on_drag_maximize = false"),
        "config not saved:\n{text}"
    );
    assert!(
        text.contains("# Hold this while clicking Maximize"),
        "documentation comments kept"
    );
    eprintln!("OK: config.toml written with the new values");

    // 3. Applied live: Ctrl now bypasses, Shift no longer does.
    let mut original = POINT::default();
    unsafe { GetCursorPos(&mut original).unwrap() };
    let _mouse = MouseGuard(original);
    reset_window(h, &zone);
    let (max_btn, _) = title_bar_buttons(h);
    {
        let _ctrl = Keys::down(&[VK_CONTROL]);
        std::thread::sleep(Duration::from_millis(80));
        click(h, max_btn.x, max_btn.y);
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(
        wait_until(Duration::from_secs(2), || zoomed(h)),
        "Ctrl + max button should now maximize natively; see {}",
        log.display()
    );
    reset_window(h, &zone);
    let (max_btn, _) = title_bar_buttons(h);
    {
        let _shift = Keys::down(&[VK_SHIFT]);
        std::thread::sleep(Duration::from_millis(80));
        click(h, max_btn.x, max_btn.y);
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(
        wait_until(Duration::from_secs(2), || !zoomed(h) && frame(h).near(&zone, 1)),
        "Shift is no longer the bypass, so the click should snap"
    );
    eprintln!("applied live: Ctrl bypasses, Shift snaps");
}
