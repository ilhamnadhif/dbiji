//! Mock data generation: fills a table with generated test data. The
//! generators are chosen from the column names, types and constraints
//! (`auto_generators`) and can be changed per column.

use super::{
    MessageLog, Running, combo, connection_name, error_text, next_uid, not_connected, number_edit, on_error_options,
    opt_number, parsed_edit, progress_ui, reload_table_def, section, table_def_of,
};
use crate::command::AppCommand;
use crate::cx::AppContext;
use crate::tabs::TabView;
use crate::transferexec::AppSessionFactory;
use chrono::{Duration as ChronoDuration, NaiveDate, NaiveDateTime};
use dbiji_core::ddl::{ColumnDef, TableDef};
use dbiji_core::meta::ObjectRef;
use dbiji_core::{tr, trf};
use dbiji_transfer::{ColumnGenerator, GeneratorKind, MockConfig, MockMode, MockSummary, SessionFactory, auto_generators, run_mock};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use std::mem::discriminant;
use std::sync::Arc;

const DATE_FORMAT: &str = "%Y-%m-%d";
const DATETIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// The mock data tab.
pub struct MockDataTab {
    uid: u64,
    conn: String,
    table: ObjectRef,
    def: Option<Arc<TableDef>>,
    def_error: Option<String>,
    /// Generators picked automatically (offered again per column).
    auto: Vec<ColumnGenerator>,
    config: MockConfig,
    run: Option<Running<MockSummary>>,
    log: MessageLog,
    result: Option<Result<MockSummary, String>>,
    error: Option<String>,
    confirm: bool,
}

impl MockDataTab {
    /// Generate rows for `table` of `conn`.
    pub fn new(conn: String, table: ObjectRef) -> Self {
        MockDataTab {
            uid: next_uid(),
            conn,
            table,
            def: None,
            def_error: None,
            auto: Vec::new(),
            config: MockConfig::default(),
            run: None,
            log: MessageLog::default(),
            result: None,
            error: None,
            confirm: false,
        }
    }

    // ------------------------------------------------------------ public API

    /// The table definition is loaded and the generators are prepared.
    pub fn definition_loaded(&self) -> bool {
        self.def.is_some()
    }

    /// Rows, generators, batch size, seed, mode and error handling.
    pub fn config_mut(&mut self) -> &mut MockConfig {
        &mut self.config
    }

    /// Generator of a column.
    pub fn generator_mut(&mut self, column: &str) -> Option<&mut ColumnGenerator> {
        self.config.generators.iter_mut().find(|g| g.column == column)
    }

    /// Generate and insert the rows (the "Generate" button without the confirmation).
    pub fn start(&mut self, cx: &mut AppContext) {
        if self.run.is_some() {
            return;
        }
        if let Err(e) = self.validate() {
            self.error = Some(e);
            return;
        }
        if let Some(cfg) = cx.conns.config(&self.conn)
            && cfg.read_only
        {
            self.error = Some(trf("The connection \"{}\" is read-only.", &[&cfg.name]));
            return;
        }
        if cx.conns.live(&self.conn).is_none() {
            self.error = Some(not_connected(cx, &self.conn));
            return;
        }
        let Some(def) = self.def.clone() else { return };
        let Some(live) = cx.conns.live(&self.conn) else { return };
        let factory = AppSessionFactory::from_live(live, self.table.database.as_deref());
        let mut config = self.config.clone();
        config.table = (*def).clone();
        self.error = None;
        self.result = None;
        self.log.clear();
        let title = trf("Mock data: {}", &[&self.table.display_path()]);
        self.run = Some(Running::spawn(cx, title, move |progress, cancel| async move {
            let mut session = factory.open().await?;
            let result = run_mock(&config, session.as_mut(), &progress, &cancel).await;
            let _ = session.close().await;
            result
        }));
    }

    pub fn is_running(&self) -> bool {
        self.run.is_some()
    }

    pub fn cancel(&self) {
        if let Some(r) = &self.run {
            r.cancel();
        }
    }

    pub fn last_summary(&self) -> Option<&MockSummary> {
        self.result.as_ref().and_then(|r| r.as_ref().ok())
    }

    pub fn last_error(&self) -> Option<&str> {
        match &self.result {
            Some(Err(e)) => Some(e),
            _ => self.error.as_deref().or(self.def_error.as_deref()),
        }
    }

    // --------------------------------------------------------------- helpers

    fn validate(&self) -> Result<(), String> {
        if self.def.is_none() {
            return Err(tr("The table definition is not loaded yet.").to_string());
        }
        if self.config.rows == 0 {
            return Err(tr("Choose how many rows to generate.").to_string());
        }
        if self.config.generators.iter().all(|g| g.generator == GeneratorKind::Skip) {
            return Err(tr("Every column is left out; choose a generator for at least one column.").to_string());
        }
        Ok(())
    }

    fn poll(&mut self, cx: &mut AppContext) {
        if self.def.is_none() && self.def_error.is_none() {
            let (conn, table) = (self.conn.clone(), self.table.clone());
            match table_def_of(cx, &conn, &table) {
                Some(Ok(def)) => {
                    self.auto = auto_generators(&def);
                    self.config.generators = self.auto.clone();
                    self.config.table = (*def).clone();
                    self.def = Some(def);
                }
                Some(Err(e)) => self.def_error = Some(e.full_text()),
                None => {}
            }
        }
        let mut finished = None;
        if let Some(run) = &mut self.run {
            self.log.drain(&run.progress);
            if let Some(result) = run.poll(cx, describe) {
                self.log.drain(&run.progress);
                finished = Some(result);
            }
        }
        if let Some(result) = finished {
            self.run = None;
            match result {
                Ok(summary) => {
                    if summary.rows_failed > 0 {
                        cx.toasts.warning(describe(&summary), summary.errors.first().map(|e| e.message.clone()));
                    } else {
                        cx.toasts.success(describe(&summary));
                    }
                    cx.push(AppCommand::RefreshMetadata { connection: self.conn.clone(), object: Some(self.table.clone()), schema: None });
                    self.result = Some(Ok(summary));
                }
                Err(e) => {
                    if e.kind != dbiji_core::ErrorKind::Cancelled {
                        cx.toasts.error(tr("Generating mock data failed"), Some(e.full_text()));
                    }
                    self.result = Some(Err(error_text(&e)));
                }
            }
        }
    }

    fn settings_ui(&mut self, ui: &mut Ui) {
        let id = self.uid;
        let c = &mut self.config;
        egui::Grid::new(("mock_settings", id)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label(tr("Rows"));
            ui.add(egui::DragValue::new(&mut c.rows).range(1..=100_000_000).speed(10.0));
            ui.end_row();
            ui.label(tr("Rows per batch"));
            ui.add(egui::DragValue::new(&mut c.batch_size).range(1..=100_000));
            ui.end_row();
            ui.label(tr("Seed"));
            ui.horizontal(|ui| {
                opt_number(ui, &mut c.seed, tr("random"));
                ui.weak(tr("the same seed generates the same data"));
            });
            ui.end_row();
            ui.label(tr("Existing rows"));
            combo(ui, ("mock_mode", id), &mut c.mode, &[
                (MockMode::Append, tr("Keep them and add the new rows").to_string()),
                (MockMode::TruncateFirst, tr("Delete them first").to_string()),
            ]);
            ui.end_row();
            ui.label(tr("On error"));
            combo(ui, ("mock_err", id), &mut c.on_error, &on_error_options());
            ui.end_row();
        });
    }

    fn generators_ui(&mut self, ui: &mut Ui) {
        let Some(def) = self.def.clone() else { return };
        let id = self.uid;
        ui.horizontal(|ui| {
            if ui.button(format!("{} {}", icon::MAGIC_WAND, tr("Choose automatically again"))).clicked() {
                self.config.generators = self.auto.clone();
            }
            ui.weak(tr("Columns left out get their default value (auto-increment, defaults)."));
        });
        egui::ScrollArea::horizontal().id_salt(("mock_gen_h", id)).show(ui, |ui| {
            egui::Grid::new(("mock_gen", id)).num_columns(6).spacing([10.0, 4.0]).striped(true).show(ui, |ui| {
                ui.strong(tr("Column"));
                ui.strong(tr("Type"));
                ui.strong(tr("Generator"));
                ui.strong(tr("Settings"));
                ui.strong(tr("NULL"));
                ui.strong(tr("Unique"));
                ui.end_row();
                for (i, col) in def.columns.iter().enumerate() {
                    let auto = self.auto.iter().find(|g| g.column == col.name).map(|g| g.generator.clone());
                    if !self.config.generators.iter().any(|g| g.column == col.name) {
                        self.config.generators.push(ColumnGenerator::new(&col.name, GeneratorKind::Skip));
                    }
                    let Some(g) = self.config.generators.iter_mut().find(|g| g.column == col.name) else { continue };
                    let mut name = RichText::new(&col.name);
                    if def.primary_key.as_ref().is_some_and(|k| k.columns.contains(&col.name)) {
                        name = name.strong();
                    }
                    ui.label(name);
                    let mut ty = col.data_type.clone();
                    if !col.nullable {
                        ty.push_str(" NOT NULL");
                    }
                    if col.auto_increment {
                        ty.push_str(" AUTO");
                    }
                    ui.weak(ty);
                    generator_combo(ui, (id, i), col, &mut g.generator, auto.as_ref());
                    ui.horizontal(|ui| params_ui(ui, &mut g.generator));
                    ui.add_enabled_ui(col.nullable && g.generator != GeneratorKind::Skip, |ui| {
                        let mut percent = (g.null_ratio * 100.0).round();
                        if ui.add(egui::DragValue::new(&mut percent).range(0.0..=100.0).suffix(" %")).changed() {
                            g.null_ratio = (percent / 100.0).clamp(0.0, 1.0);
                        }
                    });
                    let textual = col.kind.is_textual() || matches!(col.kind, dbiji_core::DataKind::Enum);
                    ui.add_enabled_ui(textual && g.generator != GeneratorKind::Skip, |ui| {
                        ui.checkbox(&mut g.unique, "").on_hover_text(tr("Append the row number to make every value unique"));
                    });
                    ui.end_row();
                }
            });
        });
    }

    fn result_ui(&mut self, ui: &mut Ui) {
        let dark = ui.visuals().dark_mode;
        match &self.result {
            Some(Ok(s)) => {
                section(ui, &describe(s));
                ui.label(crate::widgets::format_duration(std::time::Duration::from_millis(s.duration_ms)));
                for w in &s.warnings {
                    ui.label(RichText::new(format!("{} {w}", icon::WARNING)).color(crate::theme::warning_color(dark)));
                }
                if !s.errors.is_empty() {
                    section(ui, tr("Rejected rows"));
                    for e in s.errors.iter().take(100) {
                        ui.label(RichText::new(trf("Row {}: {}", &[&e.row, &e.message])).small().color(crate::theme::error_color(dark)));
                    }
                }
            }
            Some(Err(e)) => {
                section(ui, tr("Generating mock data failed"));
                crate::widgets::error_box(ui, e);
            }
            None => {}
        }
    }

    fn confirm_ui(&mut self, ctx: &egui::Context, cx: &mut AppContext) {
        if !self.confirm {
            return;
        }
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new(("mock_confirm", self.uid))).show(ctx, |ui| {
            ui.heading(tr("Delete the existing rows?"));
            ui.label(trf("All rows of {} are deleted before the new rows are generated.", &[&self.table.display_path()]));
            ui.horizontal(|ui| {
                if ui.button(RichText::new(tr("Delete and generate")).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
                    self.start(cx);
                    close = true;
                }
                if ui.button(tr("Cancel")).clicked() {
                    close = true;
                }
            });
        });
        if close || modal.should_close() {
            self.confirm = false;
        }
    }
}

fn describe(s: &MockSummary) -> String {
    if s.rows_failed > 0 {
        trf("{} rows generated, {} rejected", &[&s.rows_inserted, &s.rows_failed])
    } else {
        trf("{} rows generated", &[&s.rows_inserted])
    }
}

/// Generators offered for a column, with their default settings.
fn choices(col: &ColumnDef, auto: Option<&GeneratorKind>, current: &GeneratorKind) -> Vec<(GeneratorKind, String)> {
    let now = chrono::Local::now().naive_local();
    let today = now.date();
    let five_years = today - ChronoDuration::days(5 * 365);
    let limit = text_limit(col).unwrap_or(40);
    let mut out = vec![
        (GeneratorKind::Skip, tr("Leave out (default value)").to_string()),
        (GeneratorKind::Null, tr("NULL").to_string()),
        (GeneratorKind::Constant { value: String::new() }, tr("Fixed value").to_string()),
        (GeneratorKind::Sequence { start: 1, step: 1, after_max: true }, tr("Sequence").to_string()),
        (GeneratorKind::IntRange { min: 1, max: 1000 }, tr("Whole numbers").to_string()),
        (GeneratorKind::DecimalRange { min: 0.0, max: 1000.0, scale: 2 }, tr("Decimal numbers").to_string()),
        (GeneratorKind::FloatRange { min: 0.0, max: 1000.0, decimals: 2 }, tr("Floating point numbers").to_string()),
        (GeneratorKind::Bool { true_ratio: 0.5 }, tr("True / false").to_string()),
        (GeneratorKind::DateRange { from: five_years, to: today }, tr("Dates").to_string()),
        (
            GeneratorKind::DateTimeRange { from: now - ChronoDuration::days(730), to: now },
            tr("Timestamps").to_string(),
        ),
        (GeneratorKind::Uuid, tr("UUID").to_string()),
        (GeneratorKind::Text { min: 5.min(limit), max: limit.min(40) }, tr("Lorem ipsum text").to_string()),
        (GeneratorKind::Word, tr("Word").to_string()),
        (GeneratorKind::Words { min: 1, max: 4 }, tr("Words").to_string()),
        (GeneratorKind::Sentence, tr("Sentence").to_string()),
        (GeneratorKind::Paragraph, tr("Paragraph").to_string()),
        (GeneratorKind::FirstName, tr("First name").to_string()),
        (GeneratorKind::LastName, tr("Last name").to_string()),
        (GeneratorKind::FullName, tr("Full name").to_string()),
        (GeneratorKind::UserName, tr("User name").to_string()),
        (GeneratorKind::Email, tr("E-mail address").to_string()),
        (GeneratorKind::Phone, tr("Phone number").to_string()),
        (GeneratorKind::City, tr("City").to_string()),
        (GeneratorKind::Country, tr("Country").to_string()),
        (GeneratorKind::StreetAddress, tr("Street address").to_string()),
        (GeneratorKind::Company, tr("Company").to_string()),
        (GeneratorKind::Url, tr("URL").to_string()),
        (GeneratorKind::IpV4, tr("IPv4 address").to_string()),
        (GeneratorKind::Enum { values: vec!["A".into(), "B".into(), "C".into()] }, tr("One of a list").to_string()),
        (GeneratorKind::Json, tr("JSON object").to_string()),
        (GeneratorKind::Bytes { min: 8, max: 32 }, tr("Random bytes").to_string()),
        (GeneratorKind::Pattern { pattern: "???-####".into() }, tr("Pattern").to_string()),
    ];
    let fk = [auto, Some(current)].into_iter().flatten().find(|g| matches!(g, GeneratorKind::ForeignKey { .. })).cloned();
    if let Some(fk) = fk {
        out.push((fk, tr("Value of the referenced table").to_string()));
    }
    // Keep the automatic choice's settings (sizes, ranges, enum values).
    if let Some(a) = auto
        && let Some(slot) = out.iter_mut().find(|(g, _)| discriminant(g) == discriminant(a))
    {
        slot.0 = a.clone();
    }
    out
}

/// `varchar(40)` → 40.
fn text_limit(c: &ColumnDef) -> Option<usize> {
    let t = c.data_type.to_ascii_lowercase();
    let inner = t.split_once('(')?.1.split(')').next()?;
    inner.split(',').next()?.trim().parse::<usize>().ok().filter(|n| *n > 0)
}

fn generator_combo(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    col: &ColumnDef,
    value: &mut GeneratorKind,
    auto: Option<&GeneratorKind>,
) {
    let options = choices(col, auto, value);
    let current =
        options.iter().find(|(g, _)| discriminant(g) == discriminant(value)).map(|(_, l)| l.clone()).unwrap_or_default();
    egui::ComboBox::from_id_salt(id).selected_text(current).width(190.0).height(420.0).show_ui(ui, |ui| {
        for (g, label) in options {
            let selected = discriminant(&g) == discriminant(value);
            if ui.selectable_label(selected, label).clicked() && !selected {
                *value = g;
            }
        }
    });
}

fn date_edit(ui: &mut Ui, value: &mut NaiveDate) {
    parsed_edit(ui, value.format(DATE_FORMAT).to_string(), DATE_FORMAT, 90.0, false, |t| {
        if let Ok(d) = NaiveDate::parse_from_str(t.trim(), DATE_FORMAT) {
            *value = d;
        }
    });
}

fn datetime_edit(ui: &mut Ui, value: &mut NaiveDateTime) {
    parsed_edit(ui, value.format(DATETIME_FORMAT).to_string(), DATETIME_FORMAT, 150.0, false, |t| {
        let t = t.trim();
        if let Ok(d) = NaiveDateTime::parse_from_str(t, DATETIME_FORMAT) {
            *value = d;
        } else if let Ok(d) = NaiveDate::parse_from_str(t, DATE_FORMAT) {
            *value = d.and_time(chrono::NaiveTime::MIN);
        }
    });
}

/// Settings of a generator.
fn params_ui(ui: &mut Ui, g: &mut GeneratorKind) {
    let unit = if matches!(g, GeneratorKind::Text { .. }) { tr("characters") } else { tr("words") };
    match g {
        GeneratorKind::Constant { value } => {
            ui.add(egui::TextEdit::singleline(value).hint_text(tr("value")).desired_width(160.0));
        }
        GeneratorKind::Sequence { start, step, after_max } => {
            ui.label(tr("from"));
            number_edit(ui, start, 70.0);
            ui.label(tr("step"));
            number_edit(ui, step, 50.0);
            ui.checkbox(after_max, tr("after the largest value"));
        }
        GeneratorKind::IntRange { min, max } => {
            number_edit(ui, min, 80.0);
            ui.label("–");
            number_edit(ui, max, 80.0);
        }
        GeneratorKind::FloatRange { min, max, decimals } => {
            number_edit(ui, min, 80.0);
            ui.label("–");
            number_edit(ui, max, 80.0);
            ui.label(tr("decimals"));
            ui.add(egui::DragValue::new(decimals).range(0..=15));
        }
        GeneratorKind::DecimalRange { min, max, scale } => {
            number_edit(ui, min, 80.0);
            ui.label("–");
            number_edit(ui, max, 80.0);
            ui.label(tr("decimals"));
            ui.add(egui::DragValue::new(scale).range(0..=18));
        }
        GeneratorKind::Bool { true_ratio } => {
            let mut percent = (*true_ratio * 100.0).round();
            if ui.add(egui::DragValue::new(&mut percent).range(0.0..=100.0).suffix(" %")).changed() {
                *true_ratio = (percent / 100.0).clamp(0.0, 1.0);
            }
            ui.weak(tr("true"));
        }
        GeneratorKind::DateRange { from, to } => {
            date_edit(ui, from);
            ui.label("–");
            date_edit(ui, to);
        }
        GeneratorKind::DateTimeRange { from, to } => {
            datetime_edit(ui, from);
            ui.label("–");
            datetime_edit(ui, to);
        }
        GeneratorKind::Text { min, max } | GeneratorKind::Words { min, max } => {
            ui.add(egui::DragValue::new(min).range(0..=10_000));
            ui.label("–");
            ui.add(egui::DragValue::new(max).range(0..=10_000));
            ui.weak(unit);
        }
        GeneratorKind::Bytes { min, max } => {
            ui.add(egui::DragValue::new(min).range(0..=1_048_576));
            ui.label("–");
            ui.add(egui::DragValue::new(max).range(0..=1_048_576));
            ui.weak(tr("bytes"));
        }
        GeneratorKind::Enum { values } => {
            let mut text = values.join(", ");
            if ui.add(egui::TextEdit::singleline(&mut text).hint_text(tr("comma separated values")).desired_width(220.0)).changed() {
                *values = text.split(',').map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect();
            }
        }
        GeneratorKind::Pattern { pattern } => {
            ui.add(egui::TextEdit::singleline(pattern).desired_width(140.0))
                .on_hover_text(tr("# digit, ? upper case letter, * letter or digit, \\ escapes the next character"));
        }
        GeneratorKind::ForeignKey { table, column, .. } => {
            ui.weak(format!("{}.{column}", table.display_path()));
        }
        _ => {}
    }
}

impl TabView for MockDataTab {
    fn title(&self) -> String {
        trf("Mock data: {}", &[&self.table.name])
    }

    fn icon(&self) -> &'static str {
        icon::SHUFFLE
    }

    fn connection_id(&self) -> Option<&str> {
        Some(&self.conn)
    }

    fn dedupe_key(&self) -> Option<String> {
        Some(format!("mock:{}:{}", self.conn, self.table.display_path()))
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        self.poll(cx);
        let uid = self.uid;
        egui::Panel::top(egui::Id::new(("mock_top", uid))).show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading(format!("{} {}", icon::SHUFFLE, tr("Generate mock data")));
                ui.label(RichText::new(self.table.display_path()).strong());
                ui.weak(connection_name(cx, &self.conn));
            });
            ui.add_space(2.0);
        });
        egui::Panel::bottom(egui::Id::new(("mock_bottom", uid))).show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let running = self.run.is_some();
                let ok = self.validate().is_ok() && !running;
                if ui.add_enabled(ok, egui::Button::new(format!("{} {}", icon::PLAY, tr("Generate")))).clicked() {
                    if self.config.mode == MockMode::TruncateFirst {
                        self.confirm = true;
                    } else {
                        self.start(cx);
                    }
                }
                if let Some(e) = &self.error {
                    ui.label(RichText::new(e).color(crate::theme::error_color(ui.visuals().dark_mode)));
                }
            });
            ui.add_space(2.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt(("mock_body", uid)).auto_shrink([false, false]).show(ui, |ui| {
                if cx.conns.live(&self.conn).is_none() {
                    ui.horizontal(|ui| {
                        ui.weak(tr("The connection is closed."));
                        if ui.button(format!("{} {}", icon::PLUGS_CONNECTED, tr("Connect"))).clicked() {
                            cx.push(AppCommand::Connect(self.conn.clone()));
                        }
                    });
                    return;
                }
                if let Some(e) = self.def_error.clone() {
                    crate::widgets::error_box(ui, &trf("Cannot read the table definition: {}", &[&e]));
                    if ui.button(format!("{} {}", icon::ARROW_CLOCKWISE, tr("Retry"))).clicked() {
                        reload_table_def(cx, &self.conn, &self.table);
                        self.def_error = None;
                    }
                    return;
                }
                if self.def.is_none() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak(tr("Reading the table definition…"));
                    });
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                    return;
                }
                if let Some(run) = &self.run {
                    section(ui, tr("Generating…"));
                    if progress_ui(ui, &run.progress, run.is_cancelling()) {
                        run.cancel();
                    }
                    self.log.ui(ui, ("mock_log", uid), 200.0);
                    return;
                }
                section(ui, tr("Settings"));
                self.settings_ui(ui);
                section(ui, tr("Columns"));
                self.generators_ui(ui);
                self.result_ui(ui);
                if self.result.is_some() {
                    self.log.ui(ui, ("mock_log", uid), 200.0);
                }
            });
        });
        let ctx = ui.ctx().clone();
        self.confirm_ui(&ctx, cx);
    }

    fn on_close(&mut self, cx: &mut AppContext) -> bool {
        if let Some(run) = &mut self.run {
            run.abandon(cx);
        }
        true
    }

    crate::impl_tab_any!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_keep_automatic_settings() {
        let col = ColumnDef::new("status", "varchar(10)");
        let auto = GeneratorKind::Enum { values: vec!["x".into()] };
        let list = choices(&col, Some(&auto), &auto);
        let e = list.iter().find(|(g, _)| matches!(g, GeneratorKind::Enum { .. })).unwrap();
        assert_eq!(e.0, auto);
        assert!(!list.iter().any(|(g, _)| matches!(g, GeneratorKind::ForeignKey { .. })));
        let text = list.iter().find(|(g, _)| matches!(g, GeneratorKind::Text { .. })).unwrap();
        assert_eq!(text.0, GeneratorKind::Text { min: 5, max: 10 });
        assert_eq!(text_limit(&ColumnDef::new("n", "numeric(10,2)")), Some(10));
    }
}
