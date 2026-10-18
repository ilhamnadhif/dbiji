//! References panel: foreign key relations from / to the current table and
//! the related rows of the active row (FR-D5). The panel only renders; the
//! application loads related rows on [`GridAction::RequestReferences`] and
//! hands them over with [`ReferencesPanelState::set_related_rows`].

use crate::grid::{DataGrid, DataGridState, GridAction, GridOptions};
use crate::model::{ForeignKeyLink, ResultModel};
use dbiji_core::{ColumnMeta, ObjectRef, Row, Value, tr, trf};
use egui::{RichText, Ui};

/// Direction of a relation seen from the current result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelationDirection {
    /// This table references `table` (a foreign key of ours).
    References,
    /// `table` references this table (their foreign key).
    ReferencedBy,
}

/// A relation between the current result and another table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    /// Constraint name.
    pub name: Option<String>,
    /// Direction.
    pub direction: RelationDirection,
    /// The other table.
    pub table: ObjectRef,
    /// Columns in the other table.
    pub columns: Vec<String>,
    /// Matching result columns of the current result (same order).
    pub key_columns: Vec<usize>,
}

struct Related {
    key: Vec<Value>,
    model: ResultModel,
    grid: DataGridState,
}

/// State of the references panel.
#[derive(Default)]
pub struct ReferencesPanelState {
    relations: Vec<Relation>,
    selected: Option<usize>,
    requested: Option<(usize, Vec<Value>)>,
    related: Option<(usize, Related)>,
    options: GridOptions,
}

impl std::fmt::Debug for ReferencesPanelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReferencesPanelState").field("relations", &self.relations).field("selected", &self.selected).finish_non_exhaustive()
    }
}

impl ReferencesPanelState {
    /// Relations of the current table (supplied by the application from metadata).
    pub fn set_relations(&mut self, relations: Vec<Relation>) {
        if self.relations != relations {
            self.relations = relations;
            self.selected = (!self.relations.is_empty()).then_some(0);
            self.related = None;
            self.requested = None;
        }
    }

    /// Relations shown.
    pub fn relations(&self) -> &[Relation] {
        &self.relations
    }

    /// Supply the rows related to `key` through relation `relation`.
    pub fn set_related_rows(&mut self, relation: usize, key: Vec<Value>, columns: Vec<ColumnMeta>, rows: Vec<Row>) {
        let model = ResultModel::from_rows(columns, rows);
        let grid = DataGridState::for_model(&model);
        self.related = Some((relation, Related { key, model, grid }));
    }

    /// Build relations from the result's foreign keys (outgoing only).
    pub fn relations_from_foreign_keys(fks: &[ForeignKeyLink]) -> Vec<Relation> {
        fks.iter()
            .map(|fk| Relation {
                name: fk.name.clone(),
                direction: RelationDirection::References,
                table: fk.ref_table.clone(),
                columns: fk.ref_columns.clone(),
                key_columns: fk.columns.clone(),
            })
            .collect()
    }
}

/// The references panel.
pub struct ReferencesPanel<'a> {
    model: &'a ResultModel,
    grid: &'a DataGridState,
    state: &'a mut ReferencesPanelState,
}

impl<'a> ReferencesPanel<'a> {
    /// New panel.
    pub fn new(model: &'a ResultModel, grid: &'a DataGridState, state: &'a mut ReferencesPanelState) -> Self {
        ReferencesPanel { model, grid, state }
    }

    /// Show the panel. Emits `RequestReferences` when related rows are needed
    /// and `NavigateForeignKey` to open the related table.
    pub fn show(self, ui: &mut Ui) -> Vec<GridAction> {
        let ReferencesPanel { model, grid, state } = self;
        let mut actions = Vec::new();
        if state.relations.is_empty() {
            ui.weak(tr("No foreign key relations are known for this result"));
            return actions;
        }
        ui.horizontal_wrapped(|ui| {
            for (i, r) in state.relations.iter().enumerate() {
                let arrow = match r.direction {
                    RelationDirection::References => "→",
                    RelationDirection::ReferencedBy => "←",
                };
                let label = format!("{arrow} {}", r.table.display_path());
                if ui.selectable_label(state.selected == Some(i), label).on_hover_text(r.name.clone().unwrap_or_default()).clicked() {
                    state.selected = Some(i);
                }
            }
        });
        ui.separator();
        let Some(sel) = state.selected.filter(|s| *s < state.relations.len()) else { return actions };
        let Some((row, _)) = grid.active_cell() else {
            ui.weak(tr("Select a row to see related rows"));
            return actions;
        };
        let rel = state.relations[sel].clone();
        let key: Vec<Value> = rel.key_columns.iter().map(|&c| model.cell(row, c).unwrap_or_default()).collect();
        ui.horizontal(|ui| {
            let desc =
                rel.columns.iter().zip(&key).map(|(c, v)| format!("{c} = {}", if v.is_null() { "NULL".into() } else { v.to_text().into_owned() })).collect::<Vec<_>>().join(", ");
            ui.label(RichText::new(trf("{} where {}", &[&rel.table.display_path(), &desc])).strong());
            if ui.button(format!("{} {}", egui_phosphor::regular::ARROW_SQUARE_OUT, tr("Open"))).clicked() {
                let link = ForeignKeyLink { name: rel.name.clone(), columns: rel.key_columns.clone(), ref_table: rel.table.clone(), ref_columns: rel.columns.clone() };
                actions.push(GridAction::NavigateForeignKey { link, values: key.clone() });
            }
        });
        if key.iter().all(Value::is_null) {
            ui.weak(tr("The key is NULL; there are no related rows"));
            return actions;
        }
        let have = state.related.as_ref().is_some_and(|(r, d)| *r == sel && d.key == key);
        if !have && state.requested.as_ref() != Some(&(sel, key.clone())) {
            state.requested = Some((sel, key.clone()));
            actions.push(GridAction::RequestReferences { relation: sel, values: key.clone() });
        }
        match state.related.as_mut() {
            Some((r, d)) if *r == sel && d.key == key => {
                let options = state.options.clone();
                let resp = DataGrid::new(&mut d.model, &mut d.grid, &options).id_salt(("dbiji_references_grid", sel)).show(ui);
                for a in resp.actions {
                    if let GridAction::NavigateForeignKey { .. } = a {
                        actions.push(a);
                    }
                }
            }
            _ => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak(tr("Loading related rows…"));
                });
            }
        }
        actions
    }
}
