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
//! rects. It has no pixels of its own, only thumbnails, all drawn by the compositor:
//!
//! 1. **Background:** live thumbnails of every window beneath the target, and of the
//!    desktop, in z-order. Together they show exactly what the target is about to cover,
//!    so the real window can move into its zone underneath without being seen.
//! 2. **Old:** the old window's frame, copied once into a cloaked helper window.
//! 3. **New:** the real window, live, at its new size.
//!
//! Only the old window's frame is copied from the screen, and nothing has to be uploaded
//! for the background, so the animation starts within a frame or two of the gesture.
//! (Copying the whole area through GDI took 70-120 ms on a 4K hybrid-GPU laptop, and
//! showing it as a layered window another 100 ms.)
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
    ANIMATIONINFO, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows, GW_HWNDPREV,
    GWL_EXSTYLE, GetClientRect, GetMessageW, GetWindow, GetWindowLongPtrW, HTTRANSPARENT, HWND_TOP, HWND_TOPMOST,
    IsWindow, IsWindowVisible, LWA_ALPHA, MSG, PM_NOREMOVE, PM_REMOVE, PeekMessageW, PostThreadMessageW,
    RegisterClassExW, SPI_GETANIMATION, SPI_GETCLIENTAREAANIMATION, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetTimer, SetWindowPos,
    ShowWindow, SystemParametersInfoW, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_ERASEBKGND, WM_NCHITTEST,
    WM_PAINT, WM_QUIT, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{BOOL, HSTRING, PCWSTR, w};

use crate::engine::Hwnd;
use crate::geometry::{Margins, Rect};
use crate::win::{hwnd_from, rect_from, rect_to};
use crate::window;

pub const OVERLAY_CLASS: PCWSTR = w!("fz-maximize.animation");
const WM_APP_ANIMATE: u32 = WM_APP + 10;
/// Above `WM_APP_ANIMATE`, so a running animation leaves it queued (see
/// `pump_and_check_new_request`).
const WM_APP_PREPARE: u32 = WM_APP + 11;
/// A prepared picture older than this isn't used (the window may have changed).
const PREPARED_MAX_AGE: Duration = Duration::from_millis(1500);
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

/// `rects` minus `cut`: the parts of the (non-overlapping) rects outside `cut`, still
/// non-overlapping. Each rect splits into at most four (above, below, left, right).
pub fn subtract(rects: &[Rect], cut: &Rect) -> Vec<Rect> {
    let mut out = Vec::with_capacity(rects.len() + 4);
    for r in rects {
        let i = r.intersection(cut);
        if i.is_empty() {
            out.push(*r);
            continue;
        }
        for piece in [
            Rect::new(r.left, r.top, r.right, i.top),
            Rect::new(r.left, i.bottom, r.right, r.bottom),
            Rect::new(r.left, i.top, i.left, i.bottom),
            Rect::new(i.right, i.top, r.right, i.bottom),
        ] {
            if !piece.is_empty() {
                out.push(piece);
            }
        }
    }
    out
}

/// Which parts of each window to draw so that every pixel of `area` is drawn once,
/// given windows from top to bottom as (frame, opaque). A window gets the parts of
/// `area` that no opaque window above it covers; a see-through one doesn't hide what's
/// beneath it. Stops once `area` is covered. Returns (window index, pieces), top first,
/// and whether opaque windows cover all of `area`.
pub fn visible_pieces(area: &Rect, windows: &[(Rect, bool)]) -> (Vec<(usize, Vec<Rect>)>, bool) {
    let mut uncovered = vec![*area];
    let mut out = Vec::new();
    for (i, (frame, opaque)) in windows.iter().enumerate() {
        if uncovered.is_empty() {
            break;
        }
        let pieces: Vec<Rect> = uncovered
            .iter()
            .map(|u| u.intersection(frame))
            .filter(|p| !p.is_empty())
            .collect();
        if pieces.is_empty() {
            continue;
        }
        if *opaque {
            uncovered = subtract(&uncovered, frame);
        }
        out.push((i, pieces));
    }
    let covered = uncovered.is_empty();
    (out, covered)
}

/// The part of a thumbnail source (`full`, which shows the window's `frame`) that shows
/// `piece` of the frame. Scales when the source isn't 1:1 with the screen (apps that
/// Windows scales for DPI).
pub fn source_piece(full: &Rect, frame: &Rect, piece: &Rect) -> Rect {
    let sx = f64::from(full.width()) / f64::from(frame.width().max(1));
    let sy = f64::from(full.height()) / f64::from(frame.height().max(1));
    let x = |v: i32| full.left + (f64::from(v - frame.left) * sx).round() as i32;
    let y = |v: i32| full.top + (f64::from(v - frame.top) * sy).round() as i32;
    Rect::new(x(piece.left), y(piece.top), x(piece.right), y(piece.bottom))
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
                        destroy_windows(w);
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

    /// A maximize gesture has begun on `target`: copy its picture now, so the animation
    /// can start without waiting for the copy.
    pub fn prepare(&self, target: Hwnd) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_APP_PREPARE, WPARAM(target as usize), LPARAM(0));
        }
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

/// The old window's visible frame, painted by the cloaked helper window (`source`).
struct Scene {
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
                // Only the helper paints; the overlay has no surface (thumbnails only).
                SCENE.with_borrow(|scene| match scene {
                    Some(s) if s.source == hwnd => blit(hdc, &client, s.old_window.dc, 0, 0),
                    _ => fill(hdc, &client, KEY),
                });
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Live thumbnails of what's beneath `target` in z-order (other windows and the
/// desktop), drawn in the overlay where they really are: together, the screen as it
/// looks without the target. Only the visible part of each window is drawn, so every
/// pixel is drawn about once. Unregistered on drop.
struct Background {
    thumbs: Vec<isize>,
}

/// More pieces than this: don't animate (each piece is a thumbnail to compose per frame).
const MAX_BACKGROUND_PIECES: usize = 32;

impl Background {
    fn build(overlay: HWND, target: HWND, area: &Rect) -> Option<Self> {
        // Top-level windows, topmost first (EnumWindows' order is z-order).
        unsafe extern "system" fn cb(h: HWND, data: LPARAM) -> BOOL {
            // SAFETY: `data` is the &mut Vec below, alive for the whole enumeration.
            unsafe { &mut *(data.0 as *mut Vec<HWND>) }.push(h);
            BOOL(1)
        }
        let mut all: Vec<HWND> = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(cb), LPARAM(&mut all as *mut _ as isize));
        }
        let below = all.iter().position(|&h| h == target)? + 1;
        // (window, window rect, visible frame), top first.
        let mut beneath: Vec<(HWND, Rect, Rect)> = Vec::new();
        let mut layers: Vec<(Rect, bool)> = Vec::new();
        for &h in &all[below..] {
            if h == overlay || !unsafe { IsWindowVisible(h) }.as_bool() || window::is_iconic(h) || window::is_cloaked(h)
            {
                continue;
            }
            let Some(win) = window::window_rect(h) else {
                continue;
            };
            // Windows without a DWM frame (the desktop) report none: the window rect is it.
            let frame = window::frame_rect(h).unwrap_or(win);
            if frame.intersection(area).is_empty() {
                continue;
            }
            // Layered windows may be see-through: draw them, but not as covering.
            let ex = unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } as u32;
            beneath.push((h, win, frame));
            layers.push((frame, ex & WS_EX_LAYERED.0 == 0));
        }
        // Only the on-screen part matters: a window may start partly off-screen.
        let on_screen = area.intersection(&virtual_screen());
        let (visible, covered) = visible_pieces(&on_screen, &layers);
        let pieces: usize = visible.iter().map(|(_, p)| p.len()).sum();
        // Anything left uncovered would let the real window show through.
        if !covered || pieces > MAX_BACKGROUND_PIECES {
            tracing::debug!(
                windows = beneath.len(),
                pieces,
                covered,
                "no complete background; not animating"
            );
            return None;
        }
        let mut bg = Background {
            thumbs: Vec::with_capacity(pieces),
        };
        // Bottom first: the compositor draws later thumbnails over earlier ones.
        for (i, rects) in visible.into_iter().rev() {
            let (h, win, frame) = beneath[i];
            for piece in rects {
                let Ok(thumb) = (unsafe { DwmRegisterThumbnail(overlay, h) }) else {
                    tracing::debug!(hwnd = h.0 as isize, "background thumbnail unavailable; not animating");
                    return None;
                };
                bg.thumbs.push(thumb);
                let margins = Margins::between(win, frame);
                let (w, ht) = unsafe { DwmQueryThumbnailSourceSize(thumb) }
                    .map(|s| (s.cx, s.cy))
                    .unwrap_or((frame.width(), frame.height()));
                let full = frame_source(w, ht, w >= win.width() - 1, &margins);
                set_thumb(
                    thumb,
                    &piece.offset(-area.left, -area.top),
                    &source_piece(&full, &frame, &piece),
                    255,
                );
            }
        }
        Some(bg)
    }
}

/// The bounding box of all monitors.
fn virtual_screen() -> Rect {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };
    let (x, y, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    Rect::new(x, y, x + w, y + h)
}

impl Drop for Background {
    fn drop(&mut self) {
        for &t in &self.thumbs {
            unsafe {
                let _ = DwmUnregisterThumbnail(t);
            }
        }
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
        SetTimer(None, 0, 2_000, None);
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
        if msg.message == WM_APP_PREPARE {
            prepare(msg.wParam.0 as Hwnd);
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

/// The overlay and the cloaked helper window, kept between animations and destroyed
/// after a while unused. Thumbnails are registered per animation, bottom to top, because
/// the compositor draws them in the order they were registered.
struct Windows {
    overlay: HWND,
    source: HWND,
    last_used: Instant,
}

/// Destroy the cached windows after this long without an animation.
const KEEP_WINDOWS: Duration = Duration::from_secs(30);

thread_local! {
    static WINDOWS: RefCell<Option<Windows>> = const { RefCell::new(None) };
}

fn cached_windows() -> Option<(HWND, HWND)> {
    WINDOWS.with_borrow_mut(|w| {
        if let Some(w) = w.as_mut() {
            w.last_used = Instant::now();
            return Some((w.overlay, w.source));
        }
        let tiny = Rect::new(0, 0, 1, 1);
        let source = create_popup(&tiny, WINDOW_EX_STYLE(0))?;
        // The overlay has no surface of its own: no pixels to paint or upload, however
        // big it is; it only hosts thumbnails.
        let Some(overlay) = create_popup(&tiny, WS_EX_NOREDIRECTIONBITMAP) else {
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
            let _ = SetLayeredWindowAttributes(overlay, COLORREF(0), 255, LWA_ALPHA);
        }
        *w = Some(Windows {
            overlay,
            source,
            last_used: Instant::now(),
        });
        Some((overlay, source))
    })
}

fn destroy_windows(w: Windows) {
    unsafe {
        let _ = DestroyWindow(w.overlay);
        let _ = DestroyWindow(w.source);
    }
}

/// Called on the thread's idle timer.
fn drop_idle_windows() {
    // A prepared picture no maximize used (a single title-bar click, a cancelled press).
    PREPARED.with_borrow_mut(|p| {
        if p.as_ref().is_some_and(|p| p.at.elapsed() > PREPARED_MAX_AGE) {
            *p = None;
        }
    });
    WINDOWS.with_borrow_mut(|w| {
        if w.as_ref().is_some_and(|w| w.last_used.elapsed() >= KEEP_WINDOWS) {
            destroy_windows(w.take().unwrap());
        }
    });
}

/// Unregisters a thumbnail on drop.
struct Thumb(isize);

impl Thumb {
    fn register(dest: HWND, source: HWND) -> Option<Self> {
        unsafe { DwmRegisterThumbnail(dest, source) }.ok().map(Thumb)
    }
}

impl Drop for Thumb {
    fn drop(&mut self) {
        unsafe {
            let _ = DwmUnregisterThumbnail(self.0);
        }
    }
}

/// The invisible resize borders of `target` (whose window rect is `win`).
fn margins_of(target: HWND, win: &Rect) -> Margins {
    window::frame_rect(target)
        .map(|f| Margins::between(*win, f))
        .filter(Margins::is_plausible)
        .unwrap_or_default()
}

/// A window's picture copied ahead of a likely maximize (see `Animator::prepare`).
struct Prepared {
    target: Hwnd,
    /// The window rect and visible frame it was copied at.
    window: Rect,
    frame: Rect,
    picture: Canvas,
    at: Instant,
}

thread_local! {
    static PREPARED: RefCell<Option<Prepared>> = const { RefCell::new(None) };
}

fn prepare(target: Hwnd) {
    let hwnd = hwnd_from(target);
    let Some(win) = window::window_rect(hwnd) else {
        return;
    };
    let frame = win.shrink(margins_of(hwnd, &win));
    if frame.is_empty() {
        return;
    }
    let start = Instant::now();
    let Some(picture) = Canvas::capture(&frame) else {
        return;
    };
    tracing::debug!(
        hwnd = target,
        ms = start.elapsed().as_millis() as u64,
        "picture copied ahead of a likely maximize"
    );
    PREPARED.with_borrow_mut(|p| {
        *p = Some(Prepared {
            target,
            window: win,
            frame,
            picture,
            at: Instant::now(),
        })
    });
}

/// The prepared picture, if it was copied from `target` as it still is (`window`,
/// `frame`) and recently. Taken either way: a picture is used at most once.
fn take_prepared(target: Hwnd, window: &Rect, frame: &Rect) -> Option<Canvas> {
    let p = PREPARED.with_borrow_mut(Option::take)?;
    (p.target == target && p.window == *window && p.frame == *frame && p.at.elapsed() <= PREPARED_MAX_AGE)
        .then_some(p.picture)
}

fn animate_move(target: HWND, req: &Request) -> bool {
    // Animate the visible frames, not the invisible resize borders around them.
    let margins = margins_of(target, &req.from);
    let (from, to) = (req.from.shrink(margins), req.to.shrink(margins));
    if from.is_empty() || to.is_empty() {
        return false;
    }
    let setup_start = Instant::now();
    let area = union(&req.from, &req.to);
    let rel = |r: &Rect| r.offset(-area.left, -area.top);
    let from_r = rel(&from);

    // 1. The old window's frame: the only pixels copied from the screen, usually already
    //    while the gesture was being made.
    let prepared = take_prepared(req.target, &req.from, &from);
    let was_prepared = prepared.is_some();
    let Some(old_window) = prepared.or_else(|| Canvas::capture(&from)) else {
        return false;
    };
    let capture_ms = setup_start.elapsed().as_millis() as u64;
    let old_src = Rect::new(0, 0, from.width(), from.height());

    // 2. The overlay, directly above the target in z-order, with its thumbnails from
    //    bottom to top: background, old, new. Nothing is visible yet.
    let Some((overlay, source)) = cached_windows() else {
        return false;
    };
    SCENE.with_borrow_mut(|s| *s = Some(Scene { old_window, source }));
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
        let _ = SetWindowPos(
            overlay,
            None,
            area.left,
            area.top,
            area.width(),
            area.height(),
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    let Some(background) = Background::build(overlay, target, &area) else {
        return false;
    };
    let (Some(old), Some(new)) = (Thumb::register(overlay, source), Thumb::register(overlay, target)) else {
        return false;
    };
    set_thumb(old.0, &from_r, &old_src, 255);
    set_thumb(new.0, &rel(&req.to), &Rect::default(), 0);
    let composed_ms = setup_start.elapsed().as_millis() as u64;

    // 3. Show it: it looks exactly like the screen beneath it.
    let topmost = (unsafe { GetWindowLongPtrW(target, GWL_EXSTYLE) } as u32) & WS_EX_TOPMOST.0 != 0;
    unsafe {
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
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
    let shown_ms = setup_start.elapsed().as_millis() as u64;
    flush();
    let flushed_ms = setup_start.elapsed().as_millis() as u64;

    // Does DWM give us the window with its invisible borders, or just the frame? Ask now,
    // while the window still has its old, known size.
    let initial = unsafe { DwmQueryThumbnailSourceSize(new.0) }.map(|s| (s.cx, s.cy)).ok();
    let source_has_borders = initial.is_some_and(|(w, _)| w >= req.from.width() - 1);

    // 4. Move the real window underneath; animate.
    req.place();
    let placed_ms = setup_start.elapsed().as_millis() as u64;
    // The new image: the visible frame part of the real window (see `frame_source`).
    let new_src = || {
        let (w, h) = unsafe { DwmQueryThumbnailSourceSize(new.0) }
            .map(|s| (s.cx, s.cy))
            .unwrap_or((req.to.width(), req.to.height()));
        frame_source(w, h, source_has_borders, &margins)
    };
    let setup_ms = setup_start.elapsed().as_millis() as u64;
    let start = Instant::now();
    let mut frames = 0u32;
    loop {
        let f = frame(req.style, &from, &to, start.elapsed(), req.duration);
        set_thumb(old.0, &rel(&f.old), &old_src, f.old_opacity);
        set_thumb(new.0, &rel(&f.new), &new_src(), f.new_opacity);
        flush();
        frames += 1;
        if f.done || pump_and_check_new_request() {
            break;
        }
    }
    let elapsed = start.elapsed();

    // 5. Final frame: the new image exactly over the real window; then reveal it.
    set_thumb(old.0, &rel(&from), &old_src, 0);
    set_thumb(new.0, &rel(&to), &new_src(), 255);
    wait_for_arrival(target, &req.to);
    flush();
    unsafe {
        let _ = ShowWindow(overlay, SW_HIDE);
    }
    let background_windows = background.thumbs.len();
    drop((new, old, background));
    tracing::debug!(
        style = ?req.style,
        frames,
        ms = elapsed.as_millis() as u64,
        fps = (f64::from(frames) / elapsed.as_secs_f64().max(0.001)).round(),
        setup_ms,
        capture_ms,
        composed_ms,
        shown_ms,
        flushed_ms,
        placed_ms,
        background_windows,
        was_prepared,
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

    fn total_area(rects: &[Rect]) -> i64 {
        rects.iter().map(Rect::area).sum()
    }

    #[test]
    fn subtract_leaves_the_rest_without_overlaps() {
        let r = Rect::new(0, 0, 100, 100);
        // A hole in the middle: four pieces around it.
        let rest = subtract(&[r], &Rect::new(25, 25, 75, 75));
        assert_eq!(rest.len(), 4);
        assert_eq!(total_area(&rest), 100 * 100 - 50 * 50);
        for (i, a) in rest.iter().enumerate() {
            for b in &rest[i + 1..] {
                assert!(a.intersection(b).is_empty(), "{a} overlaps {b}");
            }
        }
        assert!(subtract(&[r], &Rect::new(-10, -10, 200, 200)).is_empty());
        assert_eq!(subtract(&[r], &Rect::new(200, 200, 300, 300)), vec![r]);
    }

    #[test]
    fn background_draws_each_pixel_once() {
        let area = Rect::new(0, 0, 1000, 1000);
        let desktop = (Rect::new(-500, -500, 4000, 3000), true);
        // A maximized window hides everything beneath it: the desktop isn't drawn at all.
        let maximized = (Rect::new(0, 0, 3840, 2300), true);
        let hidden = (Rect::new(100, 100, 600, 600), true);
        let (pieces, covered) = visible_pieces(&area, &[maximized, hidden, desktop]);
        assert!(covered);
        assert_eq!(pieces, vec![(0, vec![area])]);

        // Two windows side by side, the desktop shows in between.
        let left = (Rect::new(0, 0, 400, 1000), true);
        let right = (Rect::new(600, 0, 1000, 1000), true);
        let (pieces, covered) = visible_pieces(&area, &[left, right, desktop]);
        assert!(covered);
        let drawn: Vec<Rect> = pieces.iter().flat_map(|(_, p)| p.clone()).collect();
        assert_eq!(total_area(&drawn), area.area(), "every pixel exactly once");
        assert_eq!(pieces[2], (2, vec![Rect::new(400, 0, 600, 1000)]));

        // A see-through window is drawn but doesn't hide what's beneath it.
        let glass = (Rect::new(0, 0, 1000, 1000), false);
        let (pieces, covered) = visible_pieces(&area, &[glass, desktop]);
        assert!(covered);
        assert_eq!(pieces.len(), 2);

        // Nothing opaque under part of the area: not covered.
        let (_, covered) = visible_pieces(&area, &[left]);
        assert!(!covered);
    }

    #[test]
    fn source_pieces_follow_the_window() {
        let frame = Rect::new(100, 100, 600, 400);
        let full = Rect::new(7, 0, 507, 300); // border-offset source, 1:1
        assert_eq!(source_piece(&full, &frame, &frame), full);
        assert_eq!(
            source_piece(&full, &frame, &Rect::new(200, 150, 300, 250)),
            Rect::new(107, 50, 207, 150)
        );
        // Source at half the screen size (an app scaled by Windows for DPI).
        let half = Rect::new(0, 0, 250, 150);
        assert_eq!(
            source_piece(&half, &frame, &Rect::new(300, 200, 600, 400)),
            Rect::new(100, 50, 250, 150)
        );
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
