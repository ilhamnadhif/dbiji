//! Fixed-width plain text table of the fetched rows.

use crate::grid::{DataGridState, GridOptions};
use crate::model::ResultModel;
use dbiji_core::{Value, tr, trf};
use egui::Ui;
use std::sync::Arc;

/// Maximum number of rows rendered by the text view.
pub const MAX_TEXT_ROWS: usize = 10_000;
/// Maximum characters of one cell in the text view.
const MAX_CELL_CHARS: usize = 60;

/// Cached rendering of the text view.
#[derive(Debug, Clone, Default)]
pub struct TextViewState {
    key: Option<(u64, Vec<usize>, usize)>,
    lines: Arc<Vec<String>>,
    max_chars: usize,
    truncated: bool,
}

fn cell_text(v: &Value, null_text: &str) -> String {
    let t = if v.is_null() { null_text.to_string() } else { v.to_text().into_owned() };
    let t = t.replace(['\n', '\r'], "↵").replace('\t', " ");
    if t.chars().count() > MAX_CELL_CHARS {
        let mut s: String = t.chars().take(MAX_CELL_CHARS - 1).collect();
        s.push('…');
        s
    } else {
        t
    }
}

/// Render rows `0..limit` of `columns` as an aligned text table.
/// Returns the lines and the longest line length (characters).
pub fn render_text_table(model: &ResultModel, columns: &[usize], limit: usize, null_text: &str) -> (Vec<String>, usize) {
    let n = model.row_count().min(limit);
    let headers: Vec<String> = columns.iter().map(|&c| cell_text(&Value::text(model.column_name(c)), "")).collect();
    let mut cells: Vec<Vec<String>> = Vec::with_capacity(n);
    let mut numeric = vec![true; columns.len()];
    let mut start = 0;
    while start < n {
        let end = (start + 2048).min(n);
        model.visit_rows(start..end, |_, row| {
            let mut line = Vec::with_capacity(columns.len());
            for (i, &c) in columns.iter().enumerate() {
                let v = row.value_of(c).unwrap_or_default();
                if !v.is_null() && !matches!(v, Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_)) {
                    numeric[i] = false;
                }
                line.push(cell_text(&v, null_text));
            }
            cells.push(line);
        });
        start = end;
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for line in &cells {
        for (i, c) in line.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let pad = |s: &str, w: usize, right: bool| {
        let n = s.chars().count();
        let fill = " ".repeat(w.saturating_sub(n));
        if right { format!("{fill}{s}") } else { format!("{s}{fill}") }
    };
    let mut lines = Vec::with_capacity(cells.len() + 2);
    lines.push(headers.iter().enumerate().map(|(i, h)| pad(h, widths[i], false)).collect::<Vec<_>>().join(" | "));
    lines.push(widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("-+-"));
    for line in &cells {
        lines.push(line.iter().enumerate().map(|(i, c)| pad(c, widths[i], numeric[i])).collect::<Vec<_>>().join(" | "));
    }
    let max = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    (lines, max)
}

/// Plain text view of the result (fixed-width table, first [`MAX_TEXT_ROWS`] rows).
pub struct TextView<'a> {
    model: &'a ResultModel,
    grid: &'a DataGridState,
    options: &'a GridOptions,
    state: &'a mut TextViewState,
}

impl<'a> TextView<'a> {
    /// New text view.
    pub fn new(model: &'a ResultModel, grid: &'a DataGridState, options: &'a GridOptions, state: &'a mut TextViewState) -> Self {
        TextView { model, grid, options, state }
    }

    /// Show the view.
    pub fn show(self, ui: &mut Ui) {
        let TextView { model, grid, options, state } = self;
        let columns = grid.visible_columns();
        let key = (model.generation(), columns.clone(), model.row_count());
        if state.key.as_ref() != Some(&key) {
            let (lines, max) = render_text_table(model, &columns, MAX_TEXT_ROWS, &options.format.null_text);
            state.lines = Arc::new(lines);
            state.max_chars = max;
            state.truncated = model.row_count() > MAX_TEXT_ROWS;
            state.key = Some(key);
        }
        ui.horizontal(|ui| {
            if ui.button(format!("{}  {}", egui_phosphor::regular::COPY, tr("Copy text"))).clicked() {
                ui.ctx().copy_text(state.lines.join("\n"));
            }
            if state.truncated {
                ui.weak(trf(
                    "Showing the first {} of {} rows",
                    &[&crate::format::group_thousands(&MAX_TEXT_ROWS.to_string()), &crate::format::group_thousands(&model.row_count().to_string())],
                ));
            }
        });
        super::show_lines(ui, "dbiji_text_view", &state.lines, state.max_chars);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::{ColumnMeta, DataKind};

    #[test]
    fn aligned_table() {
        let m = ResultModel::from_rows(
            vec![ColumnMeta::new("id", "int", DataKind::Int), ColumnMeta::new("name", "text", DataKind::Text)],
            vec![vec![Value::Int(7), Value::text("a\nb")], vec![Value::Int(1234), Value::Null]],
        );
        let (lines, max) = render_text_table(&m, &[0, 1], 10, "NULL");
        assert_eq!(lines, vec!["id   | name", "-----+-----", "   7 | a↵b ", "1234 | NULL"]);
        assert_eq!(max, 11);
        let (lines, _) = render_text_table(&m, &[1], 1, "");
        assert_eq!(lines.len(), 3);
    }
}
