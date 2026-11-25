//! Parsers for the FancyZones / PowerToys JSON files. Lenient in the same way FancyZones
//! is: malformed entries are skipped individually instead of failing the whole file.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

use super::layout::{CanvasInfo, CanvasZone, CustomLayout, GridInfo, LayoutSpec, LayoutType};
use super::matching::{DeviceId, MonitorId, convert_obsolete_device_id};

pub const NULL_GUID: &str = "{00000000-0000-0000-0000-000000000000}";

/// Normalize a GUID string to `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}` (uppercase,
/// braced). Returns `None` when the input is not a GUID.
pub fn normalize_guid(s: &str) -> Option<String> {
    let t = s.trim();
    let inner = t.strip_prefix('{').and_then(|x| x.strip_suffix('}')).unwrap_or(t);
    let groups: Vec<&str> = inner.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    if groups.len() != 5
        || groups
            .iter()
            .zip(lens)
            .any(|(g, l)| g.len() != l || !g.chars().all(|c| c.is_ascii_hexdigit()))
    {
        return None;
    }
    Some(format!("{{{}}}", inner.to_ascii_uppercase()))
}

/// Format a little-endian Win32 `GUID` (16 raw bytes, as stored in the registry).
pub fn guid_from_le_bytes(b: &[u8]) -> Option<String> {
    if b.len() < 16 {
        return None;
    }
    let d1 = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let d2 = u16::from_le_bytes([b[4], b[5]]);
    let d3 = u16::from_le_bytes([b[6], b[7]]);
    Some(format!(
        "{{{d1:08X}-{d2:04X}-{d3:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    ))
}

/// One entry of `applied-layouts.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedLayout {
    pub monitor: MonitorId,
    /// Normalized GUID string.
    pub virtual_desktop: String,
    /// Normalized GUID string (identifies the custom layout for `type = custom`).
    pub uuid: String,
    pub spec: LayoutSpec,
    pub sensitivity_radius: i32,
}

fn num_i32(v: &Value) -> Option<i32> {
    v.as_f64().map(|f| f as i32)
}

fn layout_spec_from(obj: &Value) -> Option<(String, LayoutSpec, i32)> {
    let uuid = normalize_guid(obj.get("uuid")?.as_str()?)?;
    let kind = LayoutType::from_fz_str(obj.get("type")?.as_str()?);
    let show_spacing = obj.get("show-spacing")?.as_bool()?;
    let spacing = num_i32(obj.get("spacing")?)?;
    let zone_count = num_i32(obj.get("zone-count")?)?;
    let sensitivity = obj.get("sensitivity-radius").and_then(num_i32).unwrap_or(20);
    Some((
        uuid,
        LayoutSpec {
            kind,
            zone_count,
            show_spacing,
            spacing,
        },
        sensitivity,
    ))
}

fn applied_entry_from(v: &Value) -> Option<AppliedLayout> {
    // Only the current ("device" object) format is supported; the pre-0.60 "device-id"
    // string format embedded the resolution and is long obsolete.
    let device = v.get("device")?;
    let monitor = device.get("monitor")?.as_str()?;
    let instance = device.get("monitor-instance").and_then(Value::as_str).unwrap_or("");
    let serial = device.get("serial-number").and_then(Value::as_str).unwrap_or("");
    let number = device.get("monitor-number").and_then(num_i32).unwrap_or(0);
    let vd = normalize_guid(device.get("virtual-desktop")?.as_str()?)?;

    let device_id = if instance.is_empty() {
        convert_obsolete_device_id(monitor)
    } else {
        DeviceId {
            id: monitor.to_string(),
            instance_id: instance.to_string(),
            number,
        }
    };
    let (uuid, spec, sensitivity_radius) = layout_spec_from(v.get("applied-layout")?)?;
    Some(AppliedLayout {
        monitor: MonitorId {
            device: device_id,
            serial: serial.to_string(),
        },
        virtual_desktop: vd,
        uuid,
        spec,
        sensitivity_radius,
    })
}

/// Parse `applied-layouts.json`. Returns the entries and the number skipped as malformed.
pub fn parse_applied_layouts(json: &str) -> Result<(Vec<AppliedLayout>, usize), serde_json::Error> {
    let root: Value = serde_json::from_str(json)?;
    let arr = root
        .get("applied-layouts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(arr.len());
    let mut skipped = 0;
    for v in &arr {
        match applied_entry_from(v) {
            Some(e) => out.push(e),
            None => skipped += 1,
        }
    }
    Ok((out, skipped))
}

fn int_vec(v: &Value) -> Option<Vec<i32>> {
    v.as_array()?.iter().map(num_i32).collect()
}

fn grid_info_from(info: &Value) -> Option<GridInfo> {
    let rows = usize::try_from(num_i32(info.get("rows")?)?).ok()?;
    let columns = usize::try_from(num_i32(info.get("columns")?)?).ok()?;
    let rows_percents = int_vec(info.get("rows-percentage")?)?;
    let columns_percents = int_vec(info.get("columns-percentage")?)?;
    let cell_child_map: Vec<Vec<i32>> = info
        .get("cell-child-map")?
        .as_array()?
        .iter()
        .map(int_vec)
        .collect::<Option<_>>()?;
    let g = GridInfo {
        rows,
        columns,
        rows_percents,
        columns_percents,
        cell_child_map,
    };
    g.is_well_formed().then_some(g)
}

fn canvas_info_from(info: &Value) -> Option<CanvasInfo> {
    let ref_width = num_i32(info.get("ref-width")?)?;
    let ref_height = num_i32(info.get("ref-height")?)?;
    let zones = info
        .get("zones")?
        .as_array()?
        .iter()
        .map(|z| {
            Some(CanvasZone {
                x: num_i32(z.get("X")?)?,
                y: num_i32(z.get("Y")?)?,
                width: num_i32(z.get("width")?)?,
                height: num_i32(z.get("height")?)?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(CanvasInfo {
        ref_width,
        ref_height,
        zones,
    })
}

/// A custom layout plus its display name (for diagnostics).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedCustomLayout {
    pub name: String,
    pub layout: CustomLayout,
}

/// Parse `custom-layouts.json` into a map keyed by normalized uuid.
pub fn parse_custom_layouts(json: &str) -> Result<(HashMap<String, NamedCustomLayout>, usize), serde_json::Error> {
    let root: Value = serde_json::from_str(json)?;
    let arr = root
        .get("custom-layouts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = HashMap::new();
    let mut skipped = 0;
    for v in &arr {
        let parsed = (|| {
            let uuid = normalize_guid(v.get("uuid")?.as_str()?)?;
            let name = v.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let info = v.get("info")?;
            let layout = match v.get("type")?.as_str()? {
                "canvas" => CustomLayout::Canvas(canvas_info_from(info)?),
                "grid" => CustomLayout::Grid(grid_info_from(info)?),
                _ => return None,
            };
            Some((uuid, NamedCustomLayout { name, layout }))
        })();
        match parsed {
            Some((uuid, l)) => {
                out.entry(uuid).or_insert(l);
            }
            None => skipped += 1,
        }
    }
    Ok((out, skipped))
}

/// FancyZones' default layouts for monitors that have no applied layout yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultLayouts {
    pub horizontal: (String, LayoutSpec),
    pub vertical: (String, LayoutSpec),
}

impl Default for DefaultLayouts {
    /// Built-in defaults (`DefaultLayouts::GetDefaultLayout` / `LayoutDefaults.h`).
    fn default() -> Self {
        Self {
            horizontal: (
                NULL_GUID.into(),
                LayoutSpec {
                    kind: LayoutType::PriorityGrid,
                    zone_count: 3,
                    show_spacing: true,
                    spacing: 16,
                },
            ),
            vertical: (
                NULL_GUID.into(),
                LayoutSpec {
                    kind: LayoutType::Rows,
                    zone_count: 3,
                    show_spacing: true,
                    spacing: 16,
                },
            ),
        }
    }
}

/// Parse `default-layouts.json` on top of the built-in defaults.
pub fn parse_default_layouts(json: &str) -> Result<DefaultLayouts, serde_json::Error> {
    let root: Value = serde_json::from_str(json)?;
    let mut d = DefaultLayouts::default();
    for v in root
        .get("default-layouts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(layout) = v.get("layout") else { continue };
        let Some(kind) = layout.get("type").and_then(Value::as_str) else {
            continue;
        };
        let uuid = layout
            .get("uuid")
            .and_then(Value::as_str)
            .and_then(normalize_guid)
            .unwrap_or_else(|| NULL_GUID.to_string());
        let spec = LayoutSpec {
            kind: LayoutType::from_fz_str(kind),
            show_spacing: layout.get("show-spacing").and_then(Value::as_bool).unwrap_or(true),
            spacing: layout.get("spacing").and_then(num_i32).unwrap_or(16),
            zone_count: layout.get("zone-count").and_then(num_i32).unwrap_or(3),
        };
        match v.get("monitor-configuration").and_then(Value::as_str) {
            Some("vertical") => d.vertical = (uuid, spec),
            _ => d.horizontal = (uuid, spec),
        }
    }
    Ok(d)
}

/// The FancyZones module settings we honor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FzSettings {
    /// Upper-cased, trimmed entries of "Excluded apps".
    pub excluded_apps: Vec<String>,
    pub allow_child_window_snap: bool,
    pub allow_popup_window_snap: bool,
}

/// Parse `FancyZones/settings.json`.
pub fn parse_fz_settings(json: &str) -> Result<FzSettings, serde_json::Error> {
    let root: Value = serde_json::from_str(json)?;
    let prop = |name: &str| {
        root.get("properties")
            .and_then(|p| p.get(name))
            .and_then(|p| p.get("value"))
    };
    let excluded = prop("fancyzones_excluded_apps").and_then(Value::as_str).unwrap_or("");
    Ok(FzSettings {
        excluded_apps: split_excluded_apps(excluded),
        allow_child_window_snap: prop("fancyzones_allowChildWindowSnap")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        allow_popup_window_snap: prop("fancyzones_allowPopupWindowSnap")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// FancyZones' excluded-apps parsing: upper-case, split on CR/LF, trim, drop empties.
pub fn split_excluded_apps(s: &str) -> Vec<String> {
    s.to_uppercase()
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse `last-used-virtual-desktop.json`.
pub fn parse_last_used_desktop(json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct F {
        #[serde(rename = "last-used-virtual-desktop")]
        id: String,
    }
    serde_json::from_str::<F>(json).ok().and_then(|f| normalize_guid(&f.id))
}

/// The installed PowerToys version from the global `settings.json` (`powertoys_version`).
pub fn parse_powertoys_version(json: &str) -> Option<String> {
    serde_json::from_str::<Value>(json)
        .ok()?
        .get("powertoys_version")?
        .as_str()
        .map(str::to_string)
}

/// Parse the global PowerToys `settings.json` and report whether FancyZones is enabled.
/// Missing information counts as enabled (we only opt out on an explicit `false`).
pub fn parse_fancyzones_enabled(json: &str) -> bool {
    serde_json::from_str::<Value>(json)
        .ok()
        .and_then(|v| v.get("enabled")?.get("FancyZones")?.as_bool())
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPLIED_FIXTURE: &str = include_str!("../../tests/fixtures/applied-layouts.json");
    const CUSTOM_FIXTURE: &str = include_str!("../../tests/fixtures/custom-layouts.json");

    #[test]
    fn guid_normalization() {
        assert_eq!(
            normalize_guid("{6ad31dde-e515-4558-9d15-21dbe1193f72}").as_deref(),
            Some("{6AD31DDE-E515-4558-9D15-21DBE1193F72}")
        );
        assert_eq!(
            normalize_guid(" 6AD31DDE-E515-4558-9D15-21DBE1193F72 ").as_deref(),
            Some("{6AD31DDE-E515-4558-9D15-21DBE1193F72}")
        );
        assert_eq!(normalize_guid("{not-a-guid}"), None);
        assert_eq!(normalize_guid(""), None);
        assert_eq!(normalize_guid("{6AD31DDE-E515-4558-9D15-21DBE1193F7Z}"), None);
    }

    #[test]
    fn guid_from_registry_bytes() {
        // {6AD31DDE-E515-4558-9D15-21DBE1193F72} in Win32 GUID memory layout.
        let bytes = [
            0xDE, 0x1D, 0xD3, 0x6A, 0x15, 0xE5, 0x58, 0x45, 0x9D, 0x15, 0x21, 0xDB, 0xE1, 0x19, 0x3F, 0x72,
        ];
        assert_eq!(
            guid_from_le_bytes(&bytes).as_deref(),
            Some("{6AD31DDE-E515-4558-9D15-21DBE1193F72}")
        );
        assert_eq!(guid_from_le_bytes(&bytes[..8]), None);
    }

    #[test]
    fn parses_real_world_applied_layouts() {
        let (entries, skipped) = parse_applied_layouts(APPLIED_FIXTURE).unwrap();
        assert_eq!(skipped, 1, "the deliberately malformed entry is skipped");
        assert_eq!(entries.len(), 7);

        let legacy = &entries[0];
        assert_eq!(legacy.monitor.device.id, r"\\.\DISPLAY1");
        assert_eq!(legacy.monitor.device.instance_id, "");

        let ultrawide = entries.iter().find(|e| e.monitor.device.id == "SAM73CE").unwrap();
        assert_eq!(ultrawide.monitor.device.instance_id, "5&1498d1d&0&UID4352");
        assert_eq!(ultrawide.monitor.device.number, 5);
        assert_eq!(ultrawide.monitor.serial, "TESTSERIAL01");
        assert_eq!(ultrawide.virtual_desktop, "{BC41CE51-0EC0-4EC5-A579-8917EFEF2FA5}");
        assert_eq!(ultrawide.spec.kind, LayoutType::PriorityGrid);
        assert_eq!(ultrawide.spec.zone_count, 3);
        assert!(ultrawide.spec.show_spacing);
        assert_eq!(ultrawide.spec.spacing, 16);
        assert_eq!(ultrawide.sensitivity_radius, 20);

        let custom = entries.iter().find(|e| e.spec.kind == LayoutType::Custom).unwrap();
        assert_eq!(custom.uuid, "{A1B2C3D4-0000-4000-8000-000000000001}");

        let blank = entries.iter().find(|e| e.spec.kind == LayoutType::Blank).unwrap();
        assert_eq!(blank.monitor.device.id, "BOE0A52");
    }

    #[test]
    fn applied_layouts_tolerates_missing_array_and_rejects_bad_json() {
        assert_eq!(parse_applied_layouts("{}").unwrap().0.len(), 0);
        assert!(parse_applied_layouts("{not json").is_err());
    }

    #[test]
    fn parses_custom_layouts() {
        let (map, skipped) = parse_custom_layouts(CUSTOM_FIXTURE).unwrap();
        assert_eq!(skipped, 1);
        let grid = &map["{A1B2C3D4-0000-4000-8000-000000000001}"];
        assert_eq!(grid.name, "Ultrawide 25/50/25 split");
        match &grid.layout {
            CustomLayout::Grid(g) => {
                assert_eq!((g.rows, g.columns), (2, 3));
                assert_eq!(g.columns_percents, vec![2500, 5000, 2500]);
                assert_eq!(g.cell_child_map, vec![vec![0, 1, 2], vec![3, 1, 4]]);
            }
            other => panic!("expected grid, got {other:?}"),
        }
        let canvas = &map["{A1B2C3D4-0000-4000-8000-000000000002}"];
        match &canvas.layout {
            CustomLayout::Canvas(c) => {
                assert_eq!((c.ref_width, c.ref_height), (3840, 1080));
                assert_eq!(c.zones.len(), 3);
                assert_eq!(
                    c.zones[1],
                    CanvasZone {
                        x: 960,
                        y: 0,
                        width: 1920,
                        height: 1080
                    }
                );
            }
            other => panic!("expected canvas, got {other:?}"),
        }
    }

    #[test]
    fn default_layouts_parse_and_fallback() {
        let d = parse_default_layouts("{}").unwrap();
        assert_eq!(d, DefaultLayouts::default());
        let d = parse_default_layouts(
            r#"{"default-layouts":[{"monitor-configuration":"vertical","layout":{"type":"columns","zone-count":2,"show-spacing":false,"spacing":4}}]}"#,
        )
        .unwrap();
        assert_eq!(d.vertical.1.kind, LayoutType::Columns);
        assert_eq!(d.vertical.1.zone_count, 2);
        assert!(!d.vertical.1.show_spacing);
        assert_eq!(d.horizontal.1.kind, LayoutType::PriorityGrid);
    }

    #[test]
    fn fz_settings_and_excluded_apps() {
        let s = parse_fz_settings(
            r#"{"properties":{"fancyzones_excluded_apps":{"value":"  notepad.exe\r\nSpotify\n\n  Calc  "},"fancyzones_allowChildWindowSnap":{"value":true}}}"#,
        )
        .unwrap();
        assert_eq!(s.excluded_apps, vec!["NOTEPAD.EXE", "SPOTIFY", "CALC"]);
        assert!(s.allow_child_window_snap);
        assert!(!s.allow_popup_window_snap);
        assert_eq!(parse_fz_settings("{}").unwrap(), FzSettings::default());
    }

    #[test]
    fn last_used_desktop_and_enabled_flag() {
        assert_eq!(
            parse_last_used_desktop(r#"{"last-used-virtual-desktop":"{6ad31dde-e515-4558-9d15-21dbe1193f72}"}"#)
                .as_deref(),
            Some("{6AD31DDE-E515-4558-9D15-21DBE1193F72}")
        );
        assert_eq!(parse_last_used_desktop("{}"), None);
        assert!(parse_fancyzones_enabled(r#"{"enabled":{"FancyZones":true}}"#));
        assert!(!parse_fancyzones_enabled(r#"{"enabled":{"FancyZones":false}}"#));
        assert!(parse_fancyzones_enabled("{}"));
        assert!(parse_fancyzones_enabled("garbage"));
        assert_eq!(
            parse_powertoys_version(r#"{"powertoys_version":"v0.102.3101"}"#).as_deref(),
            Some("v0.102.3101")
        );
        assert_eq!(parse_powertoys_version("{}"), None);
    }
}
