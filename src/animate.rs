//! Maximize animation for intercepted gestures (setting `animate_maximize`).
//!
//! The intercepted path moves a window straight into its zone, so Windows has nothing to
//! animate. We draw the animation the way Windows' compositor does: with live DWM
//! thumbnails (the documented API behind taskbar and Alt-Tab previews), scaled and faded
//! on the GPU. Each frame is only a couple of property updates, paced to the display's
//! refresh with `DwmFlush`, so the animation runs at full frame rate.
//!
//! Two styles (setting `animation_style`, `auto` picks the one matching the running
//! Windows):
//!
//! * **Zoom** (Windows 10): the window's new content appears at its new position at about
//!   95% size (110% when restoring to a smaller size) and settles to 100% while fading
//!   in. Measured from Windows 10 22H2's own maximize (`measure_windows_maximize_animation`
//!   in tests/e2e.rs).
//! * **Morph** (Windows 11): the window's rectangle moves from the old rect to the new one,
//!   both directions at once, crossfading from the old content to the new.
//!
//! Both only scale the content slightly or briefly, and the app has already laid itself
//! out at its new size: the window really is moved first, underneath the overlay, so
//! what fades in is its real new content.
//!
//! How the move stays hidden: a click-through overlay just above the window covers both
//! rects. Before the window moves, it shows a snapshot of the screen, so nothing visibly
//! changes. The old window's pixels go into a cloaked helper window, which the overlay
//! shows as a thumbnail ("old"); the real window is shown as a second thumbnail ("new").
//! The overlay paints the snapshot (the desktop as it was) where the real window now is
//! and the old one wasn't, and is transparent elsewhere, so the desktop the old window
//! uncovered shows as it really is.
//!
//! Runs on its own thread so the main thread keeps handling events. A new request
//! finishes the current animation immediately.

use std::cell::RefCell;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_RECTSOURCE,
    DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE, DWMWA_CLOAK, DwmFlush, DwmQueryThumbnailSourceSize,
    DwmRegisterThumbnail, DwmSetWindowAttribute, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CAPTUREBLT, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, FillRect, GetDC, HBITMAP, HDC, HGDIOBJ, InvalidateRect, PAINTSTRUCT, ROP_CODE, ReleaseDC,
    SRCCOPY, SelectObject, UpdateWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ANIMATIONINFO, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GW_HWNDPREV, GWL_EXSTYLE,
    GetClientRect, GetMessageW, GetWindow, GetWindowLongPtrW, HTTRANSPARENT, HWND_TOP, HWND_TOPMOST, IsWindow,
    LWA_ALPHA, LWA_COLORKEY, MSG, PM_NOREMOVE, PM_REMOVE, PeekMessageW, PostThreadMessageW, RegisterClassExW,
    SPI_GETANIMATION, SPI_GETCLIENTAREAANIMATION, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetTimer, SetWindowPos, ShowWindow,
    SystemParametersInfoW, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_ERASEBKGND, WM_NCHITTEST, WM_PAINT, WM_QUIT,
    WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};
use windows::core::{BOOL, HSTRING, PCWSTR, w};

use crate::engine::Hwnd;
use crate::geometry::{Margins, Rect};
use crate::win::{hwnd_from, rect_from, rect_to};
use crate::window;

pub const OVERLAY_CLASS: PCWSTR = w!("fz-maximize.animation");
const WM_APP_ANIMATE: u32 = WM_APP + 10;
/// Colour key: overlay pixels of this colour are fully transparent.
const KEY: COLORREF = COLORREF(0x00FF_00FF);
/// After the last frame, how long to wait for the real window to be in place.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------------------
// Pure animation math (unit tested)
// ---------------------------------------------------------------------------------------

/// The animation's look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// Windows 10: new content zooms slightly into place at its new position, fading in.
    Zoom,
    /// Windows 11: the rectangle moves from the old rect to the new one, crossfading.
    Morph,
}

impl Style {
    /// The style of the running Windows: Windows 11 is build 22000 and later.
    pub fn of_this_windows() -> Self {
        match windows_build() {
            Some(b) if b >= 22000 => Style::Morph,
            _ => Style::Zoom,
        }
    }
}

/// Windows' build number (e.g. 19045 for Windows 10 22H2, 22631 for Windows 11 23H2).
pub fn windows_build() -> Option<u32> {
    let key = HSTRING::from(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let value = HSTRING::from("CurrentBuildNumber");
    let mut buf = [0u16; 32];
    let mut size = std::mem::size_of_val(&buf) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if rc.is_err() {
        return None;
    }
    crate::win::from_wide(&buf[..size as usize / 2]).trim().parse().ok()
}

/// Ease-out quadratic: a smooth deceleration that keeps every frame moving.
pub fn ease_out(t: f64) -> f64 {
    let u = 1.0 - t.clamp(0.0, 1.0);
    1.0 - u * u
}

/// 0 before `start`, 1 after `end`, smooth in between.
pub fn fade(t: f64, start: f64, end: f64) -> f64 {
    let x = ((t - start) / (end - start)).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

pub fn lerp_rect(a: &Rect, b: &Rect, t: f64) -> Rect {
    let l = |x: i32, y: i32| (f64::from(x) + (f64::from(y) - f64::from(x)) * t + 0.5).floor() as i32;
    Rect::new(
        l(a.left, b.left),
        l(a.top, b.top),
        l(a.right, b.right),
        l(a.bottom, b.bottom),
    )
}

/// `r` scaled by `s` about its centre.
pub fn scale_about_centre(r: &Rect, s: f64) -> Rect {
    let (cx, cy) = (f64::from(r.left + r.right) / 2.0, f64::from(r.top + r.bottom) / 2.0);
    let (hw, hh) = (f64::from(r.width()) * s / 2.0, f64::from(r.height()) * s / 2.0);
    let i = |v: f64| (v + 0.5).floor() as i32;
    Rect::new(i(cx - hw), i(cy - hh), i(cx + hw), i(cy + hh))
}

pub fn contains(outer: &Rect, inner: &Rect) -> bool {
    outer.left <= inner.left && outer.top <= inner.top && outer.right >= inner.right && outer.bottom >= inner.bottom
}

pub fn union(a: &Rect, b: &Rect) -> Rect {
    Rect::new(
        a.left.min(b.left),
        a.top.min(b.top),
        a.right.max(b.right),
        a.bottom.max(b.bottom),
    )
}

/// The part of a thumbnail source that is the window's visible frame. Source coordinates
/// are window-relative (they start at the invisible resize border), but DWM may report
/// the source size either as the whole window (`has_borders`) or as just the frame
/// (`w` x `h`). Measured on Windows 10 22H2: frame-sized, still border-offset.
pub fn frame_source(w: i32, h: i32, has_borders: bool, m: &Margins) -> Rect {
    if has_borders {
        Rect::new(m.left, m.top, w - m.right, h - m.bottom)
    } else {
        Rect::new(m.left, m.top, m.left + w, m.top + h)
    }
}

/// One frame: where the old and new images are drawn and how opaque they are (0-255).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub old: Rect,
    pub old_opacity: u8,
    pub new: Rect,
    pub new_opacity: u8,
    pub done: bool,
}

fn opacity(x: f64) -> u8 {
    (x.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Where Zoom starts the new image, as a fraction of its final size: 95% when the window
/// grows into a rect that contains it (Windows 10's maximize), 110% when it shrinks
/// (Windows 10's restore). Otherwise 100%: the new image must cover wherever old and new
/// overlap, or the real window would show around it.
pub fn zoom_start_scale(from: &Rect, to: &Rect) -> f64 {
    if contains(to, from) {
        0.95
    } else if to.area() < from.area() {
        1.10
    } else {
        1.0
    }
}

/// The frame at `elapsed` into an animation of `duration` from `from` to `to` (visible
/// frame rects). Opacities: the new image is drawn over the old one.
pub fn frame(style: Style, from: &Rect, to: &Rect, elapsed: Duration, duration: Duration) -> Frame {
    let t = if duration.is_zero() {
        1.0
    } else {
        (elapsed.as_secs_f64() / duration.as_secs_f64()).min(1.0)
    };
    let e = ease_out(t);
    let done = t >= 1.0;
    match style {
        Style::Morph => {
            // Both images on the same moving rect; the old one stays opaque underneath, so
            // nothing behind shows through mid-crossfade.
            let r = lerp_rect(from, to, e);
            Frame {
                old: r,
                old_opacity: if done { 0 } else { 255 },
                new: r,
                new_opacity: opacity(fade(t, 0.1, 0.6)),
                done,
            }
        }
        Style::Zoom => {
            let s0 = zoom_start_scale(from, to);
            // Old image: when the new one ends up covering it, keep it opaque underneath.
            // Otherwise fade it out once the new image is opaque (until then it hides the
            // real window, which is already in place beneath the transparent overlay).
            let old_opacity = if contains(to, from) && !done {
                255
            } else {
                opacity(1.0 - fade(t, 0.25, 0.5))
            };
            Frame {
                old: *from,
                old_opacity,
                new: if done {
                    *to
                } else {
                    scale_about_centre(to, s0 + (1.0 - s0) * e)
                },
                new_opacity: opacity(fade(t, 0.0, 0.25)),
                done,
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Animation thread
// ---------------------------------------------------------------------------------------

/// True when Windows would animate a maximize itself, i.e. **both** of its switches are
/// on: "Animation effects" (Settings → Accessibility → Visual effects) and "Animate
/// windows when minimizing and maximizing" (System Properties → Performance Options).
/// Read on every gesture, so changes apply immediately.
pub fn system_animations_enabled() -> bool {
    let mut client_area = BOOL(1);
    let client_ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut client_area as *mut BOOL).cast()),
            Default::default(),
        )
    }
    .is_ok();
    let mut info = ANIMATIONINFO {
        cbSize: std::mem::size_of::<ANIMATIONINFO>() as u32,
        iMinAnimate: 1,
    };
    let min_max_ok = unsafe {
        SystemParametersInfoW(
            SPI_GETANIMATION,
            info.cbSize,
            Some((&mut info as *mut ANIMATIONINFO).cast()),
            Default::default(),
        )
    }
    .is_ok();
    (!client_ok || client_area.as_bool()) && (!min_max_ok || info.iMinAnimate != 0)
}

/// Whether there's anything to animate between two window rects.
pub fn should_animate(from: &Rect, to: &Rect) -> bool {
    from != to && !from.is_empty() && !to.is_empty()
}

/// One animated move of `target` from `from` to `to` (window rects, screen coords). The
/// animation thread moves the window itself, at the right moment.
#[derive(Clone, Copy, Debug)]
pub struct Request {
    pub target: Hwnd,
    pub from: Rect,
    pub to: Rect,
    pub style: Style,
    pub duration: Duration,
    pub all_monitors_same_dpi: bool,
}

impl Request {
    fn place(&self) {
        if let Err(e) = window::restore_to(hwnd_from(self.target), self.to, self.all_monitors_same_dpi) {
            tracing::debug!(error = %e, "animated move failed");
        }
    }
}

pub struct Animator {
    thread_id: u32,
    handle: Option<JoinHandle<()>>,
}

impl Animator {
    pub fn start() -> Option<Self> {
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("fzm-animation".into())
            .spawn(move || {
                // Make sure the thread has a message queue before reporting its id.
                let mut msg = MSG::default();
                unsafe {
                    let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                    // Frames are tiny but time-critical: don't let other work delay them.
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
                }
                let _ = tx.send(unsafe { GetCurrentThreadId() });
                thread_main();
                // Thread is ending: release the cached windows.
                WINDOWS.with_borrow_mut(|w| {
                    if let Some(w) = w.take() {
                        unsafe {
                            let _ = DwmUnregisterThumbnail(w.old_thumb);
                            let _ = DestroyWindow(w.overlay);
                            let _ = DestroyWindow(w.source);
                        }
                    }
                });
            })
            .ok()?;
        let thread_id = rx.recv().ok()?;
        Some(Self {
            thread_id,
            handle: Some(handle),
        })
    }

    /// Queue an animated move (finishes any running animation immediately).
    pub fn animate(&self, req: Request) {
        let raw = Box::into_raw(Box::new(req));
        if unsafe { PostThreadMessageW(self.thread_id, WM_APP_ANIMATE, WPARAM(0), LPARAM(raw as isize)) }.is_err() {
            // SAFETY: the message was not posted, so we still own the box.
            unsafe { Box::from_raw(raw) }.place();
        }
    }
}

impl Drop for Animator {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// A bitmap in a memory DC.
struct Canvas {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
}

impl Canvas {
    fn new(width: i32, height: i32) -> Option<Self> {
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return None;
            }
            let dc = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, width, height);
            ReleaseDC(None, screen);
            if dc.is_invalid() || bitmap.is_invalid() {
                return None;
            }
            let old = SelectObject(dc, HGDIOBJ(bitmap.0));
            Some(Self { dc, bitmap, old })
        }
    }

    /// A copy of the screen inside `area`.
    fn capture(area: &Rect) -> Option<Self> {
        let canvas = Self::new(area.width(), area.height())?;
        unsafe {
            let screen = GetDC(None);
            let copied = BitBlt(
                canvas.dc,
                0,
                0,
                area.width(),
                area.height(),
                Some(screen),
                area.left,
                area.top,
                ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
            );
            ReleaseDC(None, screen);
            copied.is_ok().then_some(canvas)
        }
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

fn fill(hdc: HDC, r: &Rect, colour: COLORREF) {
    if r.is_empty() {
        return;
    }
    unsafe {
        let brush = CreateSolidBrush(colour);
        FillRect(hdc, &rect_to(*r), brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }
}

fn blit(dst: HDC, d: &Rect, src: HDC, sx: i32, sy: i32) {
    if !d.is_empty() {
        unsafe {
            let _ = BitBlt(dst, d.left, d.top, d.width(), d.height(), Some(src), sx, sy, SRCCOPY);
        }
    }
}

/// The two static pictures, composed once (the animation itself is all thumbnails).
struct Scene {
    /// The overlay's background: the desktop as it was where the real window now is,
    /// transparent (colour key) elsewhere.
    background: Canvas,
    /// The old window's visible frame, for the cloaked helper window.
    old_window: Canvas,
    source: HWND,
}

thread_local! {
    static SCENE: RefCell<Option<Scene>> = const { RefCell::new(None) };
}

unsafe extern "system" fn picture_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // Never take clicks (WS_EX_TRANSPARENT already makes it click-through).
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            unsafe {
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                let client = rect_from(client);
                SCENE.with_borrow(|scene| match scene {
                    Some(s) if s.source == hwnd => blit(hdc, &client, s.old_window.dc, 0, 0),
                    Some(s) => blit(hdc, &client, s.background.dc, 0, 0),
                    None => fill(hdc, &client, KEY),
                });
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn register_class() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(picture_proc),
            hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
            lpszClassName: OVERLAY_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc) != 0
    })
}

fn create_popup(area: &Rect, ex: WINDOW_EX_STYLE) -> Option<HWND> {
    let hinstance = unsafe { GetModuleHandleW(None) }.ok().map(|m| m.into());
    unsafe {
        CreateWindowExW(
            ex | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED,
            OVERLAY_CLASS,
            PCWSTR::null(),
            WS_POPUP,
            area.left,
            area.top,
            area.width(),
            area.height(),
            None,
            None,
            hinstance,
            None,
        )
    }
    .inspect_err(|e| tracing::debug!(error = %e, "animation window unavailable"))
    .ok()
}

fn thread_main() {
    // A thread timer to let go of the cached windows when animations stop.
    unsafe {
        SetTimer(None, 0, 10_000, None);
    }
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        if msg.message == WM_TIMER && msg.hwnd.is_invalid() {
            drop_idle_windows();
            continue;
        }
        if msg.message == WM_APP_ANIMATE {
            // SAFETY: posted by `Animator::animate` with a leaked Box<Request>.
            let req = unsafe { Box::from_raw(msg.lParam.0 as *mut Request) };
            run(*req);
            continue;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Dispatch pending messages; true when a newer request (or quit) is waiting.
fn pump_and_check_new_request() -> bool {
    let mut msg = MSG::default();
    // Leave animation requests queued; they're handled after the current one ends.
    if unsafe { PeekMessageW(&mut msg, None, WM_APP_ANIMATE, WM_APP_ANIMATE, PM_NOREMOVE) }.as_bool() {
        return true;
    }
    while unsafe { PeekMessageW(&mut msg, None, 0, WM_APP_ANIMATE - 1, PM_REMOVE) }.as_bool() {
        if msg.message == WM_QUIT {
            return true;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    false
}

fn flush() {
    unsafe {
        let _ = DwmFlush();
    }
}

/// Wait (one frame at a time) until the window has arrived at `to`.
fn wait_for_arrival(target: HWND, to: &Rect) {
    let start = Instant::now();
    while start.elapsed() < SETTLE_TIMEOUT {
        if window::window_rect(target).is_some_and(|r| r.approx_eq(to, 1)) {
            break;
        }
        flush();
    }
}

/// Run one animated move to completion (or until a newer request arrives). Whatever
/// happens, the window ends up at `req.to`.
fn run(req: Request) {
    let target = hwnd_from(req.target);
    if !should_animate(&req.from, &req.to)
        || !unsafe { IsWindow(Some(target)) }.as_bool()
        || !register_class()
        || !animate_move(target, &req)
    {
        // Nothing to animate, or couldn't set up: still move the window.
        req.place();
    }
    SCENE.with_borrow_mut(|s| *s = None);
}

fn set_thumb(thumb: isize, dest: &Rect, src: &Rect, opacity: u8) {
    let props = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION
            | DWM_TNP_RECTSOURCE
            | DWM_TNP_VISIBLE
            | DWM_TNP_OPACITY
            | DWM_TNP_SOURCECLIENTAREAONLY,
        rcDestination: rect_to(*dest),
        rcSource: rect_to(*src),
        opacity,
        fVisible: BOOL::from(opacity > 0 && !dest.is_empty() && !src.is_empty()),
        fSourceClientAreaOnly: BOOL(0),
    };
    unsafe {
        let _ = DwmUpdateThumbnailProperties(thumb, &props);
    }
}

/// The overlay and the cloaked helper window, kept between animations (creating two
/// big windows costs tens of milliseconds) and destroyed after a while unused.
struct Windows {
    overlay: HWND,
    source: HWND,
    old_thumb: isize,
    last_used: Instant,
}

/// Destroy the cached windows after this long without an animation.
const KEEP_WINDOWS: Duration = Duration::from_secs(30);

thread_local! {
    static WINDOWS: RefCell<Option<Windows>> = const { RefCell::new(None) };
}

fn cached_windows() -> Option<(HWND, HWND, isize)> {
    WINDOWS.with_borrow_mut(|w| {
        if let Some(w) = w.as_mut() {
            w.last_used = Instant::now();
            return Some((w.overlay, w.source, w.old_thumb));
        }
        let tiny = Rect::new(0, 0, 1, 1);
        let source = create_popup(&tiny, WINDOW_EX_STYLE(0))?;
        let Some(overlay) = create_popup(&tiny, WINDOW_EX_STYLE(0)) else {
            unsafe {
                let _ = DestroyWindow(source);
            }
            return None;
        };
        unsafe {
            let cloak = BOOL(1);
            let _ = DwmSetWindowAttribute(
                source,
                DWMWA_CLOAK,
                (&cloak as *const BOOL).cast(),
                std::mem::size_of::<BOOL>() as u32,
            );
            let _ = SetLayeredWindowAttributes(source, COLORREF(0), 255, LWA_ALPHA);
            let _ = ShowWindow(source, SW_SHOWNOACTIVATE);
            let _ = SetLayeredWindowAttributes(overlay, KEY, 0, LWA_COLORKEY);
        }
        let Ok(old_thumb) = (unsafe { DwmRegisterThumbnail(overlay, source) }) else {
            unsafe {
                let _ = DestroyWindow(overlay);
                let _ = DestroyWindow(source);
            }
            return None;
        };
        *w = Some(Windows {
            overlay,
            source,
            old_thumb,
            last_used: Instant::now(),
        });
        Some((overlay, source, old_thumb))
    })
}

/// Called on the thread's idle timer.
fn drop_idle_windows() {
    WINDOWS.with_borrow_mut(|w| {
        if w.as_ref().is_some_and(|w| w.last_used.elapsed() >= KEEP_WINDOWS) {
            let w = w.take().unwrap();
            unsafe {
                let _ = DwmUnregisterThumbnail(w.old_thumb);
                let _ = DestroyWindow(w.overlay);
                let _ = DestroyWindow(w.source);
            }
        }
    });
}

fn animate_move(target: HWND, req: &Request) -> bool {
    // Animate the visible frames, not the invisible resize borders around them.
    let margins = window::frame_rect(target)
        .map(|f| Margins::between(req.from, f))
        .filter(Margins::is_plausible)
        .unwrap_or_default();
    let (from, to) = (req.from.shrink(margins), req.to.shrink(margins));
    if from.is_empty() || to.is_empty() {
        return false;
    }
    let setup_start = Instant::now();
    let area = union(&req.from, &req.to);
    let rel = |r: &Rect| r.offset(-area.left, -area.top);
    let (from_r, to_win_r) = (rel(&from), rel(&req.to));

    // 1. The static pictures, from one capture of the screen: the old window's frame,
    //    then, in place, the overlay's background (the desktop as it was where the real
    //    window will be, transparent elsewhere).
    let Some(background) = Canvas::capture(&area) else {
        return false;
    };
    let capture_ms = setup_start.elapsed().as_millis() as u64;
    let Some(old_window) = Canvas::new(from.width(), from.height()) else {
        return false;
    };
    let old_src = Rect::new(0, 0, from.width(), from.height());
    blit(old_window.dc, &old_src, background.dc, from_r.left, from_r.top);
    let full = Rect::new(0, 0, area.width(), area.height());
    for outside in [
        Rect::new(0, 0, full.right, to_win_r.top),
        Rect::new(0, to_win_r.bottom, full.right, full.bottom),
        Rect::new(0, to_win_r.top, to_win_r.left, to_win_r.bottom),
        Rect::new(to_win_r.right, to_win_r.top, full.right, to_win_r.bottom),
        from_r,
    ] {
        fill(background.dc, &outside, KEY);
    }
    let compose_ms = setup_start.elapsed().as_millis() as u64;

    // 2. Position the (cached) helper and overlay windows; the overlay directly above the
    //    target in z-order.
    let Some((overlay, source, old_thumb)) = cached_windows() else {
        return false;
    };
    SCENE.with_borrow_mut(|s| {
        *s = Some(Scene {
            background,
            old_window,
            source,
        })
    });
    let Ok(new_thumb) = (unsafe { DwmRegisterThumbnail(overlay, target) }) else {
        return false;
    };
    let topmost = (unsafe { GetWindowLongPtrW(target, GWL_EXSTYLE) } as u32) & WS_EX_TOPMOST.0 != 0;
    set_thumb(old_thumb, &from_r, &old_src, 255);
    set_thumb(new_thumb, &to_win_r, &Rect::default(), 0);
    unsafe {
        let _ = SetWindowPos(
            source,
            None,
            from.left,
            from.top,
            from.width(),
            from.height(),
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        let _ = InvalidateRect(Some(source), None, false);
        let _ = UpdateWindow(source);
        if topmost {
            let _ = SetWindowPos(
                overlay,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        let above = GetWindow(target, GW_HWNDPREV)
            .ok()
            .filter(|h| !h.is_invalid() && *h != overlay);
        let insert_after = above.unwrap_or(if topmost { HWND_TOPMOST } else { HWND_TOP });
        let _ = SetWindowPos(
            overlay,
            Some(insert_after),
            area.left,
            area.top,
            area.width(),
            area.height(),
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let _ = InvalidateRect(Some(overlay), None, false);
        let _ = UpdateWindow(overlay);
    }
    let shown_ms = setup_start.elapsed().as_millis() as u64;
    // The overlay now shows exactly what was on screen.
    flush();
    let flushed_ms = setup_start.elapsed().as_millis() as u64;

    // Does DWM give us the window with its invisible borders, or just the frame? Ask now,
    // while the window still has its old, known size.
    let initial = unsafe { DwmQueryThumbnailSourceSize(new_thumb) }
        .map(|s| (s.cx, s.cy))
        .ok();
    let source_has_borders = initial.is_some_and(|(w, _)| w >= req.from.width() - 1);

    // 3. Move the real window underneath; animate.
    req.place();
    let placed_ms = setup_start.elapsed().as_millis() as u64;
    // The new image: the visible frame part of the real window (see `frame_source`).
    let new_src = || {
        let (w, h) = unsafe { DwmQueryThumbnailSourceSize(new_thumb) }
            .map(|s| (s.cx, s.cy))
            .unwrap_or((req.to.width(), req.to.height()));
        frame_source(w, h, source_has_borders, &margins)
    };
    let setup_ms = setup_start.elapsed().as_millis() as u64;
    let start = Instant::now();
    let mut frames = 0u32;
    loop {
        let f = frame(req.style, &from, &to, start.elapsed(), req.duration);
        set_thumb(old_thumb, &rel(&f.old), &old_src, f.old_opacity);
        set_thumb(new_thumb, &rel(&f.new), &new_src(), f.new_opacity);
        flush();
        frames += 1;
        if f.done || pump_and_check_new_request() {
            break;
        }
    }
    let elapsed = start.elapsed();

    // 4. Final frame: the new image exactly over the real window; then reveal it.
    set_thumb(old_thumb, &rel(&from), &old_src, 0);
    set_thumb(new_thumb, &rel(&to), &new_src(), 255);
    wait_for_arrival(target, &req.to);
    flush();
    unsafe {
        let _ = DwmUnregisterThumbnail(new_thumb);
        let _ = ShowWindow(overlay, SW_HIDE);
    }
    set_thumb(old_thumb, &from_r, &old_src, 0);
    tracing::debug!(
        style = ?req.style,
        frames,
        ms = elapsed.as_millis() as u64,
        fps = (f64::from(frames) / elapsed.as_secs_f64().max(0.001)).round(),
        setup_ms,
        capture_ms,
        compose_ms,
        shown_ms,
        flushed_ms,
        placed_ms,
        source_has_borders,
        "animation finished"
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: Duration = Duration::from_millis(200);

    fn frames(style: Style, from: &Rect, to: &Rect) -> Vec<Frame> {
        (0..=200)
            .map(|ms| frame(style, from, to, Duration::from_millis(ms), D))
            .collect()
    }

    // Shapes from real use (window rects' visible frames, zone 1920x2300).
    const ZONE: Rect = Rect::new(960, 0, 2880, 2300);
    const INSIDE: Rect = Rect::new(1000, 60, 2280, 1210);
    const WIDE: Rect = Rect::new(825, 471, 2951, 2058); // wider + shorter than the zone
    const ELSEWHERE: Rect = Rect::new(2900, 300, 3800, 1200); // in another zone

    #[test]
    fn easing_and_fades() {
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
        assert_eq!(ease_out(2.0), 1.0);
        let mut prev = 0.0;
        for i in 1..=100 {
            let v = ease_out(f64::from(i) / 100.0);
            assert!(v > prev, "must keep moving every step");
            prev = v;
        }
        assert_eq!(fade(0.0, 0.1, 0.6), 0.0);
        assert_eq!(fade(0.7, 0.1, 0.6), 1.0);
        assert!((fade(0.35, 0.1, 0.6) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn windows_version_picks_the_style() {
        // Whatever this machine is, the build must be readable and map to a style.
        let build = windows_build().expect("CurrentBuildNumber");
        assert!(build >= 10240, "Windows 10 or later, got {build}");
        let expected = if build >= 22000 { Style::Morph } else { Style::Zoom };
        assert_eq!(Style::of_this_windows(), expected);
    }

    #[test]
    fn both_styles_start_on_the_old_screen_and_end_on_the_new_window() {
        for style in [Style::Zoom, Style::Morph] {
            for (from, to) in [
                (INSIDE, ZONE),
                (ZONE, INSIDE),
                (WIDE, ZONE),
                (ZONE, WIDE),
                (ELSEWHERE, ZONE),
            ] {
                let first = frame(style, &from, &to, Duration::ZERO, D);
                assert_eq!(
                    (first.old, first.old_opacity),
                    (from, 255),
                    "{style:?}: old window as it was"
                );
                assert_eq!(first.new_opacity, 0, "{style:?}: new image not visible yet");
                let last = frame(style, &from, &to, D, D);
                assert!(last.done);
                assert_eq!(
                    (last.new, last.new_opacity, last.old_opacity),
                    (to, 255, 0),
                    "{style:?}"
                );
            }
        }
    }

    #[test]
    fn morph_resizes_in_both_directions_at_once() {
        // Both directions change on every displayed frame (60 Hz steps), never one
        // direction and then the other.
        let steps: Vec<Frame> = (0..=176)
            .step_by(16)
            .map(|ms| frame(Style::Morph, &WIDE, &ZONE, Duration::from_millis(ms), D))
            .collect();
        for pair in steps.windows(2) {
            let (a, b) = (pair[0].new, pair[1].new);
            let w_moves = a.width() != b.width() || a.left != b.left;
            let h_moves = a.height() != b.height() || a.top != b.top;
            assert!(w_moves && h_moves, "{a} -> {b}");
        }
        let fs = frames(Style::Morph, &WIDE, &ZONE);
        // Old and new share the rect (a true crossfade); the old one stays opaque under it.
        for f in &fs[..fs.len() - 1] {
            assert_eq!(f.old, f.new);
            assert_eq!(f.old_opacity, 255);
        }
    }

    #[test]
    fn morph_frames_cover_where_old_and_new_overlap() {
        // Where both windows are, the overlay has no background: a frame must cover it.
        for (from, to) in [(WIDE, ZONE), (ZONE, WIDE), (INSIDE, ZONE), (ZONE, INSIDE)] {
            let overlap = from.intersection(&to);
            for f in frames(Style::Morph, &from, &to) {
                assert!(contains(&f.new, &overlap), "{} doesn't cover {overlap}", f.new);
            }
        }
    }

    #[test]
    fn zoom_matches_windows_10() {
        // Maximize: from ~95% to 100%, centred on the target.
        let fs = frames(Style::Zoom, &INSIDE, &ZONE);
        let start = fs[0].new;
        assert!((f64::from(start.width()) / f64::from(ZONE.width()) - 0.95).abs() < 0.01);
        assert_eq!(start.center(), ZONE.center());
        // Restore to a smaller rect: from ~110%.
        let back = frame(Style::Zoom, &ZONE, &INSIDE, Duration::ZERO, D).new;
        assert!((f64::from(back.width()) / f64::from(INSIDE.width()) - 1.10).abs() < 0.01);
        // Grows steadily and fades in quickly.
        for pair in fs.windows(2) {
            assert!(pair[1].new.width() >= pair[0].new.width());
            assert!(pair[1].new_opacity >= pair[0].new_opacity);
        }
        assert_eq!(fs[60].new_opacity, 255, "fully faded in by 30%");
    }

    #[test]
    fn zoom_never_leaves_the_real_window_peeking_out() {
        // Where the old and new rects overlap, the overlay is transparent, so the image(s)
        // drawn there must hide the real window: either the new image covers the overlap
        // or the old one is still opaque over it.
        for (from, to) in [
            (INSIDE, ZONE),
            (ZONE, INSIDE),
            (WIDE, ZONE),
            (ZONE, WIDE),
            (ELSEWHERE, ZONE),
        ] {
            let overlap = from.intersection(&to);
            if overlap.is_empty() {
                continue;
            }
            for f in frames(Style::Zoom, &from, &to) {
                let new_covers = contains(&f.new, &overlap) && f.new_opacity == 255;
                let old_covers = contains(&f.old, &overlap) && f.old_opacity == 255;
                assert!(
                    new_covers || old_covers,
                    "{from} -> {to}: {f:?} leaves the real window visible"
                );
            }
        }
    }

    #[test]
    fn rect_helpers() {
        let a = Rect::new(100, 100, 500, 400);
        let b = Rect::new(0, 0, 1000, 1000);
        assert_eq!(lerp_rect(&a, &b, 0.0), a);
        assert_eq!(lerp_rect(&a, &b, 1.0), b);
        assert_eq!(lerp_rect(&a, &b, 0.5), Rect::new(50, 50, 750, 700));
        assert_eq!(scale_about_centre(&b, 1.0), b);
        assert_eq!(scale_about_centre(&b, 0.5), Rect::new(250, 250, 750, 750));
        assert_eq!(union(&a, &Rect::new(450, -5, 600, 50)), Rect::new(100, -5, 600, 400));
        assert!(should_animate(&a, &b) && !should_animate(&a, &a) && !should_animate(&Rect::default(), &b));
    }
}
