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

/// Save a screen region as a 32-bit BMP (for looking at an animation frame).
fn capture_bmp(r: &R, path: &std::path::Path) {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS,
        DeleteDC, DeleteObject, GetDC, GetDIBits, HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject,
    };
    let (w, h) = (r.r - r.l, r.b - r.t);
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, HGDIOBJ(bmp.0));
        let _ = BitBlt(mem, 0, 0, w, h, Some(screen), r.l, r.t, SRCCOPY);
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut bi,
            DIB_RGB_COLORS,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
    }
    let mut f = Vec::with_capacity(54 + pixels.len());
    let file_size = 54 + pixels.len() as u32;
    f.extend_from_slice(b"BM");
    f.extend_from_slice(&file_size.to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&54u32.to_le_bytes());
    f.extend_from_slice(&40u32.to_le_bytes());
    f.extend_from_slice(&w.to_le_bytes());
    f.extend_from_slice(&(-h).to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&32u16.to_le_bytes());
    f.extend_from_slice(&[0u8; 24]);
    f.extend_from_slice(&pixels);
    std::fs::write(path, f).unwrap();
}

/// What happened during one animated gesture, sampled every few milliseconds.
struct AnimationTrace {
    overlay_first_seen: Option<Duration>,
    overlay_last_seen: Option<Duration>,
    window_reached_target: Option<Duration>,
    maximized_at_some_point: bool,
}

/// Run `gesture` and sample the overlay and the window until both have settled.
fn trace_animation(
    h: HWND,
    target: &R,
    frame_path: Option<&std::path::Path>,
    gesture: impl FnOnce(),
) -> AnimationTrace {
    let overlay_visible = || {
        unsafe { FindWindowW(w!("fz-maximize.animation"), None) }
            .ok()
            .is_some_and(|o| unsafe { IsWindowVisible(o) }.as_bool())
    };
    let watch = ZoomWatch::start(h);
    let start = Instant::now();
    gesture();
    let mut t = AnimationTrace {
        overlay_first_seen: None,
        overlay_last_seen: None,
        window_reached_target: None,
        maximized_at_some_point: false,
    };
    let mut captured = false;
    while start.elapsed() < Duration::from_secs(3) {
        let now = start.elapsed();
        if overlay_visible() {
            t.overlay_first_seen.get_or_insert(now);
            t.overlay_last_seen = Some(now);
            if let (Some(path), Some(first), false) = (frame_path, t.overlay_first_seen, captured)
                && now >= first + Duration::from_millis(60)
            {
                let region = R {
                    l: target.l.min(window_rect(h).l),
                    t: target.t.min(window_rect(h).t),
                    r: target.r.max(window_rect(h).r),
                    b: target.b.max(window_rect(h).b),
                };
                capture_bmp(&region, path);
                captured = true;
            }
        }
        if t.window_reached_target.is_none() && window_rect(h).near(target, 2) {
            t.window_reached_target = Some(now);
        }
        if t.window_reached_target.is_some() && !overlay_visible() && now > Duration::from_millis(400) {
            break;
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    t.maximized_at_some_point = watch.finish();
    t
}

#[test]
#[ignore = "drives the live desktop with real clicks; run with `task e2e`"]
fn maximize_animates_into_zone_and_back() {
    let Some((_guard, h, zones, log)) = setup_with("fz-maximize-e2e-anim.log", Some("animation_ms = 250\n")) else {
        return;
    };
    let zone = zones[1];
    let mut original = POINT::default();
    unsafe { GetCursorPos(&mut original).unwrap() };
    let _mouse = MouseGuard(original);

    // Grow: maximize button. The real window must only move at the END (preview covers it).
    let before = place_inside(h, &zone);
    force_foreground(h);
    std::thread::sleep(Duration::from_millis(500));
    let (max_btn, caption) = title_bar_buttons(h);
    let frame_path = std::env::temp_dir().join("fz-maximize-animation-frame.bmp");
    let grow = trace_animation(h, &zone_window_rect(h, &zone), Some(&frame_path), || {
        click(h, max_btn.x, max_btn.y)
    });
    eprintln!(
        "grow: overlay {:?}..{:?}, window reached zone at {:?}, frame saved to {}",
        grow.overlay_first_seen,
        grow.overlay_last_seen,
        grow.window_reached_target,
        frame_path.display()
    );
    assert_covered_animation("grow", &grow);
    assert!(frame(h).near(&zone, 1), "final frame {:?} != zone {zone:?}", frame(h));
    assert!(
        !grow.maximized_at_some_point,
        "window was maximized at some point (flash)"
    );
    assert!(foreground_is(h), "window should be active after the snap");
    std::thread::sleep(Duration::from_millis(900));

    // Shrink: double-click toggles back to the previous size, also animated.
    let (_, caption_now) = title_bar_buttons(h);
    let shrink = trace_animation(h, &before, None, || double_click(h, caption_now));
    eprintln!(
        "shrink: overlay {:?}..{:?}, window reached previous size at {:?}",
        shrink.overlay_first_seen, shrink.overlay_last_seen, shrink.window_reached_target
    );
    assert_covered_animation("shrink", &shrink);
    assert!(
        window_rect(h).near(&before, 2),
        "expected {before:?}, got {:?}",
        window_rect(h)
    );
    assert!(!shrink.maximized_at_some_point);
    let _ = (caption, log);
}

#[test]
#[ignore = "drives the live desktop with real clicks; run with `task e2e`"]
fn animation_can_be_turned_off() {
    let Some((_guard, h, zones, _log)) = setup_with("fz-maximize-e2e-noanim.log", Some("animate_maximize = false\n"))
    else {
        return;
    };
    let zone = zones[1];
    let mut original = POINT::default();
    unsafe { GetCursorPos(&mut original).unwrap() };
    let _mouse = MouseGuard(original);
    place_inside(h, &zone);
    force_foreground(h);
    std::thread::sleep(Duration::from_millis(500));
    let (max_btn, _) = title_bar_buttons(h);
    let t = trace_animation(h, &zone_window_rect(h, &zone), None, || click(h, max_btn.x, max_btn.y));
    assert!(
        t.overlay_first_seen.is_none(),
        "animation overlay shown although animate_maximize = false"
    );
    assert!(frame(h).near(&zone, 1), "final frame {:?} != zone {zone:?}", frame(h));
    eprintln!(
        "animation off: no overlay; window reached the zone at {:?}",
        t.window_reached_target
    );
}

/// The overlay must already be up when the real window moves (so the move is hidden)
/// and stay up for most of the 250 ms animation.
fn assert_covered_animation(what: &str, t: &AnimationTrace) {
    let first = t
        .overlay_first_seen
        .unwrap_or_else(|| panic!("{what}: no animation overlay appeared"));
    let last = t.overlay_last_seen.unwrap();
    let reached = t
        .window_reached_target
        .unwrap_or_else(|| panic!("{what}: window never reached its target"));
    assert!(
        first <= reached,
        "{what}: window moved at {reached:?}, before the overlay covered it ({first:?})"
    );
    assert!(
        last >= reached,
        "{what}: overlay gone ({last:?}) before the window arrived ({reached:?})"
    );
    assert!(
        last - first >= Duration::from_millis(180),
        "{what}: animation lasted only {:?}",
        last - first
    );
}

/// The window rect that puts the visible frame exactly on `zone` (adds the invisible
/// borders measured from the window right now).
fn zone_window_rect(h: HWND, zone: &R) -> R {
    let (w, f) = (window_rect(h), frame(h));
    R {
        l: zone.l - (f.l - w.l),
        t: zone.t - (f.t - w.t),
        r: zone.r + (w.r - f.r),
        b: zone.b + (w.b - f.b),
    }
}

/// One screen row `[x0, x1)` at `y`, as 0x00RRGGBB pixels.
fn capture_row(x0: i32, x1: i32, y: i32) -> Vec<u32> {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HGDIOBJ, ROP_CODE, ReleaseDC, SRCCOPY, SelectObject,
    };
    let w = x1 - x0;
    let mut px = vec![0u32; w as usize];
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, 1);
        let old = SelectObject(mem, HGDIOBJ(bmp.0));
        let _ = BitBlt(mem, 0, 0, w, 1, Some(screen), x0, y, ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0));
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -1,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        GetDIBits(mem, bmp, 0, 1, Some(px.as_mut_ptr().cast()), &mut bi, DIB_RGB_COLORS);
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
    }
    px.iter_mut().for_each(|p| *p &= 0x00FF_FFFF);
    px
}

/// The near-white run in `row` that contains index `at`: (start, end) indices.
fn white_run(row: &[u32], at: usize) -> Option<(usize, usize)> {
    let white = |p: u32| (p >> 16 & 0xFF) > 0xF0 && (p >> 8 & 0xFF) > 0xF0 && (p & 0xFF) > 0xF0;
    if !white(*row.get(at)?) {
        return None;
    }
    let mut l = at;
    while l > 0 && white(row[l - 1]) {
        l -= 1;
    }
    let mut r = at;
    while r + 1 < row.len() && white(row[r + 1]) {
        r += 1;
    }
    Some((l, r + 1))
}

/// Sample the width of Notepad's white client area on one row for `span`.
fn sample_widths(x0: i32, x1: i32, y: i32, at: usize, span: Duration, kick: impl FnOnce()) -> Vec<(f64, usize)> {
    let mut samples = Vec::new();
    let start = Instant::now();
    kick();
    while start.elapsed() < span {
        let row = capture_row(x0, x1, y);
        if let Some((l, r)) = white_run(&row, at) {
            samples.push((start.elapsed().as_secs_f64() * 1000.0, r - l));
        }
    }
    samples
}

/// Describe one measured animation: duration and progress at fractions of it.
fn report_animation(what: &str, samples: &[(f64, usize)]) {
    let (w0, w1) = (samples[0].1 as f64, samples.last().unwrap().1 as f64);
    let moving: Vec<_> = samples.iter().filter(|s| (s.1 as f64 - w0).abs() > 2.0).collect();
    let settled = samples
        .iter()
        .position(|s| (s.1 as f64 - w1).abs() <= 2.0 && s.0 > moving.first().map_or(0.0, |m| m.0));
    let (Some(first), Some(end)) = (moving.first(), settled.map(|i| samples[i])) else {
        eprintln!("{what}: no animation seen (are Windows animations on?)");
        return;
    };
    // The last sample before movement bounds the true start.
    let before = samples.iter().rfind(|s| s.0 < first.0).map_or(0.0, |s| s.0);
    let start = (before + first.0) / 2.0;
    let dur = end.0 - start;
    eprintln!(
        "{what}: width {w0} -> {w1} px; motion starts ~{start:.0} ms after the command, lasts ~{dur:.0} ms ({} samples, ~{:.1} ms apart)",
        samples.len(),
        samples.last().unwrap().0 / samples.len() as f64
    );
    eprintln!("{what}: progress at fraction of duration (measured | ease-out cubic):");
    for s in samples.iter().filter(|s| s.0 >= start && s.0 <= end.0) {
        let t = (s.0 - start) / dur;
        let p = (s.1 as f64 - w0) / (w1 - w0);
        let ease = 1.0 - (1.0 - t).powi(3);
        eprintln!("  t={t:.2}  measured={p:.2}  cubic={ease:.2}");
    }
}

#[test]
#[ignore = "measures Windows' own maximize animation; run alone, with fz-maximize stopped"]
fn measure_windows_maximize_animation() {
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_TOPMOST, SC_MAXIMIZE, SC_RESTORE, SWP_NOACTIVATE, SetWindowPos, WM_SYSCOMMAND,
    };
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    if unsafe { FindWindowW(w!("fz-maximize.hidden-window"), None) }.is_ok() {
        eprintln!("SKIP: fz-maximize is running (it would intercept the maximize); task stop first");
        return;
    }
    let mut notepad = Command::new("notepad.exe").spawn().expect("start notepad");
    let pid = notepad.id();
    let mut hwnd = None;
    assert!(wait_until(Duration::from_secs(5), || {
        hwnd = find_notepad(pid);
        hwnd.is_some()
    }));
    let h = hwnd.unwrap();
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut mi);
    }
    let work = mi.rcWork;
    let (ww, wh) = ((work.right - work.left) / 4, (work.bottom - work.top) / 4);
    let (x, y) = (
        work.left + (work.right - work.left) * 3 / 8,
        work.top + (work.bottom - work.top) * 3 / 8,
    );
    unsafe {
        let _ = SetWindowPos(h, Some(HWND_TOPMOST), x, y, ww, wh, SWP_NOACTIVATE);
    }
    force_foreground(h);
    std::thread::sleep(Duration::from_millis(800));
    let row_y = y + wh / 2;
    let at = (x + ww / 2 - work.left) as usize;
    let span = Duration::from_millis(900);
    let post = |cmd: u32| unsafe {
        let _ = PostMessageW(Some(h), WM_SYSCOMMAND, WPARAM(cmd as usize), LPARAM(0));
    };

    let max = sample_widths(work.left, work.right, row_y, at, span, || post(SC_MAXIMIZE));
    std::thread::sleep(Duration::from_millis(500));
    let restore = sample_widths(work.left, work.right, row_y, at, span, || post(SC_RESTORE));
    unsafe {
        let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
    if !wait_until(Duration::from_secs(2), || notepad.try_wait().ok().flatten().is_some()) {
        let _ = notepad.kill();
    }
    report_animation("maximize", &max);
    report_animation("restore", &restore);
}

#[test]
#[ignore = "drives the live desktop with real clicks; run with `task e2e`"]
fn animates_wherever_the_window_starts() {
    // Real windows rarely sit neatly inside a zone. These shapes come from real use: wider
    // than the zone and sticking out of both sides, and mostly in one zone but reaching
    // into the next. Each must animate (both ways), not just jump. Uses the Morph style
    // (the other animation tests use this machine's default).
    let Some((_guard, h, zones, _log)) = setup_with(
        "fz-maximize-e2e-anim-any.log",
        Some("animation_ms = 250\nanimation_style = \"morph\"\n"),
    ) else {
        return;
    };
    let mut original = POINT::default();
    unsafe { GetCursorPos(&mut original).unwrap() };
    let _mouse = MouseGuard(original);

    let (z0, z1) = (zones[0], zones[1]);
    let half = (z1.b - z1.t) / 2;
    let cases = [
        // (description, start rect x, y, w, h, expected zone)
        (
            "wider than the zone, out both sides",
            z1.l - 100,
            z1.t + 300,
            (z1.r - z1.l) + 200,
            half,
            z1,
        ),
        (
            "mostly in one zone, reaching into the next",
            z0.l + 40,
            z0.t + 200,
            (z0.r - z0.l) + 110,
            half,
            z0,
        ),
    ];
    for (what, x, y, w, hgt, zone) in cases {
        unsafe { SetWindowPos(h, None, x, y, w, hgt, SWP_NOZORDER).unwrap() };
        std::thread::sleep(Duration::from_millis(400));
        let before = window_rect(h);
        force_foreground(h);
        std::thread::sleep(Duration::from_millis(400));

        let (max_btn, _) = title_bar_buttons(h);
        let snap = trace_animation(h, &zone_window_rect(h, &zone), None, || click(h, max_btn.x, max_btn.y));
        eprintln!(
            "{what}: overlay {:?}..{:?}, reached zone at {:?}",
            snap.overlay_first_seen, snap.overlay_last_seen, snap.window_reached_target
        );
        assert_covered_animation(what, &snap);
        assert!(
            frame(h).near(&zone, 1),
            "{what}: final frame {:?} != zone {zone:?}",
            frame(h)
        );
        assert!(!snap.maximized_at_some_point, "{what}: maximized at some point (flash)");
        std::thread::sleep(Duration::from_millis(700));

        let (_, caption) = title_bar_buttons(h);
        let back = trace_animation(h, &before, None, || double_click(h, caption));
        assert_covered_animation(&format!("{what} (toggle back)"), &back);
        assert!(
            window_rect(h).near(&before, 2),
            "{what}: expected {before:?}, got {:?}",
            window_rect(h)
        );
        std::thread::sleep(Duration::from_millis(700));
    }
}

/// Record screenshots through a real animation (both styles) into %TEMP%, for looking at
/// with your own eyes, and print the frame rate the app logged.
#[test]
#[ignore = "drives the live desktop; diagnostic: writes animation frames to %TEMP%"]
fn record_animation_frames() {
    for style in ["zoom", "morph"] {
        let log_name = format!("fz-maximize-e2e-record-{style}.log");
        let config = format!("animation_style = \"{style}\"\n");
        let Some((guard, h, zones, log)) = setup_with(&log_name, Some(&config)) else {
            return;
        };
        let mut original = POINT::default();
        unsafe { GetCursorPos(&mut original).unwrap() };
        let mouse = MouseGuard(original);
        let z = zones[1];
        // Wider than the zone, sticking out of both sides (the case from real use).
        unsafe {
            SetWindowPos(
                h,
                None,
                z.l - 100,
                z.t + 300,
                (z.r - z.l) + 200,
                (z.b - z.t) / 2,
                SWP_NOZORDER,
            )
            .unwrap()
        };
        std::thread::sleep(Duration::from_millis(400));
        force_foreground(h);
        std::thread::sleep(Duration::from_millis(500));
        let region = {
            let w = window_rect(h);
            R {
                l: w.l.min(z.l) - 20,
                t: z.t,
                r: w.r.max(z.r) + 20,
                b: z.b,
            }
        };
        let dir = std::env::temp_dir().join(format!("fz-maximize-frames-{style}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (max_btn, _) = title_bar_buttons(h);
        let start = Instant::now();
        click(h, max_btn.x, max_btn.y);
        let mut n = 0;
        while start.elapsed() < Duration::from_millis(650) {
            let ms = start.elapsed().as_millis();
            capture_bmp(&region, &dir.join(format!("{n:02}-{ms:03}ms.bmp")));
            n += 1;
        }
        std::thread::sleep(Duration::from_millis(300));
        let logged = std::fs::read_to_string(&log).unwrap_or_default();
        let line = logged
            .lines()
            .rfind(|l| l.contains("animation finished"))
            .unwrap_or("(no animation logged)");
        eprintln!("{style}: {n} captures in {}; app: {line}", dir.display());
        drop(mouse);
        drop(guard);
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// (ms since the click, white run on the row, white run on the column).
type EdgeSample = (f64, Option<(usize, usize)>, Option<(usize, usize)>);

/// One screen column `[y0, y1)` at `x`, as 0x00RRGGBB pixels.
fn capture_col(x: i32, y0: i32, y1: i32) -> Vec<u32> {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HGDIOBJ, ROP_CODE, ReleaseDC, SRCCOPY, SelectObject,
    };
    let h = y1 - y0;
    let mut px = vec![0u32; h as usize];
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, 1, h);
        let old = SelectObject(mem, HGDIOBJ(bmp.0));
        let _ = BitBlt(mem, 0, 0, 1, h, Some(screen), x, y0, ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0));
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: 1,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            Some(px.as_mut_ptr().cast()),
            &mut bi,
            DIB_RGB_COLORS,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
    }
    px.iter_mut().for_each(|p| *p &= 0x00FF_FFFF);
    px
}

/// Track the window's white client area edges (left, right on one row; top, bottom on
/// one column) through a real animation, sampled every couple of milliseconds, to see
/// whether both directions move together and how evenly.
#[test]
#[ignore = "drives the live desktop; diagnostic: prints the animated edges over time"]
fn trace_animation_edges() {
    for style in ["zoom", "morph"] {
        let log_name = format!("fz-maximize-e2e-edges-{style}.log");
        let config = format!("animation_style = \"{style}\"\n");
        let Some((guard, h, zones, log)) = setup_with(&log_name, Some(&config)) else {
            return;
        };
        let mut original = POINT::default();
        unsafe { GetCursorPos(&mut original).unwrap() };
        let mouse = MouseGuard(original);
        let z = zones[1];
        unsafe {
            SetWindowPos(
                h,
                None,
                z.l - 100,
                z.t + 300,
                (z.r - z.l) + 200,
                (z.b - z.t) / 2,
                SWP_NOZORDER,
            )
            .unwrap()
        };
        std::thread::sleep(Duration::from_millis(400));
        force_foreground(h);
        std::thread::sleep(Duration::from_millis(500));
        let w = window_rect(h);
        let (cx, cy) = ((z.l + z.r) / 2, (w.t + w.b) / 2);
        let (x0, x1, y0, y1) = (w.l.min(z.l) - 50, w.r.max(z.r) + 50, z.t, z.b);
        let (max_btn, _) = title_bar_buttons(h);
        let mut samples: Vec<EdgeSample> = Vec::new();
        let start = Instant::now();
        click(h, max_btn.x, max_btn.y);
        while start.elapsed() < Duration::from_millis(700) {
            let row = white_run(&capture_row(x0, x1, cy), (cx - x0) as usize);
            let col = white_run(&capture_col(cx, y0, y1), (cy - y0) as usize);
            samples.push((start.elapsed().as_secs_f64() * 1000.0, row, col));
        }
        std::thread::sleep(Duration::from_millis(300));
        let logged = std::fs::read_to_string(&log).unwrap_or_default();
        let line = logged
            .lines()
            .rfind(|l| l.contains("animation finished"))
            .unwrap_or("(none)");
        eprintln!(
            "== {style}: {} samples; app: {}",
            samples.len(),
            line.split("DEBUG").nth(1).unwrap_or(line)
        );
        eprintln!("   ms    left  right | top  bottom   (screen px; '-' = not white there)");
        let fmt = |v: Option<(usize, usize)>, o: i32| {
            v.map_or("    -      -".to_string(), |(a, b)| {
                format!("{:5}  {:5}", a as i32 + o, b as i32 + o)
            })
        };
        let mut last = None;
        for (ms, row, col) in &samples {
            if last != Some((*row, *col)) {
                eprintln!("{ms:6.1}  {} | {}", fmt(*row, x0), fmt(*col, y0));
                last = Some((*row, *col));
            }
        }
        drop(mouse);
        drop(guard);
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// How long a screen capture of a window-sized area takes, with and without CAPTUREBLT.
#[test]
#[ignore = "diagnostic: benchmarks screen capture"]
fn bench_screen_capture() {
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CAPTUREBLT, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, HGDIOBJ,
        ROP_CODE, ReleaseDC, SRCCOPY, SelectObject,
    };
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    for (w, h) in [(2220, 2300), (1934, 1150), (600, 600)] {
        for (name, rop) in [
            ("SRCCOPY", SRCCOPY),
            ("SRCCOPY|CAPTUREBLT", ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0)),
        ] {
            let mut times = Vec::new();
            for _ in 0..5 {
                unsafe {
                    let screen = GetDC(None);
                    let mem = CreateCompatibleDC(Some(screen));
                    let bmp = CreateCompatibleBitmap(screen, w, h);
                    let old = SelectObject(mem, HGDIOBJ(bmp.0));
                    let t = Instant::now();
                    let _ = BitBlt(mem, 0, 0, w, h, Some(screen), 100, 0, rop);
                    times.push(t.elapsed().as_secs_f64() * 1000.0);
                    SelectObject(mem, old);
                    let _ = DeleteObject(HGDIOBJ(bmp.0));
                    let _ = DeleteDC(mem);
                    ReleaseDC(None, screen);
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            eprintln!(
                "{w}x{h} {name}: {:?} ms",
                times.iter().map(|t| t.round() as i64).collect::<Vec<_>>()
            );
        }
    }
}
