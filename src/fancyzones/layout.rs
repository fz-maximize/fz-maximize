//! Port of FancyZones' `LayoutConfigurator` (PowerToys `FancyZonesLib/LayoutConfigurator.cpp`).
//!
//! The arithmetic mirrors the C++ exactly (integer truncation order, spacing rules, float
//! handling for canvas layouts) so the zones computed here match the ones FancyZones
//! snaps windows to, to the pixel. All rectangles are **relative to the work-area
//! origin**; callers offset them by `rcWork.left/top` to get screen coordinates.

use crate::geometry::Rect;

const C_MULTIPLIER: i64 = 10_000;
/// `ZoneConstants::MAX_NEGATIVE_SPACING`
const MAX_NEGATIVE_SPACING: i32 = -20;
const DEFAULT_DPI: f32 = 96.0;

/// The `type` of an applied layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutType {
    Blank,
    Focus,
    Columns,
    Rows,
    Grid,
    PriorityGrid,
    Custom,
}

impl LayoutType {
    /// `FancyZonesDataTypes::TypeFromString`: unknown strings map to `Blank`.
    pub fn from_fz_str(s: &str) -> Self {
        match s {
            "focus" => Self::Focus,
            "columns" => Self::Columns,
            "rows" => Self::Rows,
            "grid" => Self::Grid,
            "priority-grid" => Self::PriorityGrid,
            "custom" => Self::Custom,
            _ => Self::Blank,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Blank => "blank",
            Self::Focus => "focus",
            Self::Columns => "columns",
            Self::Rows => "rows",
            Self::Grid => "grid",
            Self::PriorityGrid => "priority-grid",
            Self::Custom => "custom",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GridInfo {
    pub rows: usize,
    pub columns: usize,
    pub rows_percents: Vec<i32>,
    pub columns_percents: Vec<i32>,
    pub cell_child_map: Vec<Vec<i32>>,
}

impl GridInfo {
    fn full(rows: usize, columns: usize, rp: &[i32], cp: &[i32], map: &[&[i32]]) -> Self {
        Self {
            rows,
            columns,
            rows_percents: rp.to_vec(),
            columns_percents: cp.to_vec(),
            cell_child_map: map.iter().map(|r| r.to_vec()).collect(),
        }
    }

    /// Structural validity (sizes agree); FancyZones rejects mismatched data on parse.
    pub fn is_well_formed(&self) -> bool {
        self.rows > 0
            && self.columns > 0
            && self.rows_percents.len() == self.rows
            && self.columns_percents.len() == self.columns
            && self.cell_child_map.len() == self.rows
            && self.cell_child_map.iter().all(|r| r.len() == self.columns)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CanvasZone {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanvasInfo {
    pub ref_width: i32,
    pub ref_height: i32,
    pub zones: Vec<CanvasZone>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CustomLayout {
    Grid(GridInfo),
    Canvas(CanvasInfo),
}

/// The part of an applied layout that determines geometry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutSpec {
    pub kind: LayoutType,
    pub zone_count: i32,
    pub show_spacing: bool,
    pub spacing: i32,
}

impl LayoutSpec {
    /// `Layout::Init`: spacing only applies when `show-spacing` is set.
    pub fn effective_spacing(&self) -> i32 {
        if self.show_spacing { self.spacing } else { 0 }
    }
}

/// Why a layout produced no zones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutError {
    InvalidWorkArea,
    InvalidZoneCount,
    CustomLayoutMissing,
    InvalidZone,
    DuplicateZoneId,
    /// FancyZones' `Layout::Init` fails unless it produces exactly `zone-count` zones.
    ZoneCountMismatch {
        expected: i32,
        actual: usize,
    },
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidWorkArea => write!(f, "invalid work area"),
            Self::InvalidZoneCount => write!(f, "invalid zone count"),
            Self::CustomLayoutMissing => write!(f, "custom layout not found in custom-layouts.json"),
            Self::InvalidZone => write!(f, "layout produced an invalid zone"),
            Self::DuplicateZoneId => write!(f, "layout has a duplicate zone id"),
            Self::ZoneCountMismatch { expected, actual } => {
                write!(f, "layout produced {actual} zones but zone-count is {expected}")
            }
        }
    }
}

/// Compute the zones for a layout on a work area of `width` x `height` physical pixels.
///
/// `custom` must be supplied for [`LayoutType::Custom`]; `dpi` is the monitor's effective
/// DPI (only used by canvas layouts, where it cancels out except for float rounding).
pub fn compute_zones(
    spec: &LayoutSpec,
    width: i32,
    height: i32,
    dpi: u32,
    custom: Option<&CustomLayout>,
) -> Result<Vec<Rect>, LayoutError> {
    if width == 0 || height == 0 {
        return Err(LayoutError::InvalidWorkArea);
    }
    let is_grid_type = matches!(
        spec.kind,
        LayoutType::Columns | LayoutType::Rows | LayoutType::Grid | LayoutType::PriorityGrid
    );
    if spec.zone_count < 0 || (spec.zone_count == 0 && is_grid_type) {
        return Err(LayoutError::InvalidZoneCount);
    }

    let spacing = spec.effective_spacing();
    let n = spec.zone_count;
    let zones = match spec.kind {
        LayoutType::Blank => Vec::new(),
        LayoutType::Focus => focus(width, height, n)?,
        LayoutType::Columns => columns(width, height, n, spacing)?,
        LayoutType::Rows => rows(width, height, n, spacing)?,
        LayoutType::Grid => grid(width, height, n, spacing)?,
        LayoutType::PriorityGrid => priority_grid(width, height, n, spacing)?,
        LayoutType::Custom => match custom {
            Some(CustomLayout::Grid(info)) => calculate_grid_zones(width, height, info, spacing)?,
            Some(CustomLayout::Canvas(info)) => canvas(width, height, dpi, info)?,
            None => return Err(LayoutError::CustomLayoutMissing),
        },
    };

    if zones.len() != usize::try_from(n).unwrap_or(usize::MAX) {
        return Err(LayoutError::ZoneCountMismatch {
            expected: n,
            actual: zones.len(),
        });
    }
    Ok(zones)
}

fn zone_is_valid(r: &Rect) -> bool {
    r.left >= MAX_NEGATIVE_SPACING
        && r.right >= MAX_NEGATIVE_SPACING
        && r.top >= MAX_NEGATIVE_SPACING
        && r.bottom >= MAX_NEGATIVE_SPACING
        && r.width() >= 0
        && r.height() >= 0
}

fn push_zone(zones: &mut Vec<Rect>, r: Rect) -> Result<(), LayoutError> {
    if !zone_is_valid(&r) {
        return Err(LayoutError::InvalidZone);
    }
    zones.push(r);
    Ok(())
}

/// `CalculateGridZones`
pub fn calculate_grid_zones(width: i32, height: i32, info: &GridInfo, spacing: i32) -> Result<Vec<Rect>, LayoutError> {
    if !info.is_well_formed() {
        return Err(LayoutError::InvalidZone);
    }
    let total_w = i64::from(width);
    let total_h = i64::from(height);

    // (start, end) per row/column; written so the extents sum exactly to the total.
    let spans = |percents: &[i32], total: i64| -> Vec<(i64, i64)> {
        let mut acc: i64 = 0;
        percents
            .iter()
            .map(|p| {
                let start = acc * total / C_MULTIPLIER;
                acc += i64::from(*p);
                (start, acc * total / C_MULTIPLIER)
            })
            .collect()
    };
    let row_info = spans(&info.rows_percents, total_h);
    let col_info = spans(&info.columns_percents, total_w);

    let map = &info.cell_child_map;
    let half = spacing / 2;
    let mut by_id: Vec<(i32, Rect)> = Vec::new();

    for row in 0..info.rows {
        for col in 0..info.columns {
            let id = map[row][col];
            let first_in_row = row == 0 || map[row - 1][col] != id;
            let first_in_col = col == 0 || map[row][col - 1] != id;
            if !(first_in_row && first_in_col) {
                continue;
            }

            let mut max_row = row;
            while max_row + 1 < info.rows && map[max_row + 1][col] == id {
                max_row += 1;
            }
            let mut max_col = col;
            while max_col + 1 < info.columns && map[row][max_col + 1] == id {
                max_col += 1;
            }

            let mut left = col_info[col].0 as i32;
            let mut top = row_info[row].0 as i32;
            let mut right = col_info[max_col].1 as i32;
            let mut bottom = row_info[max_row].1 as i32;

            top += if row == 0 { spacing } else { half };
            bottom -= if max_row == info.rows - 1 { spacing } else { half };
            left += if col == 0 { spacing } else { half };
            right -= if max_col == info.columns - 1 { spacing } else { half };

            let r = Rect::new(left, top, right, bottom);
            if id < 0 || !zone_is_valid(&r) {
                return Err(LayoutError::InvalidZone);
            }
            if by_id.iter().any(|(existing, _)| *existing == id) {
                return Err(LayoutError::DuplicateZoneId);
            }
            by_id.push((id, r));
        }
    }

    // FancyZones stores zones in a map keyed by id, so zone order == id order.
    by_id.sort_by_key(|(id, _)| *id);
    Ok(by_id.into_iter().map(|(_, r)| r).collect())
}

/// `LayoutConfigurator::Focus`
fn focus(width: i32, height: i32, zone_count: i32) -> Result<Vec<Rect>, LayoutError> {
    let left = 100;
    let top = 100;
    let right = left + (f64::from(width) * 0.4) as i32;
    let bottom = top + (f64::from(height) * 0.4) as i32;
    let mut r = Rect::new(left, top, right, bottom);
    let inc = if zone_count <= 1 { 0 } else { 50 };

    let mut zones = Vec::new();
    for _ in 0..zone_count {
        push_zone(&mut zones, r)?;
        r = r.offset(inc, inc);
    }
    Ok(zones)
}

/// `LayoutConfigurator::Rows`
fn rows(width: i32, height: i32, zone_count: i32, spacing: i32) -> Result<Vec<Rect>, LayoutError> {
    if zone_count == 0 {
        return Ok(Vec::new());
    }
    let total_w = i64::from(width - spacing * 2);
    let total_h = i64::from(height - spacing * (zone_count + 1));
    let n = i64::from(zone_count);
    let mut top = i64::from(spacing);
    let left = i64::from(spacing);
    let mut zones = Vec::new();
    for i in 0..n {
        let right = total_w + i64::from(spacing);
        let bottom = top + (i + 1) * total_h / n - i * total_h / n;
        push_zone(
            &mut zones,
            Rect::new(left as i32, top as i32, right as i32, bottom as i32),
        )?;
        top = bottom + i64::from(spacing);
    }
    Ok(zones)
}

/// `LayoutConfigurator::Columns`
fn columns(width: i32, height: i32, zone_count: i32, spacing: i32) -> Result<Vec<Rect>, LayoutError> {
    if zone_count == 0 {
        return Ok(Vec::new());
    }
    let total_w = i64::from(width - spacing * (zone_count + 1));
    let total_h = i64::from(height - spacing * 2);
    let n = i64::from(zone_count);
    let top = i64::from(spacing);
    let mut left = i64::from(spacing);
    let mut zones = Vec::new();
    for i in 0..n {
        let right = left + (i + 1) * total_w / n - i * total_w / n;
        let bottom = total_h + i64::from(spacing);
        push_zone(
            &mut zones,
            Rect::new(left as i32, top as i32, right as i32, bottom as i32),
        )?;
        left = right + i64::from(spacing);
    }
    Ok(zones)
}

/// The grid that `LayoutConfigurator::Grid` builds for `zone_count` zones.
pub fn grid_info_for(zone_count: i32) -> GridInfo {
    let mut rows: i32 = 1;
    while zone_count / rows >= rows {
        rows += 1;
    }
    rows -= 1;
    let mut columns = zone_count / rows;
    if zone_count % rows != 0 {
        columns += 1;
    }

    let m = C_MULTIPLIER as i32;
    let rows_percents = (0..rows).map(|r| m * (r + 1) / rows - m * r / rows).collect();
    let columns_percents = (0..columns).map(|c| m * (c + 1) / columns - m * c / columns).collect();

    let mut index = 0;
    let mut cell_child_map = vec![vec![0; columns as usize]; rows as usize];
    for row in cell_child_map.iter_mut() {
        for cell in row.iter_mut() {
            *cell = index;
            index += 1;
            if index == zone_count {
                index -= 1;
            }
        }
    }

    GridInfo {
        rows: rows as usize,
        columns: columns as usize,
        rows_percents,
        columns_percents,
        cell_child_map,
    }
}

/// `LayoutConfigurator::Grid`
fn grid(width: i32, height: i32, zone_count: i32, spacing: i32) -> Result<Vec<Rect>, LayoutError> {
    if zone_count == 0 {
        return Ok(Vec::new());
    }
    calculate_grid_zones(width, height, &grid_info_for(zone_count), spacing)
}

/// The predefined priority grids (`predefinedPriorityGridLayouts`), for 1..=11 zones.
pub fn priority_grid_info(zone_count: i32) -> Option<GridInfo> {
    const R1: &[i32] = &[10000];
    const R2: &[i32] = &[5000, 5000];
    const R3: &[i32] = &[3333, 3334, 3333];
    const C3: &[i32] = &[2500, 5000, 2500];
    const C4: &[i32] = &[2500, 2500, 2500, 2500];
    let g = match zone_count {
        1 => GridInfo::full(1, 1, R1, R1, &[&[0]]),
        2 => GridInfo::full(1, 2, R1, &[6667, 3333], &[&[0, 1]]),
        3 => GridInfo::full(1, 3, R1, C3, &[&[0, 1, 2]]),
        4 => GridInfo::full(2, 3, R2, C3, &[&[0, 1, 2], &[0, 1, 3]]),
        5 => GridInfo::full(2, 3, R2, C3, &[&[0, 1, 2], &[3, 1, 4]]),
        6 => GridInfo::full(3, 3, R3, C3, &[&[0, 1, 2], &[0, 1, 3], &[4, 1, 5]]),
        7 => GridInfo::full(3, 3, R3, C3, &[&[0, 1, 2], &[3, 1, 4], &[5, 1, 6]]),
        8 => GridInfo::full(3, 4, R3, C4, &[&[0, 1, 2, 3], &[4, 1, 2, 5], &[6, 1, 2, 7]]),
        9 => GridInfo::full(3, 4, R3, C4, &[&[0, 1, 2, 3], &[4, 1, 2, 5], &[6, 1, 7, 8]]),
        10 => GridInfo::full(3, 4, R3, C4, &[&[0, 1, 2, 3], &[4, 1, 5, 6], &[7, 1, 8, 9]]),
        11 => GridInfo::full(3, 4, R3, C4, &[&[0, 1, 2, 3], &[4, 1, 5, 6], &[7, 8, 9, 10]]),
        _ => return None,
    };
    Some(g)
}

/// `LayoutConfigurator::PriorityGrid`: predefined up to 11 zones, plain grid beyond.
fn priority_grid(width: i32, height: i32, zone_count: i32, spacing: i32) -> Result<Vec<Rect>, LayoutError> {
    if zone_count <= 0 {
        return Ok(Vec::new());
    }
    match priority_grid_info(zone_count) {
        Some(info) => calculate_grid_zones(width, height, &info, spacing),
        None => grid(width, height, zone_count, spacing),
    }
}

/// `LayoutConfigurator::Custom` for canvas layouts. Spacing does not apply to canvases.
fn canvas(width: i32, height: i32, dpi: u32, info: &CanvasInfo) -> Result<Vec<Rect>, LayoutError> {
    if info.ref_width == 0 || info.ref_height == 0 {
        return Err(LayoutError::InvalidZone);
    }
    let dpi = if dpi == 0 { DEFAULT_DPI } else { dpi as f32 };
    // DPIAware::InverseConvert
    let w = width as f32 * DEFAULT_DPI / dpi;
    let h = height as f32 * DEFAULT_DPI / dpi;
    let ref_w = info.ref_width as f32;
    let ref_h = info.ref_height as f32;

    let mut zones = Vec::new();
    for z in &info.zones {
        let mut x = z.x as f32 * w / ref_w;
        let mut y = z.y as f32 * h / ref_h;
        let mut zw = z.width as f32 * w / ref_w;
        let mut zh = z.height as f32 * h / ref_h;
        // DPIAware::Convert
        x = x * dpi / DEFAULT_DPI;
        y = y * dpi / DEFAULT_DPI;
        zw = zw * dpi / DEFAULT_DPI;
        zh = zh * dpi / DEFAULT_DPI;
        push_zone(
            &mut zones,
            Rect::new(x as i32, y as i32, (x + zw) as i32, (y + zh) as i32),
        )?;
    }
    Ok(zones)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: LayoutType, zone_count: i32, spacing: Option<i32>) -> LayoutSpec {
        LayoutSpec {
            kind,
            zone_count,
            show_spacing: spacing.is_some(),
            spacing: spacing.unwrap_or(16),
        }
    }

    #[test]
    fn layout_type_strings_roundtrip() {
        for t in [
            LayoutType::Blank,
            LayoutType::Focus,
            LayoutType::Columns,
            LayoutType::Rows,
            LayoutType::Grid,
            LayoutType::PriorityGrid,
            LayoutType::Custom,
        ] {
            assert_eq!(LayoutType::from_fz_str(t.as_str()), t);
        }
        assert_eq!(LayoutType::from_fz_str("something-new"), LayoutType::Blank);
    }

    #[test]
    fn priority_grid_3_ultrawide_dual_4k_with_spacing() {
        // 7680x2160 panel with a 48px bottom taskbar → 7680x2112 work area.
        let z = compute_zones(&spec(LayoutType::PriorityGrid, 3, Some(16)), 7680, 2112, 96, None).unwrap();
        assert_eq!(
            z,
            vec![
                Rect::new(16, 16, 1912, 2096),   // 25 %
                Rect::new(1928, 16, 5752, 2096), // 50 %
                Rect::new(5768, 16, 7664, 2096), // 25 %
            ]
        );
    }

    #[test]
    fn priority_grid_3_without_spacing_is_exact_quarters() {
        let z = compute_zones(&spec(LayoutType::PriorityGrid, 3, None), 7680, 2160, 96, None).unwrap();
        assert_eq!(
            z,
            vec![
                Rect::new(0, 0, 1920, 2160),
                Rect::new(1920, 0, 5760, 2160),
                Rect::new(5760, 0, 7680, 2160),
            ]
        );
        // Zones tile the work area exactly.
        let total: i64 = z.iter().map(|r| r.area()).sum();
        assert_eq!(total, 7680 * 2160);
    }

    #[test]
    fn priority_grid_odd_widths_tile_exactly() {
        // Widths not divisible by 4 must still tile with no gaps/overlap.
        for w in [1919, 2561, 3437, 5119] {
            let z = compute_zones(&spec(LayoutType::PriorityGrid, 3, None), w, 1000, 96, None).unwrap();
            assert_eq!(z[0].left, 0);
            assert_eq!(z[0].right, z[1].left);
            assert_eq!(z[1].right, z[2].left);
            assert_eq!(z[2].right, w);
        }
    }

    #[test]
    fn priority_grid_4_merges_center_column_and_left_column() {
        // {0,1,2},{0,1,3}: zone 0 spans both rows of col 0, zone 1 spans col 1.
        let z = compute_zones(&spec(LayoutType::PriorityGrid, 4, None), 4000, 1000, 96, None).unwrap();
        assert_eq!(
            z,
            vec![
                Rect::new(0, 0, 1000, 1000),
                Rect::new(1000, 0, 3000, 1000),
                Rect::new(3000, 0, 4000, 500),
                Rect::new(3000, 500, 4000, 1000),
            ]
        );
    }

    #[test]
    fn priority_grid_spacing_inner_edges_use_half_spacing() {
        let z = compute_zones(&spec(LayoutType::PriorityGrid, 4, Some(10)), 4000, 1000, 96, None).unwrap();
        // Zone 2 (top-right): top edge is outer (+10), bottom edge is inner (-5).
        assert_eq!(z[2], Rect::new(3005, 10, 3990, 495));
        assert_eq!(z[3], Rect::new(3005, 505, 3990, 990));
    }

    #[test]
    fn priority_grid_all_predefined_counts_are_valid() {
        for n in 1..=11 {
            let z = compute_zones(&spec(LayoutType::PriorityGrid, n, Some(16)), 3840, 2112, 96, None)
                .unwrap_or_else(|e| panic!("zone count {n}: {e}"));
            assert_eq!(z.len(), n as usize);
        }
        // Beyond 11 falls back to a plain grid.
        let z = compute_zones(&spec(LayoutType::PriorityGrid, 12, None), 1200, 900, 96, None).unwrap();
        assert_eq!(z.len(), 12);
    }

    #[test]
    fn columns_with_spacing() {
        let z = compute_zones(&spec(LayoutType::Columns, 3, Some(16)), 1000, 500, 96, None).unwrap();
        // total_w = 1000 - 64 = 936 → 312 each.
        assert_eq!(
            z,
            vec![
                Rect::new(16, 16, 328, 484),
                Rect::new(344, 16, 656, 484),
                Rect::new(672, 16, 984, 484),
            ]
        );
    }

    #[test]
    fn rows_with_spacing() {
        let z = compute_zones(&spec(LayoutType::Rows, 3, Some(16)), 500, 1000, 96, None).unwrap();
        assert_eq!(
            z,
            vec![
                Rect::new(16, 16, 484, 328),
                Rect::new(16, 344, 484, 656),
                Rect::new(16, 672, 484, 984),
            ]
        );
    }

    #[test]
    fn columns_uneven_division_sums_exactly() {
        let z = compute_zones(&spec(LayoutType::Columns, 3, None), 1000, 100, 96, None).unwrap();
        assert_eq!(z[0], Rect::new(0, 0, 333, 100));
        assert_eq!(z[1], Rect::new(333, 0, 666, 100));
        assert_eq!(z[2], Rect::new(666, 0, 1000, 100));
    }

    #[test]
    fn grid_info_shapes() {
        let g = grid_info_for(4);
        assert_eq!((g.rows, g.columns), (2, 2));
        let g = grid_info_for(5);
        assert_eq!((g.rows, g.columns), (2, 3));
        // Last zone absorbs the trailing cell.
        assert_eq!(g.cell_child_map, vec![vec![0, 1, 2], vec![3, 4, 4]]);
        let g = grid_info_for(3);
        assert_eq!((g.rows, g.columns), (1, 3));
        let g = grid_info_for(9);
        assert_eq!((g.rows, g.columns), (3, 3));
        // Generated grids put the rounding remainder last (unlike the priority grids).
        assert_eq!(g.rows_percents, vec![3333, 3333, 3334]);
        assert_eq!(g.rows_percents.iter().sum::<i32>(), 10000);
    }

    #[test]
    fn grid_5_last_zone_spans_two_cells() {
        let z = compute_zones(&spec(LayoutType::Grid, 5, None), 900, 600, 96, None).unwrap();
        assert_eq!(z.len(), 5);
        // Column percents are 3333/3333/3334, so column 1 starts at 3333*900/10000 = 299.
        assert_eq!(z[4], Rect::new(299, 300, 900, 600));
    }

    #[test]
    fn focus_layout() {
        let z = compute_zones(&spec(LayoutType::Focus, 3, None), 1000, 500, 96, None).unwrap();
        assert_eq!(
            z,
            vec![
                Rect::new(100, 100, 500, 300),
                Rect::new(150, 150, 550, 350),
                Rect::new(200, 200, 600, 400),
            ]
        );
    }

    #[test]
    fn blank_layout_has_no_zones() {
        let z = compute_zones(&spec(LayoutType::Blank, 0, None), 1000, 500, 96, None).unwrap();
        assert!(z.is_empty());
    }

    #[test]
    fn invalid_inputs() {
        assert_eq!(
            compute_zones(&spec(LayoutType::Grid, 3, None), 0, 500, 96, None),
            Err(LayoutError::InvalidWorkArea)
        );
        assert_eq!(
            compute_zones(&spec(LayoutType::Columns, 0, None), 100, 100, 96, None),
            Err(LayoutError::InvalidZoneCount)
        );
        assert_eq!(
            compute_zones(&spec(LayoutType::Custom, 2, None), 100, 100, 96, None),
            Err(LayoutError::CustomLayoutMissing)
        );
        // Spacing so large zones invert → invalid.
        assert_eq!(
            compute_zones(&spec(LayoutType::Columns, 3, Some(400)), 1000, 500, 96, None),
            Err(LayoutError::InvalidZone)
        );
    }

    #[test]
    fn custom_grid_25_50_25_with_merged_cells() {
        // Two rows; the center column is one tall zone; sides are split.
        let info = GridInfo::full(2, 3, &[5000, 5000], &[2500, 5000, 2500], &[&[0, 1, 2], &[3, 1, 4]]);
        let s = spec(LayoutType::Custom, 5, None);
        let z = compute_zones(&s, 7680, 2160, 96, Some(&CustomLayout::Grid(info))).unwrap();
        assert_eq!(z[1], Rect::new(1920, 0, 5760, 2160));
        assert_eq!(z[0], Rect::new(0, 0, 1920, 1080));
        assert_eq!(z[3], Rect::new(0, 1080, 1920, 2160));
        assert_eq!(z[4], Rect::new(5760, 1080, 7680, 2160));
    }

    #[test]
    fn custom_grid_zone_order_follows_ids_not_position() {
        let info = GridInfo::full(1, 3, &[10000], &[2500, 5000, 2500], &[&[2, 0, 1]]);
        let z = compute_zones(
            &spec(LayoutType::Custom, 3, None),
            4000,
            1000,
            96,
            Some(&CustomLayout::Grid(info)),
        )
        .unwrap();
        assert_eq!(z[0], Rect::new(1000, 0, 3000, 1000)); // id 0 is the middle cell
        assert_eq!(z[2], Rect::new(0, 0, 1000, 1000));
    }

    #[test]
    fn custom_grid_rejects_duplicate_disjoint_ids_and_bad_shapes() {
        let dup = GridInfo::full(1, 3, &[10000], &[3333, 3334, 3333], &[&[0, 1, 0]]);
        assert_eq!(
            calculate_grid_zones(900, 100, &dup, 0),
            Err(LayoutError::DuplicateZoneId)
        );
        let bad = GridInfo {
            rows: 2,
            columns: 2,
            rows_percents: vec![10000],
            columns_percents: vec![5000, 5000],
            cell_child_map: vec![vec![0, 1]],
        };
        assert!(calculate_grid_zones(900, 100, &bad, 0).is_err());
    }

    #[test]
    fn custom_layout_zone_count_mismatch_is_rejected() {
        let info = GridInfo::full(1, 2, &[10000], &[5000, 5000], &[&[0, 1]]);
        let r = compute_zones(
            &spec(LayoutType::Custom, 3, None),
            1000,
            1000,
            96,
            Some(&CustomLayout::Grid(info)),
        );
        assert_eq!(r, Err(LayoutError::ZoneCountMismatch { expected: 3, actual: 2 }));
    }

    #[test]
    fn canvas_scales_from_reference_size_and_ignores_spacing() {
        let info = CanvasInfo {
            ref_width: 3840,
            ref_height: 1080,
            zones: vec![
                CanvasZone {
                    x: 0,
                    y: 0,
                    width: 960,
                    height: 1080,
                },
                CanvasZone {
                    x: 960,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
                CanvasZone {
                    x: 2880,
                    y: 0,
                    width: 960,
                    height: 1080,
                },
            ],
        };
        // Physical 7680x2160 at 200% scaling: logical size equals the reference size × 1.
        let z = compute_zones(
            &spec(LayoutType::Custom, 3, Some(16)),
            7680,
            2160,
            192,
            Some(&CustomLayout::Canvas(info.clone())),
        )
        .unwrap();
        assert_eq!(z[0], Rect::new(0, 0, 1920, 2160));
        assert_eq!(z[1], Rect::new(1920, 0, 5760, 2160));
        assert_eq!(z[2], Rect::new(5760, 0, 7680, 2160));

        // DPI cancels out: same physical result at 100 %.
        let z96 = compute_zones(
            &spec(LayoutType::Custom, 3, None),
            7680,
            2160,
            96,
            Some(&CustomLayout::Canvas(info)),
        )
        .unwrap();
        assert_eq!(z, z96);
    }

    #[test]
    fn canvas_allows_slightly_negative_and_overlapping_zones() {
        let info = CanvasInfo {
            ref_width: 1000,
            ref_height: 1000,
            zones: vec![
                CanvasZone {
                    x: -10,
                    y: -10,
                    width: 600,
                    height: 600,
                },
                CanvasZone {
                    x: 400,
                    y: 400,
                    width: 600,
                    height: 600,
                },
            ],
        };
        let z = compute_zones(
            &spec(LayoutType::Custom, 2, None),
            1000,
            1000,
            96,
            Some(&CustomLayout::Canvas(info)),
        )
        .unwrap();
        assert_eq!(z[0], Rect::new(-10, -10, 590, 590));
        let too_negative = CanvasInfo {
            ref_width: 1000,
            ref_height: 1000,
            zones: vec![CanvasZone {
                x: -50,
                y: 0,
                width: 100,
                height: 100,
            }],
        };
        assert_eq!(canvas(1000, 1000, 96, &too_negative), Err(LayoutError::InvalidZone));
    }
}
