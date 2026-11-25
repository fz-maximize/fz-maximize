//! Monitor identity and layout lookup, ported from FancyZones
//! (`FancyZonesDataTypes.h` equality operators and `MonitorUtils.cpp` id parsing).

use serde::Serialize;

use super::model::AppliedLayout;

/// `FancyZonesDataTypes::DeviceId`
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DeviceId {
    /// PnP hardware id, e.g. `SAM73CE`, or `\\.\DISPLAY1` for legacy / fallback ids.
    pub id: String,
    /// Device instance, e.g. `5&1498d1d&0&UID4352`.
    pub instance_id: String,
    /// Display number (`\\.\DISPLAY<n>`).
    pub number: i32,
}

/// `FancyZonesDataTypes::MonitorId` (without the live `HMONITOR`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MonitorId {
    pub device: DeviceId,
    /// EDID serial as FancyZones/WMI report it (`SerialNumberID`), may be empty.
    pub serial: String,
}

/// `operator==(DeviceId, DeviceId)`: ids must match; when instance ids differ the
/// display numbers must match instead.
pub fn device_ids_match(a: &DeviceId, b: &DeviceId) -> bool {
    if a.id != b.id {
        return false;
    }
    if a.instance_id != b.instance_id {
        return a.number == b.number;
    }
    true
}

/// `operator==(MonitorId, MonitorId)` for ids without an `HMONITOR`: when both serials
/// are known they must be equal, then the device ids must match.
///
/// `check_serial = false` skips the serial comparison; used as a fallback because
/// FancyZones assigns serials per *hardware id* (identical twin monitors can end up with
/// each other's serial in its data) while we read the true per-instance EDID.
pub fn monitor_ids_match(a: &MonitorId, b: &MonitorId, check_serial: bool) -> bool {
    if check_serial && !a.serial.is_empty() && !b.serial.is_empty() && a.serial != b.serial {
        return false;
    }
    device_ids_match(&a.device, &b.device)
}

/// `MonitorUtils::Display::SplitDisplayDeviceId`.
///
/// `\\?\DISPLAY#GSM1388#4&125707d6&0&UID8388688#{e6f07b5f-...}` → `{GSM1388, 4&125707d6&0&UID8388688}`.
/// Unparseable input becomes the id with an empty instance.
pub fn split_display_device_id(s: &str) -> DeviceId {
    let parts: Option<(usize, usize, usize)> = (|| {
        let name_start = s.find('#')?;
        let uid_start = name_start + 1 + s[name_start + 1..].find('#')?;
        let uid_end = uid_start + 1 + s[uid_start + 1..].find('#')?;
        Some((name_start, uid_start, uid_end))
    })();
    match parts {
        Some((n, u, e)) => DeviceId {
            id: s[n + 1..u].to_string(),
            instance_id: s[u + 1..e].to_string(),
            number: 0,
        },
        None => DeviceId {
            id: s.to_string(),
            ..Default::default()
        },
    }
}

/// `MonitorUtils::Display::ConvertObsoleteDeviceId`: `GSM1388#4&125707d6&0&UID8388688`
/// → `{GSM1388, 4&125707d6&0&UID8388688}`; no `#` → whole string is the id.
pub fn convert_obsolete_device_id(s: &str) -> DeviceId {
    match s.find('#') {
        Some(p) => DeviceId {
            id: s[..p].to_string(),
            instance_id: s[p + 1..].to_string(),
            number: 0,
        },
        None => DeviceId {
            id: s.to_string(),
            ..Default::default()
        },
    }
}

/// Display number from a device name: `\\.\DISPLAY12\Monitor0` → 12, `\\.\DISPLAY3` → 3.
/// FancyZones strips the trailing `\MonitorN` component, then keeps only the digits.
pub fn number_from_device_name(name: &str) -> Option<i32> {
    let trimmed = match name.rfind('\\') {
        // Only strip a trailing component that is not the DISPLAYn part itself.
        Some(p) if name[..p].contains("DISPLAY") => &name[..p],
        _ => name,
    };
    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// How an applied-layout entry was matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MatchQuality {
    /// Matched exactly like FancyZones would.
    Exact,
    /// Matched only after ignoring the serial number.
    IgnoringSerial,
}

/// Find the applied layout for `monitor`, trying each virtual desktop in `desktops`
/// (already normalized GUID strings) in priority order. The first entry in file order
/// wins, matching FancyZones' parse behavior (later duplicates are ignored).
pub fn find_applied<'a>(
    entries: &'a [AppliedLayout],
    monitor: &MonitorId,
    desktops: &[String],
) -> Option<(&'a AppliedLayout, usize, MatchQuality)> {
    for (di, desktop) in desktops.iter().enumerate() {
        for (check_serial, quality) in [(true, MatchQuality::Exact), (false, MatchQuality::IgnoringSerial)] {
            if let Some(e) = entries.iter().find(|e| {
                e.virtual_desktop.eq_ignore_ascii_case(desktop) && monitor_ids_match(&e.monitor, monitor, check_serial)
            }) {
                return Some((e, di, quality));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fancyzones::layout::{LayoutSpec, LayoutType};

    fn dev(id: &str, inst: &str, num: i32) -> DeviceId {
        DeviceId {
            id: id.into(),
            instance_id: inst.into(),
            number: num,
        }
    }

    fn mon(id: &str, inst: &str, num: i32, serial: &str) -> MonitorId {
        MonitorId {
            device: dev(id, inst, num),
            serial: serial.into(),
        }
    }

    fn entry(m: MonitorId, vd: &str, zones: i32) -> AppliedLayout {
        AppliedLayout {
            monitor: m,
            virtual_desktop: vd.into(),
            uuid: "{00000000-0000-0000-0000-000000000000}".into(),
            spec: LayoutSpec {
                kind: LayoutType::PriorityGrid,
                zone_count: zones,
                show_spacing: true,
                spacing: 16,
            },
            sensitivity_radius: 20,
        }
    }

    const VD1: &str = "{6AD31DDE-E515-4558-9D15-21DBE1193F72}";
    const VD2: &str = "{8221805E-1886-4183-996F-8FAD682B24F3}";

    #[test]
    fn split_device_interface_name() {
        let d = split_display_device_id(
            r"\\?\DISPLAY#GSM1388#4&125707d6&0&UID8388688#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}",
        );
        assert_eq!(d, dev("GSM1388", "4&125707d6&0&UID8388688", 0));
        let d = split_display_device_id("garbage");
        assert_eq!(d, dev("garbage", "", 0));
        let d = split_display_device_id(r"\\?\DISPLAY#ONLYONE");
        assert_eq!(d.instance_id, "");
    }

    #[test]
    fn obsolete_device_ids() {
        assert_eq!(
            convert_obsolete_device_id("GSM1388#4&125707d6&0&UID8388688"),
            dev("GSM1388", "4&125707d6&0&UID8388688", 0)
        );
        assert_eq!(convert_obsolete_device_id(r"\\.\DISPLAY1"), dev(r"\\.\DISPLAY1", "", 0));
    }

    #[test]
    fn display_numbers() {
        assert_eq!(number_from_device_name(r"\\.\DISPLAY1\Monitor0"), Some(1));
        assert_eq!(number_from_device_name(r"\\.\DISPLAY12\Monitor3"), Some(12));
        assert_eq!(number_from_device_name(r"\\.\DISPLAY5"), Some(5));
        assert_eq!(number_from_device_name("nothing"), None);
    }

    #[test]
    fn device_equality_semantics() {
        // Same id + instance → equal regardless of number.
        assert!(device_ids_match(&dev("SAM", "A", 1), &dev("SAM", "A", 5)));
        // Different instance → falls back to number.
        assert!(device_ids_match(&dev("SAM", "A", 5), &dev("SAM", "B", 5)));
        assert!(!device_ids_match(&dev("SAM", "A", 1), &dev("SAM", "B", 5)));
        // Different id → never equal.
        assert!(!device_ids_match(&dev("SAM", "A", 1), &dev("GSM", "A", 1)));
    }

    #[test]
    fn serial_rules() {
        let a = mon("SAM", "A", 1, "S1");
        assert!(monitor_ids_match(&a, &mon("SAM", "A", 1, "S1"), true));
        assert!(!monitor_ids_match(&a, &mon("SAM", "A", 1, "S2"), true));
        assert!(monitor_ids_match(&a, &mon("SAM", "A", 1, "S2"), false));
        // Empty serial on either side skips the check.
        assert!(monitor_ids_match(&a, &mon("SAM", "A", 1, ""), true));
    }

    #[test]
    fn lookup_prefers_desktop_order_then_file_order() {
        let live = mon("SAM73CE", "5&1&0&UID4352", 5, "SERIAL1");
        let entries = vec![
            entry(mon("SAM73CE", "5&1&0&UID4352", 5, "SERIAL1"), VD2, 4),
            entry(mon("SAM73CE", "5&1&0&UID4352", 5, "SERIAL1"), VD1, 3),
            entry(mon("SAM73CE", "5&1&0&UID4352", 5, "SERIAL1"), VD1, 7), // duplicate, ignored
        ];
        let (e, di, q) = find_applied(&entries, &live, &[VD1.into(), VD2.into()]).unwrap();
        assert_eq!(e.spec.zone_count, 3);
        assert_eq!(di, 0);
        assert_eq!(q, MatchQuality::Exact);

        let (e, di, _) = find_applied(&entries, &live, &["{nope}".into(), VD2.into()]).unwrap();
        assert_eq!(e.spec.zone_count, 4);
        assert_eq!(di, 1);

        assert!(find_applied(&entries, &live, &["{nope}".into()]).is_none());
    }

    #[test]
    fn lookup_is_case_insensitive_on_desktop_guid() {
        let live = mon("BOE0A52", "4&a&0&UID1", 1, "0");
        let entries = vec![entry(live.clone(), VD1, 3)];
        assert!(find_applied(&entries, &live, &[VD1.to_lowercase()]).is_some());
    }

    #[test]
    fn lookup_falls_back_to_ignoring_serial() {
        let live = mon("SAM", "A", 1, "REAL");
        let entries = vec![entry(mon("SAM", "A", 1, "TWIN"), VD1, 3)];
        let (_, _, q) = find_applied(&entries, &live, &[VD1.into()]).unwrap();
        assert_eq!(q, MatchQuality::IgnoringSerial);
    }

    #[test]
    fn lookup_does_not_confuse_different_models_on_same_port() {
        // SAM73CE / SAM73D2 share an instance path but are different monitors.
        let live = mon("SAM73D2", "5&1&0&UID4352", 5, "X");
        let entries = vec![entry(mon("SAM73CE", "5&1&0&UID4352", 5, "X"), VD1, 3)];
        assert!(find_applied(&entries, &live, &[VD1.into()]).is_none());
    }
}
