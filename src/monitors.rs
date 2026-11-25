//! Monitor enumeration and identification, mirroring FancyZones'
//! `MonitorUtils::IdentifyMonitors`. The serial number comes from the EDID in the
//! registry (the same value WMI's `WmiMonitorID.SerialNumberID` reports) instead of a
//! WMI query: no COM, no WMI service round-trip.

use std::collections::{HashMap, HashSet};

use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo,
    GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, LPARAM, RECT};
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
    /// Last identity Windows reported for each display (loaded on first use).
    known: Option<Vec<KnownMonitor>>,
}

/// The last identity Windows reported for a display, remembered (and saved to
/// `%LOCALAPPDATA%\fz-maximize\monitors.json`) because Windows sometimes can't report it:
/// on some PCs the monitor's device briefly disappears from every display API (seen on a
/// hybrid-GPU laptop). FancyZones isn't affected (it keeps the identity it read when it
/// started), so falling back to the display name would pick a different FancyZones layout
/// entry than FancyZones uses.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KnownMonitor {
    pub device_name: String,
    /// Monitor bounds when it was identified: a different monitor that takes over the same
    /// display name (after unplugging one) almost always differs here.
    pub bounds: [i32; 4],
    pub id: MonitorId,
}

fn bounds_of(r: &Rect) -> [i32; 4] {
    [r.left, r.top, r.right, r.bottom]
}

/// Record `id` for display `device_name` with `bounds`. True when something changed.
pub fn remember(known: &mut Vec<KnownMonitor>, device_name: &str, bounds: [i32; 4], id: &MonitorId) -> bool {
    let entry = KnownMonitor {
        device_name: device_name.to_string(),
        bounds,
        id: id.clone(),
    };
    match known.iter_mut().find(|k| k.device_name == device_name) {
        Some(k) if *k == entry => false,
        Some(k) => {
            *k = entry;
            true
        }
        None => {
            known.push(entry);
            true
        }
    }
}

/// The remembered identity for display `device_name`, if it still has the same bounds.
pub fn recall(known: &[KnownMonitor], device_name: &str, bounds: [i32; 4]) -> Option<MonitorId> {
    known
        .iter()
        .find(|k| k.device_name == device_name && k.bounds == bounds)
        .map(|k| k.id.clone())
}

fn known_path() -> Option<std::path::PathBuf> {
    crate::logging::log_dir().map(|d| d.join("monitors.json"))
}

impl MonitorCache {
    pub fn clear(&mut self) {
        self.ids.clear();
        self.fallback_logged.clear();
    }

    fn known(&mut self) -> &mut Vec<KnownMonitor> {
        self.known.get_or_insert_with(|| {
            known_path()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default()
        })
    }

    fn save_known(&mut self) {
        let Some(path) = known_path() else { return };
        let text = serde_json::to_string_pretty(self.known()).unwrap_or_default();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
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
                let bounds = bounds_of(&rc_monitor);
                if reliable {
                    self.ids.insert(key, id.clone());
                    if remember(self.known(), &device_name, bounds, &id) {
                        self.save_known();
                    }
                    id
                } else if let Some(known) = recall(self.known(), &device_name, bounds) {
                    // Not cached: the next lookup asks Windows again.
                    if self.fallback_logged.insert(device_name.clone()) {
                        tracing::info!(
                            device_name = %device_name,
                            monitor = %known.device.id,
                            "Windows reports no identity for this monitor right now; using the one it reported last"
                        );
                    }
                    known
                } else {
                    if self.fallback_logged.insert(device_name.clone()) {
                        tracing::info!(
                            device_name = %device_name,
                            "display driver reports no monitor device; using the display name as id (same fallback as FancyZones)"
                        );
                    }
                    id
                }
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

    // EnumDisplayDevices sometimes returns nothing at all (seen on a hybrid-GPU laptop,
    // error 203 for the very first adapter) while FancyZones, which identified the
    // monitor earlier and keeps it, is unaffected. Using the display-name fallback then
    // would pick a different (often stale) FancyZones layout entry, so ask the display
    // configuration API, which reports the same monitor device path.
    if let Some(path) = monitor_path_via_display_config(device_name) {
        let mut device = split_display_device_id(&path);
        if !device.instance_id.is_empty() {
            device.number = number_from_device_name(device_name).unwrap_or(0);
            let serial = read_edid(&device.id, &device.instance_id)
                .and_then(|e| edid_serial(&e))
                .unwrap_or_default();
            tracing::debug!(
                device_name,
                path,
                "monitor identified via the display configuration API"
            );
            return (MonitorId { device, serial }, true);
        }
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

/// The monitor device path (`\\?\DISPLAY#BOE0A52#4&a918e15&0&UID8388688#{...}`, the same
/// string `EnumDisplayDevices` gives with `EDD_GET_DEVICE_INTERFACE_NAME`) of the monitor
/// shown on GDI display `device_name` (`\\.\DISPLAY1`), from the display configuration
/// (CCD) API.
fn monitor_path_via_display_config(device_name: &str) -> Option<String> {
    let (mut paths_len, mut modes_len) = (0u32, 0u32);
    let rc = unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths_len, &mut modes_len) };
    if rc != ERROR_SUCCESS {
        tracing::debug!(rc = rc.0, "GetDisplayConfigBufferSizes failed");
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); paths_len as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); modes_len as usize];
    let rc = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut paths_len,
            paths.as_mut_ptr(),
            &mut modes_len,
            modes.as_mut_ptr(),
            None,
        )
    };
    if rc != ERROR_SUCCESS {
        tracing::debug!(rc = rc.0, "QueryDisplayConfig failed");
        return None;
    }
    paths.truncate(paths_len as usize);
    tracing::debug!(paths = paths.len(), "display configuration paths");
    for p in &paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: p.sourceInfo.adapterId,
                id: p.sourceInfo.id,
            },
            ..Default::default()
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0
            || !from_wide(&source.viewGdiDeviceName).eq_ignore_ascii_case(device_name)
        {
            continue;
        }
        let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: p.targetInfo.adapterId,
                id: p.targetInfo.id,
            },
            ..Default::default()
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } == 0 {
            let path = from_wide(&target.monitorDevicePath);
            if !path.is_empty() {
                return Some(path);
            }
        }
    }
    None
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

    #[test]
    fn known_monitors_are_remembered_and_recalled() {
        let id = MonitorId {
            device: DeviceId {
                id: "BOE0A52".into(),
                instance_id: "4&a918e15&0&UID8388688".into(),
                number: 1,
            },
            serial: "0".into(),
        };
        let mut known = Vec::new();
        let b = [0, 0, 3840, 2400];
        let display1 = r"\\.\DISPLAY1";
        assert!(remember(&mut known, display1, b, &id));
        assert!(!remember(&mut known, display1, b, &id), "unchanged: no save");
        assert_eq!(recall(&known, display1, b), Some(id.clone()));
        assert_eq!(recall(&known, display1, [0, 0, 1920, 1080]), None, "different monitor");
        assert_eq!(recall(&known, r"\\.\DISPLAY2", b), None);
        // A newer identity for the same display replaces the old one.
        let other = MonitorId {
            serial: "7".into(),
            ..id.clone()
        };
        assert!(remember(&mut known, display1, b, &other));
        assert_eq!((known.len(), recall(&known, display1, b)), (1, Some(other)));
        // Survives a save and load (including the backslashes in display names).
        let text = serde_json::to_string(&known).unwrap();
        let back: Vec<KnownMonitor> = serde_json::from_str(&text).unwrap();
        assert_eq!(back, known);
        assert_eq!(back[0].device_name, display1);
    }

    /// Live diagnostic: what Windows reports for every display, and what we end up using.
    #[test]
    #[ignore = "reads the live display configuration"]
    fn diagnose_identification() {
        for h in enumerate_handles() {
            let (name, ..) = monitor_info(h).unwrap();
            let (id, reliable) = identify(&name);
            eprintln!("{name}: Windows reports {id:?} (reliable: {reliable})");
            let path = monitor_path_via_display_config(&name);
            eprintln!("{name}: display configuration path {path:?}");
            let used = MonitorCache::default().describe(h).map(|m| m.desc.id);
            eprintln!("{name}: using {used:?}");
        }
    }

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
