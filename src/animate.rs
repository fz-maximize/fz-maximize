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
//! The rectangle moves in two phases through a "waypoint" (the narrower width and shorter
//! height of the two windows, between them), so there is always an image at least as
//! large as each frame:
//!
//! 1. toward the waypoint, shrinking where needed, showing a snapshot of the old window;
//! 2. on to the target, growing where needed, showing four live DWM thumbnails of the
//!    window's new content (the documented API behind taskbar and Alt-Tab previews).
//!
//! A window inside its zone only has phase 2; the toggle back usually only phase 1; a
//! window poking out of the zone, or somewhere else entirely, has both. Every move is
//! animated, wherever the window starts.
//!
//! How the overlay hides the move: it covers both rects. Before the real window moves,
//! the overlay shows a snapshot of the screen, so nothing visibly changes. Then the real
//! window moves underneath. On every frame, the overlay shows the animated rect's content,
//! the snapshot (the desktop as it was) over the rest of the new rect, where the real
//! window already is, and is transparent everywhere else, so the desktop the old window
//! uncovered shows as it really is.
//!
//! Runs on its own thread (frames paced with `DwmFlush`) so the main thread keeps
//! handling events. A new request finishes the current animation immediately.

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
    ANIMATIONINFO, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GW_HWNDPREV, GWL_EXSTYLE,
    GetMessageW, GetWindow, GetWindowLongPtrW, HTTRANSPARENT, HWND_TOP, HWND_TOPMOST, IsWindow, LWA_COLORKEY, MSG,
    PM_NOREMOVE, PM_REMOVE, PeekMessageW, PostThreadMessageW, RegisterClassExW, SPI_GETANIMATION,
    SPI_GETCLIENTAREAANIMATION, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetLayeredWindowAttributes,
    SetWindowPos, ShowWindow, SystemParametersInfoW, TranslateMessage, WM_APP, WM_ERASEBKGND, WM_NCHITTEST, WM_PAINT,
    WM_QUIT, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};
use windows::core::{BOOL, PCWSTR, w};

use crate::engine::Hwnd;
use crate::geometry::Rect;
use crate::win::{hwnd_from, rect_to};
use crate::window;

pub const OVERLAY_CLASS: PCWSTR = w!("fz-maximize.animation");
const WM_APP_ANIMATE: u32 = WM_APP + 10;
/// Colour key: overlay pixels of this colour are fully transparent.
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
    let l = |x: i32, y: i32| (f64::from(x) + (f64::from(y) - f64::from(x)) * t + 0.5).floor() as i32;
    let (left, top) = (l(a.left, b.left), l(a.top, b.top));
    // Rounding the edges separately can make a frame 1 px larger (or smaller) than both
    // ends; keep its size between theirs, so it never needs more image than exists.
    let w = (l(a.right, b.right) - left).clamp(a.width().min(b.width()), a.width().max(b.width()));
    let h = (l(a.bottom, b.bottom) - top).clamp(a.height().min(b.height()), a.height().max(b.height()));
    Rect::new(left, top, left + w, top + h)
}

#[cfg(test)]
fn contains(outer: &Rect, inner: &Rect) -> bool {
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

/// One axis of [`waypoint`]: an interval as long as the shorter of `a` and `b`, centred
/// between them, shifted to contain their overlap (if any).
fn waypoint_axis(a0: i32, a1: i32, b0: i32, b1: i32) -> (i32, i32) {
    let len = (a1 - a0).min(b1 - b0);
    let centre = f64::from(a0 + a1 + b0 + b1) / 4.0;
    let mut start = (centre - f64::from(len) / 2.0).round() as i32;
    let (o0, o1) = (a0.max(b0), a1.min(b1));
    if o0 < o1 {
        start = start.clamp(o1 - len, o0);
    }
    (start, start + len)
}

/// The rect the animation passes through between the two phases: as large as fits in
/// both windows (the narrower width, the shorter height), between them, and covering
/// their overlap. So phase 1 (old image) only ever shrinks the old window, phase 2 (new
/// image) only grows toward the new one, and wherever both windows are, a frame covers
/// it. Works for any two rects: nested, poking out, or far apart. `None` only when
/// there's nothing to animate.
pub fn waypoint(from: &Rect, to: &Rect) -> Option<Rect> {
    if from == to || from.is_empty() || to.is_empty() {
        return None;
    }
    let (left, right) = waypoint_axis(from.left, from.right, to.left, to.right);
    let (top, bottom) = waypoint_axis(from.top, from.bottom, to.top, to.bottom);
    Some(Rect::new(left, top, right, bottom))
}

/// Which image a frame shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Toward the waypoint: the snapshot of the old window (frame no larger than it).
    Old,
    /// On to the target: the window's live new content (frame no larger than it).
    New,
}

/// Sum of how far the edges move between two rects.
fn travel(a: &Rect, b: &Rect) -> f64 {
    f64::from((a.left - b.left).abs() + (a.top - b.top).abs() + (a.right - b.right).abs() + (a.bottom - b.bottom).abs())
}

/// The frame at `elapsed` into an animation of `duration` from `from` through `mid` (the
/// waypoint) to `to`: its rect, which image it shows, and whether the animation is done.
/// One eased timeline is split between the two phases in proportion to how far the edges
/// travel in each, so the motion keeps a steady pace.
pub fn frame(from: &Rect, mid: &Rect, to: &Rect, elapsed: Duration, duration: Duration) -> (Rect, Phase, bool) {
    if duration.is_zero() || elapsed >= duration {
        return (*to, Phase::New, true);
    }
    let s = ease_out_cubic(elapsed.as_secs_f64() / duration.as_secs_f64());
    let (d1, d2) = (travel(from, mid), travel(mid, to));
    let split = if d1 + d2 > 0.0 { d1 / (d1 + d2) } else { 0.0 };
    if s < split {
        (lerp_rect(from, mid, s / split), Phase::Old, false)
    } else if split < 1.0 {
        (lerp_rect(mid, to, (s - split) / (1.0 - split)), Phase::New, false)
    } else {
        (*mid, Phase::New, false)
    }
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

/// A bitmap in a memory DC: a snapshot of the screen, or a back buffer.
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

/// What the overlay shows. All rects are relative to the overlay (which covers
/// `from ∪ to`). It lives on the animation thread, as does the overlay.
struct Scene {
    /// The screen under the overlay before the window moved.
    snapshot: Canvas,
    /// Frames are composed here, then copied to the window in one go (no flicker).
    back: Canvas,
    size: (i32, i32),
    from: Rect,
    to: Rect,
    rect: Rect,
    phase: Phase,
}

thread_local! {
    static SCENE: RefCell<Option<Scene>> = const { RefCell::new(None) };
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

fn paint(hdc: HDC, client: &Rect) {
    SCENE.with_borrow(|scene| {
        let Some(s) = scene else {
            fill(hdc, client, KEY);
            return;
        };
        let (b, snap) = (s.back.dc, s.snapshot.dc);
        // Transparent by default: the desktop the old window uncovered shows as it is.
        fill(b, &Rect::new(0, 0, s.size.0, s.size.1), KEY);
        // Where the real window already is, show the desktop as it was.
        blit(b, &s.to, snap, s.to.left, s.to.top);
        if s.phase == Phase::Old {
            // The old window's corners (it's in the snapshot at `from`).
            for (d, src) in corners(&s.rect, s.from.width(), s.from.height()) {
                blit(b, &d, snap, s.from.left + src.left, s.from.top + src.top);
            }
        }
        // (Phase::New: the live thumbnails are drawn over this by DWM.)
        blit(hdc, client, b, client.left, client.top);
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

/// A shown, click-through, colour-keyed overlay covering `area`, directly above `target`
/// in z-order. Painted from [`SCENE`], which must be set before calling.
fn show_overlay(target: HWND, area: &Rect) -> Option<HWND> {
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
        let _ = SetLayeredWindowAttributes(overlay, KEY, 0, LWA_COLORKEY);
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
    let Some(mid) = waypoint(&req.from, &req.to) else {
        req.place();
        return;
    };
    if !unsafe { IsWindow(Some(target)) }.as_bool() || !register_class() || !animate_move(target, &req, &mid) {
        // Couldn't set up the overlay: still move the window (no animation).
        req.place();
    }
    SCENE.with_borrow_mut(|s| *s = None);
}

fn animate_move(target: HWND, req: &Request, mid: &Rect) -> bool {
    let area = union(&req.from, &req.to);
    let rel = |r: &Rect| r.offset(-area.left, -area.top);
    let (Some(snapshot), Some(back)) = (Canvas::capture(&area), Canvas::new(area.width(), area.height())) else {
        return false;
    };
    // 1. Freeze the screen: the overlay shows exactly what's there (the old window).
    SCENE.with_borrow_mut(|s| {
        *s = Some(Scene {
            snapshot,
            back,
            size: (area.width(), area.height()),
            from: rel(&req.from),
            to: rel(&req.to),
            rect: rel(&req.from),
            phase: Phase::Old,
        })
    });
    let Some(overlay) = show_overlay(target, &area) else {
        return false;
    };

    // 2. Move the real window underneath and let it lay itself out at its new size.
    req.place();
    wait_for_arrival(target, &req.to);
    for _ in 0..REPAINT_FRAMES {
        flush();
    }

    // 3. Animate. Four live thumbnails show the new content's corners in phase 2.
    let mut thumbs = Vec::with_capacity(4);
    for _ in 0..4 {
        match unsafe { DwmRegisterThumbnail(overlay, target) } {
            Ok(t) => thumbs.push(t),
            Err(e) => tracing::debug!(error = %e, "DwmRegisterThumbnail failed"),
        }
    }
    let source = thumbs
        .first()
        .and_then(|t| unsafe { DwmQueryThumbnailSourceSize(*t) }.ok())
        .map_or((req.to.width(), req.to.height()), |s| (s.cx, s.cy));
    let draw = |rect: &Rect, phase: Phase| {
        let r = rel(rect);
        SCENE.with_borrow_mut(|s| {
            if let Some(s) = s {
                s.rect = r;
                s.phase = phase;
            }
        });
        unsafe {
            let _ = InvalidateRect(Some(overlay), None, false);
            let _ = UpdateWindow(overlay);
        }
        let pieces = corners(&r, source.0, source.1);
        for (thumb, (dest, src)) in thumbs.iter().zip(pieces) {
            set_thumb(*thumb, &dest, &src, phase == Phase::New);
        }
    };
    if thumbs.len() == 4 {
        let start = Instant::now();
        loop {
            let (rect, phase, done) = frame(&req.from, mid, &req.to, start.elapsed(), req.duration);
            draw(&rect, phase);
            flush();
            if done || pump_and_check_new_request() {
                break;
            }
        }
    }
    // Final frame: the new content exactly where the real window is.
    draw(&req.to, Phase::New);
    flush();
    unsafe {
        for t in thumbs {
            let _ = DwmUnregisterThumbnail(t);
        }
        let _ = DestroyWindow(overlay);
    }
    true
}

fn set_thumb(thumb: isize, dest: &Rect, src: &Rect, visible: bool) {
    let props = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION
            | DWM_TNP_RECTSOURCE
            | DWM_TNP_VISIBLE
            | DWM_TNP_OPACITY
            | DWM_TNP_SOURCECLIENTAREAONLY,
        rcDestination: rect_to(*dest),
        rcSource: rect_to(*src),
        opacity: 255,
        fVisible: BOOL::from(visible && !dest.is_empty()),
        fSourceClientAreaOnly: BOOL(0),
    };
    unsafe {
        let _ = DwmUpdateThumbnailProperties(thumb, &props);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: Duration = Duration::from_millis(200);

    fn frames(from: &Rect, to: &Rect) -> Vec<(Rect, Phase)> {
        let mid = waypoint(from, to).expect("should animate");
        (0..=210)
            .map(|ms| {
                let (r, p, _) = frame(from, &mid, to, Duration::from_millis(ms), D);
                (r, p)
            })
            .collect()
    }

    /// The invariants that make the overlay correct, for any pair of rects:
    /// * every frame covers the overlap (where both old and new window are), so the
    ///   snapshot is only ever shown where it holds the plain desktop;
    /// * phase-1 frames are no larger than the old window, phase-2 frames no larger than
    ///   the new one, so the corner pieces always have enough image (no gaps, no
    ///   stretching).
    fn check_invariants(from: Rect, to: Rect) {
        let overlap = from.intersection(&to);
        let fits = |r: &Rect, img: &Rect| r.width() <= img.width() && r.height() <= img.height();
        let fs = frames(&from, &to);
        for (r, p) in &fs {
            if !overlap.is_empty() {
                assert!(contains(r, &overlap), "{r} doesn't cover the overlap {overlap}");
            }
            match p {
                Phase::Old => assert!(fits(r, &from), "old-phase frame {r} larger than {from}"),
                Phase::New => assert!(fits(r, &to), "new-phase frame {r} larger than {to}"),
            }
        }
        assert_eq!(fs.first().unwrap().0, from, "starts where the window was");
        assert_eq!(*fs.last().unwrap(), (to, Phase::New), "ends exactly on the target");
        // Old phase (if any) strictly before the new one.
        let first_new = fs.iter().position(|f| f.1 == Phase::New).unwrap();
        assert!(fs[first_new..].iter().all(|f| f.1 == Phase::New));
    }

    #[test]
    fn easing() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert_eq!(ease_out_cubic(-1.0), 0.0);
        assert_eq!(ease_out_cubic(2.0), 1.0);
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
    fn waypoints() {
        let zone = Rect::new(0, 0, 1000, 1000);
        let inside = Rect::new(100, 100, 500, 400);
        assert_eq!(waypoint(&inside, &zone), Some(inside), "growing: no old phase");
        assert_eq!(waypoint(&zone, &inside), Some(inside), "toggle back: no new phase");
        // Poking out on the right: as wide as the window, shifted to cover the overlap.
        let straddling = Rect::new(800, 100, 1300, 400);
        assert_eq!(waypoint(&straddling, &zone), Some(Rect::new(525, 100, 1025, 400)));
        // Beside it: halfway across, the smaller size, covering the vertical overlap.
        let far = Rect::new(2000, 0, 2500, 500);
        assert_eq!(waypoint(&far, &zone), Some(Rect::new(1125, 0, 1625, 500)));
        assert_eq!(waypoint(&zone, &zone), None);
        assert_eq!(waypoint(&Rect::default(), &zone), None);
    }

    #[test]
    fn window_far_from_its_zone_still_animates() {
        // In another zone entirely (no overlap), larger in one direction, smaller in the
        // other; and the toggle back.
        let (from, to) = (Rect::new(2900, 300, 3800, 1200), Rect::new(953, -7, 2887, 2307));
        check_invariants(from, to);
        check_invariants(to, from);
        // Touching edges only, and diagonal.
        check_invariants(Rect::new(0, 0, 100, 100), Rect::new(100, 100, 400, 300));
        check_invariants(Rect::new(0, 0, 1000, 200), Rect::new(1500, 900, 1700, 2000));
    }

    #[test]
    fn invariants_hold_for_many_rect_pairs() {
        // A deterministic sweep of positions and sizes (nested, overlapping, apart).
        let mut seed = 0x2545_F491_u32;
        let mut next = |n: i32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed % n as u32) as i32
        };
        for _ in 0..300 {
            let a = Rect::from_xywh(next(3000) - 500, next(2000) - 200, 50 + next(2500), 50 + next(2200));
            let b = Rect::from_xywh(next(3000) - 500, next(2000) - 200, 50 + next(2500), 50 + next(2200));
            if a != b {
                check_invariants(a, b);
            }
        }
    }

    #[test]
    fn window_inside_its_zone_only_grows() {
        let (from, to) = (Rect::new(120, 80, 640, 520), Rect::new(-7, 0, 967, 2307));
        check_invariants(from, to);
        assert!(frames(&from, &to).iter().all(|f| f.1 == Phase::New));
    }

    #[test]
    fn toggle_back_into_a_smaller_rect_only_shrinks() {
        let (from, to) = (Rect::new(-7, 0, 967, 2307), Rect::new(120, 80, 640, 520));
        check_invariants(from, to);
        let fs = frames(&from, &to);
        assert!(fs.iter().filter(|f| f.0 != to).all(|f| f.1 == Phase::Old));
    }

    #[test]
    fn wider_shorter_window_narrows_then_grows() {
        // A real case from the log: a 2140x1594 window poking out of both sides of a
        // 1920x2300 zone.
        let (from, to) = (Rect::new(818, 471, 2958, 2065), Rect::new(953, -7, 2887, 2307));
        check_invariants(from, to);
        check_invariants(to, from); // and the toggle back
        let fs = frames(&from, &to);
        assert!(fs.iter().any(|f| f.1 == Phase::Old) && fs.iter().any(|f| f.1 == Phase::New));
    }

    #[test]
    fn window_poking_out_of_two_sides() {
        let (from, to) = (Rect::new(1005, 65, 2940, 2350), Rect::new(953, -7, 2887, 2307));
        check_invariants(from, to);
        check_invariants(to, from);
    }

    #[test]
    fn phases_split_by_distance_and_keep_moving() {
        // Small inward move then a big outward one: the old phase is short.
        let (from, to) = (Rect::new(0, 100, 1100, 600), Rect::new(0, 0, 1000, 2000));
        let fs = frames(&from, &to);
        let old = fs.iter().filter(|f| f.1 == Phase::Old).count();
        assert!(old > 0 && old < fs.len() / 4, "{old} old-phase frames of {}", fs.len());
    }

    #[test]
    fn zero_duration_jumps_to_the_end() {
        let (from, to) = (Rect::new(0, 0, 10, 10), Rect::new(0, 0, 100, 100));
        let mid = waypoint(&from, &to).unwrap();
        assert_eq!(
            frame(&from, &mid, &to, Duration::ZERO, Duration::ZERO),
            (to, Phase::New, true)
        );
        assert_eq!(frame(&from, &mid, &to, D, D), (to, Phase::New, true));
        assert_eq!(frame(&from, &mid, &to, Duration::ZERO, D).0, from);
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
        for (d, s) in corners(&dest, 974, 2307) {
            assert_eq!(d, s, "at full size each piece shows exactly its own area");
        }
    }

    #[test]
    fn corners_clamp_to_the_image() {
        for (d, s) in corners(&Rect::new(0, 0, 500, 500), 100, 80) {
            assert_eq!((d.width(), d.height()), (s.width(), s.height()));
            assert!(contains(&Rect::new(0, 0, 100, 80), &s));
        }
    }
}
