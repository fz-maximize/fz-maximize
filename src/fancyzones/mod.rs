//! FancyZones data: locating, lazily (re)loading and resolving it to zones.
//!
//! Instead of a file watcher thread, [`FzStore::refresh`] compares file modification
//! times and re-parses only what changed. It is called right before the data is needed
//! (a maximize), which is rare, so the check costs nothing in steady state.

pub mod layout;
pub mod matching;
pub mod model;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;

use crate::geometry::Rect;
use layout::{LayoutSpec, LayoutType, compute_zones};
use matching::{MatchQuality, MonitorId, find_applied};
use model::{AppliedLayout, DefaultLayouts, FzSettings, NamedCustomLayout};

/// `%LOCALAPPDATA%\Microsoft\PowerToys`
pub fn powertoys_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("Microsoft").join("PowerToys"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let md = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: md.modified().ok(),
        len: md.len(),
    })
}

/// A file whose parsed contents are cached until its stamp changes.
#[derive(Debug)]
struct Cached<T> {
    path: PathBuf,
    stamp: Option<Stamp>,
    value: T,
}

impl<T: Default> Cached<T> {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            stamp: None,
            value: T::default(),
        }
    }

    /// Re-parse when the file changed. Missing file → default value. A parse error keeps
    /// the previous value and leaves the stamp unset so the next call retries (the file
    /// may have been caught mid-write).
    fn refresh(&mut self, parse: impl FnOnce(&str) -> Result<T, String>) -> bool {
        let now = stamp(&self.path);
        if now == self.stamp && now.is_some() {
            return false;
        }
        match now {
            None => {
                let changed = self.stamp.is_some();
                self.stamp = None;
                if changed {
                    tracing::info!(path = %self.path.display(), "file removed; using defaults");
                    self.value = T::default();
                }
                changed
            }
            Some(s) => match std::fs::read_to_string(&self.path)
                .map_err(|e| e.to_string())
                .and_then(|text| parse(&text))
            {
                Ok(v) => {
                    self.value = v;
                    self.stamp = Some(s);
                    tracing::debug!(path = %self.path.display(), "loaded");
                    true
                }
                Err(e) => {
                    tracing::warn!(path = %self.path.display(), error = %e, "failed to load; keeping previous data");
                    self.stamp = None;
                    false
                }
            },
        }
    }
}

/// What we read from the global PowerToys `settings.json`.
#[derive(Debug, Clone)]
pub struct PtSettings {
    /// FancyZones module enabled (defaults to true when unknown).
    pub enabled: bool,
    /// e.g. `v0.102.3101`
    pub version: Option<String>,
}
impl Default for PtSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            version: None,
        }
    }
}

#[derive(Debug)]
pub struct FzStore {
    applied: Cached<Vec<AppliedLayout>>,
    custom: Cached<HashMap<String, NamedCustomLayout>>,
    defaults: Cached<DefaultLayouts>,
    settings: Cached<FzSettings>,
    last_used_vd: Cached<Option<String>>,
    powertoys: Cached<PtSettings>,
}

impl FzStore {
    /// `powertoys` is `%LOCALAPPDATA%\Microsoft\PowerToys`.
    pub fn new(powertoys: &Path) -> Self {
        let fz = powertoys.join("FancyZones");
        Self {
            applied: Cached::new(fz.join("applied-layouts.json")),
            custom: Cached::new(fz.join("custom-layouts.json")),
            defaults: Cached::new(fz.join("default-layouts.json")),
            settings: Cached::new(fz.join("settings.json")),
            last_used_vd: Cached::new(fz.join("last-used-virtual-desktop.json")),
            powertoys: Cached::new(powertoys.join("settings.json")),
        }
    }

    pub fn fancyzones_dir(&self) -> &Path {
        self.applied.path.parent().unwrap_or(Path::new("."))
    }

    /// Reload whatever changed on disk. Returns true when anything changed.
    pub fn refresh(&mut self) -> bool {
        let mut changed = false;
        changed |= self.applied.refresh(|t| {
            let (v, skipped) = model::parse_applied_layouts(t).map_err(|e| e.to_string())?;
            if skipped > 0 {
                tracing::warn!(skipped, "skipped malformed applied-layouts entries");
            }
            tracing::info!(entries = v.len(), "applied-layouts.json loaded");
            Ok(v)
        });
        changed |= self.custom.refresh(|t| {
            let (v, skipped) = model::parse_custom_layouts(t).map_err(|e| e.to_string())?;
            if skipped > 0 {
                tracing::warn!(skipped, "skipped malformed custom-layouts entries");
            }
            Ok(v)
        });
        changed |= self
            .defaults
            .refresh(|t| model::parse_default_layouts(t).map_err(|e| e.to_string()));
        changed |= self
            .settings
            .refresh(|t| model::parse_fz_settings(t).map_err(|e| e.to_string()));
        changed |= self.last_used_vd.refresh(|t| Ok(model::parse_last_used_desktop(t)));
        changed |= self.powertoys.refresh(|t| {
            Ok(PtSettings {
                enabled: model::parse_fancyzones_enabled(t),
                version: model::parse_powertoys_version(t),
            })
        });
        changed
    }

    /// Force a full reload on the next [`refresh`](Self::refresh).
    pub fn invalidate(&mut self) {
        self.applied.stamp = None;
        self.custom.stamp = None;
        self.defaults.stamp = None;
        self.settings.stamp = None;
        self.last_used_vd.stamp = None;
        self.powertoys.stamp = None;
    }

    pub fn data(&self) -> FzView<'_> {
        FzView {
            applied: &self.applied.value,
            custom: &self.custom.value,
            defaults: &self.defaults.value,
            settings: &self.settings.value,
            last_used_vd: self.last_used_vd.value.as_deref(),
            enabled: self.powertoys.value.enabled,
            powertoys_version: self.powertoys.value.version.as_deref(),
        }
    }
}

/// Borrowed snapshot of the FancyZones data (constructible in tests).
#[derive(Clone, Copy, Debug)]
pub struct FzView<'a> {
    pub applied: &'a [AppliedLayout],
    pub custom: &'a HashMap<String, NamedCustomLayout>,
    pub defaults: &'a DefaultLayouts,
    pub settings: &'a FzSettings,
    pub last_used_vd: Option<&'a str>,
    pub enabled: bool,
    pub powertoys_version: Option<&'a str>,
}

/// The monitor facts needed to resolve zones.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MonitorDesc {
    pub id: MonitorId,
    pub device_name: String,
    pub rc_monitor: Rect,
    pub rc_work: Rect,
    pub dpi: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum LayoutSource {
    /// Matched an `applied-layouts.json` entry for this virtual desktop.
    Applied {
        virtual_desktop: String,
        quality: MatchQuality,
    },
    /// No entry: FancyZones applies its default layout to such monitors (in memory).
    Default,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Resolution {
    pub source: LayoutSource,
    #[serde(rename = "type")]
    pub kind: LayoutType,
    pub uuid: String,
    pub zone_count: i32,
    pub spacing: i32,
    pub custom_name: Option<String>,
    /// Zones in screen coordinates (physical pixels). Empty on error.
    pub zones: Vec<Rect>,
    pub error: Option<String>,
}

impl Resolution {
    /// True when maximize should be redirected on this monitor.
    pub fn is_multi_zone(&self) -> bool {
        self.zones.len() >= 2
    }
}

/// Resolve which layout FancyZones uses on `monitor` and compute its zones.
///
/// `desktops` are candidate virtual-desktop ids in priority order (see
/// [`desktop_candidates`]). `use_default_layout` mirrors FancyZones applying its default
/// layout to monitors without an entry.
pub fn resolve(
    fz: &FzView<'_>,
    monitor: &MonitorDesc,
    desktops: &[String],
    use_default_layout: bool,
) -> Option<Resolution> {
    let (source, uuid, spec): (LayoutSource, String, LayoutSpec) = match find_applied(fz.applied, &monitor.id, desktops)
    {
        Some((entry, di, quality)) => (
            LayoutSource::Applied {
                virtual_desktop: desktops[di].clone(),
                quality,
            },
            entry.uuid.clone(),
            entry.spec.clone(),
        ),
        None if use_default_layout => {
            let vertical = monitor.rc_monitor.height() > monitor.rc_monitor.width();
            let (uuid, spec) = if vertical {
                fz.defaults.vertical.clone()
            } else {
                fz.defaults.horizontal.clone()
            };
            (LayoutSource::Default, uuid, spec)
        }
        None => return None,
    };

    let custom = (spec.kind == LayoutType::Custom)
        .then(|| fz.custom.get(&uuid))
        .flatten();
    let work = monitor.rc_work;
    let (zones, error) = match compute_zones(
        &spec,
        work.width(),
        work.height(),
        monitor.dpi,
        custom.map(|c| &c.layout),
    ) {
        Ok(z) => (z.into_iter().map(|r| r.offset(work.left, work.top)).collect(), None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };

    Some(Resolution {
        source,
        kind: spec.kind,
        uuid,
        zone_count: spec.zone_count,
        spacing: spec.effective_spacing(),
        custom_name: custom.map(|c| c.name.clone()),
        zones,
        error,
    })
}

/// Candidate virtual desktops in priority order, de-duplicated: the window's own desktop,
/// the current desktop from the registry, FancyZones' last-used desktop, then GUID_NULL
/// (used by FancyZones when virtual desktops are unavailable).
pub fn desktop_candidates(
    window_desktop: Option<&str>,
    registry_current: Option<&str>,
    last_used: Option<&str>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(4);
    for c in [window_desktop, registry_current, last_used].into_iter().flatten() {
        if let Some(g) = model::normalize_guid(c)
            && g != model::NULL_GUID
            && !out.contains(&g)
        {
            out.push(g);
        }
    }
    out.push(model::NULL_GUID.to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use matching::DeviceId;

    const VD1: &str = "{6AD31DDE-E515-4558-9D15-21DBE1193F72}";
    const VD3: &str = "{BC41CE51-0EC0-4EC5-A579-8917EFEF2FA5}";

    struct Fixture {
        applied: Vec<AppliedLayout>,
        custom: HashMap<String, NamedCustomLayout>,
        defaults: DefaultLayouts,
        settings: FzSettings,
    }

    impl Fixture {
        fn load() -> Self {
            let (applied, _) =
                model::parse_applied_layouts(include_str!("../../tests/fixtures/applied-layouts.json")).unwrap();
            let (custom, _) =
                model::parse_custom_layouts(include_str!("../../tests/fixtures/custom-layouts.json")).unwrap();
            Self {
                applied,
                custom,
                defaults: DefaultLayouts::default(),
                settings: FzSettings::default(),
            }
        }
        fn view(&self) -> FzView<'_> {
            FzView {
                applied: &self.applied,
                custom: &self.custom,
                defaults: &self.defaults,
                settings: &self.settings,
                last_used_vd: None,
                enabled: true,
                powertoys_version: None,
            }
        }
    }

    fn monitor(id: &str, inst: &str, number: i32, serial: &str, mon: Rect, work: Rect) -> MonitorDesc {
        MonitorDesc {
            id: MonitorId {
                device: DeviceId {
                    id: id.into(),
                    instance_id: inst.into(),
                    number,
                },
                serial: serial.into(),
            },
            device_name: format!(r"\\.\DISPLAY{number}"),
            rc_monitor: mon,
            rc_work: work,
            dpi: 96,
        }
    }

    fn ultrawide_at(x: i32) -> MonitorDesc {
        monitor(
            "SAM73CE",
            "5&1498d1d&0&UID4352",
            5,
            "TESTSERIAL01",
            Rect::new(x, 0, x + 7680, 2160),
            Rect::new(x, 0, x + 7680, 2112),
        )
    }

    #[test]
    fn ultrawide_resolves_to_25_50_25_in_screen_coordinates() {
        let f = Fixture::load();
        // Monitor placed to the right of another one; zones must be offset.
        let m = ultrawide_at(1920);
        let r = resolve(&f.view(), &m, &[VD1.into()], true).unwrap();
        assert_eq!(
            r.source,
            LayoutSource::Applied {
                virtual_desktop: VD1.into(),
                quality: MatchQuality::Exact
            }
        );
        assert_eq!(r.kind, LayoutType::PriorityGrid);
        assert_eq!(
            r.zones,
            vec![
                Rect::new(1920 + 16, 16, 1920 + 1912, 2096),
                Rect::new(1920 + 1928, 16, 1920 + 5752, 2096),
                Rect::new(1920 + 5768, 16, 1920 + 7664, 2096),
            ]
        );
        assert!(r.is_multi_zone());
    }

    #[test]
    fn virtual_desktop_selects_entry() {
        let f = Fixture::load();
        let m = ultrawide_at(0);
        let r = resolve(&f.view(), &m, &[VD3.into()], false).unwrap();
        assert!(matches!(r.source, LayoutSource::Applied { ref virtual_desktop, .. } if virtual_desktop == VD3));
    }

    #[test]
    fn blank_layout_is_pass_through() {
        let f = Fixture::load();
        let m = monitor(
            "BOE0A52",
            "4&a918e15&0&UID8388688",
            1,
            "0",
            Rect::new(0, 0, 1920, 1200),
            Rect::new(0, 0, 1920, 1152),
        );
        let r = resolve(&f.view(), &m, &[VD1.into()], true).unwrap();
        assert_eq!(r.kind, LayoutType::Blank);
        assert!(r.zones.is_empty());
        assert!(!r.is_multi_zone());
        assert!(r.error.is_none());
    }

    #[test]
    fn custom_grid_layout_uses_applied_spacing() {
        let f = Fixture::load();
        // GSM0001 entry: custom grid, applied show-spacing=false (the custom layout's own
        // spacing=8 is NOT used, matching FancyZones' Layout::Init).
        let m = monitor(
            "GSM0001",
            "4&a918e15&0&UID20549",
            2,
            "16843009",
            Rect::new(0, 0, 4000, 1000),
            Rect::new(0, 0, 4000, 1000),
        );
        let r = resolve(&f.view(), &m, &[VD1.into()], true).unwrap();
        assert_eq!(r.kind, LayoutType::Custom);
        assert_eq!(r.custom_name.as_deref(), Some("Ultrawide 25/50/25 split"));
        assert_eq!(r.zones.len(), 5);
        assert_eq!(r.zones[1], Rect::new(1000, 0, 3000, 1000));
    }

    #[test]
    fn unknown_monitor_uses_default_layout_or_none() {
        let f = Fixture::load();
        let m = monitor(
            "XYZ0001",
            "1&2&3",
            3,
            "",
            Rect::new(0, 0, 1080, 1920),
            Rect::new(0, 0, 1080, 1872),
        );
        assert!(resolve(&f.view(), &m, &[VD1.into()], false).is_none());
        let r = resolve(&f.view(), &m, &[VD1.into()], true).unwrap();
        assert_eq!(r.source, LayoutSource::Default);
        // Portrait monitor → default is 3 rows.
        assert_eq!(r.kind, LayoutType::Rows);
        assert_eq!(r.zones.len(), 3);
    }

    #[test]
    fn legacy_display_name_entry_matches_fallback_monitor_id() {
        let f = Fixture::load();
        // A display whose device couldn't be identified gets id = \\.\DISPLAY1, no instance.
        let m = monitor(
            r"\\.\DISPLAY1",
            "",
            1,
            "",
            Rect::new(0, 0, 2560, 1440),
            Rect::new(0, 0, 2560, 1400),
        );
        let r = resolve(&f.view(), &m, &[VD1.into()], false).unwrap();
        assert_eq!(r.kind, LayoutType::PriorityGrid);
    }

    #[test]
    fn candidates_are_ordered_and_deduplicated() {
        let c = desktop_candidates(Some(VD1), Some(&VD1.to_lowercase()), Some(VD3));
        assert_eq!(c, vec![VD1.to_string(), VD3.to_string(), model::NULL_GUID.to_string()]);
        // A window reporting GUID_NULL doesn't push NULL ahead of real candidates.
        let c = desktop_candidates(Some(model::NULL_GUID), None, Some(VD3));
        assert_eq!(c, vec![VD3.to_string(), model::NULL_GUID.to_string()]);
        let c = desktop_candidates(Some("garbage"), None, None);
        assert_eq!(c, vec![model::NULL_GUID.to_string()]);
    }

    #[test]
    fn store_reloads_only_on_change() {
        let dir = std::env::temp_dir().join(format!("fzm-store-test-{}", std::process::id()));
        let fz = dir.join("FancyZones");
        std::fs::create_dir_all(&fz).unwrap();
        let path = fz.join("applied-layouts.json");
        std::fs::write(&path, include_str!("../../tests/fixtures/applied-layouts.json")).unwrap();

        let mut store = FzStore::new(&dir);
        assert!(store.refresh());
        assert_eq!(store.data().applied.len(), 7);
        assert!(!store.refresh(), "unchanged files are not re-read");

        // A corrupt write keeps the previous data.
        std::fs::write(&path, "{ truncated").unwrap();
        store.refresh();
        assert_eq!(store.data().applied.len(), 7);

        // A valid rewrite is picked up.
        std::fs::write(&path, r#"{"applied-layouts":[]}"#).unwrap();
        assert!(store.refresh());
        assert!(store.data().applied.is_empty());

        // Removal resets to defaults.
        std::fs::remove_file(&path).unwrap();
        store.refresh();
        assert!(store.data().applied.is_empty());
        assert!(store.data().enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
