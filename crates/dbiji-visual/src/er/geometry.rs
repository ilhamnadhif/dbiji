//! Box geometry shared by the layout, the canvas and the image export.
//!
//! All sizes are in diagram units (points at 100 % zoom) and derived from
//! font independent text metrics, so the same diagram has the same geometry
//! everywhere.

use super::diagram::{ColumnDisplay, ErSettings};
use super::model::ErTable;
use crate::common::text::{UI_SAFETY, text_width, truncate_chars};
use egui::{Vec2, vec2};

/// Font size of the table name.
pub(crate) const HEADER_FONT: f32 = 13.0;
/// Font size of the schema name in the header.
pub(crate) const SCHEMA_FONT: f32 = 10.5;
/// Font size of column rows.
pub(crate) const ROW_FONT: f32 = 12.0;
/// Font size of the comment line.
pub(crate) const COMMENT_FONT: f32 = 11.0;
/// Height of the header band.
pub(crate) const HEADER_HEIGHT: f32 = 28.0;
/// Height of the optional comment line.
pub(crate) const COMMENT_HEIGHT: f32 = 18.0;
/// Height of one column row.
pub(crate) const ROW_HEIGHT: f32 = 20.0;
/// Horizontal padding inside the box.
pub(crate) const PAD_X: f32 = 10.0;
/// Width reserved for the key icon of a row and the kind icon of the header.
pub(crate) const ICON_WIDTH: f32 = 18.0;
/// Gap between the name and type columns.
pub(crate) const TYPE_GAP: f32 = 14.0;
/// Padding below the last row.
pub(crate) const BOTTOM_PAD: f32 = 5.0;
/// Corner radius of boxes.
pub(crate) const CORNER_RADIUS: f32 = 6.0;
/// Minimum and maximum box width.
pub(crate) const MIN_WIDTH: f32 = 140.0;
pub(crate) const MAX_WIDTH: f32 = 460.0;
/// Maximum characters of the comment line.
const COMMENT_MAX_CHARS: usize = 80;

/// Geometry of one box, relative to its top-left corner.
#[derive(Debug, Clone, PartialEq)]
pub struct TableGeometry {
    /// Outer size of the box.
    pub size: Vec2,
    /// Height of the header band (name line).
    pub header_height: f32,
    /// Y offset of the first column row (header plus optional comment).
    pub body_top: f32,
    /// Indices into `ErTable::columns` of the listed columns, in order.
    pub rows: Vec<usize>,
    /// X offset of column names.
    pub name_x: f32,
    /// X offset of column types.
    pub type_x: f32,
    /// Comment line (already shortened), when shown.
    pub comment: Option<String>,
}

impl TableGeometry {
    /// Y offset of the top of row `i` (index into [`Self::rows`]).
    pub fn row_top(&self, i: usize) -> f32 {
        self.body_top + i as f32 * ROW_HEIGHT
    }

    /// Y offset (relative to the box top) where a relation attaches for
    /// `column`: the middle of its row, or the middle of the header when the
    /// column is not listed.
    pub fn anchor_y(&self, table: &ErTable, column: Option<&str>) -> f32 {
        if let Some(col) = column
            && let Some(i) = self.rows.iter().position(|&r| table.columns.get(r).map(|c| c.name == col).unwrap_or(false)) {
                return self.row_top(i) + ROW_HEIGHT * 0.5;
            }
        self.header_height * 0.5
    }

    /// Index into `ErTable::columns` of the row at `y` (relative), if any.
    pub fn row_at(&self, y: f32) -> Option<usize> {
        if y < self.body_top {
            return None;
        }
        let i = ((y - self.body_top) / ROW_HEIGHT).floor() as usize;
        self.rows.get(i).copied()
    }
}

/// Indices of the columns listed for `display`.
pub(crate) fn listed_columns(table: &ErTable, display: ColumnDisplay) -> Vec<usize> {
    match display {
        ColumnDisplay::All => (0..table.columns.len()).collect(),
        ColumnDisplay::KeysOnly => table.columns.iter().enumerate().filter(|(_, c)| c.is_key()).map(|(i, _)| i).collect(),
        ColumnDisplay::None => Vec::new(),
    }
}

/// Computes the geometry of `table` for `settings`.
pub fn table_geometry(table: &ErTable, settings: &ErSettings) -> TableGeometry {
    let rows = listed_columns(table, settings.columns);
    let comment = if settings.show_comments {
        table.comment.as_deref().map(|c| truncate_chars(c.lines().next().unwrap_or(""), COMMENT_MAX_CHARS).into_owned())
    } else {
        None
    };

    let mut header_w = PAD_X + ICON_WIDTH + text_width(&table.name, HEADER_FONT, true) * UI_SAFETY + PAD_X;
    if let Some(schema) = table.schema.as_deref().filter(|s| !s.is_empty()) {
        header_w += 10.0 + text_width(schema, SCHEMA_FONT, false) * UI_SAFETY;
    }
    let comment_w = comment.as_deref().map(|c| 2.0 * PAD_X + text_width(c, COMMENT_FONT, false) * UI_SAFETY).unwrap_or(0.0);

    let mut name_w: f32 = 0.0;
    let mut type_w: f32 = 0.0;
    for &i in &rows {
        if let Some(c) = table.columns.get(i) {
            name_w = name_w.max(text_width(&c.name, ROW_FONT, c.pk) * UI_SAFETY);
            if settings.show_types && !c.data_type.is_empty() {
                type_w = type_w.max(text_width(&c.data_type, ROW_FONT, false) * UI_SAFETY);
            }
        }
    }
    let name_x = PAD_X + ICON_WIDTH;
    let rows_w = if rows.is_empty() {
        0.0
    } else {
        name_x + name_w + if type_w > 0.0 { TYPE_GAP + type_w } else { 0.0 } + PAD_X
    };
    let width = header_w.max(rows_w).max(comment_w.min(MAX_WIDTH)).clamp(MIN_WIDTH, MAX_WIDTH).round();
    // Types are right-aligned at the right padding; names get what is left.
    let type_x = if type_w > 0.0 { (width - PAD_X - type_w).max(name_x + 20.0) } else { width - PAD_X };

    let body_top = HEADER_HEIGHT + if comment.is_some() { COMMENT_HEIGHT } else { 0.0 };
    let height = body_top + rows.len() as f32 * ROW_HEIGHT + if rows.is_empty() { 0.0 } else { BOTTOM_PAD };
    TableGeometry { size: vec2(width, height.round()), header_height: HEADER_HEIGHT, body_top, rows, name_x, type_x, comment }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::er::model::{ErColumn, ErTableKind};

    fn table() -> ErTable {
        ErTable::new(Some("public"), "customer_orders", ErTableKind::Table)
            .with_column(ErColumn::new("id", "bigint").primary_key())
            .with_column(ErColumn::new("customer_id", "bigint").foreign_key())
            .with_column(ErColumn::new("note", "character varying(4000)"))
    }

    #[test]
    fn sizes_follow_settings() {
        let t = table();
        let all = table_geometry(&t, &ErSettings::default());
        assert_eq!(all.rows, vec![0, 1, 2]);
        assert_eq!(all.size.y, HEADER_HEIGHT + 3.0 * ROW_HEIGHT + BOTTOM_PAD);
        let keys = table_geometry(&t, &ErSettings { columns: ColumnDisplay::KeysOnly, ..Default::default() });
        assert_eq!(keys.rows, vec![0, 1]);
        let none = table_geometry(&t, &ErSettings { columns: ColumnDisplay::None, ..Default::default() });
        assert_eq!(none.size.y, HEADER_HEIGHT);
        let no_types = table_geometry(&t, &ErSettings { show_types: false, ..Default::default() });
        assert!(no_types.size.x < all.size.x);
        assert!(all.size.x >= MIN_WIDTH && all.size.x <= MAX_WIDTH);
    }

    #[test]
    fn anchors_and_rows() {
        let t = table();
        let g = table_geometry(&t, &ErSettings::default());
        assert_eq!(g.anchor_y(&t, Some("customer_id")), HEADER_HEIGHT + ROW_HEIGHT * 1.5);
        assert_eq!(g.anchor_y(&t, Some("missing")), HEADER_HEIGHT / 2.0);
        assert_eq!(g.row_at(HEADER_HEIGHT + 1.0), Some(0));
        assert_eq!(g.row_at(5.0), None);
        assert_eq!(g.row_at(HEADER_HEIGHT + 2.5 * ROW_HEIGHT), Some(2));
        assert_eq!(g.row_at(HEADER_HEIGHT + 3.5 * ROW_HEIGHT), None);
    }

    #[test]
    fn comment_line_adds_height() {
        let mut t = table();
        t.comment = Some("Orders placed by customers\nsecond line".into());
        let g = table_geometry(&t, &ErSettings { show_comments: true, ..Default::default() });
        assert_eq!(g.comment.as_deref(), Some("Orders placed by customers"));
        assert_eq!(g.body_top, HEADER_HEIGHT + COMMENT_HEIGHT);
    }
}
