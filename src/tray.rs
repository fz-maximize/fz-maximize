//! Notification-area icon and its context menu. The icon (three zones, 25/50/25) is
//! rendered at runtime at the system small-icon size, so no resource file is needed.
//! While an update is waiting, it carries a yellow dot in its corner.

use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject, HBITMAP,
    HGDIOBJ,
};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos, HICON, ICONINFO,
    MENUINFO, MENUITEMINFOW, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MIIM_BITMAP, MIM_STYLE, MNS_CHECKORBMP,
    PostMessageW, SM_CXSMICON, SetForegroundWindow, SetMenuDefaultItem, SetMenuInfo, SetMenuItemInfoW, TPM_BOTTOMALIGN,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_NULL,
};
use windows::core::{BOOL, HSTRING, PCWSTR};

const TRAY_ID: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Settings,
    TogglePause,
    Reload,
    OpenConfig,
    OpenLogs,
    ToggleAutostart,
    CheckUpdates,
    ShowUpdate,
    About,
    Exit,
}

impl Command {
    const ALL: [Command; 10] = [
        Command::Settings,
        Command::TogglePause,
        Command::Reload,
        Command::OpenConfig,
        Command::OpenLogs,
        Command::ToggleAutostart,
        Command::CheckUpdates,
        Command::ShowUpdate,
        Command::About,
        Command::Exit,
    ];

    fn id(self) -> usize {
        100 + self as usize
    }

    fn from_id(id: usize) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.id() == id)
    }
}

/// What the menu should show.
pub struct MenuState {
    pub paused: bool,
    pub autostart: bool,
    pub status: String,
    pub check: String,
    /// "Update to 0.3.5..." when an update is available (opens the update window).
    pub update: Option<String>,
    /// The update is waiting for a decision: the item shows the icon's dot too.
    pub update_dot: bool,
}

pub struct Tray {
    hwnd: HWND,
    callback_msg: u32,
    /// Indexed by `paused * 2 + dot`.
    icons: [HICON; 4],
    paused: bool,
    dot: bool,
    tip: String,
}

fn copy_wide(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

impl Tray {
    pub fn new(hwnd: HWND, callback_msg: u32) -> Self {
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) }.clamp(16, 64);
        let icon = |paused, dot| icon_from_pixels(size, &render_icon(size, paused, dot)).unwrap_or_default();
        let tray = Self {
            hwnd,
            callback_msg,
            icons: [
                icon(false, false),
                icon(false, true),
                icon(true, false),
                icon(true, true),
            ],
            paused: false,
            dot: false,
            tip: "fz-maximize".into(),
        };
        tray.add();
        tray
    }

    fn base(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ID,
            ..Default::default()
        }
    }

    fn icon(&self) -> HICON {
        self.icons[usize::from(self.paused) * 2 + usize::from(self.dot)]
    }

    /// Add the icon (also used to re-add after Explorer restarts).
    pub fn add(&self) {
        let mut nid = self.base();
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        nid.uCallbackMessage = self.callback_msg;
        nid.hIcon = self.icon();
        copy_wide(&mut nid.szTip, &self.tip);
        unsafe {
            if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                tracing::warn!("Shell_NotifyIcon(NIM_ADD) failed (taskbar not ready?)");
            }
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
    }

    /// `dot`: an update is waiting for a decision.
    pub fn set_state(&mut self, paused: bool, dot: bool, tip: &str) {
        self.paused = paused;
        self.dot = dot;
        self.tip = tip.to_string();
        let mut nid = self.base();
        nid.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        nid.hIcon = self.icon();
        copy_wide(&mut nid.szTip, &self.tip);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    pub fn notify(&self, title: &str, text: &str, warning: bool) {
        let mut nid = self.base();
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO };
        copy_wide(&mut nid.szInfoTitle, title);
        copy_wide(&mut nid.szInfo, text);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }
}

/// Show the context menu at the cursor; returns the chosen command. A free function so
/// callers hold no app state during the modal menu loop.
pub fn show_menu(hwnd: HWND, state: &MenuState) -> Option<Command> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let add = |flags, cmd: Option<Command>, text: &str| {
            let text = HSTRING::from(text);
            let _ = AppendMenuW(menu, flags, cmd.map_or(0, Command::id), PCWSTR(text.as_ptr()));
        };
        // Checkmarks and the update item's dot share one column.
        let style = MENUINFO {
            cbSize: std::mem::size_of::<MENUINFO>() as u32,
            fMask: MIM_STYLE,
            dwStyle: MNS_CHECKORBMP,
            ..Default::default()
        };
        let _ = SetMenuInfo(menu, &style);
        add(MF_STRING | MF_GRAYED, None, &state.status);
        add(MF_STRING | MF_GRAYED, None, &state.check);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        // An update comes first, in bold (the menu's default item), with the icon's dot
        // while it's waiting for a decision.
        let mut dot = None;
        if let Some(label) = &state.update {
            let id = Command::ShowUpdate.id() as u32;
            add(MF_STRING, Some(Command::ShowUpdate), label);
            let _ = SetMenuDefaultItem(menu, id, 0);
            if state.update_dot {
                dot = dot_bitmap(hwnd);
            }
            if let Some(bmp) = dot {
                let item = MENUITEMINFOW {
                    cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_BITMAP,
                    hbmpItem: bmp,
                    ..Default::default()
                };
                let _ = SetMenuItemInfoW(menu, id, false, &item);
            }
        }
        add(MF_STRING, Some(Command::Settings), "&Settings...");
        add(
            MF_STRING,
            Some(Command::TogglePause),
            if state.paused { "&Resume" } else { "&Pause" },
        );
        add(MF_STRING, Some(Command::Reload), "Re&load layouts && config");
        add(MF_STRING, Some(Command::OpenConfig), "Open &config file");
        add(MF_STRING, Some(Command::OpenLogs), "Open &log folder");
        let auto_flags = if state.autostart {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        add(auto_flags, Some(Command::ToggleAutostart), "Start with &Windows");
        add(MF_STRING, Some(Command::CheckUpdates), "Check for &updates now");
        add(MF_STRING, Some(Command::About), "&About fz-maximize");
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        add(MF_STRING, Some(Command::Exit), "E&xit");

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when clicking elsewhere (KB135788).
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        if let Some(bmp) = dot {
            let _ = DeleteObject(HGDIOBJ(bmp.0));
        }
        Command::from_id(chosen.0 as usize)
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        let nid = self.base();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            for icon in self.icons {
                if !icon.is_invalid() {
                    let _ = DestroyIcon(icon);
                }
            }
        }
    }
}

/// Render the tray icon as straight-alpha ARGB pixels, top-down rows.
/// Active: blue rounded tile with three white zones (25/50/25). Paused: grey tile.
/// `dot`: the update dot in the bottom-right corner.
pub fn render_icon(size: i32, paused: bool, dot: bool) -> Vec<u32> {
    let s = size.max(8);
    let bg: u32 = if paused { 0x0080_8080 } else { 0x0000_78D4 };
    let fg: u32 = if paused { 0x00D0_D0D0 } else { 0x00FF_FFFF };
    let radius = (s / 6).max(2);
    let pad = (s / 8).max(2);
    let gap = (s / 16).max(1);
    let inner = s - 2 * pad;
    let b1 = pad + inner / 4;
    let b2 = pad + inner * 3 / 4;
    let zones = [(pad, b1 - gap), (b1, b2 - gap), (b2, s - pad)];

    let mut px = vec![0u32; (s * s) as usize];
    for y in 0..s {
        for x in 0..s {
            // Rounded-corner mask.
            let cx = if x < radius {
                radius - x
            } else if x >= s - radius {
                x - (s - radius - 1)
            } else {
                0
            };
            let cy = if y < radius {
                radius - y
            } else if y >= s - radius {
                y - (s - radius - 1)
            } else {
                0
            };
            if cx * cx + cy * cy > radius * radius {
                continue;
            }
            let in_zone = y >= pad && y < s - pad && zones.iter().any(|&(l, r)| x >= l && x < r);
            px[(y * s + x) as usize] = 0xFF00_0000 | if in_zone { fg } else { bg };
        }
    }
    if dot {
        let (r, ring) = dot_geometry(s);
        let c = s as f32 - r;
        draw_dot(&mut px, s, c, c, r, ring);
    }
    px
}

/// The update dot's radius (a 7 px dot at 16 px) and the width of the see-through ring
/// around it, for an icon of `s` pixels.
fn dot_geometry(s: i32) -> (f32, f32) {
    let s = s as f32;
    ((s * 0.45).round() / 2.0, (s / 16.0).max(1.0))
}

/// Yellow with a darker gold edge, so the dot shows on light taskbars too.
const DOT_FILL: [f32; 3] = [255.0, 185.0, 0.0];
const DOT_EDGE: [f32; 3] = [168.0, 121.0, 0.0];

/// Draw the dot (radius `r`, centered at `cx`, `cy`) into straight-alpha ARGB pixels and
/// clear a see-through ring `ring` wide around it. Smoothed with 5x5 samples per pixel;
/// pixels clear of the ring are left exactly as they were.
fn draw_dot(px: &mut [u32], s: i32, cx: f32, cy: f32, r: f32, ring: f32) {
    const N: usize = 5;
    let edge = (s as f32 / 16.0).max(1.0);
    for y in 0..s {
        for x in 0..s {
            let (xf, yf) = (x as f32, y as f32);
            // Distance from the center to the pixel's nearest point.
            let near_x = (xf - cx).max(cx - xf - 1.0).max(0.0);
            let near_y = (yf - cy).max(cy - yf - 1.0).max(0.0);
            if near_x.hypot(near_y) > r + ring {
                continue;
            }
            let i = (y * s + x) as usize;
            let p = px[i];
            let tile_a = (p >> 24) as f32 / 255.0;
            let tile = [((p >> 16) & 0xFF) as f32, ((p >> 8) & 0xFF) as f32, (p & 0xFF) as f32];
            let (mut sum, mut alpha) = ([0f32; 3], 0f32);
            for j in 0..N {
                for k in 0..N {
                    let dx = xf + (k as f32 + 0.5) / N as f32 - cx;
                    let dy = yf + (j as f32 + 0.5) / N as f32 - cy;
                    let d = dx.hypot(dy);
                    let (color, a) = if d <= r {
                        (if d > r - edge { DOT_EDGE } else { DOT_FILL }, 1.0)
                    } else if d > r + ring {
                        (tile, tile_a)
                    } else {
                        continue; // the see-through ring
                    };
                    for (total, c) in sum.iter_mut().zip(color) {
                        *total += c * a;
                    }
                    alpha += a;
                }
            }
            px[i] = if alpha > 0.0 {
                let channel = |c: f32| (c / alpha).round().clamp(0.0, 255.0) as u32;
                let a = (alpha / (N * N) as f32 * 255.0).round() as u32;
                (a << 24) | (channel(sum[0]) << 16) | (channel(sum[1]) << 8) | channel(sum[2])
            } else {
                0
            };
        }
    }
}

/// The dot on its own, centered, as straight-alpha ARGB pixels (for the menu).
pub fn render_dot(size: i32) -> Vec<u32> {
    let s = size.max(8);
    let mut px = vec![0u32; (s * s) as usize];
    let c = s as f32 / 2.0;
    draw_dot(&mut px, s, c, c, s as f32 * 0.3, 0.0);
    px
}

fn premultiply(p: u32) -> u32 {
    let a = p >> 24;
    let m = |c: u32| (c * a + 127) / 255;
    (a << 24) | (m((p >> 16) & 0xFF) << 16) | (m((p >> 8) & 0xFF) << 8) | m(p & 0xFF)
}

/// The dot as a menu item's picture: menus draw 32-bit pictures with premultiplied alpha.
fn dot_bitmap(hwnd: HWND) -> Option<HBITMAP> {
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) }.clamp(16, 64);
    let pixels: Vec<u32> = render_dot(size).into_iter().map(premultiply).collect();
    dib(size, &pixels)
}

/// A top-down 32-bit bitmap holding `pixels` (`size` x `size`).
fn dib(size: i32, pixels: &[u32]) -> Option<HBITMAP> {
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            return None;
        }
        let n = pixels.len().min((size * size) as usize);
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u32>(), n);
        Some(bmp)
    }
}

/// The app's icon (no dot), for windows.
pub fn create_icon(size: i32, paused: bool) -> Option<HICON> {
    icon_from_pixels(size, &render_icon(size, paused, false))
}

fn icon_from_pixels(size: i32, pixels: &[u32]) -> Option<HICON> {
    let color = dib(size, pixels)?;
    unsafe {
        // All-zero AND mask: the alpha channel of the color bitmap decides transparency.
        let row_bytes = (((size + 15) / 16) * 2) as usize;
        let mask_bits = vec![0u8; row_bytes * size as usize];
        let mask = CreateBitmap(size, size, 1, 1, Some(mask_bits.as_ptr().cast()));

        let info = ICONINFO {
            fIcon: BOOL(1),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
        icon
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_roundtrip() {
        for c in Command::ALL {
            assert_eq!(Command::from_id(c.id()), Some(c));
        }
        assert_eq!(Command::from_id(0), None);
    }

    #[test]
    fn icon_rendering() {
        for size in [16, 20, 24, 32, 48] {
            let px = render_icon(size, false, false);
            assert_eq!(px.len(), (size * size) as usize);
            // Corners are transparent, center is an opaque white zone pixel.
            assert_eq!(px[0] >> 24, 0, "corner transparent at {size}");
            let center = px[((size / 2) * size + size / 2) as usize];
            assert_eq!(center, 0xFFFF_FFFF, "center is zone at {size}");
            // Left padding column (inside the tile) is background blue.
            let edge = px[((size / 2) * size + 1) as usize];
            assert_eq!(edge, 0xFF00_78D4, "padding is background at {size}");
        }
        let paused = render_icon(16, true, false);
        assert_eq!(paused[(8 * 16 + 1) as usize], 0xFF80_8080);
    }

    #[test]
    fn update_dot() {
        for size in [16, 20, 24, 32, 48] {
            for paused in [false, true] {
                let plain = render_icon(size, paused, false);
                let dot = render_icon(size, paused, true);
                let (r, ring) = dot_geometry(size);
                let c = size as f32 - r;
                let at = |px: &[u32], x: f32, y: f32| px[(y as i32 * size + x as i32) as usize];
                assert_eq!(at(&dot, c, c), 0xFFFF_B900, "yellow center at {size}");
                assert_eq!(at(&dot, c, size as f32 - 0.5), 0xFFA8_7900, "gold edge at {size}");
                assert_eq!(at(&dot, c - r - ring / 2.0, c) >> 24, 0, "see-through ring at {size}");
                let mut changed = 0;
                for y in 0..size {
                    for x in 0..size {
                        let i = (y * size + x) as usize;
                        let far = (x as f32 + 0.5 - c).hypot(y as f32 + 0.5 - c) > r + ring + 1.0;
                        if far {
                            assert_eq!(dot[i], plain[i], "({x}, {y}) changed at {size}");
                        } else if dot[i] != plain[i] {
                            changed += 1;
                        }
                    }
                }
                assert!(changed > 0, "dot drawn at {size}");
            }
        }
    }

    #[test]
    fn menu_dot() {
        for size in [16, 24, 32] {
            let px = render_dot(size);
            let center = (size / 2 * size + size / 2) as usize;
            assert_eq!(px[center], 0xFFFF_B900, "center at {size}");
            assert_eq!(px[0], 0, "corner at {size}");
        }
        assert_eq!(premultiply(0x80FF_B900), 0x8080_5D00);
        assert_eq!(premultiply(0xFFFF_B900), 0xFFFF_B900);
    }
}
