//! Pure rectangle math: overlap, distances and zone selection.
//!
//! Everything here is independent of Win32 so it can be unit tested without a desktop.
//! Rectangles follow the Win32 `RECT` convention: `right`/`bottom` are exclusive.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// Build a rect from an origin and a size.
    #[cfg(test)]
    pub const fn from_xywh(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self::new(x, y, x + width, y + height)
    }

    pub const fn width(&self) -> i32 {
        self.right - self.left
    }

    pub const fn height(&self) -> i32 {
        self.bottom - self.top
    }

    pub const fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    /// Area in pixels; zero for empty / inverted rectangles.
    pub fn area(&self) -> i64 {
        if self.is_empty() {
            0
        } else {
            i64::from(self.width()) * i64::from(self.height())
        }
    }

    pub fn intersection(&self, other: &Rect) -> Rect {
        let r = Rect::new(
            self.left.max(other.left),
            self.top.max(other.top),
            self.right.min(other.right),
            self.bottom.min(other.bottom),
        );
        if r.is_empty() { Rect::default() } else { r }
    }

    pub fn intersection_area(&self, other: &Rect) -> i64 {
        self.intersection(other).area()
    }

    pub const fn center(&self) -> Point {
        Point::new(self.left + self.width() / 2, self.top + self.height() / 2)
    }

    pub const fn contains(&self, p: Point) -> bool {
        p.x >= self.left && p.x < self.right && p.y >= self.top && p.y < self.bottom
    }

    pub const fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.left + dx, self.top + dy, self.right + dx, self.bottom + dy)
    }

    /// Squared distance from `p` to the closest point of this rect (0 when inside).
    pub fn distance_sq_to(&self, p: Point) -> i64 {
        let dx = if p.x < self.left {
            self.left - p.x
        } else if p.x >= self.right {
            p.x - (self.right - 1)
        } else {
            0
        };
        let dy = if p.y < self.top {
            self.top - p.y
        } else if p.y >= self.bottom {
            p.y - (self.bottom - 1)
        } else {
            0
        };
        i64::from(dx) * i64::from(dx) + i64::from(dy) * i64::from(dy)
    }

    /// True when every edge is within `tolerance` pixels of `other`'s.
    pub fn approx_eq(&self, other: &Rect, tolerance: i32) -> bool {
        (self.left - other.left).abs() <= tolerance
            && (self.top - other.top).abs() <= tolerance
            && (self.right - other.right).abs() <= tolerance
            && (self.bottom - other.bottom).abs() <= tolerance
    }

    /// Grow outward by `m` (frame rect → window rect).
    pub const fn grow(&self, m: Margins) -> Rect {
        Rect::new(
            self.left - m.left,
            self.top - m.top,
            self.right + m.right,
            self.bottom + m.bottom,
        )
    }

    /// Shrink inward by `m` (window rect → frame rect).
    pub const fn shrink(&self, m: Margins) -> Rect {
        Rect::new(
            self.left + m.left,
            self.top + m.top,
            self.right - m.right,
            self.bottom - m.bottom,
        )
    }
}

impl std::fmt::Display for Rect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "({}, {})-({}, {}) [{}x{}]",
            self.left,
            self.top,
            self.right,
            self.bottom,
            self.width(),
            self.height()
        )
    }
}

/// The invisible resize borders between a window's `GetWindowRect` and its visible
/// DWM frame (`DWMWA_EXTENDED_FRAME_BOUNDS`). Positive values mean the window rect is
/// larger than the visible frame on that side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
pub struct Margins {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Margins {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// Margins from a window rect and its visible frame rect.
    pub const fn between(window: Rect, frame: Rect) -> Self {
        Self::new(
            frame.left - window.left,
            frame.top - window.top,
            window.right - frame.right,
            window.bottom - frame.bottom,
        )
    }

    /// Margins outside this range are treated as measurement garbage (e.g. a window
    /// whose DWM frame is not yet initialised).
    pub fn is_plausible(&self) -> bool {
        let ok = |v: i32| (0..=64).contains(&v);
        ok(self.left) && ok(self.top) && ok(self.right) && ok(self.bottom)
    }
}

/// Pick the zone that best matches a window rect: the largest overlap wins; ties are
/// broken by the zone containing the window's center, then by the closest zone center,
/// then by the lowest index. When nothing overlaps, fall back to the zone nearest the
/// window's center.
pub fn best_zone_for_rect(zones: &[Rect], window: &Rect) -> Option<usize> {
    if zones.is_empty() {
        return None;
    }
    let center = window.center();
    let center_dist = |z: &Rect| {
        let c = z.center();
        let dx = i64::from(c.x - center.x);
        let dy = i64::from(c.y - center.y);
        dx * dx + dy * dy
    };

    let mut best: Option<(usize, i64, bool, i64)> = None;
    for (i, z) in zones.iter().enumerate() {
        let overlap = z.intersection_area(window);
        let contains = z.contains(center);
        let dist = center_dist(z);
        let better = match best {
            None => true,
            Some((_, bo, bc, bd)) => {
                overlap > bo || (overlap == bo && ((contains && !bc) || (contains == bc && dist < bd)))
            }
        };
        if better {
            best = Some((i, overlap, contains, dist));
        }
    }

    match best {
        Some((i, overlap, _, _)) if overlap > 0 => Some(i),
        _ => nearest_zone_to_point(zones, center),
    }
}

/// The zone closest to `p` (distance 0 when inside). Ties prefer the smaller zone so
/// that, with overlapping canvas zones, the more specific one wins; then lowest index.
pub fn nearest_zone_to_point(zones: &[Rect], p: Point) -> Option<usize> {
    zones
        .iter()
        .enumerate()
        .min_by_key(|(i, z)| (z.distance_sq_to(p), z.area(), *i))
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEFT: Rect = Rect::new(0, 0, 1000, 1000);
    const MID: Rect = Rect::new(1000, 0, 3000, 1000);
    const RIGHT: Rect = Rect::new(3000, 0, 4000, 1000);
    const ZONES: [Rect; 3] = [LEFT, MID, RIGHT];

    #[test]
    fn basic_dimensions() {
        let r = Rect::from_xywh(10, 20, 30, 40);
        assert_eq!(r, Rect::new(10, 20, 40, 60));
        assert_eq!(r.width(), 30);
        assert_eq!(r.height(), 40);
        assert_eq!(r.area(), 1200);
        assert_eq!(r.center(), Point::new(25, 40));
        assert!(Rect::new(5, 5, 5, 10).is_empty());
        assert_eq!(Rect::new(10, 10, 0, 0).area(), 0);
    }

    #[test]
    fn intersection_and_contains() {
        let a = Rect::new(0, 0, 100, 100);
        let b = Rect::new(50, 50, 150, 150);
        assert_eq!(a.intersection(&b), Rect::new(50, 50, 100, 100));
        assert_eq!(a.intersection_area(&b), 2500);
        let c = Rect::new(100, 0, 200, 100); // touching edge only
        assert_eq!(a.intersection_area(&c), 0);
        assert!(a.contains(Point::new(0, 0)));
        assert!(!a.contains(Point::new(100, 50)));
    }

    #[test]
    fn distance_to_point() {
        let r = Rect::new(10, 10, 20, 20);
        assert_eq!(r.distance_sq_to(Point::new(15, 15)), 0);
        assert_eq!(r.distance_sq_to(Point::new(5, 15)), 25);
        assert_eq!(r.distance_sq_to(Point::new(22, 15)), 9); // right edge pixel is 19
        assert_eq!(r.distance_sq_to(Point::new(7, 6)), 9 + 16);
    }

    #[test]
    fn grow_shrink_roundtrip_and_margins() {
        let frame = Rect::new(100, 100, 500, 400);
        let m = Margins::new(7, 0, 7, 7);
        let window = frame.grow(m);
        assert_eq!(window, Rect::new(93, 100, 507, 407));
        assert_eq!(window.shrink(m), frame);
        assert_eq!(Margins::between(window, frame), m);
        assert!(m.is_plausible());
        assert!(!Margins::new(-1, 0, 0, 0).is_plausible());
        assert!(!Margins::new(0, 0, 500, 0).is_plausible());
    }

    #[test]
    fn approx_eq_tolerance() {
        let a = Rect::new(0, 0, 100, 100);
        assert!(a.approx_eq(&Rect::new(2, -2, 101, 99), 2));
        assert!(!a.approx_eq(&Rect::new(3, 0, 100, 100), 2));
    }

    #[test]
    fn best_zone_largest_overlap() {
        // Mostly in the middle zone, poking into the left one.
        let w = Rect::new(800, 100, 2000, 900);
        assert_eq!(best_zone_for_rect(&ZONES, &w), Some(1));
        // Mostly in the left zone.
        let w = Rect::new(100, 100, 1100, 900);
        assert_eq!(best_zone_for_rect(&ZONES, &w), Some(0));
        // Entirely in the right zone.
        let w = Rect::new(3100, 100, 3900, 900);
        assert_eq!(best_zone_for_rect(&ZONES, &w), Some(2));
    }

    #[test]
    fn best_zone_tie_prefers_zone_containing_center() {
        // Equal overlap (500 px wide each) with left and middle; center x=1000 is in MID.
        let w = Rect::new(500, 0, 1500, 1000);
        assert_eq!(LEFT.intersection_area(&w), MID.intersection_area(&w));
        assert_eq!(best_zone_for_rect(&ZONES, &w), Some(1));
    }

    #[test]
    fn best_zone_no_overlap_falls_back_to_nearest() {
        let w = Rect::new(4100, 0, 4500, 500); // entirely right of all zones
        assert_eq!(best_zone_for_rect(&ZONES, &w), Some(2));
        let w = Rect::new(-900, 0, -100, 500); // entirely left
        assert_eq!(best_zone_for_rect(&ZONES, &w), Some(0));
        assert_eq!(best_zone_for_rect(&[], &w), None);
    }

    #[test]
    fn nearest_zone_handles_spacing_gaps() {
        // Zones inset by 16px spacing; cursor at the very top edge (y=0) sits in the gap.
        let zones = [
            Rect::new(16, 16, 992, 984),
            Rect::new(1008, 16, 2992, 984),
            Rect::new(3008, 16, 3984, 984),
        ];
        assert_eq!(nearest_zone_to_point(&zones, Point::new(500, 0)), Some(0));
        assert_eq!(nearest_zone_to_point(&zones, Point::new(2000, 0)), Some(1));
        assert_eq!(nearest_zone_to_point(&zones, Point::new(3999, 0)), Some(2));
        // In the gap between zone 0 and 1, slightly closer to zone 1.
        assert_eq!(nearest_zone_to_point(&zones, Point::new(1003, 500)), Some(1));
    }

    #[test]
    fn nearest_zone_prefers_smaller_overlapping_zone() {
        let big = Rect::new(0, 0, 1000, 1000);
        let small = Rect::new(100, 100, 300, 300);
        assert_eq!(nearest_zone_to_point(&[big, small], Point::new(150, 150)), Some(1));
        assert_eq!(nearest_zone_to_point(&[big, small], Point::new(800, 800)), Some(0));
    }
}
