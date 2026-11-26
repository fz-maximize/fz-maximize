//! Notification-area icon and its context menu. The icon (three zones, 25/50/25) is
//! rendered at runtime at the system small-icon size, so no resource file is needed.

use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject, HGDIOBJ,
};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos, HICON, ICONINFO,
    MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, PostMessageW, SM_CXSMICON, SetForegroundWindow, TPM_BOTTOMALIGN,
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
    Exit,
}

impl Command {
    const ALL: [Command; 9] = [
        Command::Settings,
        Command::TogglePause,
        Command::Reload,
        Command::OpenConfig,
        Command::OpenLogs,
        Command::ToggleAutostart,
        Command::CheckUpdates,
        Command::ShowUpdate,
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
}

pub struct Tray {
    hwnd: HWND,
    callback_msg: u32,
    icon_active: HICON,
    icon_paused: HICON,
    paused: bool,
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
        let tray = Self {
            hwnd,
            callback_msg,
            icon_active: create_icon(size, false).unwrap_or_default(),
            icon_paused: create_icon(size, true).unwrap_or_default(),
            paused: false,
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
        if self.paused {
            self.icon_paused
        } else {
            self.icon_active
        }
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

    pub fn set_state(&mut self, paused: bool, tip: &str) {
        self.paused = paused;
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
        add(MF_STRING | MF_GRAYED, None, &state.status);
        add(MF_STRING | MF_GRAYED, None, &state.check);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        add(MF_STRING, Some(Command::Settings), "&Settings...");
        if let Some(label) = &state.update {
            add(MF_STRING, Some(Command::ShowUpdate), label);
        }
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
        Command::from_id(chosen.0 as usize)
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        let nid = self.base();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            for icon in [self.icon_active, self.icon_paused] {
                if !icon.is_invalid() {
                    let _ = DestroyIcon(icon);
                }
            }
        }
    }
}

/// Render the tray icon as straight-alpha ARGB pixels, top-down rows.
/// Active: blue rounded tile with three white zones (25/50/25). Paused: grey tile.
pub fn render_icon(size: i32, paused: bool) -> Vec<u32> {
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
    px
}

pub fn create_icon(size: i32, paused: bool) -> Option<HICON> {
    let pixels = render_icon(size, paused);
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
        let color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(color.0));
            return None;
        }
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u32>(), pixels.len());

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
            let px = render_icon(size, false);
            assert_eq!(px.len(), (size * size) as usize);
            // Corners are transparent, center is an opaque white zone pixel.
            assert_eq!(px[0] >> 24, 0, "corner transparent at {size}");
            let center = px[((size / 2) * size + size / 2) as usize];
            assert_eq!(center, 0xFFFF_FFFF, "center is zone at {size}");
            // Left padding column (inside the tile) is background blue.
            let edge = px[((size / 2) * size + 1) as usize];
            assert_eq!(edge, 0xFF00_78D4, "padding is background at {size}");
        }
        let paused = render_icon(16, true);
        assert_eq!(paused[(8 * 16 + 1) as usize], 0xFF80_8080);
    }
}
