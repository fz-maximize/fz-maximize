//! FancyZones agreement check ("drift detection"). Pure logic; Win32 glue lives in app.rs.
//!
//! When FancyZones itself snaps a window (Shift-drag), it stamps the window with the
//! `FancyZones_zones` property: a bitmask of the zone indices it used
//! (`FancyZonesWindowProperties::StampZoneIndexProperty`). Each such snap is a free
//! calibration sample: we compute our own rectangle for those zones and compare it with
//! where FancyZones actually put the window. Disagreement means our port of the layout
//! math / monitor matching no longer matches the installed FancyZones.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::geometry::Rect;

/// Edge tolerance (px) for "FancyZones and we agree".
pub const TOLERANCE: i32 = 2;
/// Distinct mismatching samples needed before alerting (filters out odd apps).
pub const CONFIRMATIONS: usize = 2;
const RECENT_CAP: usize = 10;

/// Decode the two 64-bit halves of FancyZones' zone bitmask into zone indices.
/// On 64-bit Windows the property's HANDLE value *is* the 64-bit mask (two int32s
/// memcpy'd low-then-high).
pub fn decode_stamp(part1: u64, part2: u64) -> Vec<usize> {
    let mut out = Vec::new();
    for (base, bits) in [(0usize, part1), (64, part2)] {
        for i in 0..64 {
            if bits & (1u64 << i) != 0 {
                out.push(base + i);
            }
        }
    }
    out
}

/// Bounding box of the given zones (FancyZones snaps a multi-zone selection to it).
/// `Err(zone_count)` when an index is out of range.
pub fn expected_rect(zones: &[Rect], indices: &[usize]) -> Result<Rect, usize> {
    let mut it = indices.iter();
    let first = *it.next().ok_or(zones.len())?;
    let mut r = *zones.get(first).ok_or(zones.len())?;
    for &i in it {
        let z = zones.get(i).ok_or(zones.len())?;
        r = Rect::new(
            r.left.min(z.left),
            r.top.min(z.top),
            r.right.max(z.right),
            r.bottom.max(z.bottom),
        );
    }
    Ok(r)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum Outcome {
    /// FancyZones' placement equals our computed zone.
    Match { expected: Rect, actual: Rect },
    /// Edges differ by more than the tolerance.
    Mismatch {
        expected: Rect,
        actual: Rect,
        max_delta: i32,
    },
    /// Same origin, different size: the app constrained its own size (minimum size,
    /// character-grid terminals). Not counted as drift.
    SizeOnly { expected: Rect, actual: Rect },
    /// FancyZones snapped here but we resolve no layout: monitor/desktop matching drift.
    NoLayout { actual: Rect },
    /// FancyZones used a zone index our layout doesn't have: zone-count drift.
    ZoneOutOfRange { zone_count: usize, indices: Vec<usize> },
}

impl Outcome {
    pub fn is_drift(&self) -> bool {
        matches!(
            self,
            Outcome::Mismatch { .. } | Outcome::NoLayout { .. } | Outcome::ZoneOutOfRange { .. }
        )
    }

    pub fn describe(&self) -> String {
        match self {
            Outcome::Match { expected, .. } => format!("match {expected}"),
            Outcome::Mismatch {
                expected,
                actual,
                max_delta,
            } => format!("MISMATCH by {max_delta}px: we compute {expected}, FancyZones placed {actual}"),
            Outcome::SizeOnly { expected, actual } => {
                format!("inconclusive (app constrained its size): zone {expected}, window {actual}")
            }
            Outcome::NoLayout { actual } => {
                format!("MISMATCH: FancyZones snapped to {actual} but we find no layout for this monitor/desktop")
            }
            Outcome::ZoneOutOfRange { zone_count, indices } => {
                format!("MISMATCH: FancyZones used zone(s) {indices:?} but our layout has {zone_count} zones")
            }
        }
    }
}

/// Compare FancyZones' placement (`actual`, visible frame) with our zones.
pub fn evaluate(zones: Option<&[Rect]>, indices: &[usize], actual: Rect) -> Outcome {
    let Some(zones) = zones.filter(|z| !z.is_empty()) else {
        return Outcome::NoLayout { actual };
    };
    let expected = match expected_rect(zones, indices) {
        Ok(r) => r,
        Err(zone_count) => {
            return Outcome::ZoneOutOfRange {
                zone_count,
                indices: indices.to_vec(),
            };
        }
    };
    let deltas = [
        (actual.left - expected.left).abs(),
        (actual.top - expected.top).abs(),
        (actual.right - expected.right).abs(),
        (actual.bottom - expected.bottom).abs(),
    ];
    let max_delta = deltas.into_iter().max().unwrap_or(0);
    if max_delta <= TOLERANCE {
        Outcome::Match { expected, actual }
    } else if deltas[0] <= TOLERANCE && deltas[1] <= TOLERANCE {
        Outcome::SizeOnly { expected, actual }
    } else {
        Outcome::Mismatch {
            expected,
            actual,
            max_delta,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub at_unix: u64,
    pub exe: String,
    pub monitor: String,
    pub layout: String,
    pub zones: Vec<usize>,
    pub outcome: Option<Outcome>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    pub powertoys_version: String,
    pub checks: u64,
    pub matches: u64,
    pub mismatches: u64,
    pub inconclusive: u64,
    pub first_check_unix: u64,
    pub last_check_unix: u64,
    pub last_match_unix: u64,
}

/// Persisted state (`%LOCALAPPDATA%\fz-maximize\fancyzones-check.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DriftState {
    pub current: Counters,
    /// Counters of the previous PowerToys version, kept after an update.
    pub previous: Option<Counters>,
    /// Keys (monitor + layout) with confirmed drift for the current version.
    pub confirmed: Vec<String>,
    pub recent: Vec<Record>,
}

impl DriftState {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap_or_default())?;
        std::fs::rename(tmp, path)
    }

    /// One-line summary for the tray menu / logs.
    pub fn summary(&self) -> String {
        let c = &self.current;
        if c.checks == 0 {
            return "FancyZones agreement: not checked yet (Shift-drag a window into a zone)".into();
        }
        let mut s = format!("FancyZones agreement: {}/{} match", c.matches, c.matches + c.mismatches);
        if c.inconclusive > 0 {
            s.push_str(&format!(", {} inconclusive", c.inconclusive));
        }
        if !self.confirmed.is_empty() {
            s.push_str(" — DRIFT DETECTED");
        }
        s
    }
}

/// A confirmed disagreement, to show the user once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    pub key: String,
    pub detail: String,
}

/// Tracks checks and decides when a disagreement is confirmed.
#[derive(Debug, Default)]
pub struct DriftTracker {
    pub state: DriftState,
    /// Distinct mismatching samples per key: (window, zone indices).
    evidence: HashMap<String, Vec<(isize, Vec<usize>)>>,
}

impl DriftTracker {
    pub fn new(state: DriftState) -> Self {
        Self {
            state,
            evidence: HashMap::new(),
        }
    }

    /// Start fresh counters when PowerToys was updated (keep the old ones as `previous`).
    pub fn set_powertoys_version(&mut self, version: &str) {
        if self.state.current.powertoys_version == version {
            return;
        }
        if self.state.current.checks > 0 {
            self.state.previous = Some(self.state.current.clone());
        }
        self.state.current = Counters {
            powertoys_version: version.to_string(),
            ..Default::default()
        };
        self.state.confirmed.clear();
        self.evidence.clear();
    }

    pub fn is_confirmed(&self, key: &str) -> bool {
        self.state.confirmed.iter().any(|k| k == key)
    }

    /// Record one check. Returns an alert the first time drift is confirmed for `key`.
    pub fn record(
        &mut self,
        key: &str,
        sample: (isize, Vec<usize>),
        mut record: Record,
        outcome: Outcome,
    ) -> Option<Alert> {
        let now = record.at_unix;
        let c = &mut self.state.current;
        c.checks += 1;
        if c.first_check_unix == 0 {
            c.first_check_unix = now;
        }
        c.last_check_unix = now;
        match &outcome {
            Outcome::Match { .. } => {
                c.matches += 1;
                c.last_match_unix = now;
            }
            Outcome::SizeOnly { .. } => c.inconclusive += 1,
            _ => c.mismatches += 1,
        }

        let is_drift = outcome.is_drift();
        let detail = outcome.describe();
        record.outcome = Some(outcome);
        self.state.recent.insert(0, record);
        self.state.recent.truncate(RECENT_CAP);

        if !is_drift || self.is_confirmed(key) {
            return None;
        }
        let ev = self.evidence.entry(key.to_string()).or_default();
        if !ev.contains(&sample) {
            ev.push(sample);
        }
        if ev.len() >= CONFIRMATIONS {
            self.state.confirmed.push(key.to_string());
            return Some(Alert {
                key: key.to_string(),
                detail,
            });
        }
        None
    }
}

/// `2026-11-25 15:48:22 UTC` from unix seconds (no date-time dependency needed).
pub fn format_unix(secs: u64) -> String {
    if secs == 0 {
        return "never".into();
    }
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const Z: [Rect; 3] = [
        Rect::new(16, 16, 952, 2284),
        Rect::new(968, 16, 2872, 2284),
        Rect::new(2888, 16, 3824, 2284),
    ];

    #[test]
    fn stamp_decoding() {
        assert_eq!(decode_stamp(0, 0), Vec::<usize>::new());
        assert_eq!(decode_stamp(0b1, 0), vec![0]);
        assert_eq!(decode_stamp(0b110, 0), vec![1, 2]);
        assert_eq!(decode_stamp(1 << 63, 1), vec![63, 64]);
    }

    #[test]
    fn expected_rect_unions_multi_zone_spans() {
        assert_eq!(expected_rect(&Z, &[1]), Ok(Z[1]));
        assert_eq!(expected_rect(&Z, &[0, 1]), Ok(Rect::new(16, 16, 2872, 2284)));
        assert_eq!(expected_rect(&Z, &[5]), Err(3));
        assert_eq!(expected_rect(&Z, &[]), Err(3));
    }

    #[test]
    fn evaluation() {
        assert!(matches!(evaluate(Some(&Z), &[1], Z[1]), Outcome::Match { .. }));
        let nearly = Rect::new(969, 15, 2873, 2286);
        assert!(matches!(evaluate(Some(&Z), &[1], nearly), Outcome::Match { .. }));

        let shifted = Rect::new(1100, 16, 2872, 2284);
        match evaluate(Some(&Z), &[1], shifted) {
            Outcome::Mismatch { max_delta, .. } => assert_eq!(max_delta, 132),
            o => panic!("{o:?}"),
        }
        let constrained = Rect::new(968, 16, 2500, 2000);
        assert!(matches!(
            evaluate(Some(&Z), &[1], constrained),
            Outcome::SizeOnly { .. }
        ));
        assert!(matches!(evaluate(None, &[1], Z[1]), Outcome::NoLayout { .. }));
        assert!(matches!(evaluate(Some(&[]), &[1], Z[1]), Outcome::NoLayout { .. }));
        assert!(matches!(
            evaluate(Some(&Z), &[3], Z[1]),
            Outcome::ZoneOutOfRange { zone_count: 3, .. }
        ));
        assert!(Outcome::NoLayout { actual: Z[0] }.is_drift());
        assert!(
            !Outcome::SizeOnly {
                expected: Z[0],
                actual: Z[0]
            }
            .is_drift()
        );
    }

    fn rec(at: u64) -> Record {
        Record {
            at_unix: at,
            exe: "notepad.exe".into(),
            monitor: "SAM73CE".into(),
            layout: "priority-grid/3".into(),
            zones: vec![1],
            outcome: None,
        }
    }

    fn mismatch() -> Outcome {
        Outcome::Mismatch {
            expected: Z[1],
            actual: Rect::new(1100, 16, 2872, 2284),
            max_delta: 132,
        }
    }

    #[test]
    fn drift_needs_two_distinct_samples_and_alerts_once() {
        let mut t = DriftTracker::new(DriftState::default());
        t.set_powertoys_version("v0.102");
        let key = "SAM73CE|priority-grid/3";
        assert!(t.record(key, (1, vec![1]), rec(10), mismatch()).is_none());
        // Same window + zone again is not new evidence.
        assert!(t.record(key, (1, vec![1]), rec(11), mismatch()).is_none());
        // A different window confirms it.
        let alert = t.record(key, (2, vec![1]), rec(12), mismatch()).unwrap();
        assert_eq!(alert.key, key);
        assert!(t.is_confirmed(key));
        // Only once.
        assert!(t.record(key, (3, vec![2]), rec(13), mismatch()).is_none());
        assert_eq!(t.state.current.checks, 4);
        assert_eq!(t.state.current.mismatches, 4);
        assert!(t.state.summary().contains("DRIFT"));
    }

    #[test]
    fn matches_and_inconclusive_are_counted_not_alerted() {
        let mut t = DriftTracker::new(DriftState::default());
        t.set_powertoys_version("v0.102");
        let k = "k";
        for i in 0..3 {
            let o = Outcome::Match {
                expected: Z[0],
                actual: Z[0],
            };
            assert!(t.record(k, (i, vec![0]), rec(20 + i as u64), o).is_none());
        }
        let o = Outcome::SizeOnly {
            expected: Z[0],
            actual: Z[0],
        };
        assert!(t.record(k, (9, vec![0]), rec(30), o.clone()).is_none());
        assert!(t.record(k, (10, vec![0]), rec(31), o).is_none());
        let c = &t.state.current;
        assert_eq!((c.checks, c.matches, c.inconclusive, c.mismatches), (5, 3, 2, 0));
        assert_eq!(c.first_check_unix, 20);
        assert_eq!(c.last_match_unix, 22);
        assert_eq!(t.state.summary(), "FancyZones agreement: 3/3 match, 2 inconclusive");
        assert_eq!(t.state.recent.len(), 5);
        assert_eq!(t.state.recent[0].at_unix, 31, "newest first");
    }

    #[test]
    fn powertoys_update_starts_fresh_counters() {
        let mut t = DriftTracker::new(DriftState::default());
        t.set_powertoys_version("v0.102");
        t.record("k", (1, vec![1]), rec(1), mismatch());
        t.record("k", (2, vec![1]), rec(2), mismatch());
        assert!(t.is_confirmed("k"));
        t.set_powertoys_version("v0.103");
        assert!(!t.is_confirmed("k"));
        assert_eq!(t.state.current.checks, 0);
        assert_eq!(t.state.current.powertoys_version, "v0.103");
        assert_eq!(t.state.previous.as_ref().unwrap().checks, 2);
        // Same version again: no reset.
        t.record("k", (1, vec![1]), rec(3), mismatch());
        t.set_powertoys_version("v0.103");
        assert_eq!(t.state.current.checks, 1);
    }

    #[test]
    fn state_roundtrip_and_summary_when_unchecked() {
        let dir = std::env::temp_dir().join(format!("fzm-drift-test-{}", std::process::id()));
        let path = dir.join("fancyzones-check.json");
        let mut t = DriftTracker::new(DriftState::default());
        t.set_powertoys_version("v0.102");
        t.record("k", (1, vec![1]), rec(5), mismatch());
        t.state.save(&path).unwrap();
        assert_eq!(DriftState::load(&path), t.state);
        assert_eq!(DriftState::load(&dir.join("missing.json")), DriftState::default());
        assert!(DriftState::default().summary().contains("not checked yet"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unix_formatting() {
        assert_eq!(format_unix(0), "never");
        assert_eq!(format_unix(1), "1970-01-01 00:00:01 UTC");
        assert_eq!(format_unix(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(format_unix(1_795_621_702), "2026-11-25 15:48:22 UTC");
    }
}
