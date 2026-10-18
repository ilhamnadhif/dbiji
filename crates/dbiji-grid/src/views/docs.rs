//! JSON and XML document views of the result or of the selected rows.

use crate::copy::{CopyColumn, CopyData, CopyFormat, CopyOptions, CopyRow, render};
use crate::grid::{DataGridState, GridOptions};
use crate::model::ResultModel;
use dbiji_core::{tr, trf};
use egui::Ui;
use std::sync::Arc;

/// Rows rendered at most by the document views.
pub const MAX_DOCUMENT_ROWS: usize = 10_000;

/// Which document to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocumentKind {
    /// JSON array of objects.
    Json,
    /// XML document.
    Xml,
}

/// Cache key: model generation, kind, selected-only flag, columns, rows.
type DocumentKey = (u64, DocumentKind, bool, Vec<usize>, Vec<usize>);

/// Cached rendering of a document view.
#[derive(Debug, Clone, Default)]
pub struct DocumentViewState {
    /// Render only the selected rows.
    pub selected_only: bool,
    key: Option<DocumentKey>,
    lines: Arc<Vec<String>>,
    text: Arc<String>,
    max_chars: usize,
    truncated: bool,
}

/// Read-only JSON / XML view with copy.
pub struct DocumentView<'a> {
    model: &'a ResultModel,
    grid: &'a DataGridState,
    options: &'a GridOptions,
    state: &'a mut DocumentViewState,
    kind: DocumentKind,
}

impl<'a> DocumentView<'a> {
    /// New document view.
    pub fn new(model: &'a ResultModel, grid: &'a DataGridState, options: &'a GridOptions, state: &'a mut DocumentViewState, kind: DocumentKind) -> Self {
        DocumentView { model, grid, options, state, kind }
    }

    /// Render the document text (used by `show`, exposed for tests and exports).
    pub fn render_text(model: &ResultModel, columns: &[usize], rows: &[usize], kind: DocumentKind, options: &GridOptions) -> String {
        let cols: Vec<CopyColumn> = columns
            .iter()
            .map(|&c| CopyColumn {
                name: model.column_name(c).to_string(),
                kind: model.column_kind(c),
                type_name: model.column_meta(c).map(|m| m.type_name.clone()).unwrap_or_default(),
                format: None,
            })
            .collect();
        let values = model.values(rows, columns);
        let data = CopyData { columns: cols, rows: rows.iter().zip(values).map(|(&r, v)| CopyRow { number: r + 1, cells: v.into_iter().map(Some).collect() }).collect() };
        let format = match kind {
            DocumentKind::Json => CopyFormat::Json,
            DocumentKind::Xml => CopyFormat::Xml,
        };
        let opts = CopyOptions { format, include_header: true, ..Default::default() };
        render(&data, &opts, &options.format, options.dialect.as_deref())
    }

    /// Show the view.
    pub fn show(self, ui: &mut Ui) {
        let DocumentView { model, grid, options, state, kind } = self;
        let columns = grid.visible_columns();
        let rows: Vec<usize> =
            if state.selected_only { grid.selected_rows(model.row_count(), MAX_DOCUMENT_ROWS + 1) } else { (0..model.row_count().min(MAX_DOCUMENT_ROWS + 1)).collect() };
        let truncated = rows.len() > MAX_DOCUMENT_ROWS;
        let rows: Vec<usize> = rows.into_iter().take(MAX_DOCUMENT_ROWS).collect();
        let key = (model.generation(), kind, state.selected_only, columns.clone(), if state.selected_only { rows.clone() } else { vec![rows.len()] });
        if state.key.as_ref() != Some(&key) {
            let text = Self::render_text(model, &columns, &rows, kind, options);
            let lines: Vec<String> = text.lines().map(str::to_string).collect();
            state.max_chars = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
            state.lines = Arc::new(lines);
            state.text = Arc::new(text);
            state.truncated = truncated;
            state.key = Some(key);
        }
        ui.horizontal(|ui| {
            if ui.button(format!("{}  {}", egui_phosphor::regular::COPY, tr("Copy"))).clicked() {
                ui.ctx().copy_text(state.text.as_str().to_string());
            }
            ui.checkbox(&mut state.selected_only, tr("Selected rows only"));
            if state.truncated {
                ui.weak(trf("Showing the first {} rows", &[&crate::format::group_thousands(&MAX_DOCUMENT_ROWS.to_string())]));
            }
        });
        let salt = match kind {
            DocumentKind::Json => "dbiji_json_view",
            DocumentKind::Xml => "dbiji_xml_view",
        };
        super::show_lines(ui, salt, &state.lines, state.max_chars);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::{ColumnMeta, DataKind, Value};

    #[test]
    fn json_and_xml_documents() {
        let m = ResultModel::from_rows(
            vec![ColumnMeta::new("id", "int", DataKind::Int), ColumnMeta::new("doc", "jsonb", DataKind::Json)],
            vec![vec![Value::Int(1), Value::json(r#"{"a":1}"#)], vec![Value::Int(2), Value::Null]],
        );
        let o = GridOptions::default();
        let json = DocumentView::render_text(&m, &[0, 1], &[0, 1], DocumentKind::Json, &o);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v[0]["doc"]["a"], 1, "JSON values are embedded, not quoted");
        assert_eq!(v[1]["doc"], serde_json::Value::Null);
        let xml = DocumentView::render_text(&m, &[0], &[1], DocumentKind::Xml, &o);
        assert!(xml.contains("<id>2</id>"), "{xml}");
    }
}
