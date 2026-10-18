//! Small reusable widgets: virtualised tables, property sheets, status lines.

use crate::backend::Loadable;
use dbiji_core::meta::Property;
use dbiji_core::tr;
use egui::{RichText, Ui};
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as icon;

/// Response of [`simple_table`].
#[derive(Default)]
pub struct TableResponse {
    pub clicked: Option<usize>,
    pub double_clicked: Option<usize>,
    pub context_row: Option<usize>,
}

/// Virtualised, striped, resizable table of text cells.
pub fn simple_table(ui: &mut Ui, id_salt: impl std::hash::Hash + std::fmt::Debug, headers: &[&str], rows: &[Vec<String>], selected: Option<usize>) -> TableResponse {
    let mut out = TableResponse::default();
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
    let mut tb = TableBuilder::new(ui)
        .id_salt(id_salt)
        .striped(true)
        .resizable(true)
        .sense(egui::Sense::click())
        .auto_shrink([false, false])
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
    for (i, _) in headers.iter().enumerate() {
        tb = if i + 1 == headers.len() {
            tb.column(Column::remainder().at_least(60.0).clip(true))
        } else {
            tb.column(Column::auto().at_least(40.0).at_most(480.0).clip(true).resizable(true))
        };
    }
    tb.header(row_h + 2.0, |mut header| {
        for h in headers {
            header.col(|ui| {
                ui.strong(*h);
            });
        }
    })
    .body(|body| {
        body.rows(row_h, rows.len(), |mut row| {
            let i = row.index();
            row.set_selected(selected == Some(i));
            if let Some(cells) = rows.get(i) {
                for c in cells.iter().take(headers.len()) {
                    row.col(|ui| {
                        let short: String = c.chars().take(300).collect();
                        let first_line = short.lines().next().unwrap_or("");
                        let resp = ui.add(egui::Label::new(first_line).truncate().selectable(false));
                        if c.len() > first_line.len() {
                            resp.on_hover_text(c.chars().take(4000).collect::<String>());
                        }
                    });
                }
            }
            let resp = row.response();
            if resp.clicked() {
                out.clicked = Some(i);
            }
            if resp.double_clicked() {
                out.double_clicked = Some(i);
            }
            if resp.secondary_clicked() {
                out.context_row = Some(i);
            }
        });
    });
    out
}

/// Name/value list, grouped when properties carry a group.
pub fn property_grid(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug + Copy, props: &[Property]) {
    let mut groups: Vec<Option<&str>> = Vec::new();
    for p in props {
        let g = p.group.as_deref();
        if !groups.contains(&g) {
            groups.push(g);
        }
    }
    for g in groups {
        if let Some(name) = g {
            ui.add_space(4.0);
            ui.label(RichText::new(name).strong());
        }
        egui::Grid::new((id, g.unwrap_or(""))).num_columns(2).striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
            for p in props.iter().filter(|p| p.group.as_deref() == g) {
                ui.label(RichText::new(&p.name).weak());
                ui.add(egui::Label::new(&p.value).wrap());
                ui.end_row();
            }
        });
    }
}

/// Shows a spinner / error with retry for a [`Loadable`]. Returns true when
/// the value is loaded (caller draws it), and sets `retry` when the user asks.
pub fn load_state<T>(ui: &mut Ui, value: &Loadable<T>, retry: &mut bool) -> bool {
    match value {
        Loadable::Loaded(_) => true,
        Loadable::NotLoaded | Loadable::Loading(_) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(tr("Loading…"));
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            false
        }
        Loadable::Failed(e) => {
            error_box(ui, &e.full_text());
            if ui.button(format!("{} {}", icon::ARROW_CLOCKWISE, tr("Retry"))).clicked() {
                *retry = true;
            }
            false
        }
    }
}

pub fn error_box(ui: &mut Ui, text: &str) {
    let color = crate::theme::error_color(ui.visuals().dark_mode);
    egui::Frame::group(ui.style()).stroke(egui::Stroke::new(1.0, color)).show(ui, |ui| {
        ui.horizontal_top(|ui| {
            ui.label(RichText::new(icon::WARNING_CIRCLE).color(color));
            ui.add(egui::Label::new(RichText::new(text).color(color)).wrap().selectable(true));
        });
    });
}

/// Read-only, selectable monospace text (DDL, logs) with a copy button.
pub fn code_view(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, text: &str) {
    ui.horizontal(|ui| {
        if ui.small_button(format!("{} {}", icon::COPY, tr("Copy"))).clicked() {
            ui.ctx().copy_text(text.to_string());
        }
    });
    egui::ScrollArea::both().id_salt(id).auto_shrink([false, false]).show(ui, |ui| {
        let mut t = text;
        ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_width(f32::INFINITY).interactive(true));
    });
}

/// Human readable byte size.
pub fn format_bytes(n: i64) -> String {
    let n = n as f64;
    if n < 1024.0 {
        format!("{n:.0} B")
    } else if n < 1024.0 * 1024.0 {
        format!("{:.1} KB", n / 1024.0)
    } else if n < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB", n / 1_048_576.0)
    } else {
        format!("{:.2} GB", n / 1_073_741_824.0)
    }
}

/// Human readable duration.
pub fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else if ms < 60_000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else {
        let s = d.as_secs();
        format!("{}m {:02}s", s / 60, s % 60)
    }
}
