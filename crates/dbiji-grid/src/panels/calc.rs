//! Calc panel: statistics of the selected cells (FR-D5).

use crate::calc::{CalcStats, compute_stats, format_number};
use crate::grid::{DataGridState, Selection};
use crate::model::ResultModel;
use dbiji_core::{Value, tr, trf};
use egui::Ui;

/// Cells analysed at most.
pub const MAX_CALC_CELLS: usize = 500_000;

/// Cached statistics of the selection.
#[derive(Debug, Clone, Default)]
pub struct CalcPanelState {
    key: Option<(u64, Selection, Vec<usize>)>,
    stats: CalcStats,
}

/// Statistics of selected cells.
pub struct CalcPanel<'a> {
    model: &'a ResultModel,
    grid: &'a DataGridState,
    state: &'a mut CalcPanelState,
}

/// Compute statistics of the current selection (at most `limit` cells).
pub fn selection_stats(model: &ResultModel, grid: &DataGridState, limit: usize) -> CalcStats {
    let visible = grid.visible_columns();
    let sel = grid.selection();
    let nrows = model.row_count();
    let col_pos = sel.columns(visible.len());
    let cols: Vec<usize> = col_pos.iter().map(|&p| visible[p]).collect();
    if cols.is_empty() {
        return CalcStats::default();
    }
    let row_limit = (limit / cols.len()).max(1) + 1;
    let rows = sel.rows(nrows, row_limit);
    let values = model.values(&rows, &cols);
    let mut flat: Vec<&Value> = Vec::with_capacity(values.len() * cols.len());
    let all = sel.is_all();
    for (r, vals) in rows.iter().zip(&values) {
        for (v, &p) in vals.iter().zip(&col_pos) {
            if all || sel.contains(*r, p) {
                flat.push(v);
            }
        }
    }
    let mut stats = compute_stats(flat, limit);
    if sel.cell_count(nrows, visible.len()) > stats.count {
        stats.truncated = true;
    }
    stats
}

impl<'a> CalcPanel<'a> {
    /// New panel.
    pub fn new(model: &'a ResultModel, grid: &'a DataGridState, state: &'a mut CalcPanelState) -> Self {
        CalcPanel { model, grid, state }
    }

    /// Show the panel.
    pub fn show(self, ui: &mut Ui) {
        let key = (self.model.generation(), self.grid.selection().clone(), self.grid.visible_columns());
        if self.state.key.as_ref() != Some(&key) {
            self.state.stats = selection_stats(self.model, self.grid, MAX_CALC_CELLS);
            self.state.key = Some(key);
        }
        let s = &self.state.stats;
        if s.count == 0 {
            ui.weak(tr("Select cells to calculate statistics"));
            return;
        }
        let n = |v: usize| crate::format::group_thousands(&v.to_string());
        let fmt_v = |v: &Option<Value>| v.as_ref().map(|v| v.to_text().into_owned()).unwrap_or_default();
        let mut rows: Vec<(&str, String)> = vec![(tr("Count"), n(s.count)), (tr("Distinct"), n(s.distinct)), (tr("NULLs"), n(s.nulls))];
        if s.numeric_count > 0 {
            rows.push((tr("Numbers"), n(s.numeric_count)));
            rows.push((tr("Sum"), s.sum_text().unwrap_or_default()));
            rows.push((tr("Average"), s.avg.map(format_number).unwrap_or_default()));
            rows.push((tr("Median"), s.median.map(format_number).unwrap_or_default()));
        }
        // The most frequent value, when one repeats.
        if let Some((value, times)) = s.mode.as_ref().filter(|(_, t)| *t > 1) {
            rows.push((tr("Mode"), trf("{} (×{})", &[value, &n(*times)])));
        }
        rows.push((tr("Min"), fmt_v(&s.min)));
        rows.push((tr("Max"), fmt_v(&s.max)));
        if let (Some(a), Some(b)) = (s.min_len, s.max_len) {
            rows.push((tr("Min length"), n(a)));
            rows.push((tr("Max length"), n(b)));
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            stats_table(ui, &rows);
            if s.truncated {
                ui.add_space(4.0);
                ui.weak(trf("Only the first {} cells were analysed", &[&crate::format::group_thousands(&MAX_CALC_CELLS.to_string())]));
            }
        });
    }
}

/// A two column table (function, value) drawn like the result grid; a click on a
/// row copies its value.
fn stats_table(ui: &mut Ui, rows: &[(&str, String)]) {
    use egui::{Align2, Sense, Stroke, TextStyle, pos2, vec2};
    let theme = crate::theme::GridTheme::from_visuals(ui.visuals());
    let row_h = ui.text_style_height(&TextStyle::Body) + 8.0;
    let width = ui.available_width();
    let name_w = (width * 0.42).clamp(90.0, 170.0);
    let font = TextStyle::Body.resolve(ui.style());
    let mono = TextStyle::Monospace.resolve(ui.style());
    let line = Stroke::new(1.0, theme.line);
    let (rect, _) = ui.allocate_exact_size(vec2(width, row_h * (rows.len() + 1) as f32), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme.bg);
    // Header.
    let header = egui::Rect::from_min_size(rect.min, vec2(width, row_h));
    painter.rect_filled(header, 0.0, theme.header_bg);
    painter.text(pos2(header.left() + 6.0, header.center().y), Align2::LEFT_CENTER, tr("Function"), font.clone(), theme.header_text);
    painter.text(pos2(header.left() + name_w + 6.0, header.center().y), Align2::LEFT_CENTER, tr("Value"), font.clone(), theme.header_text);
    for (i, (name, value)) in rows.iter().enumerate() {
        let r = egui::Rect::from_min_size(pos2(rect.left(), rect.top() + row_h * (i + 1) as f32), vec2(width, row_h));
        let resp = ui.interact(r, ui.id().with(("calc_row", i)), Sense::click());
        let fill = if resp.hovered() {
            theme.selection
        } else if i % 2 == 1 {
            theme.stripe
        } else {
            theme.bg
        };
        painter.rect_filled(r, 0.0, fill);
        painter.text(pos2(r.left() + 6.0, r.center().y), Align2::LEFT_CENTER, *name, font.clone(), theme.weak);
        let value_rect = egui::Rect::from_min_max(pos2(r.left() + name_w + 6.0, r.top()), pos2(r.right() - 4.0, r.bottom()));
        let galley = painter.layout(value.clone(), mono.clone(), theme.text, f32::INFINITY);
        painter.with_clip_rect(value_rect).galley(pos2(value_rect.left(), r.center().y - galley.size().y / 2.0), galley, theme.text);
        if resp.on_hover_text(format!("{value}\n{}", tr("Click to copy"))).clicked() {
            ui.ctx().copy_text(value.clone());
        }
    }
    // Grid lines: rows, the column border and the frame.
    for i in 1..=rows.len() {
        let y = rect.top() + row_h * i as f32;
        painter.hline(rect.x_range(), y, line);
    }
    painter.vline(rect.left() + name_w, rect.y_range(), line);
    painter.rect_stroke(rect, 0.0, line, egui::StrokeKind::Inside);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::CellPos;
    use dbiji_core::{ColumnMeta, DataKind};

    #[test]
    fn stats_of_selection() {
        let m = ResultModel::from_rows(
            vec![ColumnMeta::new("a", "int", DataKind::Int), ColumnMeta::new("b", "text", DataKind::Text)],
            (0..10).map(|i| vec![Value::Int(i), Value::text("x".repeat(i as usize))]).collect(),
        );
        let mut g = DataGridState::for_model(&m);
        g.selection_mut().select_cell(CellPos::new(0, 0));
        g.selection_mut().extend_to(CellPos::new(4, 0));
        let s = selection_stats(&m, &g, 1000);
        assert_eq!(s.count, 5);
        assert_eq!(s.int_sum, Some(10));
        g.selection_mut().select_columns(1, 1, false);
        let s = selection_stats(&m, &g, 1000);
        assert_eq!(s.count, 10);
        assert_eq!(s.max_len, Some(9));
        g.selection_mut().select_all();
        let s = selection_stats(&m, &g, 7);
        assert!(s.truncated);
    }
}
