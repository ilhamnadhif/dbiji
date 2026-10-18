//! Metadata panel: the result columns (FR-D5).

use crate::grid::DataGridState;
use crate::model::ResultModel;
use dbiji_core::{DataKind, tr};
use egui::{RichText, ScrollArea, Ui};

/// Rows shown by the metadata panel (also used by tests).
pub fn metadata_rows(model: &ResultModel) -> Vec<[String; 7]> {
    let key_cols: Vec<usize> = model.editability().as_ref().map(|t| t.key_columns.clone()).unwrap_or_default();
    (0..model.column_count())
        .map(|c| {
            let name = model.column_name(c).to_string();
            match model.column_meta(c) {
                Some(m) => {
                    let mut flags = Vec::new();
                    if key_cols.contains(&c) {
                        flags.push(tr("key"));
                    }
                    if m.auto_increment == Some(true) {
                        flags.push(tr("auto increment"));
                    }
                    if model.foreign_key_for_column(c).is_some() {
                        flags.push(tr("foreign key"));
                    }
                    let size = match (m.precision, m.scale, m.max_length) {
                        (Some(p), Some(s), _) => format!("{p},{s}"),
                        (Some(p), None, _) => p.to_string(),
                        (None, _, Some(l)) => l.to_string(),
                        _ => String::new(),
                    };
                    [
                        name,
                        m.type_name.clone(),
                        format!("{:?}", m.kind),
                        match m.nullable {
                            Some(true) => tr("yes").to_string(),
                            Some(false) => tr("no").to_string(),
                            None => "?".to_string(),
                        },
                        m.source.as_ref().map_or_else(String::new, |s| {
                            let mut p = String::new();
                            if let Some(sc) = &s.schema {
                                p.push_str(sc);
                                p.push('.');
                            }
                            p.push_str(&s.table);
                            p.push('.');
                            p.push_str(&s.column);
                            p
                        }),
                        size,
                        flags.join(", "),
                    ]
                }
                None => {
                    let expr = model.virtual_columns().get(c - model.base_column_count()).map(|v| v.expression.source().to_string()).unwrap_or_default();
                    [name, tr("virtual").to_string(), format!("{:?}", DataKind::Other), String::new(), expr, String::new(), tr("computed").to_string()]
                }
            }
        })
        .collect()
}

/// Column list: name, type, kind, nullable, source table.column, precision/scale.
pub struct MetadataPanel<'a> {
    model: &'a ResultModel,
    grid: &'a mut DataGridState,
}

impl<'a> MetadataPanel<'a> {
    /// New panel.
    pub fn new(model: &'a ResultModel, grid: &'a mut DataGridState) -> Self {
        MetadataPanel { model, grid }
    }

    /// Show the panel. Clicking a column selects it in the grid; the checkbox toggles visibility.
    pub fn show(self, ui: &mut Ui) {
        let rows = metadata_rows(self.model);
        match self.model.editability() {
            Ok(t) => {
                ui.label(RichText::new(format!("{} {}", egui_phosphor::regular::TABLE, t.table.display_path())).strong());
            }
            Err(r) => {
                ui.label(RichText::new(format!("{} {}", egui_phosphor::regular::LOCK_SIMPLE, r.short_label())).color(ui.visuals().warn_fg_color)).on_hover_text(r.message());
            }
        }
        ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("dbiji_metadata_grid").striped(true).num_columns(8).spacing([10.0, 3.0]).show(ui, |ui| {
                for h in ["", tr("Column"), tr("Type"), tr("Kind"), tr("Nullable"), tr("Source"), tr("Size"), tr("Flags")] {
                    ui.strong(h);
                }
                ui.end_row();
                for (c, r) in rows.iter().enumerate() {
                    let mut visible = self.grid.columns.get(c).is_some_and(|s| s.visible);
                    if ui.checkbox(&mut visible, "").on_hover_text(tr("Show column")).changed() {
                        self.grid.set_column_visible(c, visible);
                    }
                    let name = if self.model.is_virtual_column(c) { RichText::new(&r[0]).italics() } else { RichText::new(&r[0]) };
                    if ui.selectable_label(false, name).clicked() {
                        let row = self.grid.active_cell().map_or(0, |(row, _)| row);
                        self.grid.select_cell(row, c);
                    }
                    for v in &r[1..] {
                        ui.label(v);
                    }
                    ui.end_row();
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::{ColumnMeta, ColumnSource, Value};

    #[test]
    fn rows() {
        let mut c = ColumnMeta::new("price", "numeric(10,2)", DataKind::Decimal).with_source(ColumnSource {
            database: None,
            schema: Some("s".into()),
            table: "t".into(),
            column: "price".into(),
        });
        c.precision = Some(10);
        c.scale = Some(2);
        c.nullable = Some(false);
        let mut m = ResultModel::from_rows(vec![c], vec![vec![Value::decimal("1.00")]]);
        m.add_virtual_column("twice", "price * 2").unwrap();
        let r = metadata_rows(&m);
        assert_eq!(r[0][0], "price");
        assert_eq!(r[0][3], "no");
        assert_eq!(r[0][4], "s.t.price");
        assert_eq!(r[0][5], "10,2");
        assert_eq!(r[1][4], "price * 2");
    }
}
