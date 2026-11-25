//! Monitor enumeration and identification, mirroring FancyZones'
//! `MonitorUtils::IdentifyMonitors`. The serial number comes from the EDID in the
//! registry (the same value WMI's `WmiMonitorID.SerialNumberID` reports) instead of a
//! WMI query: no COM, no WMI service round-trip.

use std::collections::{HashMap, HashSet};

use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    DISPLAY_DEVICE_ACTIVE, DISPLAY_DEVICE_MIRRORING_DRIVER, DISPLAY_DEVICEW, EnumDisplayDevicesW, EnumDisplayMonitors,
    GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW, MonitorFromWindow,
};
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_BINARY, RegGetValueW};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{EDD_GET_DEVICE_INTERFACE_NAME, MONITORINFOF_PRIMARY};
use windows::core::{BOOL, HSTRING, PCWSTR};

use crate::fancyzones::MonitorDesc;
use crate::fancyzones::matching::{DeviceId, MonitorId, number_from_device_name, split_display_device_id};
use crate::geometry::Rect;
use crate::win::{from_wide, rect_from};

#[derive(Clone, Debug)]
pub struct LiveMonitor {
    pub primary: bool,
    pub desc: MonitorDesc,
}

/// Caches monitor identities (the expensive part: device enumeration + registry).
/// Geometry and DPI are always read fresh. Cleared on `WM_DISPLAYCHANGE`.
#[derive(Debug, Default)]
pub struct MonitorCache {
    ids: HashMap<(isize, String), MonitorId>,
    /// Displays whose fallback id was already logged (avoid repeating it per maximize).
    fallback_logged: HashSet<String>,
}

impl MonitorCache {
    pub fn clear(&mut self) {
        self.ids.clear();
        self.fallback_logged.clear();
    }

    /// Describe the monitor `hwnd` is on (nearest).
    pub fn for_window(&mut self, hwnd: windows::Win32::Foundation::HWND) -> Option<LiveMonitor> {
        let h = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
        self.describe(h)
    }

    pub fn describe(&mut self, h: HMONITOR) -> Option<LiveMonitor> {
        let (device_name, rc_monitor, rc_work, primary) = monitor_info(h)?;
        let key = (h.0 as isize, device_name.clone());
        let id = match self.ids.get(&key) {
            Some(id) => id.clone(),
            None => {
                let (id, reliable) = identify(&device_name);
                if reliable {
                    self.ids.insert(key, id.clone());
                } else if self.fallback_logged.insert(device_name.clone()) {
                    tracing::info!(
                        device_name = %device_name,
                        "display driver reports no monitor device; using the display name as id (same fallback as FancyZones)"
                    );
                }
                id
            }
        };
        Some(LiveMonitor {
            primary,
            desc: MonitorDesc {
                id,
                device_name,
                rc_monitor,
                rc_work,
                dpi: monitor_dpi(h),
            },
        })
    }

    /// All active monitors.
    pub fn all(&mut self) -> Vec<LiveMonitor> {
        enumerate_handles()
            .into_iter()
            .filter_map(|h| self.describe(h))
            .collect()
    }

    /// True when every active monitor has the same effective DPI.
    pub fn all_same_dpi(&self) -> bool {
        let dpis: Vec<u32> = enumerate_handles().into_iter().map(monitor_dpi).collect();
        dpis.windows(2).all(|w| w[0] == w[1])
    }
}

fn enumerate_handles() -> Vec<HMONITOR> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the &mut Vec passed below, alive for the whole enumeration.
        let v = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        v.push(h);
        BOOL(1)
    }
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut handles as *mut _ as isize));
    }
    handles
}

fn monitor_info(h: HMONITOR) -> Option<(String, Rect, Rect, bool)> {
    let mut mi = MONITORINFOEXW::default();
    mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    let ok = unsafe { GetMonitorInfoW(h, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO) };
    if !ok.as_bool() {
        return None;
    }
    Some((
        from_wide(&mi.szDevice),
        rect_from(mi.monitorInfo.rcMonitor),
        rect_from(mi.monitorInfo.rcWork),
        mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    ))
}

fn monitor_dpi(h: HMONITOR) -> u32 {
    let (mut x, mut y) = (0u32, 0u32);
    match unsafe { GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut x, &mut y) } {
        Ok(()) if x > 0 => x,
        _ => 96,
    }
}

/// Identify a display like FancyZones' `Display::GetDisplays`. Returns the id and whether
/// it is reliable (a real device was found) and therefore cacheable.
fn identify(device_name: &str) -> (MonitorId, bool) {
    let name = HSTRING::from(device_name);
    let mut index = 0u32;
    loop {
        let mut dd = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        let ok = unsafe { EnumDisplayDevicesW(PCWSTR(name.as_ptr()), index, &mut dd, EDD_GET_DEVICE_INTERFACE_NAME) };
        if !ok.as_bool() {
            break;
        }
        let flags = dd.StateFlags;
        if flags.0 & DISPLAY_DEVICE_ACTIVE.0 != 0 && flags.0 & DISPLAY_DEVICE_MIRRORING_DRIVER.0 == 0 {
            let mut device = split_display_device_id(&from_wide(&dd.DeviceID));
            device.number = number_from_device_name(&from_wide(&dd.DeviceName)).unwrap_or(0);
            let serial = read_edid(&device.id, &device.instance_id)
                .and_then(|e| edid_serial(&e))
                .unwrap_or_default();
            return (MonitorId { device, serial }, true);
        }
        index += 1;
    }

    // FancyZones' fallback when no active device is found: the display name itself.
    (
        MonitorId {
            device: DeviceId {
                id: device_name.to_string(),
                instance_id: String::new(),
                number: number_from_device_name(device_name).unwrap_or(0),
            },
            serial: String::new(),
        },
        false,
    )
}

/// `HKLM\SYSTEM\CurrentControlSet\Enum\DISPLAY\<id>\<instance>\Device Parameters\EDID`
fn read_edid(id: &str, instance: &str) -> Option<Vec<u8>> {
    if id.is_empty() || instance.is_empty() || id.contains('\\') || instance.contains('\\') {
        return None;
    }
    let subkey = HSTRING::from(format!(
        r"SYSTEM\CurrentControlSet\Enum\DISPLAY\{id}\{instance}\Device Parameters"
    ));
    let value = HSTRING::from("EDID");
    let mut buf = vec![0u8; 1024];
    let mut size = buf.len() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if rc.is_err() {
        return None;
    }
    buf.truncate(size as usize);
    Some(buf)
}

/// The monitor serial as WMI's `SerialNumberID` reports it: the EDID "display product
/// serial number" descriptor (tag 0xFF) when present, otherwise the 32-bit serial from
/// the EDID header in decimal.
pub fn edid_serial(edid: &[u8]) -> Option<String> {
    const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
    if edid.len() < 128 || edid[..8] != HEADER {
        return None;
    }
    for off in [54usize, 72, 90, 108] {
        let d = &edid[off..off + 18];
        if d[0] == 0 && d[1] == 0 && d[2] == 0 && d[3] == 0xFF {
            let text: String = d[5..18]
                .iter()
                .take_while(|&&b| b != 0x0A && b != 0)
                .map(|&b| b as char)
                .collect();
            let text = text.trim();
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    Some(u32::from_le_bytes([edid[12], edid[13], edid[14], edid[15]]).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_edid(header_serial: u32) -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        e[12..16].copy_from_slice(&header_serial.to_le_bytes());
        // Descriptor 1: detailed timing (non-zero pixel clock) – must be ignored.
        e[54] = 0x01;
        e[55] = 0x1D;
        e
    }

    fn put_text_descriptor(e: &mut [u8], off: usize, tag: u8, text: &str) {
        e[off..off + 5].copy_from_slice(&[0, 0, 0, tag, 0]);
        let mut payload = [0x20u8; 13];
        let bytes = text.as_bytes();
        payload[..bytes.len()].copy_from_slice(bytes);
        if bytes.len() < 13 {
            payload[bytes.len()] = 0x0A;
        }
        e[off + 5..off + 18].copy_from_slice(&payload);
    }

    #[test]
    fn serial_descriptor_wins() {
        let mut e = base_edid(1);
        put_text_descriptor(&mut e, 72, 0xFC, "Odyssey G95NC"); // monitor name, ignored
        put_text_descriptor(&mut e, 90, 0xFF, "HNTX600052");
        assert_eq!(edid_serial(&e).as_deref(), Some("HNTX600052"));
    }

    #[test]
    fn header_serial_when_no_descriptor() {
        assert_eq!(edid_serial(&base_edid(0)).as_deref(), Some("0"));
        assert_eq!(edid_serial(&base_edid(0x0101_0101)).as_deref(), Some("16843009"));
    }

    #[test]
    fn blank_serial_descriptor_falls_back_to_header() {
        let mut e = base_edid(7);
        put_text_descriptor(&mut e, 108, 0xFF, "");
        assert_eq!(edid_serial(&e).as_deref(), Some("7"));
    }

    #[test]
    fn invalid_edid() {
        assert_eq!(edid_serial(&[0u8; 64]), None);
        let mut e = base_edid(1);
        e[0] = 0x12;
        assert_eq!(edid_serial(&e), None);
    }
}
