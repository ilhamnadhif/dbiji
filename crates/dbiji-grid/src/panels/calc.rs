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
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(egui_phosphor::regular::CALCULATOR).size(28.0).color(ui.visuals().weak_text_color()));
                ui.weak(tr("Select cells to calculate statistics"));
            });
            return;
        }
        let n = |v: usize| crate::format::group_thousands(&v.to_string());
        let fmt_v = |v: &Option<Value>| v.as_ref().map(|v| v.to_text().chars().take(200).collect::<String>()).unwrap_or_default();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            super::style::stat_tiles(ui, &[(tr("Count"), n(s.count)), (tr("Distinct"), n(s.distinct)), (tr("NULLs"), n(s.nulls))]);
            if s.numeric_count > 0 {
                super::style::section_title(ui, &trf("Numbers ({})", &[&n(s.numeric_count)]));
                super::style::stat_tiles(
                    ui,
                    &[
                        (tr("Sum"), s.sum_text().unwrap_or_default()),
                        (tr("Average"), s.avg.map(format_number).unwrap_or_default()),
                        (tr("Median"), s.median.map(format_number).unwrap_or_default()),
                    ],
                );
            }
            super::style::section_title(ui, tr("Range"));
            super::style::card(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                super::style::kv_row(ui, tr("Min"), &fmt_v(&s.min));
                super::style::kv_row(ui, tr("Max"), &fmt_v(&s.max));
                if let (Some(a), Some(b)) = (s.min_len, s.max_len) {
                    super::style::kv_row(ui, tr("Min length"), &a.to_string());
                    super::style::kv_row(ui, tr("Max length"), &b.to_string());
                }
            });
            if s.truncated {
                ui.add_space(6.0);
                ui.weak(trf("Only the first {} cells were analysed", &[&crate::format::group_thousands(&MAX_CALC_CELLS.to_string())]));
            }
        });
    }
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
