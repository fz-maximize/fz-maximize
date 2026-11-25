//! Small Win32 conversion helpers shared by the platform modules.

use windows::Win32::Foundation::{HWND, POINT, RECT};

use crate::engine::Hwnd;
use crate::geometry::{Point, Rect};

/// UTF-16 buffer (possibly NUL-terminated) → String.
pub fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

pub const fn rect_from(r: RECT) -> Rect {
    Rect::new(r.left, r.top, r.right, r.bottom)
}

pub const fn rect_to(r: Rect) -> RECT {
    RECT {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

pub const fn point_from(p: POINT) -> Point {
    Point::new(p.x, p.y)
}

pub fn hwnd_key(h: HWND) -> Hwnd {
    h.0 as isize
}

pub fn hwnd_from(k: Hwnd) -> HWND {
    HWND(k as *mut core::ffi::c_void)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings() {
        let w: Vec<u16> = "DISPLAY1\0garbage".encode_utf16().collect();
        assert_eq!(from_wide(&w), "DISPLAY1");
        let w: Vec<u16> = "no-nul".encode_utf16().collect();
        assert_eq!(from_wide(&w), "no-nul");
    }

    #[test]
    fn rect_roundtrip() {
        let r = Rect::new(-10, 5, 300, 400);
        assert_eq!(rect_from(rect_to(r)), r);
        let h = hwnd_from(0x1234);
        assert_eq!(hwnd_key(h), 0x1234);
    }
}
