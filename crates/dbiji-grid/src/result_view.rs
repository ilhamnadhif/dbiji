//! Complete result tab: filter bar, view (grid / text / record / JSON / XML),
//! toolbar with status, and side panels. Use this for the common case, or
//! compose the individual widgets yourself.

use crate::grid::{
    DataGrid, DataGridState, DataMode, GridAction, GridOptions, GridResponse, PanelKind, ResultViewKind,
};
use crate::model::ResultModel;
use crate::panels::{
    CalcPanel, CalcPanelState, GroupingAction, GroupingPanel, GroupingPanelState, MetadataPanel, ReferencesPanel,
    ReferencesPanelState, ValuePanel, ValuePanelState,
};
use crate::toolbar::{FilterBar, ResultToolbar, ToolbarState};
use crate::views::{
    DocumentKind, DocumentView, DocumentViewState, RecordView, RecordViewState, TextView, TextViewState,
};
use dbiji_core::tr;
use egui::{Frame, Id, Panel, Ui};
use serde::{Deserialize, Serialize};

/// All state of a result tab. The grid layout and toolbar settings are
/// serialisable; panel caches are transient.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ResultViewState {
    /// Grid layout, selection, sort.
    pub grid: DataGridState,
    /// Filter bar / toolbar settings.
    pub toolbar: ToolbarState,
    /// Panel tab shown in the side area.
    pub active_panel: Option<PanelKind>,
    /// Width of the side panel area.
    pub side_width: Option<f32>,
    /// Record view state.
    #[serde(skip)]
    pub record: RecordViewState,
    /// Text view cache.
    #[serde(skip)]
    pub text: TextViewState,
    /// JSON / XML view cache.
    #[serde(skip)]
    pub documents: DocumentViewState,
    /// Value panel state (editor text, image preview cache).
    #[serde(skip)]
    pub value: ValuePanelState,
    /// Calc panel cache.
    #[serde(skip)]
    pub calc: CalcPanelState,
    /// Grouping panel state.
    #[serde(skip)]
    pub grouping: GroupingPanelState,
    /// References panel state; feed it with
    /// [`ReferencesPanelState::set_relations`] and
    /// [`ReferencesPanelState::set_related_rows`].
    #[serde(skip)]
    pub references: ReferencesPanelState,
    #[serde(skip)]
    last_client_filter: Option<String>,
}

impl ResultViewState {
    /// State for a model.
    pub fn new(model: &ResultModel) -> Self {
        ResultViewState { grid: DataGridState::for_model(model), ..Default::default() }
    }

    /// Open a side panel and make it the active tab.
    pub fn open_panel(&mut self, p: PanelKind) {
        self.toolbar.set_panel_open(p, true);
        self.active_panel = Some(p);
    }
}

/// Response of [`ResultViewer::show`].
#[derive(Debug)]
pub struct ResultViewResponse {
    /// Actions for the application (see [`GridAction`]).
    pub actions: Vec<GridAction>,
    /// The grid response when the grid view was shown.
    pub grid: Option<GridResponse>,
}

/// A complete result tab.
pub struct ResultViewer<'a> {
    model: &'a mut ResultModel,
    state: &'a mut ResultViewState,
    options: &'a GridOptions,
    id_salt: Id,
}

fn panel_label(p: PanelKind) -> String {
    use egui_phosphor::regular as icon;
    match p {
        PanelKind::Value => format!("{} {}", icon::EYE, tr("Value")),
        PanelKind::Metadata => format!("{} {}", icon::INFO, tr("Metadata")),
        PanelKind::Grouping => format!("{} {}", icon::STACK, tr("Grouping")),
        PanelKind::Calc => format!("{} {}", icon::CALCULATOR, tr("Calc")),
        PanelKind::References => format!("{} {}", icon::TREE_STRUCTURE, tr("References")),
    }
}

impl<'a> ResultViewer<'a> {
    /// New result viewer.
    pub fn new(model: &'a mut ResultModel, state: &'a mut ResultViewState, options: &'a GridOptions) -> Self {
        ResultViewer { model, state, options, id_salt: Id::new("dbiji_result_viewer") }
    }

    /// Distinguish several viewers in one `Ui`.
    pub fn id_salt(mut self, salt: impl egui::AsId) -> Self {
        self.id_salt = Id::new(salt);
        self
    }

    /// Show everything in the available space.
    pub fn show(self, ui: &mut Ui) -> ResultViewResponse {
        let ResultViewer { model, state, options, id_salt } = self;
        let id = ui.make_persistent_id(id_salt);
        let mut actions: Vec<GridAction> = Vec::new();
        let mut grid_response = None;
        let bar_frame = Frame::new().inner_margin(egui::Margin::symmetric(4, 3)).fill(ui.visuals().panel_fill);
        Panel::top(id.with("filter_bar")).frame(bar_frame).show(ui, |ui| {
            actions.extend(FilterBar::new(model, &mut state.grid, &mut state.toolbar).show(ui));
        });
        Panel::bottom(id.with("toolbar")).frame(bar_frame).show(ui, |ui| {
            actions.extend(ResultToolbar::new(model, &mut state.grid, &mut state.toolbar).show(ui));
        });
        if !state.toolbar.open_panels.is_empty() {
            if state.active_panel.is_none_or(|p| !state.toolbar.is_panel_open(p)) {
                state.active_panel = state.toolbar.open_panels.last().copied();
            }
            Panel::right(id.with("side"))
                .resizable(true)
                .default_size(state.side_width.unwrap_or(340.0))
                .size_range(220.0..=900.0)
                .frame(Frame::new().inner_margin(6).fill(ui.visuals().panel_fill))
                .show(ui, |ui| {
                    state.side_width = Some(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        for p in state.toolbar.open_panels.clone() {
                            ui.selectable_value(&mut state.active_panel, Some(p), panel_label(p));
                        }
                    });
                    ui.separator();
                    match state.active_panel {
                        Some(PanelKind::Value) => {
                            actions.extend(ValuePanel::new(model, &state.grid, options, &mut state.value).show(ui));
                        }
                        Some(PanelKind::Metadata) => MetadataPanel::new(model, &mut state.grid).show(ui),
                        Some(PanelKind::Calc) => CalcPanel::new(model, &state.grid, &mut state.calc).show(ui),
                        Some(PanelKind::Grouping) => {
                            if let Some(GroupingAction::Filtered(text)) =
                                GroupingPanel::new(model, &mut state.grouping).show(ui)
                            {
                                state.grid.mode = DataMode::Client;
                                state.toolbar.filter_text = text;
                            }
                        }
                        Some(PanelKind::References) => {
                            actions.extend(ReferencesPanel::new(model, &state.grid, &mut state.references).show(ui));
                        }
                        None => {}
                    }
                });
        }
        egui::CentralPanel::no_frame().show(ui, |ui| match state.toolbar.view {
            ResultViewKind::Grid => {
                let r = DataGrid::new(model, &mut state.grid, options).id_salt(id.with("grid")).show(ui);
                actions.extend(r.actions.iter().cloned());
                grid_response = Some(r);
            }
            ResultViewKind::Text => TextView::new(model, &state.grid, options, &mut state.text).show(ui),
            ResultViewKind::Record => {
                actions.extend(RecordView::new(model, &mut state.grid, options, &mut state.record).show(ui))
            }
            ResultViewKind::Json => {
                DocumentView::new(model, &state.grid, options, &mut state.documents, DocumentKind::Json).show(ui)
            }
            ResultViewKind::Xml => {
                DocumentView::new(model, &state.grid, options, &mut state.documents, DocumentKind::Xml).show(ui)
            }
        });
        // Internal handling of some actions (they are still reported).
        for a in &actions {
            match a {
                GridAction::OpenValuePanel { row, col } => {
                    state.grid.select_cell(*row, *col);
                    state.open_panel(PanelKind::Value);
                }
                GridAction::ServerQueryChanged { where_clause, order_by } => {
                    if let Some(w) = where_clause
                        && !w.is_empty()
                        && !state.toolbar.filter_text.contains(w.as_str())
                    {
                        state.toolbar.filter_text = if state.toolbar.filter_text.trim().is_empty() {
                            w.clone()
                        } else {
                            format!("({}) AND ({w})", state.toolbar.filter_text.trim())
                        };
                    }
                    if let Some(o) = order_by {
                        state.toolbar.order_text = o.clone();
                    }
                }
                GridAction::TogglePanel(p) if state.toolbar.is_panel_open(*p) => {
                    state.active_panel = Some(*p);
                }
                _ => {}
            }
        }
        // Show client filters applied from menus in the filter bar.
        let current = model.client_view_spec().filter.map(|f| f.text().to_string());
        if current != state.last_client_filter {
            if let Some(t) = &current
                && state.grid.mode == DataMode::Client
            {
                state.toolbar.filter_text = t.clone();
            }
            state.last_client_filter = current;
        }
        ResultViewResponse { actions, grid: grid_response }
    }
}
