//! Query manager (FR-S7): search the history of executed statements and
//! reopen them in an editor.

use crate::command::AppCommand;
use crate::cx::AppContext;
use crate::tabs::TabView;
use dbiji_config::{HistoryEntry, HistoryQuery, QueryStatus};
use dbiji_core::{tr, trf};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Period {
    Day,
    Week,
    Month,
    All,
}

pub struct HistoryTab {
    text: String,
    connection: Option<String>,
    status: Option<QueryStatus>,
    period: Period,
    entries: Vec<HistoryEntry>,
    total: u64,
    selected: Option<usize>,
    dirty_at: Option<Instant>,
    loaded: bool,
    error: Option<String>,
    limit: usize,
}

impl Default for HistoryTab {
    fn default() -> Self {
        HistoryTab {
            text: String::new(),
            connection: None,
            status: None,
            period: Period::Week,
            entries: Vec::new(),
            total: 0,
            selected: None,
            dirty_at: None,
            loaded: false,
            error: None,
            limit: 500,
        }
    }
}

impl HistoryTab {
    pub fn for_connection(connection: Option<String>) -> Self {
        HistoryTab { connection, ..Default::default() }
    }

    fn query(&self) -> HistoryQuery {
        let now = chrono::Utc::now().timestamp_millis();
        let since = match self.period {
            Period::Day => Some(now - 86_400_000),
            Period::Week => Some(now - 7 * 86_400_000),
            Period::Month => Some(now - 30 * 86_400_000),
            Period::All => None,
        };
        HistoryQuery {
            text: Some(self.text.trim().to_string()).filter(|t| !t.is_empty()),
            connection_id: self.connection.clone(),
            since,
            until: None,
            status: self.status,
            limit: self.limit,
            offset: 0,
        }
    }

    fn reload(&mut self, cx: &mut AppContext) {
        self.loaded = true;
        self.dirty_at = None;
        let Some(store) = cx.services.history.clone() else {
            self.error = Some(tr("The query history is disabled.").to_string());
            return;
        };
        let q = self.query();
        let store = store.lock();
        match store.search(&q) {
            Ok(e) => {
                self.entries = e;
                self.total = store.count(&q).unwrap_or(self.entries.len() as u64);
                self.error = None;
                if self.selected.is_some_and(|s| s >= self.entries.len()) {
                    self.selected = None;
                }
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn open_in_editor(&self, cx: &mut AppContext, e: &HistoryEntry) {
        cx.push(AppCommand::NewSqlEditor { connection: e.connection_id.clone(), schema: None, text: Some(e.sql.clone()) });
    }
}

fn status_label(s: QueryStatus) -> &'static str {
    match s {
        QueryStatus::Ok => tr("Success"),
        QueryStatus::Error => tr("Error"),
        QueryStatus::Cancelled => tr("Cancelled"),
    }
}

impl TabView for HistoryTab {
    fn title(&self) -> String {
        tr("Query history").to_string()
    }

    fn icon(&self) -> &'static str {
        icon::CLOCK_COUNTER_CLOCKWISE
    }

    fn dedupe_key(&self) -> Option<String> {
        Some("history".into())
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if !self.loaded || self.dirty_at.is_some_and(|t| t.elapsed() > Duration::from_millis(250)) {
            self.reload(cx);
        }
        if let Some(t) = self.dirty_at {
            ui.ctx().request_repaint_after(Duration::from_millis(260).saturating_sub(t.elapsed()));
        }
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label(icon::MAGNIFYING_GLASS);
            changed |= ui.add(egui::TextEdit::singleline(&mut self.text).hint_text(tr("Search SQL text…")).desired_width(260.0)).changed();
            let conn_name = self
                .connection
                .as_deref()
                .and_then(|c| cx.conns.config(c))
                .map(|c| c.name.clone())
                .unwrap_or_else(|| tr("All connections").to_string());
            egui::ComboBox::from_id_salt("hist_conn").selected_text(conn_name).show_ui(ui, |ui| {
                changed |= ui.selectable_value(&mut self.connection, None, tr("All connections")).changed();
                for c in cx.conns.configs() {
                    changed |= ui.selectable_value(&mut self.connection, Some(c.id.clone()), &c.name).changed();
                }
            });
            let st = self.status.map(status_label).unwrap_or(tr("Any status"));
            egui::ComboBox::from_id_salt("hist_status").selected_text(st).show_ui(ui, |ui| {
                changed |= ui.selectable_value(&mut self.status, None, tr("Any status")).changed();
                for s in [QueryStatus::Ok, QueryStatus::Error, QueryStatus::Cancelled] {
                    changed |= ui.selectable_value(&mut self.status, Some(s), status_label(s)).changed();
                }
            });
            for (p, l) in [(Period::Day, tr("24 hours")), (Period::Week, tr("7 days")), (Period::Month, tr("30 days")), (Period::All, tr("All"))] {
                if ui.selectable_label(self.period == p, l).clicked() && self.period != p {
                    self.period = p;
                    changed = true;
                }
            }
            if ui.button(icon::ARROW_CLOCKWISE).on_hover_text(tr("Refresh")).clicked() {
                self.reload(cx);
            }
        });
        if changed {
            self.dirty_at = Some(Instant::now());
        }
        if let Some(e) = &self.error {
            crate::widgets::error_box(ui, e);
        }
        ui.weak(trf("{} of {} entries", &[&self.entries.len(), &self.total]));
        let sel_entry = self.selected.and_then(|i| self.entries.get(i).cloned());
        let names: std::collections::HashMap<String, String> = cx.conns.configs().iter().map(|c| (c.id.clone(), c.name.clone())).collect();
        let rows: Vec<Vec<String>> = self
            .entries
            .iter()
            .map(|e| {
                let when = chrono::DateTime::from_timestamp_millis(e.timestamp_ms)
                    .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default();
                let conn = e
                    .connection_id
                    .as_ref()
                    .and_then(|c| names.get(c).cloned())
                    .or_else(|| e.connection_name.clone())
                    .unwrap_or_default();
                vec![
                    when,
                    conn,
                    e.duration_ms.map(|d| crate::widgets::format_duration(Duration::from_millis(d))).unwrap_or_default(),
                    e.rows.map(|r| r.to_string()).unwrap_or_default(),
                    status_label(e.status).to_string(),
                    e.sql.split_whitespace().collect::<Vec<_>>().join(" "),
                ]
            })
            .collect();
        let detail_h = if sel_entry.is_some() { 160.0 } else { 0.0 };
        let avail = ui.available_height() - detail_h;
        ui.allocate_ui(egui::vec2(ui.available_width(), avail.max(80.0)), |ui| {
            let r = crate::widgets::simple_table(
                ui,
                "history_table",
                &[tr("Time"), tr("Connection"), tr("Duration"), tr("Rows"), tr("Status"), tr("SQL")],
                &rows,
                self.selected,
            );
            if let Some(i) = r.clicked {
                self.selected = Some(i);
            }
            if let Some(i) = r.double_clicked {
                if let Some(e) = self.entries.get(i).cloned() {
                    self.open_in_editor(cx, &e);
                }
            }
            if let Some(i) = r.context_row {
                self.selected = Some(i);
            }
        });
        if let Some(e) = sel_entry {
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button(format!("{} {}", icon::FILE_SQL, tr("Open in SQL editor"))).clicked() {
                    self.open_in_editor(cx, &e);
                }
                if ui.button(format!("{} {}", icon::COPY, tr("Copy"))).clicked() {
                    ui.ctx().copy_text(e.sql.clone());
                }
                if ui.button(format!("{} {}", icon::TRASH, tr("Delete"))).clicked() {
                    if let Some(store) = cx.services.history.clone() {
                        let _ = store.lock().delete(&[e.id]);
                    }
                    self.selected = None;
                    self.reload(cx);
                }
                if let Some(err) = &e.error_message {
                    ui.label(RichText::new(err.lines().next().unwrap_or("")).color(crate::theme::error_color(ui.visuals().dark_mode)));
                }
            });
            egui::ScrollArea::vertical().id_salt("hist_sql").max_height(120.0).show(ui, |ui| {
                let mut t = e.sql.as_str();
                ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_width(f32::INFINITY));
            });
        }
    }

    crate::impl_tab_any!();
}
