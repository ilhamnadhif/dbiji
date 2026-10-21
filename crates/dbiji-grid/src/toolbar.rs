//! Result toolbar and filter bar (FR-D4, FR-D6).

use crate::grid::{self, DataGridState, DataMode, GridAction, PanelKind, ResultViewKind};
use crate::model::ResultModel;
use crate::view::{ClientFilter, SortKey};
use dbiji_core::{tr, trf};
use egui::{Button, Key, Popup, PopupCloseBehavior, RichText, Ui};
use egui_phosphor::regular as icon;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const MAX_HISTORY: usize = 30;

/// Auto refresh choices.
pub const AUTO_REFRESH_CHOICES: [Option<u64>; 5] = [None, Some(5), Some(10), Some(30), Some(60)];

/// State of the toolbar and filter bar (persistable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ToolbarState {
    /// Text of the WHERE / client filter input.
    pub filter_text: String,
    /// Text of the ORDER BY input.
    pub order_text: String,
    /// Recently applied filters, newest first.
    pub filter_history: Vec<String>,
    /// Recently applied orderings, newest first.
    pub order_history: Vec<String>,
    /// Auto refresh interval in seconds (`None` = off).
    pub auto_refresh_secs: Option<u64>,
    /// Value of the "custom" auto refresh input.
    pub custom_refresh_secs: u64,
    /// Current result view.
    pub view: ResultViewKind,
    /// Open side panels.
    pub open_panels: Vec<PanelKind>,
    /// The side panel area is hidden (its panels stay open for the next show).
    pub panels_hidden: bool,
    /// Message of the application shown next to the status: (text, error).
    /// Errors open a pop-up instead (see [`ToolbarState::error_popup`]).
    #[serde(skip)]
    pub notice: Option<(String, bool)>,
    /// The error shown in a pop-up over the result.
    #[serde(skip)]
    pub error_popup: Option<String>,
    /// The fetch error that already popped up (once each).
    #[serde(skip)]
    pub(crate) seen_fetch_error: Option<String>,
    /// The fetch error the user closed (its bar over the result stays hidden).
    #[serde(skip)]
    pub(crate) dismissed_fetch_error: Option<String>,
    #[serde(skip)]
    rt: ToolbarRuntime,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct ToolbarRuntime {
    /// Column suggestions of the WHERE and ORDER BY boxes.
    where_suggest: crate::suggest::Suggest,
    order_suggest: crate::suggest::Suggest,
    filter_error: Option<String>,
    /// Focus the filter box (cursor at the end) on the next frame.
    focus_filter: bool,
    next_refresh: Option<f64>,
    confirm_discard: bool,
}

impl ToolbarState {
    /// Focus the filter box on the next frame, the cursor at the end.
    pub fn focus_filter(&mut self) {
        self.rt.focus_filter = true;
    }

    /// Is a panel open?
    pub fn is_panel_open(&self, p: PanelKind) -> bool {
        self.open_panels.contains(&p)
    }

    /// Open or close a panel.
    pub fn set_panel_open(&mut self, p: PanelKind, open: bool) {
        self.open_panels.retain(|x| *x != p);
        if open {
            self.open_panels.push(p);
        }
    }

    fn push_history(list: &mut Vec<String>, text: &str) {
        let t = text.trim();
        if t.is_empty() {
            return;
        }
        list.retain(|h| h != t);
        list.insert(0, t.to_string());
        list.truncate(MAX_HISTORY);
    }

    /// Error of the last client filter, if any.
    pub fn filter_error(&self) -> Option<&str> {
        self.rt.filter_error.as_deref()
    }
}

/// Parse an ORDER BY list (`name DESC, [order date]`) against model column names.
pub fn parse_order_by(text: &str, columns: &[String]) -> Result<Vec<SortKey>, String> {
    let mut keys = Vec::new();
    for part in text.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        let lower = p.to_ascii_lowercase();
        let (name, desc) = if lower.ends_with(" desc") {
            (p[..p.len() - 5].trim(), true)
        } else if lower.ends_with(" asc") {
            (p[..p.len() - 4].trim(), false)
        } else {
            (p, false)
        };
        let unquoted = name.strip_prefix('[').and_then(|n| n.strip_suffix(']')).or_else(|| name.strip_prefix('"').and_then(|n| n.strip_suffix('"'))).unwrap_or(name);
        let col = columns
            .iter()
            .position(|c| c == unquoted)
            .or_else(|| columns.iter().position(|c| c.eq_ignore_ascii_case(unquoted)))
            .or_else(|| unquoted.parse::<usize>().ok().filter(|n| *n >= 1 && *n <= columns.len()).map(|n| n - 1))
            .ok_or_else(|| trf("Unknown column '{}' in ORDER BY", &[&unquoted]))?;
        keys.push(SortKey { column: col, descending: desc });
    }
    Ok(keys)
}

/// Human readable duration (`120 ms`, `1.25 s`, `2 min 3 s`).
pub fn duration_text(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.2} s", d.as_secs_f64())
    } else {
        format!("{} min {} s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Status line text: fetched rows, more available, total, elapsed.
pub fn status_text(model: &ResultModel) -> String {
    let rows = crate::format::group_thousands(&model.row_count().to_string());
    let mut s = trf("{} rows", &[&rows]);
    if model.has_client_view() && model.row_count() != model.stored_row_count() {
        s.push_str(&trf(" (of {} fetched)", &[&crate::format::group_thousands(&model.stored_row_count().to_string())]));
    }
    if model.fetch.fetching {
        s.push_str(" · ");
        s.push_str(tr("fetching…"));
    } else if model.fetch.has_more {
        s.push_str(" · ");
        s.push_str(tr("more available"));
    }
    if model.fetch.counting {
        s.push_str(" · ");
        s.push_str(tr("counting…"));
    } else if let Some(t) = model.fetch.total_count {
        s.push_str(&trf(" · total {}", &[&crate::format::group_thousands(&t.to_string())]));
    }
    if let Some(e) = model.fetch.elapsed {
        s.push_str(" · ");
        s.push_str(&duration_text(e));
    }
    s
}

/// The filter bar: WHERE / client filter with history, ORDER BY, apply/clear
/// and the client/server toggle.
pub struct FilterBar<'a> {
    model: &'a mut ResultModel,
    grid: &'a mut DataGridState,
    state: &'a mut ToolbarState,
    /// Quotes suggested column names that need it (else plain, or "…").
    dialect: Option<std::sync::Arc<dyn dbiji_core::Dialect>>,
}

impl<'a> FilterBar<'a> {
    /// New filter bar.
    pub fn new(model: &'a mut ResultModel, grid: &'a mut DataGridState, state: &'a mut ToolbarState) -> Self {
        FilterBar { model, grid, state, dialect: None }
    }

    /// The database's dialect, for quoting suggested column names.
    pub fn dialect(mut self, dialect: Option<std::sync::Arc<dyn dbiji_core::Dialect>>) -> Self {
        self.dialect = dialect;
        self
    }

    /// Show it. Returns actions for the application (server side filters).
    pub fn show(self, ui: &mut Ui) -> Vec<GridAction> {
        let FilterBar { model, grid, state, dialect } = self;
        // Column names for the suggestions, written as SQL (server) or as typed (client).
        let columns: Vec<String> = model.columns().iter().map(|c| c.name.clone()).collect();
        let simple = |n: &str| n.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_') && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        let server_mode = grid.mode == DataMode::Server;
        let quote = move |n: &str| -> String {
            match &dialect {
                Some(d) if server_mode => d.quote_ident(n),
                _ if simple(n) => n.to_string(),
                _ => format!("\"{}\"", n.replace('"', "\"\"")),
            }
        };
        let mut actions = Vec::new();
        let mut apply = false;
        let mut clear = false;
        ui.horizontal(|ui| {
            // Where filter and order apply: re-run on the server, or the fetched rows.
            let mut client = grid.mode == DataMode::Client;
            // Each button says what it does: the words alone mean little to a new user.
            let server_hint = tr("Server: the database filters and sorts. The query runs again with your WHERE / ORDER BY, over all rows of the table.");
            let client_hint = tr("Client: DBiji filters and sorts the rows already fetched, without asking the database. Fast, but only those rows.");
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
                let left = egui::CornerRadius { nw: 4, sw: 4, ne: 0, se: 0 };
                let right = egui::CornerRadius { nw: 0, sw: 0, ne: 4, se: 4 };
                if ui.add(Button::new(tr("Server")).selected(!client).corner_radius(left).stroke(stroke)).on_hover_text(server_hint).clicked() {
                    client = false;
                }
                if ui.add(Button::new(tr("Client")).selected(client).corner_radius(right).stroke(stroke)).on_hover_text(client_hint).clicked() {
                    client = true;
                }
            });
            grid.mode = if client { DataMode::Client } else { DataMode::Server };
            let server = !client;
            ui.add_space(4.0);
            ui.label(RichText::new(icon::FUNNEL).size(15.0)).on_hover_text(if server {
                tr("WHERE condition sent to the database")
            } else {
                tr("Client side filter expression (see help)")
            });
            // The inputs share what is left after the fixed widgets: the
            // ORDER BY label, two history buttons and the trailing buttons.
            let label_w = ui.fonts_mut(|f| f.layout_no_wrap(tr("ORDER BY").to_string(), egui::TextStyle::Body.resolve(ui.style()), egui::Color32::WHITE).rect.width());
            let button = ui.spacing().interact_size.y + ui.spacing().button_padding.x * 2.0 + ui.spacing().item_spacing.x;
            let trailing = label_w + button * 6.0 + ui.spacing().item_spacing.x * 4.0;
            let flexible = (ui.available_width() - trailing).max(160.0);
            let hint = if server { tr("WHERE condition, e.g. status = 'open'") } else { tr("Filter, e.g. amount > 100 and city = 'Bandung'") };
            const WHERE_WORDS: [&str; 10] = ["AND", "OR", "NOT", "IS NULL", "IS NOT NULL", "LIKE", "ILIKE", "IN", "BETWEEN", "NULL"];
            let filter_resp = crate::suggest::suggest_edit(
                ui,
                ui.id().with("filter_where"),
                &mut state.filter_text,
                |t| t.hint_text(hint).desired_width(flexible * 0.62).font(egui::TextStyle::Monospace),
                &columns,
                &WHERE_WORDS,
                &quote,
                &mut state.rt.where_suggest,
            );
            if filter_resp.changed() {
                state.rt.filter_error = None;
            }
            if std::mem::take(&mut state.rt.focus_filter) {
                let end = egui::text::CCursor::new(state.filter_text.chars().count());
                let mut s = egui::text_edit::TextEditState::load(ui.ctx(), filter_resp.id).unwrap_or_default();
                s.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
                s.store(ui.ctx(), filter_resp.id);
                filter_resp.request_focus();
            }
            if filter_resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                apply = true;
            }
            if !server {
                filter_resp.clone().on_hover_text(crate::expr::help_text());
            }
            history_button(ui, "filter_history", &state.filter_history.clone(), &mut state.filter_text, &mut apply);
            ui.label(tr("ORDER BY"));
            let order_resp = crate::suggest::suggest_edit(
                ui,
                ui.id().with("filter_order"),
                &mut state.order_text,
                |t| t.hint_text(tr("column [DESC], …")).desired_width(flexible * 0.38).font(egui::TextStyle::Monospace),
                &columns,
                // The client sort knows no NULLS FIRST / LAST (NULLs sort first).
                if server_mode { &["ASC", "DESC", "NULLS FIRST", "NULLS LAST"] } else { &["ASC", "DESC"] },
                &quote,
                &mut state.rt.order_suggest,
            );
            if order_resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                apply = true;
            }
            history_button(ui, "order_history", &state.order_history.clone(), &mut state.order_text, &mut apply);
            if ui.button(dbiji_icons::ic(icon::PLAY)).on_hover_text(tr("Apply filter and order (Enter)")).clicked() {
                apply = true;
            }
            if ui.button(dbiji_icons::ic(icon::X)).on_hover_text(tr("Clear filter and order")).clicked() {
                clear = true;
            }
            if crate::model::source_table(model.columns()).is_ok() {
                ui.menu_button(icon::DOTS_THREE, |ui| {
                    let some = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
                    if ui.button((dbiji_icons::ic(icon::PUSH_PIN), tr("Save as default filter"))).on_hover_text(tr("Applied whenever this table's data is opened")).clicked() {
                        actions.push(GridAction::DefaultFilter { where_clause: some(&state.filter_text), order_by: some(&state.order_text) });
                        ui.close();
                    }
                    if ui.button((dbiji_icons::ic(icon::PUSH_PIN_SLASH), tr("Reset default filter"))).clicked() {
                        actions.push(GridAction::DefaultFilter { where_clause: None, order_by: None });
                        ui.close();
                    }
                });
            }
        });
        if let Some(e) = &state.rt.filter_error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        if clear {
            state.filter_text.clear();
            state.order_text.clear();
            state.rt.filter_error = None;
            grid.sort.clear();
            match grid.mode {
                DataMode::Client => model.clear_view(),
                DataMode::Server => actions.push(GridAction::ServerQueryChanged { where_clause: Some(String::new()), order_by: Some(String::new()) }),
            }
        }
        if apply {
            ToolbarState::push_history(&mut state.filter_history, &state.filter_text.clone());
            ToolbarState::push_history(&mut state.order_history, &state.order_text.clone());
            match grid.mode {
                DataMode::Server => {
                    actions.push(GridAction::ServerQueryChanged { where_clause: Some(state.filter_text.trim().to_string()), order_by: Some(state.order_text.trim().to_string()) });
                    grid.sort.clear();
                }
                DataMode::Client => {
                    let names = model.column_names();
                    let filter = if state.filter_text.trim().is_empty() {
                        Ok(None)
                    } else {
                        ClientFilter::parse(&state.filter_text, &names).map(Some).map_err(|e| e.display_with_position(&state.filter_text))
                    };
                    let sort = parse_order_by(&state.order_text, &names);
                    match (filter, sort) {
                        (Ok(filter), Ok(sort)) => {
                            state.rt.filter_error = None;
                            if !state.order_text.trim().is_empty() || !sort.is_empty() {
                                grid.sort = sort.clone();
                            }
                            let spec = crate::view::ViewSpec { sort: if state.order_text.trim().is_empty() { grid.sort.clone() } else { sort }, filter };
                            let ctx = ui.ctx().clone();
                            model.start_client_view(spec, Box::new(move || ctx.request_repaint()));
                        }
                        (Err(e), _) | (_, Err(e)) => state.rt.filter_error = Some(e),
                    }
                }
            }
        }
        actions
    }
}

fn history_button(ui: &mut Ui, salt: &str, history: &[String], target: &mut String, apply: &mut bool) {
    let resp = ui.add_enabled(!history.is_empty(), Button::new(dbiji_icons::ic(icon::CARET_DOWN)).small()).on_hover_text(tr("History"));
    Popup::menu(&resp).id(ui.id().with(salt)).close_behavior(PopupCloseBehavior::CloseOnClick).show(|ui| {
        ui.set_min_width(240.0);
        for h in history {
            if ui.button(RichText::new(h).monospace()).clicked() {
                *target = h.clone();
                *apply = true;
            }
        }
    });
}

/// The result toolbar: refresh, auto refresh, fetching, save/cancel,
/// row operations, export, view switcher, panel toggles and the status text.
pub struct ResultToolbar<'a> {
    model: &'a mut ResultModel,
    grid: &'a mut DataGridState,
    state: &'a mut ToolbarState,
}

impl<'a> ResultToolbar<'a> {
    /// New toolbar.
    pub fn new(model: &'a mut ResultModel, grid: &'a mut DataGridState, state: &'a mut ToolbarState) -> Self {
        ResultToolbar { model, grid, state }
    }

    /// Show it. Returns actions for the application.
    ///
    /// Groups, left to right: refresh · editing (add, duplicate, delete,
    /// save, discard) · export · view (segmented) · side panels; on the
    /// right the status, a message of the application and the fetch
    /// buttons next to the row count they change.
    pub fn show(self, ui: &mut Ui) -> Vec<GridAction> {
        let ResultToolbar { model, grid, state } = self;
        let mut actions = Vec::new();
        auto_refresh_tick(ui, model, grid.is_editing(), state, &mut actions);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            // --- refresh
            if ui.button(dbiji_icons::ic(icon::ARROWS_CLOCKWISE)).on_hover_text(tr("Refresh (Cmd+R)")).clicked() {
                actions.push(GridAction::Refresh);
            }
            let label = match state.auto_refresh_secs {
                None => icon::TIMER.to_string(),
                Some(s) => format!("{} {}", icon::TIMER, auto_label(s)),
            };
            let resp = ui.add(Button::new(label).selected(state.auto_refresh_secs.is_some())).on_hover_text(tr("Auto refresh"));
            Popup::menu(&resp).show(|ui| {
                for choice in AUTO_REFRESH_CHOICES {
                    let text = match choice {
                        None => tr("Off").to_string(),
                        Some(s) => auto_label(s),
                    };
                    if ui.selectable_label(state.auto_refresh_secs == choice, text).clicked() {
                        set_auto_refresh(ui, state, choice, &mut actions);
                        ui.close();
                    }
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(tr("Custom (s)"));
                    if state.custom_refresh_secs == 0 {
                        state.custom_refresh_secs = 15;
                    }
                    ui.add(egui::DragValue::new(&mut state.custom_refresh_secs).range(1..=86_400));
                    if ui.button(tr("Set")).clicked() {
                        let s = state.custom_refresh_secs.max(1);
                        set_auto_refresh(ui, state, Some(s), &mut actions);
                        ui.close();
                    }
                });
            });
            group_separator(ui);
            // --- editing
            let pending = model.pending_counts().total();
            let editable = model.is_editable();
            let notify = |r: Result<usize, String>, actions: &mut Vec<GridAction>| {
                if let Err(e) = r {
                    actions.push(GridAction::Notify { message: e, error: true });
                }
            };
            if ui.add_enabled(editable, Button::new(dbiji_icons::ic(icon::ROWS_PLUS_BOTTOM))).on_hover_text(tr("Add row")).clicked() {
                notify(grid::add_row(model, grid), &mut actions);
            }
            if ui.add_enabled(editable, Button::new(dbiji_icons::ic(icon::COPY_SIMPLE))).on_hover_text(tr("Duplicate selected rows")).clicked() {
                notify(grid::duplicate_selected(model, grid), &mut actions);
            }
            if ui.add_enabled(editable, Button::new(dbiji_icons::ic(icon::TRASH))).on_hover_text(tr("Delete selected rows")).clicked() {
                notify(grid::delete_selected(model, grid), &mut actions);
            }
            // Save stands out while there is something to save.
            let save = if pending > 0 {
                let v = ui.visuals();
                Button::new(RichText::new(format!("{} {} ({pending})", icon::FLOPPY_DISK, tr("Save"))).color(v.strong_text_color()).strong())
                    .fill(v.selection.bg_fill)
                    .stroke(egui::Stroke::new(1.0, v.hyperlink_color))
            } else {
                Button::new(dbiji_icons::ic(icon::FLOPPY_DISK))
            };
            if ui.add_enabled(pending > 0, save).on_hover_text(trf("Save {} pending change(s) (Cmd+S)", &[&pending])).clicked() {
                actions.push(GridAction::SaveChanges);
            }
            let discard = if pending > 0 { Button::new(RichText::new(format!("{} {}", icon::X_CIRCLE, tr("Discard")))) } else { Button::new(dbiji_icons::ic(icon::X_CIRCLE)) };
            let cancel = ui.add_enabled(pending > 0, discard).on_hover_text(tr("Discard pending changes"));
            if cancel.clicked() {
                state.rt.confirm_discard = true;
            }
            if state.rt.confirm_discard {
                let mut open = true;
                Popup::from_response(&cancel).open_bool(&mut open).close_behavior(PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                    ui.label(trf("Discard {} pending change(s)?", &[&pending]));
                    ui.horizontal(|ui| {
                        if ui.button(tr("Discard")).clicked() {
                            model.discard_changes();
                            actions.push(GridAction::ChangesDiscarded);
                            state.rt.confirm_discard = false;
                        }
                        if ui.button(tr("Keep")).clicked() {
                            state.rt.confirm_discard = false;
                        }
                    });
                });
                if !open {
                    state.rt.confirm_discard = false;
                }
            }
            if !editable && let Some(r) = model.read_only_reason() {
                ui.label(RichText::new(format!("{} {}", icon::LOCK_SIMPLE, r.short_label())).color(ui.visuals().warn_fg_color)).on_hover_text(r.message());
            }
            group_separator(ui);
            // --- export
            if ui.button((dbiji_icons::ic(icon::EXPORT), tr("Export"))).on_hover_text(tr("Export…")).clicked() {
                actions.push(GridAction::Export { rows: None, columns: grid.visible_columns() });
            }
            group_separator(ui);
            // --- view: one segmented control
            let views = [
                (ResultViewKind::Grid, icon::TABLE, tr("Grid")),
                (ResultViewKind::Text, icon::TEXT_AA, tr("Text")),
                (ResultViewKind::Record, icon::ROWS, tr("Record")),
                (ResultViewKind::Json, icon::BRACKETS_CURLY, tr("JSON")),
                (ResultViewKind::Xml, icon::BRACKETS_ANGLE, tr("XML")),
            ];
            let mut items: Vec<(&str, &str, bool)> = views.iter().map(|(k, ic, tip)| (*ic, *tip, state.view == *k)).collect();
            items.push((icon::CHART_BAR, tr("Chart"), false));
            items.push((icon::MAP_TRIFOLD, tr("Spatial view"), false));
            match segmented(ui, &items) {
                Some(i) if i < views.len() => {
                    let kind = views[i].0;
                    if state.view != kind {
                        state.view = kind;
                        actions.push(GridAction::SwitchView(kind));
                    }
                }
                Some(i) if i == views.len() => actions.push(GridAction::ShowChart),
                Some(_) => actions.push(GridAction::ShowSpatial),
                None => {}
            }
            // --- right: fetch buttons, status, messages (right to left)
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let fetching = model.fetch.fetching;
                if ui.add_enabled(!model.fetch.counting, Button::new(dbiji_icons::ic(icon::HASH))).on_hover_text(tr("Calculate total row count")).clicked() {
                    actions.push(GridAction::CountTotal);
                }
                if model.fetch.has_more {
                    if ui.add_enabled(!fetching, Button::new(dbiji_icons::ic(icon::ARROW_LINE_DOWN))).on_hover_text(tr("Fetch all rows")).clicked() {
                        actions.push(GridAction::FetchAll);
                    }
                    if ui.add_enabled(!fetching, Button::new(dbiji_icons::ic(icon::CARET_DOUBLE_DOWN))).on_hover_text(tr("Fetch next page")).clicked() {
                        actions.push(GridAction::FetchMore);
                    }
                }
                let mut status = status_text(model);
                if state.auto_refresh_secs.is_some() && (model.has_changes() || model.fetch.fetching) {
                    status.push_str(" · ");
                    status.push_str(tr("auto refresh paused"));
                }
                ui.add(egui::Label::new(RichText::new(status).color(ui.visuals().weak_text_color())).truncate());
                // A failed fetch popped up once; a click shows it again.
                if let Some(e) = &model.fetch.error {
                    let label =
                        egui::Label::new(RichText::new(format!("{} {}", icon::WARNING_CIRCLE, e)).color(ui.visuals().error_fg_color)).truncate().sense(egui::Sense::click());
                    if ui.add(label).on_hover_text(tr("Show the error")).clicked() {
                        state.error_popup = Some(e.clone());
                    }
                }
                // Errors open a pop-up (the result viewer).
                if let Some((message, error)) = state.notice.clone().filter(|(_, error)| !error) {
                    if ui.small_button(dbiji_icons::ic(icon::X)).on_hover_text(tr("Dismiss")).clicked() {
                        state.notice = None;
                    }
                    let color = if error { ui.visuals().error_fg_color } else { ui.visuals().text_color() };
                    let ic = if error { icon::WARNING_CIRCLE } else { icon::INFO };
                    ui.add(egui::Label::new(RichText::new(format!("{ic} {message}")).color(color)).truncate()).on_hover_text(message);
                }
            });
        });
        actions
    }
}

/// Space and a line between toolbar groups.
fn group_separator(ui: &mut Ui) {
    ui.add_space(2.0);
    ui.separator();
    ui.add_space(2.0);
}

/// Connected toggle buttons; returns the index of the clicked one.
fn segmented(ui: &mut Ui, items: &[(&str, &str, bool)]) -> Option<usize> {
    let mut clicked = None;
    let n = items.len();
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
        for (i, (ic, tip, selected)) in items.iter().enumerate() {
            let (first, last) = (i == 0, i + 1 == n);
            let r = 4;
            let corners = egui::CornerRadius { nw: if first { r } else { 0 }, sw: if first { r } else { 0 }, ne: if last { r } else { 0 }, se: if last { r } else { 0 } };
            if ui.add(Button::new(*ic).selected(*selected).corner_radius(corners).stroke(stroke)).on_hover_text(*tip).clicked() {
                clicked = Some(i);
            }
        }
    });
    clicked
}

fn auto_label(secs: u64) -> String {
    if secs.is_multiple_of(60) { trf("{} min", &[&(secs / 60)]) } else { trf("{} s", &[&secs]) }
}

fn set_auto_refresh(ui: &Ui, state: &mut ToolbarState, secs: Option<u64>, actions: &mut Vec<GridAction>) {
    state.auto_refresh_secs = secs;
    let now = ui.input(|i| i.time);
    state.rt.next_refresh = secs.map(|s| now + s as f64);
    actions.push(GridAction::AutoRefreshChanged(secs.map(Duration::from_secs)));
    if let Some(s) = secs {
        ui.ctx().request_repaint_after(Duration::from_secs(s));
    }
}

/// Emit `Refresh` when the auto refresh interval elapsed, unless there are
/// changes or a cell is being edited; schedules the next frame.
fn auto_refresh_tick(ui: &Ui, model: &ResultModel, editing: bool, state: &mut ToolbarState, actions: &mut Vec<GridAction>) {
    let Some(secs) = state.auto_refresh_secs else {
        state.rt.next_refresh = None;
        return;
    };
    let now = ui.input(|i| i.time);
    let next = *state.rt.next_refresh.get_or_insert(now + secs as f64);
    if now >= next {
        if !model.has_changes() && !model.fetch.fetching && !editing {
            actions.push(GridAction::Refresh);
        }
        state.rt.next_refresh = Some(now + secs as f64);
        ui.ctx().request_repaint_after(Duration::from_secs(secs));
    } else {
        ui.ctx().request_repaint_after(Duration::from_secs_f64(next - now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_by_parsing() {
        let cols: Vec<String> = ["id", "Name", "order date"].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse_order_by("name desc, id", &cols).unwrap(), vec![SortKey::desc(1), SortKey::asc(0)]);
        assert_eq!(parse_order_by("[order date] ASC", &cols).unwrap(), vec![SortKey::asc(2)]);
        assert_eq!(parse_order_by("\"order date\" DESC", &cols).unwrap(), vec![SortKey::desc(2)]);
        assert_eq!(parse_order_by("3 desc", &cols).unwrap(), vec![SortKey::desc(2)]);
        assert!(parse_order_by("nope", &cols).unwrap_err().contains("nope"));
        assert!(parse_order_by("", &cols).unwrap().is_empty());
    }

    #[test]
    fn texts() {
        assert_eq!(duration_text(Duration::from_millis(120)), "120 ms");
        assert_eq!(duration_text(Duration::from_millis(1250)), "1.25 s");
        assert_eq!(duration_text(Duration::from_secs(125)), "2 min 5 s");
        let mut m = ResultModel::from_rows(vec![dbiji_core::ColumnMeta::new("a", "int", dbiji_core::DataKind::Int)], vec![vec![dbiji_core::Value::Int(1)]; 1234]);
        m.fetch.has_more = true;
        m.fetch.total_count = Some(5000);
        m.fetch.elapsed = Some(Duration::from_millis(42));
        assert_eq!(status_text(&m), "1,234 rows · more available · total 5,000 · 42 ms");
        let mut s = ToolbarState::default();
        ToolbarState::push_history(&mut s.filter_history, " a = 1 ");
        ToolbarState::push_history(&mut s.filter_history, "b = 2");
        ToolbarState::push_history(&mut s.filter_history, "a = 1");
        assert_eq!(s.filter_history, vec!["a = 1".to_string(), "b = 2".to_string()]);
        s.set_panel_open(PanelKind::Calc, true);
        assert!(s.is_panel_open(PanelKind::Calc));
        s.set_panel_open(PanelKind::Calc, false);
        assert!(!s.is_panel_open(PanelKind::Calc));
    }
}
