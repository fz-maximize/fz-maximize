//! Maximize animation for intercepted gestures (setting `animate_maximize`).
//!
//! The intercepted path moves a window straight into its zone, so Windows has nothing to
//! animate. We draw the animation ourselves in a click-through overlay just above the
//! window, and **never scale the window's content**. Scaling squashes or stretches text
//! whenever the zone's shape differs from the window's. Instead the content is shown at
//! its real size in four corner pieces pinned to the corners of the animated rectangle.
//! That looks like a live resize: the title bar's icon and buttons, and the borders, stay
//! at the edges, and text stays crisp.
//!
//! * **Growing** (the zone contains the window): snapshot the screen under the zone, show
//!   it in the overlay (nothing visibly changes), move the real window into the zone
//!   underneath, then grow four live DWM thumbnails of its *new* content (the documented
//!   API behind taskbar and Alt-Tab previews) from the old rect to the zone.
//! * **Shrinking** (the window contains the target, e.g. the toggle back): snapshot the
//!   window, show the snapshot, move the real window, then shrink the snapshot's corners
//!   into the target. Outside the shrinking rect the overlay is see-through, so the
//!   desktop is revealed as it really is.
//! * Anything else (moving between zones): no animation, just the move.
//!
//! Runs on its own thread (frames paced with `DwmFlush`) so the main thread keeps
//! handling events. A new request finishes the current animation immediately. Disabled
//! when Windows' "Animation effects" setting is off.

use std::cell::RefCell;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_RECTSOURCE,
    DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE, DwmFlush, DwmQueryThumbnailSourceSize, DwmRegisterThumbnail,
    DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CAPTUREBLT, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, EndPaint, FillRect, GetDC, HBITMAP, HDC, HGDIOBJ, InvalidateRect, PAINTSTRUCT, ROP_CODE, ReleaseDC,
    SRCCOPY, SelectObject, UpdateWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GW_HWNDPREV, GWL_EXSTYLE, GetMessageW, GetWindow,
    GetWindowLongPtrW, HTTRANSPARENT, HWND_TOP, HWND_TOPMOST, IsWindow, LWA_ALPHA, LWA_COLORKEY, MSG, PM_NOREMOVE,
    PM_REMOVE, PeekMessageW, PostThreadMessageW, RegisterClassExW, SPI_GETCLIENTAREAANIMATION, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetLayeredWindowAttributes, SetWindowPos, ShowWindow,
    SystemParametersInfoW, TranslateMessage, WM_APP, WM_ERASEBKGND, WM_NCHITTEST, WM_PAINT, WM_QUIT, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{BOOL, PCWSTR, w};

use crate::engine::Hwnd;
use crate::geometry::Rect;
use crate::win::{hwnd_from, rect_to};
use crate::window;

pub const OVERLAY_CLASS: PCWSTR = w!("fz-maximize.animation");
const WM_APP_ANIMATE: u32 = WM_APP + 10;
/// Colour key: overlay pixels of this colour are fully transparent (shrinking only).
const KEY: COLORREF = COLORREF(0x00FF_00FF);
/// How long to wait for the real window to arrive at its new rect.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(200);
/// Frames to give a just-resized window to repaint before showing its new content.
const REPAINT_FRAMES: u32 = 3;

// ---------------------------------------------------------------------------------------
// Pure animation math (unit tested)
// ---------------------------------------------------------------------------------------

/// Ease-out cubic: fast start, gentle stop (Windows' own window animations feel like this).
pub fn ease_out_cubic(t: f64) -> f64 {
    let u = 1.0 - t.clamp(0.0, 1.0);
    1.0 - u * u * u
}

pub fn lerp_rect(a: &Rect, b: &Rect, t: f64) -> Rect {
    let l = |x: i32, y: i32| (f64::from(x) + (f64::from(y) - f64::from(x)) * t).round() as i32;
    Rect::new(
        l(a.left, b.left),
        l(a.top, b.top),
        l(a.right, b.right),
        l(a.bottom, b.bottom),
    )
}

pub fn contains(outer: &Rect, inner: &Rect) -> bool {
    outer.left <= inner.left && outer.top <= inner.top && outer.right >= inner.right && outer.bottom >= inner.bottom
}

/// Which animation (if any) takes a window from `from` to `to` (window rects).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// The target contains the window: grow live corners of the moved window.
    Grow,
    /// The window contains the target: shrink corners of a snapshot.
    Shrink,
    /// The overlay couldn't hide both positions (or nothing to animate): just move.
    NoAnimation,
}

pub fn plan(from: &Rect, to: &Rect) -> Plan {
    if from == to || from.is_empty() || to.is_empty() {
        Plan::NoAnimation
    } else if contains(to, from) {
        Plan::Grow
    } else if contains(from, to) {
        Plan::Shrink
    } else {
        Plan::NoAnimation
    }
}

/// The animated rect at `elapsed` into an animation of `duration`, and whether it's done.
pub fn frame(from: &Rect, to: &Rect, elapsed: Duration, duration: Duration) -> (Rect, bool) {
    if duration.is_zero() || elapsed >= duration {
        return (*to, true);
    }
    let t = elapsed.as_secs_f64() / duration.as_secs_f64();
    (lerp_rect(from, to, ease_out_cubic(t)), false)
}

/// Split `dest` into four corner pieces showing the matching corners of a
/// `src_w` x `src_h` image **at 1:1 scale**: `(dest piece, source piece)` pairs, with
/// source pieces in image coordinates. Together the pieces cover `dest` exactly. The
/// image must be at least as large as `dest` (it's clamped otherwise).
pub fn corners(dest: &Rect, src_w: i32, src_h: i32) -> [(Rect, Rect); 4] {
    let w = dest.width().clamp(0, src_w.max(0));
    let h = dest.height().clamp(0, src_h.max(0));
    let (lw, th) = (w / 2, h / 2);
    let (rw, bh) = (w - lw, h - th);
    let (x0, y0) = (dest.left, dest.top);
    [
        (Rect::new(x0, y0, x0 + lw, y0 + th), Rect::new(0, 0, lw, th)),
        (
            Rect::new(x0 + lw, y0, x0 + w, y0 + th),
            Rect::new(src_w - rw, 0, src_w, th),
        ),
        (
            Rect::new(x0, y0 + th, x0 + lw, y0 + h),
            Rect::new(0, src_h - bh, lw, src_h),
        ),
        (
            Rect::new(x0 + lw, y0 + th, x0 + w, y0 + h),
            Rect::new(src_w - rw, src_h - bh, src_w, src_h),
        ),
    ]
}

// ---------------------------------------------------------------------------------------
// Animation thread
// ---------------------------------------------------------------------------------------

/// True when Windows' "Animation effects" (Settings → Accessibility → Visual effects) is on.
pub fn system_animations_enabled() -> bool {
    let mut on = BOOL(1);
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut on as *mut BOOL).cast()),
            Default::default(),
        )
    };
    ok.is_err() || on.as_bool()
}

/// One animated move of `target` from `from` to `to` (window rects, screen coords). The
/// animation thread moves the window itself, at the right moment.
#[derive(Clone, Copy, Debug)]
pub struct Request {
    pub target: Hwnd,
    pub from: Rect,
    pub to: Rect,
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
                }
                let _ = tx.send(unsafe { GetCurrentThreadId() });
                thread_main();
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

/// A copy of part of the screen in a memory DC.
struct Snapshot {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    width: i32,
    height: i32,
}

impl Snapshot {
    fn capture(area: &Rect) -> Option<Self> {
        let (width, height) = (area.width(), area.height());
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return None;
            }
            let dc = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, width, height);
            let old = SelectObject(dc, HGDIOBJ(bitmap.0));
            let copied = BitBlt(
                dc,
                0,
                0,
                width,
                height,
                Some(screen),
                area.left,
                area.top,
                ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
            );
            ReleaseDC(None, screen);
            let snap = Self {
                dc,
                bitmap,
                old,
                width,
                height,
            };
            copied.is_ok().then_some(snap)
        }
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

/// What the overlay paints (the overlay lives on the animation thread, as does this).
enum Painting {
    /// The whole snapshot (growing: the live thumbnails are drawn over it).
    Whole(Snapshot),
    /// The snapshot's corners at this rect (overlay-relative), transparent elsewhere.
    Corners(Snapshot, Rect),
}

thread_local! {
    static PAINTING: RefCell<Option<Painting>> = const { RefCell::new(None) };
}

fn paint(hdc: HDC, client: &Rect) {
    PAINTING.with_borrow(|p| unsafe {
        match p {
            Some(Painting::Whole(s)) => {
                let _ = BitBlt(hdc, 0, 0, s.width, s.height, Some(s.dc), 0, 0, SRCCOPY);
            }
            Some(Painting::Corners(s, r)) => {
                // Pixels are painted exactly once (key outside, image inside): no flicker.
                let brush = CreateSolidBrush(KEY);
                for strip in [
                    Rect::new(client.left, client.top, client.right, r.top),
                    Rect::new(client.left, r.bottom, client.right, client.bottom),
                    Rect::new(client.left, r.top, r.left, r.bottom),
                    Rect::new(r.right, r.top, client.right, r.bottom),
                ] {
                    if !strip.is_empty() {
                        FillRect(hdc, &rect_to(strip), brush);
                    }
                }
                let _ = DeleteObject(HGDIOBJ(brush.0));
                for (d, src) in corners(r, s.width, s.height) {
                    if !d.is_empty() {
                        let _ = BitBlt(
                            hdc,
                            d.left,
                            d.top,
                            d.width(),
                            d.height(),
                            Some(s.dc),
                            src.left,
                            src.top,
                            SRCCOPY,
                        );
                    }
                }
            }
            None => {
                let brush = CreateSolidBrush(KEY);
                FillRect(hdc, &rect_to(*client), brush);
                let _ = DeleteObject(HGDIOBJ(brush.0));
            }
        }
    });
}

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // Never take clicks (WS_EX_TRANSPARENT already makes it click-through).
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            unsafe {
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut client = RECT::default();
                let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client);
                paint(hdc, &crate::win::rect_from(client));
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
            lpfnWndProc: Some(overlay_proc),
            hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
            lpszClassName: OVERLAY_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc) != 0
    })
}

fn thread_main() {
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
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

/// A shown, click-through overlay covering `area`, directly above `target` in z-order.
/// Painted from [`PAINTING`], which must be set before calling.
fn show_overlay(target: HWND, area: &Rect, colour_keyed: bool) -> Option<HWND> {
    let topmost = (unsafe { GetWindowLongPtrW(target, GWL_EXSTYLE) } as u32) & WS_EX_TOPMOST.0 != 0;
    let mut ex = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED;
    if topmost {
        ex |= WS_EX_TOPMOST;
    }
    let hinstance = unsafe { GetModuleHandleW(None) }.ok().map(|m| m.into());
    let overlay = unsafe {
        CreateWindowExW(
            ex,
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
    .inspect_err(|e| tracing::debug!(error = %e, "animation overlay unavailable"))
    .ok()?;
    unsafe {
        let _ = if colour_keyed {
            SetLayeredWindowAttributes(overlay, KEY, 0, LWA_COLORKEY)
        } else {
            SetLayeredWindowAttributes(overlay, COLORREF(0), 255, LWA_ALPHA)
        };
        // Directly above the target (below whatever already covers it).
        let above = GetWindow(target, GW_HWNDPREV).ok().filter(|h| !h.is_invalid());
        let insert_after = above.unwrap_or(if topmost { HWND_TOPMOST } else { HWND_TOP });
        let _ = SetWindowPos(
            overlay,
            Some(insert_after),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        let _ = ShowWindow(overlay, SW_SHOWNOACTIVATE);
        let _ = UpdateWindow(overlay);
    }
    flush();
    Some(overlay)
}

/// Run one animated move to completion (or until a newer request arrives). Whatever
/// happens, the window ends up at `req.to`.
fn run(req: Request) {
    let target = hwnd_from(req.target);
    let plan = plan(&req.from, &req.to);
    if plan == Plan::NoAnimation || !unsafe { IsWindow(Some(target)) }.as_bool() || !register_class() {
        req.place();
        return;
    }
    let ok = match plan {
        Plan::Grow => grow(target, &req),
        _ => shrink(target, &req),
    };
    if !ok {
        // Couldn't set up the overlay: still move the window (no animation).
        req.place();
    }
    PAINTING.with_borrow_mut(|p| *p = None);
}

/// Animate frames from `req.from` to `req.to`, calling `draw` with each rect.
fn animate_frames(req: &Request, mut draw: impl FnMut(&Rect)) {
    let start = Instant::now();
    loop {
        let (rect, done) = frame(&req.from, &req.to, start.elapsed(), req.duration);
        draw(&rect);
        flush();
        if done || pump_and_check_new_request() {
            break;
        }
    }
    draw(&req.to);
    flush();
}

fn grow(target: HWND, req: &Request) -> bool {
    // 1. Freeze the screen under the zone (it includes the window at its old rect).
    let Some(snap) = Snapshot::capture(&req.to) else {
        return false;
    };
    PAINTING.with_borrow_mut(|p| *p = Some(Painting::Whole(snap)));
    let Some(overlay) = show_overlay(target, &req.to, false) else {
        return false;
    };
    // 2. Move the real window under the frozen picture and let it lay itself out.
    req.place();
    wait_for_arrival(target, &req.to);
    for _ in 0..REPAINT_FRAMES {
        flush();
    }
    // 3. Grow its live, unscaled corners from the old rect to the zone.
    let mut thumbs = Vec::with_capacity(4);
    for _ in 0..4 {
        match unsafe { DwmRegisterThumbnail(overlay, target) } {
            Ok(t) => thumbs.push(t),
            Err(e) => tracing::debug!(error = %e, "DwmRegisterThumbnail failed"),
        }
    }
    if thumbs.len() == 4 {
        let size = unsafe { DwmQueryThumbnailSourceSize(thumbs[0]) }.ok();
        let (sw, sh) = size.map_or((req.to.width(), req.to.height()), |s| (s.cx, s.cy));
        animate_frames(req, |r| {
            let rel = r.offset(-req.to.left, -req.to.top);
            for (thumb, (dest, src)) in thumbs.iter().zip(corners(&rel, sw, sh)) {
                set_thumb(*thumb, &dest, &src);
            }
        });
    }
    unsafe {
        for t in thumbs {
            let _ = DwmUnregisterThumbnail(t);
        }
        let _ = DestroyWindow(overlay);
    }
    true
}

fn shrink(target: HWND, req: &Request) -> bool {
    // 1. Freeze the window as it looks now.
    let Some(snap) = Snapshot::capture(&req.from) else {
        return false;
    };
    let full = Rect::new(0, 0, req.from.width(), req.from.height());
    PAINTING.with_borrow_mut(|p| *p = Some(Painting::Corners(snap, full)));
    let Some(overlay) = show_overlay(target, &req.from, true) else {
        return false;
    };
    // 2. Move the real window (it stays covered: the target is inside every frame).
    req.place();
    // 3. Shrink the picture's corners into the target, revealing the desktop around it.
    animate_frames(req, |r| {
        let rel = r.offset(-req.from.left, -req.from.top);
        PAINTING.with_borrow_mut(|p| {
            if let Some(Painting::Corners(_, at)) = p {
                *at = rel;
            }
        });
        unsafe {
            let _ = InvalidateRect(Some(overlay), None, false);
            let _ = UpdateWindow(overlay);
        }
    });
    wait_for_arrival(target, &req.to);
    flush();
    unsafe {
        let _ = DestroyWindow(overlay);
    }
    true
}

fn set_thumb(thumb: isize, dest: &Rect, src: &Rect) {
    let props = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION
            | DWM_TNP_RECTSOURCE
            | DWM_TNP_VISIBLE
            | DWM_TNP_OPACITY
            | DWM_TNP_SOURCECLIENTAREAONLY,
        rcDestination: rect_to(*dest),
        rcSource: rect_to(*src),
        opacity: 255,
        fVisible: BOOL::from(!dest.is_empty()),
        fSourceClientAreaOnly: BOOL(0),
    };
    unsafe {
        let _ = DwmUpdateThumbnailProperties(thumb, &props);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easing() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert_eq!(ease_out_cubic(-1.0), 0.0);
        assert_eq!(ease_out_cubic(2.0), 1.0);
        // Monotonic and front-loaded (ease-out): more than half done at the midpoint.
        let mut prev = 0.0;
        for i in 1..=100 {
            let v = ease_out_cubic(f64::from(i) / 100.0);
            assert!(v >= prev);
            prev = v;
        }
        assert!(ease_out_cubic(0.5) > 0.8);
    }

    #[test]
    fn rect_interpolation() {
        let a = Rect::new(100, 100, 500, 400);
        let b = Rect::new(0, 0, 1000, 1000);
        assert_eq!(lerp_rect(&a, &b, 0.0), a);
        assert_eq!(lerp_rect(&a, &b, 1.0), b);
        assert_eq!(lerp_rect(&a, &b, 0.5), Rect::new(50, 50, 750, 700));
    }

    #[test]
    fn plans() {
        let zone = Rect::new(0, 0, 1000, 1000);
        let inside = Rect::new(100, 100, 500, 400);
        assert_eq!(plan(&inside, &zone), Plan::Grow, "growing into the zone");
        assert_eq!(plan(&zone, &inside), Plan::Shrink, "toggle back (shrinking)");
        let straddling = Rect::new(800, 100, 1300, 400);
        assert_eq!(plan(&straddling, &zone), Plan::NoAnimation, "can't be covered");
        assert_eq!(plan(&zone, &zone), Plan::NoAnimation);
        assert_eq!(plan(&Rect::default(), &zone), Plan::NoAnimation);
    }

    #[test]
    fn growing_frames_always_cover_the_old_rect() {
        // The overlay's frozen picture shows the window at `from`; every frame must hide it.
        let from = Rect::new(120, 80, 640, 520);
        let to = Rect::new(-7, 0, 967, 2307);
        let d = Duration::from_millis(200);
        for ms in 0..=210 {
            let (r, _) = frame(&from, &to, Duration::from_millis(ms), d);
            assert!(contains(&r, &from), "frame at {ms}ms = {r} doesn't cover {from}");
        }
    }

    #[test]
    fn shrinking_frames_always_cover_the_target() {
        // The real window is already at `to` under the overlay; every frame must hide it.
        let from = Rect::new(-7, 0, 967, 2307);
        let to = Rect::new(120, 80, 640, 520);
        let d = Duration::from_millis(200);
        for ms in 0..=210 {
            let (r, _) = frame(&from, &to, Duration::from_millis(ms), d);
            assert!(contains(&r, &to), "frame at {ms}ms = {r} doesn't cover {to}");
        }
    }

    #[test]
    fn frames_end_exactly_on_target() {
        let from = Rect::new(0, 0, 10, 10);
        let to = Rect::new(0, 0, 100, 100);
        let d = Duration::from_millis(200);
        assert_eq!(frame(&from, &to, Duration::ZERO, d), (from, false));
        assert_eq!(frame(&from, &to, d, d), (to, true));
        assert_eq!(frame(&from, &to, Duration::from_secs(5), d), (to, true));
        assert_eq!(frame(&from, &to, Duration::ZERO, Duration::ZERO), (to, true));
        let (mid, done) = frame(&from, &to, Duration::from_millis(100), d);
        assert!(!done && mid.width() > 55 && mid.width() < 100);
    }

    #[test]
    fn corners_are_unscaled_and_tile_the_rect() {
        let dest = Rect::new(10, 20, 10 + 301, 20 + 151); // odd sizes
        let (sw, sh) = (974, 2307);
        let pieces = corners(&dest, sw, sh);
        let mut area = 0;
        for (d, s) in &pieces {
            // 1:1: no stretching, ever.
            assert_eq!((d.width(), d.height()), (s.width(), s.height()), "{d} vs {s}");
            assert!(contains(&dest, d));
            assert!(contains(&Rect::new(0, 0, sw, sh), s));
            area += d.area();
        }
        assert_eq!(area, dest.area(), "pieces must tile the rect without gaps or overlap");
        // Corners map to corners: the title bar's left and right ends, the bottom edge.
        assert_eq!((pieces[0].1.left, pieces[0].1.top), (0, 0));
        assert_eq!((pieces[1].1.right, pieces[1].1.top), (sw, 0));
        assert_eq!((pieces[2].1.left, pieces[2].1.bottom), (0, sh));
        assert_eq!((pieces[3].1.right, pieces[3].1.bottom), (sw, sh));
    }

    #[test]
    fn corners_at_full_size_show_the_whole_image() {
        let dest = Rect::new(0, 0, 974, 2307);
        let pieces = corners(&dest, 974, 2307);
        for (d, s) in pieces {
            assert_eq!(d, s, "at full size each piece shows exactly its own area");
        }
    }

    #[test]
    fn corners_clamp_to_the_image() {
        // Never asked for in practice (the image is always the larger rect), but safe.
        for (d, s) in corners(&Rect::new(0, 0, 500, 500), 100, 80) {
            assert_eq!((d.width(), d.height()), (s.width(), s.height()));
            assert!(contains(&Rect::new(0, 0, 100, 80), &s));
        }
    }
}
