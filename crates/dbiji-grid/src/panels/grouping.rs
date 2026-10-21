//! Grouping panel: group fetched rows by columns with counts and an optional
//! SUM / AVG of a measure; clicking a group filters the grid (FR-D5).

use crate::calc::format_number;
use crate::grouping::{GroupResult, GroupSpec, compute_groups, group_filter};
use crate::model::ResultModel;
use crate::task::{BackgroundTask, TaskPoll};
use dbiji_core::{tr, trf};
use egui::{ComboBox, RichText, ScrollArea, Ui};
use std::sync::Arc;

/// Maximum number of distinct groups kept.
pub const MAX_GROUPS: usize = 10_000;

/// State of the grouping panel.
#[derive(Debug, Default)]
pub struct GroupingPanelState {
    /// Columns to group by.
    pub spec: GroupSpec,
    result: Option<GroupResult>,
    result_key: Option<(u64, GroupSpec)>,
    task: Option<BackgroundTask<GroupResult>>,
    pending_key: Option<(u64, GroupSpec)>,
    /// Auto recompute when the data changes.
    pub live: bool,
}

impl GroupingPanelState {
    /// The last computed grouping.
    pub fn result(&self) -> Option<&GroupResult> {
        self.result.as_ref()
    }

    /// True while grouping runs in the background.
    pub fn is_running(&self) -> bool {
        self.task.is_some()
    }
}

/// What the panel asks the caller to do.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupingAction {
    /// A client filter was applied for the clicked group (its text).
    Filtered(String),
}

/// The grouping panel.
pub struct GroupingPanel<'a> {
    model: &'a mut ResultModel,
    state: &'a mut GroupingPanelState,
}

impl<'a> GroupingPanel<'a> {
    /// New panel.
    pub fn new(model: &'a mut ResultModel, state: &'a mut GroupingPanelState) -> Self {
        GroupingPanel { model, state }
    }

    fn start(model: &ResultModel, state: &mut GroupingPanelState, ctx: &egui::Context) {
        let store = model.store().clone();
        let view: Option<Arc<Vec<u32>>> = model.view().map(|v| Arc::new(v.to_vec()));
        let spec = state.spec.clone();
        let vctx = model.view_context();
        let repaint = ctx.clone();
        state.pending_key = Some((model.generation(), spec.clone()));
        state.task = Some(BackgroundTask::spawn(
            "dbiji-grid-group",
            move |cancel| compute_groups(&store, view.as_deref().map(|v| v.as_slice()), &spec, &vctx, MAX_GROUPS, cancel),
            Box::new(move || repaint.request_repaint()),
        ));
    }

    /// Show the panel.
    pub fn show(self, ui: &mut Ui) -> Option<GroupingAction> {
        let GroupingPanel { model, state } = self;
        let mut action = None;
        // Poll the background task.
        if let Some(task) = &state.task {
            match task.poll() {
                TaskPoll::Running => {}
                TaskPoll::Done(r) => {
                    state.result = Some(r);
                    state.result_key = state.pending_key.take();
                    state.task = None;
                }
                TaskPoll::Failed => {
                    state.task = None;
                }
            }
        }
        let names = model.column_names();
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            ui.label(tr("Group by"));
            let mut remove = None;
            for (i, c) in state.spec.columns.iter().enumerate() {
                if ui.small_button(format!("{} ✕", names.get(*c).map_or("?", String::as_str))).clicked() {
                    remove = Some(i);
                }
            }
            if let Some(i) = remove {
                state.spec.columns.remove(i);
                changed = true;
            }
            ComboBox::from_id_salt("dbiji_group_add").selected_text(tr("Add column…")).show_ui(ui, |ui| {
                for (c, n) in names.iter().enumerate() {
                    if !state.spec.columns.contains(&c) && ui.selectable_label(false, n).clicked() {
                        state.spec.columns.push(c);
                        changed = true;
                    }
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label(tr("Measure"));
            let current = state.spec.measure.and_then(|m| names.get(m)).cloned().unwrap_or_else(|| tr("(none)").to_string());
            ComboBox::from_id_salt("dbiji_group_measure").selected_text(current).show_ui(ui, |ui| {
                if ui.selectable_label(state.spec.measure.is_none(), tr("(none)")).clicked() {
                    state.spec.measure = None;
                    changed = true;
                }
                for (c, n) in names.iter().enumerate() {
                    let numeric = model.column_kind(c).is_numeric() || model.is_virtual_column(c);
                    if numeric && ui.selectable_label(state.spec.measure == Some(c), n).clicked() {
                        state.spec.measure = Some(c);
                        changed = true;
                    }
                }
            });
            ui.checkbox(&mut state.live, tr("Live"));
            if ui.button(tr("Refresh")).clicked() {
                changed = true;
            }
        });
        let key = (model.generation(), state.spec.clone());
        let stale = state.result_key.as_ref() != Some(&key) && state.pending_key.as_ref() != Some(&key);
        if !state.spec.columns.is_empty() && (changed || (stale && (state.live || state.result.is_none()))) {
            Self::start(model, state, ui.ctx());
        }
        ui.separator();
        if state.spec.columns.is_empty() {
            ui.weak(tr("Choose one or more columns to group the fetched rows"));
            return None;
        }
        if state.task.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(tr("Grouping…"));
            });
        }
        let Some(result) = &state.result else { return None };
        ui.weak(trf("{} groups in {} rows", &[&result.groups.len(), &crate::format::group_thousands(&result.rows.to_string())]));
        if result.truncated {
            ui.colored_label(ui.visuals().warn_fg_color, trf("More than {} groups; the rest were dropped", &[&MAX_GROUPS]));
        }
        let spec = state.spec.clone();
        let has_measure = spec.measure.is_some();
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        let mut clicked: Option<usize> = None;
        ScrollArea::both().auto_shrink([false, false]).show_rows(ui, row_h, result.groups.len(), |ui, range| {
            egui::Grid::new("dbiji_groups").striped(true).num_columns(spec.columns.len() + 3).show(ui, |ui| {
                for &c in &spec.columns {
                    ui.strong(names.get(c).map_or("?", String::as_str));
                }
                ui.strong(tr("Count"));
                if has_measure {
                    ui.strong(tr("Sum"));
                    ui.strong(tr("Avg"));
                }
                ui.end_row();
                for i in range {
                    let g = &result.groups[i];
                    for k in &g.keys {
                        let t = if k.is_null() { RichText::new("NULL").weak() } else { RichText::new(k.to_text().chars().take(60).collect::<String>()) };
                        if ui.selectable_label(false, t).on_hover_text(tr("Filter the grid to this group")).clicked() {
                            clicked = Some(i);
                        }
                    }
                    ui.label(RichText::new(crate::format::group_thousands(&g.count.to_string())).monospace());
                    if has_measure {
                        ui.label(RichText::new(g.sum.map(format_number).unwrap_or_default()).monospace());
                        ui.label(RichText::new(g.avg.map(format_number).unwrap_or_default()).monospace());
                    }
                    ui.end_row();
                }
            });
        });
        if let Some(i) = clicked
            && let Some(f) = group_filter(&spec, &result.groups[i], &names)
        {
            // The groups were counted in the filtered rows: narrow that filter, do not replace it.
            let f = match model.requested_view_spec().filter {
                Some(current) => current.and(f),
                None => f,
            };
            let text = f.text().to_string();
            crate::grid::apply_client_filter(model, Some(f), ui.ctx());
            action = Some(GroupingAction::Filtered(text));
        }
        action
    }
}
