//! Pure app / monitor filtering rules.

use crate::fancyzones::MonitorDesc;
use crate::geometry::Rect;

/// FancyZones' `find_app_name_in_path` (PowerToys `common/utils/excluded_apps.h`): the
/// last occurrence of the entry must overlap the start of the file name, so `NOTEPAD`
/// matches `C:\WINDOWS\NOTEPAD.EXE` but `PAD.EXE` does not. Inputs are upper-cased.
pub fn find_app_name_in_path(path: &str, apps: &[String]) -> bool {
    let Some(last_slash) = path.rfind('\\') else {
        return false;
    };
    apps.iter().any(|row| match path.rfind(row.as_str()) {
        Some(pos) => pos <= last_slash + 1 && pos + row.len() > last_slash,
        None => false,
    })
}

/// FancyZones' `check_excluded_app`: path match, else window-title substring match.
/// `apps` must be upper-cased; `path` and `title` are upper-cased here.
pub fn is_app_excluded(path: &str, title: &str, apps: &[String]) -> bool {
    if apps.is_empty() {
        return false;
    }
    let path = path.to_uppercase();
    if find_app_name_in_path(&path, apps) {
        return true;
    }
    let title = title.to_uppercase();
    !title.is_empty() && apps.iter().any(|a| title.contains(a.as_str()))
}

/// Upper-case and trim a user-provided list, dropping empty entries.
pub fn normalize_list(items: &[String]) -> Vec<String> {
    items
        .iter()
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// True when the monitor matches any `excluded_monitors` entry.
pub fn is_monitor_excluded(list: &[String], m: &MonitorDesc) -> bool {
    list.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).any(|e| {
        e.eq_ignore_ascii_case(&m.id.device.id)
            || (!m.id.device.instance_id.is_empty() && e.eq_ignore_ascii_case(&m.id.device.instance_id))
            || e.eq_ignore_ascii_case(&m.device_name)
            || (!m.id.serial.is_empty() && e.eq_ignore_ascii_case(&m.id.serial))
    })
}

/// True when the monitor passes the `min_aspect_ratio` filter.
pub fn aspect_ratio_ok(min: f64, monitor: &Rect) -> bool {
    if min <= 0.0 || monitor.height() <= 0 {
        return true;
    }
    f64::from(monitor.width()) / f64::from(monitor.height()) >= min
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fancyzones::matching::{DeviceId, MonitorId};

    fn list(items: &[&str]) -> Vec<String> {
        normalize_list(&items.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn app_name_matching_follows_fancyzones() {
        let apps = list(&["notepad", "code.exe"]);
        assert!(find_app_name_in_path(r"C:\WINDOWS\NOTEPAD.EXE", &apps));
        assert!(find_app_name_in_path(r"C:\APPS\VSCODE\CODE.EXE", &apps));
        assert!(
            !find_app_name_in_path(r"C:\APPS\MYNOTEPAD.EXE", &apps),
            "must start at the file name"
        );
        assert!(
            !find_app_name_in_path(r"C:\NOTEPAD\OTHER.EXE", &apps),
            "directory names don't count"
        );
        assert!(
            !find_app_name_in_path("NOTEPAD.EXE", &apps),
            "no backslash never matches"
        );
        let with_dir = list(&[r"windows\notepad"]);
        assert!(find_app_name_in_path(r"C:\WINDOWS\NOTEPAD.EXE", &with_dir));
    }

    #[test]
    fn app_exclusion_by_path_or_title() {
        let apps = list(&["spotify", "Remote Desktop"]);
        assert!(is_app_excluded(
            r"C:\Users\x\AppData\Roaming\Spotify\Spotify.exe",
            "Song",
            &apps
        ));
        assert!(is_app_excluded(
            r"C:\Windows\System32\mstsc.exe",
            "server - Remote Desktop Connection",
            &apps
        ));
        assert!(!is_app_excluded(r"C:\Windows\notepad.exe", "Untitled - Notepad", &apps));
        assert!(!is_app_excluded(r"C:\a\b.exe", "", &[]));
    }

    #[test]
    fn normalize_drops_blanks() {
        assert_eq!(list(&["  a ", "", "  ", "B"]), vec!["A", "B"]);
    }

    fn desc() -> MonitorDesc {
        MonitorDesc {
            id: MonitorId {
                device: DeviceId {
                    id: "BOE0A52".into(),
                    instance_id: "4&a918e15&0&UID8388688".into(),
                    number: 1,
                },
                serial: "0".into(),
            },
            device_name: r"\\.\DISPLAY1".into(),
            rc_monitor: Rect::new(0, 0, 1920, 1200),
            rc_work: Rect::new(0, 0, 1920, 1152),
            dpi: 120,
        }
    }

    #[test]
    fn monitor_exclusion() {
        let m = desc();
        assert!(is_monitor_excluded(&["boe0a52".into()], &m));
        assert!(is_monitor_excluded(&[r"\\.\display1".into()], &m));
        assert!(is_monitor_excluded(&["4&A918E15&0&UID8388688".into()], &m));
        assert!(is_monitor_excluded(&["0".into()], &m));
        assert!(!is_monitor_excluded(&["SAM73CE".into(), "".into()], &m));
        assert!(!is_monitor_excluded(&[], &m));
    }

    #[test]
    fn aspect_ratio_filter() {
        let ultrawide = Rect::new(0, 0, 7680, 2160); // 3.56
        let laptop = Rect::new(0, 0, 1920, 1200); // 1.6
        assert!(aspect_ratio_ok(0.0, &laptop));
        assert!(aspect_ratio_ok(2.0, &ultrawide));
        assert!(!aspect_ratio_ok(2.0, &laptop));
        assert!(aspect_ratio_ok(1.6, &laptop));
    }
}
