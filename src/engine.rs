//! The maximize state machine. Pure: no Win32 calls, time is passed in.
//!
//! Flow for one window:
//! 1. [`Engine::seed`] records the initial zoomed state (startup / window shown).
//! 2. [`Engine::observe`] is fed every location change; it reports a rising edge
//!    (not-zoomed → zoomed) exactly once per maximize, or a pending verify.
//! 3. On a rising edge the caller runs [`Engine::precheck`] (cheap bypasses), resolves
//!    the zones, then asks [`Engine::decide`] what to do and reports back with
//!    [`Engine::commit_snap`] / [`Engine::commit_restore`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::AlreadyInZone;
use crate::geometry::{Margins, Point, Rect, best_zone_for_rect, nearest_zone_to_point};

pub type Hwnd = isize;

/// A maximize this soon after a move/size loop ended counts as a drag-maximize.
pub const DRAG_WINDOW: Duration = Duration::from_millis(500);
/// How long after a snap the first normal-state location change is verified.
pub const VERIFY_WINDOW: Duration = Duration::from_millis(1500);
/// Lifetime of the one-shot "allow native maximize" token armed by the hotkey.
pub const ALLOW_NATIVE_WINDOW: Duration = Duration::from_secs(2);
/// If a window re-maximizes this soon after we acted on it, the app is fighting us
/// (e.g. it forces itself maximized); back off and let it be.
pub const FIGHT_WINDOW: Duration = Duration::from_millis(750);
/// Pixel tolerance for "the window already fills this zone".
pub const IN_ZONE_TOLERANCE: i32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapRecord {
    /// Visible-frame target (the zone).
    pub zone: Rect,
    /// Window rect we applied (zone grown by the invisible borders).
    pub target: Rect,
    /// Window rect before we snapped (restore target for the toggle).
    pub pre_snap: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingVerify {
    zone: Rect,
    deadline: Instant,
}

#[derive(Clone, Debug, Default)]
struct WindowState {
    zoomed: bool,
    margins: Option<Margins>,
    snap: Option<SnapRecord>,
    in_move_size: bool,
    move_size_ended: Option<Instant>,
    allow_native_until: Option<Instant>,
    verify: Option<PendingVerify>,
    last_action: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    Nothing,
    /// Not-zoomed → zoomed transition: a maximize just happened.
    Maximized,
    /// The first normal-state change after a snap: verify the frame against this zone.
    Verify(Rect),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeReason {
    Paused,
    FancyZonesDisabled,
    HotkeyBypass,
    ModifierHeld,
    FightingApp,
    NotEligible(&'static str),
    ExcludedApp,
    NoLayout,
    LayoutError(String),
    SingleZone,
    MonitorFiltered,
    DragMaximizeDisabled,
    AlreadyInZone,
    FancyZonesDisagree,
}

impl std::fmt::Display for NativeReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Paused => write!(f, "paused"),
            Self::FancyZonesDisabled => write!(f, "FancyZones is disabled in PowerToys"),
            Self::HotkeyBypass => write!(f, "hotkey requested a full maximize"),
            Self::ModifierHeld => write!(f, "bypass modifier held"),
            Self::FightingApp => write!(f, "window re-maximized right after we moved it; backing off"),
            Self::NotEligible(why) => write!(f, "window not eligible: {why}"),
            Self::ExcludedApp => write!(f, "app is excluded"),
            Self::NoLayout => write!(f, "no FancyZones layout for this monitor/desktop"),
            Self::LayoutError(e) => write!(f, "layout error: {e}"),
            Self::SingleZone => write!(f, "layout has fewer than two zones"),
            Self::MonitorFiltered => write!(f, "monitor excluded by configuration"),
            Self::DragMaximizeDisabled => write!(f, "drag-maximize snapping disabled"),
            Self::AlreadyInZone => write!(f, "window already fills its zone"),
            Self::FancyZonesDisagree => write!(
                f,
                "confirmed disagreement with FancyZones on this monitor (on_fancyzones_mismatch = \"native\")"
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Leave the native maximize alone.
    Native(NativeReason),
    /// Restore the window into `target` (window rect, screen coordinates).
    Snap {
        zone_index: usize,
        zone: Rect,
        target: Rect,
        pre_snap: Rect,
    },
    /// Toggle: restore the window to its pre-snap rect.
    Restore { target: Rect },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    pub already_in_zone: AlreadyInZone,
    pub snap_on_drag: bool,
}

/// Facts about one maximize, gathered by the caller.
#[derive(Clone, Copy, Debug)]
pub struct MaximizeInput<'a> {
    /// Zones of the monitor the window maximized on, screen coordinates (≥ 2).
    pub zones: &'a [Rect],
    /// Pre-maximize window rect (`rcNormalPosition` in screen coordinates).
    pub normal_rect: Rect,
    pub cursor: Point,
    /// Used when the window's invisible borders were never measured.
    pub fallback_margins: Margins,
    /// Apply the drag-to-top heuristic (zone under the cursor right after a move/size
    /// loop). False for intercepted clicks: the first click of a title-bar double-click
    /// briefly enters a move loop too.
    pub consider_drag: bool,
}

#[derive(Debug, Default)]
pub struct Engine {
    windows: HashMap<Hwnd, WindowState>,
    paused: bool,
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn tracked_windows(&self) -> usize {
        self.windows.len()
    }

    pub fn is_tracked(&self, hwnd: Hwnd) -> bool {
        self.windows.contains_key(&hwnd)
    }

    /// Record a window's current zoomed state without treating it as a transition.
    pub fn seed(&mut self, hwnd: Hwnd, zoomed: bool, margins: Option<Margins>) {
        let st = self.windows.entry(hwnd).or_default();
        st.zoomed = zoomed;
        if margins.is_some() {
            st.margins = margins;
        }
    }

    pub fn forget(&mut self, hwnd: Hwnd) {
        self.windows.remove(&hwnd);
    }

    pub fn update_margins(&mut self, hwnd: Hwnd, margins: Margins) {
        if let Some(st) = self.windows.get_mut(&hwnd) {
            st.margins = Some(margins);
        }
    }

    pub fn snap_record(&self, hwnd: Hwnd) -> Option<SnapRecord> {
        self.windows.get(&hwnd).and_then(|s| s.snap)
    }

    pub fn move_size_start(&mut self, hwnd: Hwnd) {
        if let Some(st) = self.windows.get_mut(&hwnd) {
            st.in_move_size = true;
        }
    }

    pub fn move_size_end(&mut self, hwnd: Hwnd, now: Instant) {
        if let Some(st) = self.windows.get_mut(&hwnd) {
            st.in_move_size = false;
            st.move_size_ended = Some(now);
        }
    }

    /// Feed a location change of a non-minimized window. Unknown windows are seeded
    /// (a window first seen already zoomed is not treated as a fresh maximize).
    pub fn observe(&mut self, hwnd: Hwnd, zoomed: bool, now: Instant) -> Observation {
        let Some(st) = self.windows.get_mut(&hwnd) else {
            self.windows.insert(
                hwnd,
                WindowState {
                    zoomed,
                    ..Default::default()
                },
            );
            return Observation::Nothing;
        };
        if zoomed {
            if st.zoomed {
                return Observation::Nothing;
            }
            st.zoomed = true;
            st.verify = None;
            return Observation::Maximized;
        }
        st.zoomed = false;
        match st.verify.take() {
            Some(v) if now <= v.deadline => Observation::Verify(v.zone),
            _ => Observation::Nothing,
        }
    }

    /// Arm a one-shot token so the next maximize of `hwnd` stays native (hotkey).
    pub fn arm_allow_native(&mut self, hwnd: Hwnd, now: Instant) {
        self.windows.entry(hwnd).or_default().allow_native_until = Some(now + ALLOW_NATIVE_WINDOW);
    }

    /// Cheap checks that run before any zone resolution.
    pub fn precheck(&mut self, hwnd: Hwnd, now: Instant, modifier_held: bool) -> Result<(), NativeReason> {
        if self.paused {
            return Err(NativeReason::Paused);
        }
        let st = self.windows.entry(hwnd).or_default();
        if let Some(until) = st.allow_native_until.take()
            && now <= until
        {
            return Err(NativeReason::HotkeyBypass);
        }
        if modifier_held {
            return Err(NativeReason::ModifierHeld);
        }
        if st.last_action.is_some_and(|t| now.duration_since(t) < FIGHT_WINDOW) {
            return Err(NativeReason::FightingApp);
        }
        Ok(())
    }

    /// Checks for an intercepted gesture (the window is not maximized yet). Only pause and
    /// the bypass modifier apply: the hotkey token and the anti-fight guard are about
    /// maximizes Windows already performed.
    pub fn precheck_intercepted(&self, modifier_held: bool) -> Result<(), NativeReason> {
        if self.paused {
            return Err(NativeReason::Paused);
        }
        if modifier_held {
            return Err(NativeReason::ModifierHeld);
        }
        Ok(())
    }

    /// Decide what to do with a maximize of `hwnd`.
    pub fn decide(&self, hwnd: Hwnd, input: &MaximizeInput<'_>, opts: Options, now: Instant) -> Decision {
        let st = self.windows.get(&hwnd);
        if input.zones.len() < 2 {
            return Decision::Native(NativeReason::SingleZone);
        }

        let dragging = input.consider_drag
            && st.is_some_and(|s| {
                s.in_move_size || s.move_size_ended.is_some_and(|t| now.duration_since(t) <= DRAG_WINDOW)
            });
        if dragging && !opts.snap_on_drag {
            return Decision::Native(NativeReason::DragMaximizeDisabled);
        }

        let index = if dragging {
            nearest_zone_to_point(input.zones, input.cursor)
        } else {
            best_zone_for_rect(input.zones, &input.normal_rect)
        };
        let Some(index) = index else {
            return Decision::Native(NativeReason::SingleZone);
        };
        let zone = input.zones[index];
        let margins = st.and_then(|s| s.margins).unwrap_or(input.fallback_margins);

        // Our own previous snap, if the window is still exactly where we put it.
        let our_snap = st
            .and_then(|s| s.snap)
            .filter(|s| s.target.approx_eq(&input.normal_rect, IN_ZONE_TOLERANCE));

        if !dragging {
            let frame = input.normal_rect.shrink(margins);
            let fills_zone = our_snap.is_some_and(|s| s.zone == zone) || frame.approx_eq(&zone, IN_ZONE_TOLERANCE);
            if fills_zone {
                return match (opts.already_in_zone, our_snap) {
                    (AlreadyInZone::Restore, Some(s)) if s.zone == zone => Decision::Restore { target: s.pre_snap },
                    _ => Decision::Native(NativeReason::AlreadyInZone),
                };
            }
        }

        Decision::Snap {
            zone_index: index,
            zone,
            target: zone.grow(margins),
            pre_snap: our_snap.map_or(input.normal_rect, |s| s.pre_snap),
        }
    }

    /// Record a successfully applied snap and arm the verify.
    pub fn commit_snap(&mut self, hwnd: Hwnd, zone: Rect, target: Rect, pre_snap: Rect, now: Instant) {
        let st = self.windows.entry(hwnd).or_default();
        st.snap = Some(SnapRecord { zone, target, pre_snap });
        st.verify = Some(PendingVerify {
            zone,
            deadline: now + VERIFY_WINDOW,
        });
        st.last_action = Some(now);
        st.move_size_ended = None;
    }

    /// The verify step corrected the window; remember where it actually is.
    pub fn commit_correction(&mut self, hwnd: Hwnd, target: Rect) {
        if let Some(snap) = self.windows.get_mut(&hwnd).and_then(|s| s.snap.as_mut()) {
            snap.target = target;
        }
    }

    /// Record a successful toggle-restore.
    pub fn commit_restore(&mut self, hwnd: Hwnd, now: Instant) {
        let st = self.windows.entry(hwnd).or_default();
        st.snap = None;
        st.verify = None;
        st.last_action = Some(now);
    }

    /// Drop state of windows for which `alive` returns false (safety net in case a
    /// destroy event was missed).
    pub fn prune(&mut self, mut alive: impl FnMut(Hwnd) -> bool) -> usize {
        let before = self.windows.len();
        self.windows.retain(|h, _| alive(*h));
        before - self.windows.len()
    }
}

/// Drag to restore: where a window of `size` goes when the user starts dragging the title
/// bar of a zone-maximized window currently at `current`, with the cursor at `cursor`.
/// Like Windows' own drag-to-restore, the cursor keeps its relative horizontal position
/// in the title bar and its distance from the top edge.
pub fn drag_restore_rect(current: &Rect, size: (i32, i32), cursor: Point) -> Rect {
    let (w, h) = (size.0.max(1), size.1.max(1));
    let fx = (f64::from(cursor.x - current.left) / f64::from(current.width().max(1))).clamp(0.0, 1.0);
    let dy = (cursor.y - current.top).clamp(0, h - 1);
    let left = cursor.x - ((fx * f64::from(w)).round() as i32).clamp(0, w - 1);
    let top = cursor.y - dy;
    Rect::new(left, top, left + w, top + h)
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: Hwnd = 0x1234;

    #[test]
    fn drag_restore_keeps_the_cursor_on_the_title_bar() {
        // Zone 1920 wide; grabbed at 25% across, 15 px below the top.
        let zone = Rect::new(960, 0, 2880, 2300);
        let cursor = Point::new(960 + 480, 15);
        let r = drag_restore_rect(&zone, (800, 600), cursor);
        assert_eq!((r.width(), r.height()), (800, 600), "pre-snap size");
        assert_eq!(r.left, cursor.x - 200, "same relative position: 25% of 800");
        assert_eq!(r.top, 0, "same distance from the top");
        assert!(r.contains(cursor), "cursor stays on the window");
        // Grabbed at the far right: the restored window ends just past the cursor.
        let r = drag_restore_rect(&zone, (800, 600), Point::new(2879, 15));
        assert!(r.contains(Point::new(2879, 15)) && r.right == 2880);
        // Cursor lower than the restored height (tall title bar): clamped onto the window.
        let r = drag_restore_rect(&zone, (800, 40), Point::new(1500, 100));
        assert!(r.contains(Point::new(1500, 100)));
    }
    const ZONES: [Rect; 3] = [
        Rect::new(16, 16, 1912, 2096),
        Rect::new(1928, 16, 5752, 2096),
        Rect::new(5768, 16, 7664, 2096),
    ];
    const WIN10: Margins = Margins::new(7, 0, 7, 7);

    fn opts() -> Options {
        Options {
            already_in_zone: AlreadyInZone::Restore,
            snap_on_drag: true,
        }
    }

    fn input(normal: Rect) -> MaximizeInput<'static> {
        MaximizeInput {
            zones: &ZONES,
            normal_rect: normal,
            cursor: Point::new(0, 0),
            fallback_margins: WIN10,
            consider_drag: true,
        }
    }

    fn maximized(engine: &mut Engine, now: Instant) {
        assert_eq!(engine.observe(H, true, now), Observation::Maximized);
    }

    #[test]
    fn rising_edge_fires_once() {
        let mut e = Engine::new();
        let t = Instant::now();
        e.seed(H, false, None);
        assert_eq!(e.observe(H, false, t), Observation::Nothing);
        maximized(&mut e, t);
        // Subsequent location changes while still maximized are not new maximizes.
        assert_eq!(e.observe(H, true, t), Observation::Nothing);
        assert_eq!(e.observe(H, true, t), Observation::Nothing);
        // Restore + maximize again fires again.
        assert_eq!(e.observe(H, false, t), Observation::Nothing);
        maximized(&mut e, t);
    }

    #[test]
    fn windows_seeded_maximized_or_first_seen_maximized_are_not_snapped() {
        let mut e = Engine::new();
        let t = Instant::now();
        e.seed(H, true, None);
        assert_eq!(e.observe(H, true, t), Observation::Nothing);
        // Unknown window first observed zoomed.
        assert_eq!(e.observe(0x999, true, t), Observation::Nothing);
        assert!(e.is_tracked(0x999));
        assert_eq!(e.observe(0x999, true, t), Observation::Nothing);
    }

    #[test]
    fn snap_into_zone_with_largest_overlap() {
        let mut e = Engine::new();
        let t = Instant::now();
        e.seed(H, false, Some(WIN10));
        maximized(&mut e, t);
        assert_eq!(e.precheck(H, t, false), Ok(()));
        // Window mostly over the left zone.
        let normal = Rect::new(100, 200, 1500, 1200);
        match e.decide(H, &input(normal), opts(), t) {
            Decision::Snap {
                zone_index,
                zone,
                target,
                pre_snap,
            } => {
                assert_eq!(zone_index, 0);
                assert_eq!(zone, ZONES[0]);
                assert_eq!(target, ZONES[0].grow(WIN10));
                assert_eq!(pre_snap, normal);
            }
            d => panic!("unexpected {d:?}"),
        }
    }

    #[test]
    fn each_zone_is_reachable() {
        let e = Engine::new();
        let t = Instant::now();
        let cases = [
            (Rect::new(300, 300, 1200, 1200), 0),
            (Rect::new(3000, 300, 4500, 1200), 1),
            (Rect::new(6000, 300, 7000, 1200), 2),
        ];
        for (normal, want) in cases {
            match e.decide(H, &input(normal), opts(), t) {
                Decision::Snap { zone_index, .. } => assert_eq!(zone_index, want, "{normal}"),
                d => panic!("unexpected {d:?}"),
            }
        }
    }

    #[test]
    fn unknown_margins_use_fallback() {
        let e = Engine::new();
        let t = Instant::now();
        let mut i = input(Rect::new(3000, 300, 4500, 1200));
        i.fallback_margins = Margins::new(8, 0, 8, 8);
        match e.decide(H, &i, opts(), t) {
            Decision::Snap { target, .. } => assert_eq!(target, ZONES[1].grow(Margins::new(8, 0, 8, 8))),
            d => panic!("unexpected {d:?}"),
        }
    }

    #[test]
    fn prechecks() {
        let mut e = Engine::new();
        let t = Instant::now();
        e.seed(H, false, None);

        e.set_paused(true);
        assert_eq!(e.precheck(H, t, false), Err(NativeReason::Paused));
        e.set_paused(false);

        assert_eq!(e.precheck(H, t, true), Err(NativeReason::ModifierHeld));

        // Hotkey token: consumed once, then normal behavior resumes.
        e.arm_allow_native(H, t);
        assert_eq!(
            e.precheck(H, t + Duration::from_millis(100), false),
            Err(NativeReason::HotkeyBypass)
        );
        assert_eq!(e.precheck(H, t + Duration::from_millis(200), false), Ok(()));

        // Expired token is ignored.
        e.arm_allow_native(H, t);
        assert_eq!(
            e.precheck(H, t + ALLOW_NATIVE_WINDOW + Duration::from_millis(1), false),
            Ok(())
        );
    }

    #[test]
    fn app_that_fights_back_is_left_alone() {
        let mut e = Engine::new();
        let t = Instant::now();
        e.seed(H, false, Some(WIN10));
        maximized(&mut e, t);
        e.commit_snap(H, ZONES[0], ZONES[0].grow(WIN10), Rect::new(0, 0, 100, 100), t);
        // App immediately re-maximizes itself.
        assert_eq!(e.observe(H, false, t), Observation::Verify(ZONES[0]));
        maximized(&mut e, t + Duration::from_millis(50));
        assert_eq!(
            e.precheck(H, t + Duration::from_millis(50), false),
            Err(NativeReason::FightingApp)
        );
        // Much later, a user maximize works again.
        assert_eq!(e.precheck(H, t + Duration::from_secs(5), false), Ok(()));
    }

    #[test]
    fn verify_fires_once_and_expires() {
        let mut e = Engine::new();
        let t = Instant::now();
        e.seed(H, false, None);
        maximized(&mut e, t);
        e.commit_snap(H, ZONES[1], ZONES[1], Rect::default(), t);
        assert_eq!(
            e.observe(H, false, t + Duration::from_millis(30)),
            Observation::Verify(ZONES[1])
        );
        assert_eq!(e.observe(H, false, t + Duration::from_millis(40)), Observation::Nothing);

        // Expired verify does nothing.
        maximized(&mut e, t + Duration::from_secs(10));
        e.commit_snap(H, ZONES[1], ZONES[1], Rect::default(), t + Duration::from_secs(10));
        assert_eq!(
            e.observe(
                H,
                false,
                t + Duration::from_secs(10) + VERIFY_WINDOW + Duration::from_millis(1)
            ),
            Observation::Nothing
        );
    }

    #[test]
    fn second_maximize_toggles_back_to_pre_snap_rect() {
        let mut e = Engine::new();
        let t = Instant::now();
        e.seed(H, false, Some(WIN10));
        let original = Rect::new(2500, 400, 4000, 1500);

        maximized(&mut e, t);
        let Decision::Snap {
            zone, target, pre_snap, ..
        } = e.decide(H, &input(original), opts(), t)
        else {
            panic!("expected snap")
        };
        e.commit_snap(H, zone, target, pre_snap, t);
        assert_eq!(e.observe(H, false, t), Observation::Verify(zone));

        // User maximizes again later; rcNormalPosition is the rect we applied.
        let t2 = t + Duration::from_secs(3);
        maximized(&mut e, t2);
        assert_eq!(e.precheck(H, t2, false), Ok(()));
        assert_eq!(
            e.decide(H, &input(target), opts(), t2),
            Decision::Restore { target: original }
        );
        e.commit_restore(H, t2);
        assert_eq!(e.snap_record(H), None);
    }

    #[test]
    fn second_maximize_goes_native_in_maximize_mode() {
        let mut e = Engine::new();
        let t = Instant::now();
        e.seed(H, false, Some(WIN10));
        let target = ZONES[1].grow(WIN10);
        e.commit_snap(H, ZONES[1], target, Rect::new(2500, 400, 4000, 1500), t);
        let o = Options {
            already_in_zone: AlreadyInZone::Maximize,
            ..opts()
        };
        assert_eq!(
            e.decide(H, &input(target), o, t + Duration::from_secs(3)),
            Decision::Native(NativeReason::AlreadyInZone)
        );
    }

    #[test]
    fn window_snapped_by_fancyzones_goes_native() {
        // Not our snap → no pre-snap rect known → native maximize even in restore mode.
        let mut e = Engine::new();
        let t = Instant::now();
        e.seed(H, false, Some(WIN10));
        let in_zone = ZONES[2].grow(WIN10);
        assert_eq!(
            e.decide(H, &input(in_zone), opts(), t),
            Decision::Native(NativeReason::AlreadyInZone)
        );
        // Off by a couple of pixels still counts as "in zone".
        let nearly = Rect::new(in_zone.left + 2, in_zone.top - 1, in_zone.right, in_zone.bottom + 2);
        assert_eq!(
            e.decide(H, &input(nearly), opts(), t),
            Decision::Native(NativeReason::AlreadyInZone)
        );
    }

    #[test]
    fn moved_after_snap_is_a_fresh_snap() {
        let mut e = Engine::new();
        let t = Instant::now();
        e.seed(H, false, Some(WIN10));
        e.commit_snap(H, ZONES[0], ZONES[0].grow(WIN10), Rect::new(10, 10, 500, 500), t);
        // User dragged it into the right zone area and resized; then maximizes.
        let moved = Rect::new(6000, 300, 7000, 1000);
        match e.decide(H, &input(moved), opts(), t + Duration::from_secs(5)) {
            Decision::Snap {
                zone_index, pre_snap, ..
            } => {
                assert_eq!(zone_index, 2);
                assert_eq!(pre_snap, moved, "pre-snap is the new position, not the stale one");
            }
            d => panic!("unexpected {d:?}"),
        }
    }

    #[test]
    fn drag_maximize_uses_cursor_position() {
        let mut e = Engine::new();
        let t = Instant::now();
        e.seed(H, false, Some(WIN10));
        e.move_size_start(H);
        // The window's normal rect is still over the left zone, but the cursor is at
        // the top edge above the right zone (in the spacing gap, y = 0).
        let mut i = input(Rect::new(100, 100, 1500, 1500));
        i.cursor = Point::new(7000, 0);
        maximized(&mut e, t);
        match e.decide(H, &i, opts(), t) {
            Decision::Snap { zone_index, .. } => assert_eq!(zone_index, 2),
            d => panic!("unexpected {d:?}"),
        }

        // Shortly after the move/size loop ended still counts as a drag.
        e.move_size_end(H, t);
        match e.decide(H, &i, opts(), t + Duration::from_millis(200)) {
            Decision::Snap { zone_index, .. } => assert_eq!(zone_index, 2),
            d => panic!("unexpected {d:?}"),
        }
        // Long after, the normal rect is used again.
        match e.decide(H, &i, opts(), t + Duration::from_secs(2)) {
            Decision::Snap { zone_index, .. } => assert_eq!(zone_index, 0),
            d => panic!("unexpected {d:?}"),
        }
        // Disabled by configuration.
        let o = Options {
            snap_on_drag: false,
            ..opts()
        };
        assert_eq!(
            e.decide(H, &i, o, t + Duration::from_millis(100)),
            Decision::Native(NativeReason::DragMaximizeDisabled)
        );
    }

    #[test]
    fn intercepted_clicks_ignore_the_drag_heuristic() {
        // The first click of a title-bar double-click enters a move loop, so the engine
        // sees a "drag" that just ended; intercepted gestures must use the window rect.
        let mut e = Engine::new();
        let t = Instant::now();
        e.seed(H, false, Some(WIN10));
        e.move_size_start(H);
        e.move_size_end(H, t);
        let mut i = input(Rect::new(100, 100, 1500, 1500));
        i.cursor = Point::new(7000, 0);
        i.consider_drag = false;
        match e.decide(H, &i, opts(), t) {
            Decision::Snap { zone_index, .. } => assert_eq!(zone_index, 0),
            d => panic!("unexpected {d:?}"),
        }
    }

    #[test]
    fn intercepted_precheck_only_checks_pause_and_modifier() {
        let mut e = Engine::new();
        let t = Instant::now();
        e.seed(H, false, None);
        e.commit_snap(H, ZONES[0], ZONES[0], Rect::default(), t); // would trip the fight guard
        e.arm_allow_native(H, t);
        assert_eq!(e.precheck_intercepted(false), Ok(()));
        assert_eq!(e.precheck_intercepted(true), Err(NativeReason::ModifierHeld));
        e.set_paused(true);
        assert_eq!(e.precheck_intercepted(false), Err(NativeReason::Paused));
    }

    #[test]
    fn single_zone_is_native() {
        let e = Engine::new();
        let i = MaximizeInput {
            zones: &ZONES[..1],
            normal_rect: Rect::new(0, 0, 10, 10),
            cursor: Point::default(),
            fallback_margins: WIN10,
            consider_drag: true,
        };
        assert_eq!(
            e.decide(H, &i, opts(), Instant::now()),
            Decision::Native(NativeReason::SingleZone)
        );
    }

    #[test]
    fn verify_correction_updates_snap_target() {
        let mut e = Engine::new();
        let t = Instant::now();
        e.seed(H, false, None);
        e.commit_snap(H, ZONES[1], ZONES[1], Rect::new(1, 1, 2, 2), t);
        let corrected = ZONES[1].grow(WIN10);
        e.commit_correction(H, corrected);
        assert_eq!(e.snap_record(H).unwrap().target, corrected);
        // The toggle recognizes the corrected rect.
        assert_eq!(
            e.decide(H, &input(corrected), opts(), t + Duration::from_secs(2)),
            Decision::Restore {
                target: Rect::new(1, 1, 2, 2)
            }
        );
    }

    #[test]
    fn forget_and_prune() {
        let mut e = Engine::new();
        e.seed(1, false, None);
        e.seed(2, false, None);
        e.seed(3, true, None);
        e.forget(1);
        assert_eq!(e.tracked_windows(), 2);
        assert_eq!(e.prune(|h| h == 3), 1);
        assert!(e.is_tracked(3));
        assert!(!e.is_tracked(2));
    }

    #[test]
    fn move_size_events_for_unknown_windows_are_ignored() {
        let mut e = Engine::new();
        e.move_size_start(77);
        e.move_size_end(77, Instant::now());
        assert!(!e.is_tracked(77));
    }
}
