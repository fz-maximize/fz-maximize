//! Per-monitor policy: should maximize be redirected on this monitor, and to which zones?
//! Shared by the live maximize path and `--dump` so both always agree.

use crate::config::Config;
use crate::engine::NativeReason;
use crate::fancyzones::{FzView, MonitorDesc, Resolution, resolve};
use crate::filters::{aspect_ratio_ok, is_monitor_excluded};

/// Resolve the zones for `monitor`, or the reason maximize stays native there.
pub fn monitor_zones(
    cfg: &Config,
    fz: &FzView<'_>,
    monitor: &MonitorDesc,
    desktops: &[String],
) -> (Option<Resolution>, Result<(), NativeReason>) {
    if cfg.require_fancyzones_enabled && !fz.enabled {
        return (None, Err(NativeReason::FancyZonesDisabled));
    }
    if is_monitor_excluded(&cfg.excluded_monitors, monitor)
        || !aspect_ratio_ok(cfg.min_aspect_ratio, &monitor.rc_monitor)
    {
        // Still resolve so diagnostics can show what FancyZones has there.
        let r = resolve(fz, monitor, desktops, cfg.use_fancyzones_default_layout);
        return (r, Err(NativeReason::MonitorFiltered));
    }
    let Some(r) = resolve(fz, monitor, desktops, cfg.use_fancyzones_default_layout) else {
        return (None, Err(NativeReason::NoLayout));
    };
    if let Some(e) = &r.error {
        let e = e.clone();
        return (Some(r), Err(NativeReason::LayoutError(e)));
    }
    if !r.is_multi_zone() {
        return (Some(r), Err(NativeReason::SingleZone));
    }
    (Some(r), Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fancyzones::matching::{DeviceId, MonitorId};
    use crate::fancyzones::model::{self, DefaultLayouts, FzSettings};
    use crate::geometry::Rect;
    use std::collections::HashMap;

    const VD1: &str = "{6AD31DDE-E515-4558-9D15-21DBE1193F72}";

    fn applied() -> Vec<model::AppliedLayout> {
        model::parse_applied_layouts(include_str!("../tests/fixtures/applied-layouts.json"))
            .unwrap()
            .0
    }

    fn view<'a>(
        applied: &'a [model::AppliedLayout],
        custom: &'a HashMap<String, model::NamedCustomLayout>,
        defaults: &'a DefaultLayouts,
        settings: &'a FzSettings,
        enabled: bool,
    ) -> FzView<'a> {
        FzView {
            applied,
            custom,
            defaults,
            settings,
            last_used_vd: None,
            enabled,
            powertoys_version: None,
        }
    }

    fn ultrawide() -> MonitorDesc {
        MonitorDesc {
            id: MonitorId {
                device: DeviceId {
                    id: "SAM73CE".into(),
                    instance_id: "5&1498d1d&0&UID4352".into(),
                    number: 5,
                },
                serial: "TESTSERIAL01".into(),
            },
            device_name: r"\\.\DISPLAY5".into(),
            rc_monitor: Rect::new(0, 0, 7680, 2160),
            rc_work: Rect::new(0, 0, 7680, 2112),
            dpi: 96,
        }
    }

    fn laptop() -> MonitorDesc {
        MonitorDesc {
            id: MonitorId {
                device: DeviceId {
                    id: "TCL0000".into(),
                    instance_id: "4&a918e15&0&UID20549".into(),
                    number: 1,
                },
                serial: "0".into(),
            },
            device_name: r"\\.\DISPLAY1".into(),
            rc_monitor: Rect::new(-1920, 0, 0, 1080),
            rc_work: Rect::new(-1920, 0, 0, 1040),
            dpi: 96,
        }
    }

    #[test]
    fn policy_matrix() {
        let a = applied();
        let custom = HashMap::new();
        let defaults = DefaultLayouts::default();
        let settings = FzSettings::default();
        let fz = view(&a, &custom, &defaults, &settings, true);
        let desks = vec![VD1.to_string()];
        let cfg = Config::default();

        let (r, ok) = monitor_zones(&cfg, &fz, &ultrawide(), &desks);
        assert_eq!(ok, Ok(()));
        assert_eq!(r.unwrap().zones.len(), 3);

        // FancyZones disabled.
        let off = view(&a, &custom, &defaults, &settings, false);
        assert_eq!(
            monitor_zones(&cfg, &off, &ultrawide(), &desks).1,
            Err(NativeReason::FancyZonesDisabled)
        );
        let lenient = Config {
            require_fancyzones_enabled: false,
            ..Config::default()
        };
        assert_eq!(monitor_zones(&lenient, &off, &ultrawide(), &desks).1, Ok(()));

        // Aspect-ratio filter keeps the ultrawide, drops a 16:9 screen.
        let ultra_only = Config {
            min_aspect_ratio: 2.0,
            ..Config::default()
        };
        assert_eq!(monitor_zones(&ultra_only, &fz, &ultrawide(), &desks).1, Ok(()));
        let (r, res) = monitor_zones(
            &ultra_only,
            &fz,
            &laptop(),
            &["{8221805E-1886-4183-996F-8FAD682B24F3}".into()],
        );
        assert_eq!(res, Err(NativeReason::MonitorFiltered));
        assert!(
            r.is_some(),
            "filtered monitors still report their layout for diagnostics"
        );

        // Explicit monitor exclusion.
        let excl = Config {
            excluded_monitors: vec!["sam73ce".into()],
            ..Config::default()
        };
        assert_eq!(
            monitor_zones(&excl, &fz, &ultrawide(), &desks).1,
            Err(NativeReason::MonitorFiltered)
        );

        // No entry for this desktop and defaults disabled → no layout.
        let no_default = Config {
            use_fancyzones_default_layout: false,
            ..Config::default()
        };
        assert_eq!(
            monitor_zones(&no_default, &fz, &laptop(), &desks).1,
            Err(NativeReason::NoLayout)
        );
    }

    #[test]
    fn custom_layout_missing_is_a_layout_error() {
        let a = applied();
        let custom = HashMap::new(); // GSM0001 entry references a custom layout we don't have
        let defaults = DefaultLayouts::default();
        let settings = FzSettings::default();
        let fz = view(&a, &custom, &defaults, &settings, true);
        let m = MonitorDesc {
            id: MonitorId {
                device: DeviceId {
                    id: "GSM0001".into(),
                    instance_id: "4&a918e15&0&UID20549".into(),
                    number: 2,
                },
                serial: "16843009".into(),
            },
            device_name: r"\\.\DISPLAY2".into(),
            rc_monitor: Rect::new(0, 0, 2560, 1440),
            rc_work: Rect::new(0, 0, 2560, 1400),
            dpi: 96,
        };
        let (_, res) = monitor_zones(&Config::default(), &fz, &m, &[VD1.into()]);
        assert!(matches!(res, Err(NativeReason::LayoutError(_))), "{res:?}");
    }
}
