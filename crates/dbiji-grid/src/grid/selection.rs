//! Cell selection in display coordinates (row index, visible column position).

/// A cell position: display row and visible column position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct CellPos {
    /// Display row.
    pub row: usize,
    /// Visible column position (0 = first visible column).
    pub col: usize,
}

impl CellPos {
    /// New position.
    pub fn new(row: usize, col: usize) -> Self {
        CellPos { row, col }
    }
}

/// Inclusive rectangle of cells. `usize::MAX` bounds mean "to the end"
/// (whole rows / columns, select all) and grow with the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellRange {
    /// First row.
    pub r0: usize,
    /// Last row (inclusive).
    pub r1: usize,
    /// First column position.
    pub c0: usize,
    /// Last column position (inclusive).
    pub c1: usize,
}

impl CellRange {
    /// Rectangle spanned by two corners.
    pub fn span(a: CellPos, b: CellPos) -> Self {
        CellRange { r0: a.row.min(b.row), r1: a.row.max(b.row), c0: a.col.min(b.col), c1: a.col.max(b.col) }
    }

    /// Single cell.
    pub fn cell(p: CellPos) -> Self {
        Self::span(p, p)
    }

    /// Does the range contain the cell?
    pub fn contains(&self, row: usize, col: usize) -> bool {
        row >= self.r0 && row <= self.r1 && col >= self.c0 && col <= self.c1
    }

    /// Range clamped to the data size; `None` when empty after clamping.
    pub fn clamped(&self, rows: usize, cols: usize) -> Option<CellRange> {
        if rows == 0 || cols == 0 || self.r0 >= rows || self.c0 >= cols {
            return None;
        }
        Some(CellRange { r0: self.r0, r1: self.r1.min(rows - 1), c0: self.c0, c1: self.c1.min(cols - 1) })
    }

    /// Number of cells (after clamping).
    pub fn cell_count(&self, rows: usize, cols: usize) -> usize {
        self.clamped(rows, cols).map_or(0, |r| (r.r1 - r.r0 + 1).saturating_mul(r.c1 - r.c0 + 1))
    }

    /// True when the range covers entire rows.
    pub fn is_whole_rows(&self) -> bool {
        self.c0 == 0 && self.c1 == usize::MAX
    }

    /// True when the range covers entire columns.
    pub fn is_whole_columns(&self) -> bool {
        self.r0 == 0 && self.r1 == usize::MAX
    }
}

/// The grid selection: a list of ranges, an anchor for shift-extension and
/// the active (cursor) cell.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Selection {
    ranges: Vec<CellRange>,
    anchor: CellPos,
    active: CellPos,
}

impl Selection {
    /// Selection of a single cell.
    pub fn single(p: CellPos) -> Self {
        Selection { ranges: vec![CellRange::cell(p)], anchor: p, active: p }
    }

    /// The selected ranges (later ranges were added later).
    pub fn ranges(&self) -> &[CellRange] {
        &self.ranges
    }

    /// The active (cursor) cell.
    pub fn active(&self) -> CellPos {
        self.active
    }

    /// The anchor of shift-extension.
    pub fn anchor(&self) -> CellPos {
        self.anchor
    }

    /// True when nothing is selected.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Clear everything (the active cell stays).
    pub fn clear(&mut self) {
        self.ranges.clear();
    }

    /// Select exactly one cell.
    pub fn select_cell(&mut self, p: CellPos) {
        *self = Selection::single(p);
    }

    /// Cmd-click: add a cell as a new range (or remove it when it is the only
    /// cell of an existing single-cell range; a cell already selected
    /// otherwise only becomes the active one).
    pub fn toggle_cell(&mut self, p: CellPos) {
        if let Some(i) = self.ranges.iter().position(|r| *r == CellRange::cell(p))
            && self.ranges.len() > 1
        {
            self.ranges.remove(i);
            self.active = p;
            self.anchor = p;
            return;
        }
        if !self.contains(p.row, p.col) {
            self.ranges.push(CellRange::cell(p));
        }
        self.anchor = p;
        self.active = p;
    }

    /// Shift-click / shift-arrow: the last range becomes anchor..p.
    pub fn extend_to(&mut self, p: CellPos) {
        let r = CellRange::span(self.anchor, p);
        match self.ranges.last_mut() {
            Some(last) if !last.is_whole_rows() && !last.is_whole_columns() => *last = r,
            Some(last) if last.is_whole_rows() => *last = CellRange { r0: self.anchor.row.min(p.row), r1: self.anchor.row.max(p.row), c0: 0, c1: usize::MAX },
            Some(last) => *last = CellRange { r0: 0, r1: usize::MAX, c0: self.anchor.col.min(p.col), c1: self.anchor.col.max(p.col) },
            None => self.ranges.push(r),
        }
        self.active = p;
    }

    /// Select whole rows `r0..=r1`. `additive` keeps the existing ranges.
    pub fn select_rows(&mut self, r0: usize, r1: usize, additive: bool) {
        let range = CellRange { r0: r0.min(r1), r1: r0.max(r1), c0: 0, c1: usize::MAX };
        if !additive {
            self.ranges.clear();
        }
        self.ranges.push(range);
        self.anchor = CellPos::new(r0, self.active.col);
        self.active = CellPos::new(r1, self.active.col);
    }

    /// Select whole columns `c0..=c1` (positions).
    pub fn select_columns(&mut self, c0: usize, c1: usize, additive: bool) {
        let range = CellRange { r0: 0, r1: usize::MAX, c0: c0.min(c1), c1: c0.max(c1) };
        if !additive {
            self.ranges.clear();
        }
        self.ranges.push(range);
        let row = self.active.row;
        self.anchor = CellPos::new(row, c0);
        self.active = CellPos::new(row, c1);
    }

    /// Select everything (also rows fetched later).
    pub fn select_all(&mut self) {
        self.ranges = vec![CellRange { r0: 0, r1: usize::MAX, c0: 0, c1: usize::MAX }];
    }

    /// True when every cell is selected.
    pub fn is_all(&self) -> bool {
        self.ranges.iter().any(|r| r.r0 == 0 && r.c0 == 0 && r.r1 == usize::MAX && r.c1 == usize::MAX)
    }

    /// Is the cell selected?
    pub fn contains(&self, row: usize, col: usize) -> bool {
        self.ranges.iter().any(|r| r.contains(row, col))
    }

    /// Is any cell of `row` selected?
    pub fn row_has_selection(&self, row: usize) -> bool {
        self.ranges.iter().any(|r| row >= r.r0 && row <= r.r1)
    }

    /// Is the whole row selected?
    pub fn is_row_selected(&self, row: usize) -> bool {
        self.ranges.iter().any(|r| r.is_whole_rows() && row >= r.r0 && row <= r.r1)
    }

    /// Is the whole column selected?
    pub fn is_column_selected(&self, col: usize) -> bool {
        self.ranges.iter().any(|r| r.is_whole_columns() && col >= r.c0 && col <= r.c1)
    }

    /// Selected rows (sorted, unique), at most `limit`.
    pub fn rows(&self, nrows: usize, limit: usize) -> Vec<usize> {
        let mut spans: Vec<(usize, usize)> = self.ranges.iter().filter(|r| r.r0 < nrows).map(|r| (r.r0, r.r1.min(nrows.saturating_sub(1)))).collect();
        spans.sort_unstable();
        let mut out = Vec::new();
        let mut next = 0usize;
        for (a, b) in spans {
            let start = a.max(next);
            for r in start..=b {
                if out.len() >= limit {
                    return out;
                }
                out.push(r);
            }
            next = next.max(b + 1);
        }
        out
    }

    /// Selected column positions (sorted, unique).
    pub fn columns(&self, ncols: usize) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        for r in &self.ranges {
            if r.c0 >= ncols {
                continue;
            }
            for c in r.c0..=r.c1.min(ncols.saturating_sub(1)) {
                out.push(c);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Number of selected cells (overlaps counted once per range).
    pub fn cell_count(&self, nrows: usize, ncols: usize) -> usize {
        self.ranges.iter().map(|r| r.cell_count(nrows, ncols)).fold(0usize, usize::saturating_add)
    }

    /// Bounding box of all ranges, clamped.
    pub fn bounds(&self, nrows: usize, ncols: usize) -> Option<CellRange> {
        let mut it = self.ranges.iter().filter_map(|r| r.clamped(nrows, ncols));
        let first = it.next()?;
        Some(it.fold(first, |a, b| CellRange { r0: a.r0.min(b.r0), r1: a.r1.max(b.r1), c0: a.c0.min(b.c0), c1: a.c1.max(b.c1) }))
    }

    /// Keep the selection within the data size.
    pub fn clamp(&mut self, nrows: usize, ncols: usize) {
        if nrows == 0 || ncols == 0 {
            self.ranges.clear();
            self.active = CellPos::default();
            self.anchor = CellPos::default();
            return;
        }
        self.ranges.retain(|r| r.r0 < nrows && r.c0 < ncols);
        self.active.row = self.active.row.min(nrows - 1);
        self.active.col = self.active.col.min(ncols - 1);
        self.anchor.row = self.anchor.row.min(nrows - 1);
        self.anchor.col = self.anchor.col.min(ncols - 1);
    }

    /// Move the active cell by (`dr`, `dc`), clamped. With `extend` the
    /// selection grows from the anchor, otherwise it collapses to the cell.
    pub fn move_by(&mut self, dr: isize, dc: isize, nrows: usize, ncols: usize, extend: bool) {
        if nrows == 0 || ncols == 0 {
            return;
        }
        let row = (self.active.row as isize + dr).clamp(0, nrows as isize - 1) as usize;
        let col = (self.active.col as isize + dc).clamp(0, ncols as isize - 1) as usize;
        self.move_to(CellPos::new(row, col), extend);
    }

    /// Move the active cell to `p`.
    pub fn move_to(&mut self, p: CellPos, extend: bool) {
        if extend {
            self.extend_to(p);
        } else {
            self.select_cell(p);
        }
    }

    /// Tab navigation: next (or previous) cell, wrapping to the next row.
    pub fn tab(&mut self, backwards: bool, nrows: usize, ncols: usize) {
        if nrows == 0 || ncols == 0 {
            return;
        }
        let CellPos { mut row, mut col } = self.active;
        if backwards {
            if col > 0 {
                col -= 1;
            } else if row > 0 {
                row -= 1;
                col = ncols - 1;
            }
        } else if col + 1 < ncols {
            col += 1;
        } else if row + 1 < nrows {
            row += 1;
            col = 0;
        }
        self.select_cell(CellPos::new(row, col));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_extend_toggle() {
        let mut s = Selection::single(CellPos::new(2, 1));
        assert!(s.contains(2, 1));
        s.extend_to(CellPos::new(4, 3));
        assert_eq!(s.ranges(), &[CellRange { r0: 2, r1: 4, c0: 1, c1: 3 }]);
        assert_eq!(s.cell_count(100, 10), 9);
        s.extend_to(CellPos::new(0, 0));
        assert_eq!(s.ranges(), &[CellRange { r0: 0, r1: 2, c0: 0, c1: 1 }]);
        s.toggle_cell(CellPos::new(9, 9));
        assert_eq!(s.ranges().len(), 2);
        assert!(s.contains(9, 9));
        s.toggle_cell(CellPos::new(9, 9));
        assert!(!s.contains(9, 9));
        assert_eq!(s.rows(100, usize::MAX), vec![0, 1, 2]);
        assert_eq!(s.columns(10), vec![0, 1]);
    }

    #[test]
    fn rows_columns_all() {
        let mut s = Selection::default();
        s.select_rows(3, 1, false);
        assert!(s.is_row_selected(2));
        assert!(s.contains(2, 50));
        // The anchor is the first row passed (where the click/drag started).
        s.extend_to(CellPos::new(5, 0));
        assert_eq!(s.rows(100, 100), vec![3, 4, 5]);
        s.select_columns(2, 3, true);
        assert!(s.is_column_selected(3));
        assert_eq!(s.columns(5), vec![0, 1, 2, 3, 4]);
        assert_eq!(s.rows(10, 100).len(), 10);
        assert_eq!(s.rows(10, 4), vec![0, 1, 2, 3]);
        s.select_all();
        assert!(s.is_all());
        assert_eq!(s.cell_count(1_000_000, 100), 100_000_000);
        assert_eq!(s.bounds(7, 3), Some(CellRange { r0: 0, r1: 6, c0: 0, c1: 2 }));
    }

    #[test]
    fn navigation() {
        let mut s = Selection::single(CellPos::new(0, 0));
        s.move_by(-1, -1, 10, 5, false);
        assert_eq!(s.active(), CellPos::new(0, 0));
        s.move_by(3, 2, 10, 5, true);
        assert_eq!(s.ranges(), &[CellRange { r0: 0, r1: 3, c0: 0, c1: 2 }]);
        s.move_by(100, 100, 10, 5, false);
        assert_eq!(s.active(), CellPos::new(9, 4));
        s.tab(false, 10, 5);
        assert_eq!(s.active(), CellPos::new(9, 4), "stays at the last cell");
        s.select_cell(CellPos::new(1, 4));
        s.tab(false, 10, 5);
        assert_eq!(s.active(), CellPos::new(2, 0));
        s.tab(true, 10, 5);
        assert_eq!(s.active(), CellPos::new(1, 4));
        s.clamp(1, 2);
        assert_eq!(s.active(), CellPos::new(0, 1));
        s.clamp(0, 0);
        assert!(s.is_empty());
    }
}
