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

/// Space before the text of a table cell.
const CELL_PAD: f32 = 6.0;

/// The main action of a dialog or wizard: white text on the accent colour.
pub fn primary_button(ui: &Ui, glyph: &str, text: impl Into<String>) -> egui::Button<'static> {
    let fill = crate::theme::accent(ui.visuals().dark_mode);
    egui::Button::new(RichText::new(format!("{glyph}  {}", text.into())).color(egui::Color32::WHITE).strong()).fill(fill)
}

/// Escape pressed on the dialog window shown in `layer` while it is the top window,
/// no pop-up is open (a combo box list closes first) and the keyboard focus is not
/// elsewhere (an editor behind a non-modal dialog): it closes like Cancel.
pub fn escape_closes(ctx: &egui::Context, layer: Option<egui::LayerId>) -> bool {
    if layer.is_none() || ctx.top_layer_id() != layer || egui::Popup::is_any_open(ctx) || !ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        return false;
    }
    let focus = ctx.memory(|m| m.focused()).and_then(|id| ctx.read_response(id));
    focus.is_none_or(|r| Some(r.layer_id) == layer)
}

/// A heading with a coloured icon.
pub fn icon_heading(ui: &mut Ui, glyph: &str, text: impl Into<String>) -> egui::Response {
    let mut job = egui::text::LayoutJob::default();
    let style = ui.style().clone();
    let font = egui::FontSelection::Default;
    dbiji_icons::ic(glyph).heading().append_to(&mut job, &style, font.clone(), egui::Align::Center);
    RichText::new(format!(" {}", text.into())).heading().append_to(&mut job, &style, font, egui::Align::Center);
    ui.label(job)
}

/// Virtualised, striped, resizable table of text cells.
pub fn simple_table(ui: &mut Ui, id_salt: impl std::hash::Hash + std::fmt::Debug, headers: &[&str], rows: &[Vec<String>], selected: Option<usize>) -> TableResponse {
    // In its own ui: the column lines end at the last row, not at the bottom of the panel.
    ui.scope(|ui| table_in(ui, id_salt, headers, rows, selected)).inner
}

fn table_in(ui: &mut Ui, id_salt: impl std::hash::Hash + std::fmt::Debug, headers: &[&str], rows: &[Vec<String>], selected: Option<usize>) -> TableResponse {
    let mut out = TableResponse::default();
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
    // Every column at least as wide as its header (plus the cell padding).
    let font = egui::TextStyle::Body.resolve(ui.style());
    let text_w = |ui: &mut Ui, s: &str| ui.fonts_mut(|f| f.layout_no_wrap(s.to_string(), font.clone(), egui::Color32::WHITE).rect.width());
    let header_w: Vec<f32> = headers.iter().map(|h| text_w(ui, h) + 2.0 * CELL_PAD + 6.0).collect();
    // Initially as wide as the first rows' text: truncated labels never widen an auto column.
    let content_w: Vec<f32> = (0..headers.len())
        .map(|i| {
            let widest = rows.iter().take(100).filter_map(|r| r.get(i)).map(|c| text_w(ui, c.lines().next().unwrap_or("")) + 4.0).fold(0.0, f32::max);
            widest + 2.0 * CELL_PAD
        })
        .collect();
    let mut tb = TableBuilder::new(ui)
        .id_salt(id_salt)
        .striped(true)
        .resizable(true)
        .sense(egui::Sense::click())
        // As tall as its rows: the column lines end at the last row.
        .auto_shrink([false, true])
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
    for (i, w) in header_w.iter().enumerate() {
        tb = if i + 1 == headers.len() {
            tb.column(Column::remainder().at_least(w.max(60.0)).clip(true))
        } else {
            tb.column(Column::initial(content_w[i].clamp(w.max(40.0), 480.0)).at_least(w.max(40.0)).clip(true).resizable(true))
        };
    }
    tb.header(row_h + 2.0, |mut header| {
        for h in headers {
            header.col(|ui| {
                ui.add_space(CELL_PAD);
                ui.label(RichText::new(*h).strong());
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
                        ui.add_space(CELL_PAD);
                        let short: String = c.chars().take(300).collect();
                        let first_line = short.lines().next().unwrap_or("");
                        // A leading icon (e.g. the key of primary key columns) in its colour.
                        let mut chars = first_line.chars();
                        let icon = chars.next().map(String::from).and_then(|g| dbiji_icons::color(&g).map(|c| (g, c)));
                        let text = match icon {
                            Some((g, color)) => {
                                ui.label(RichText::new(g).color(color));
                                chars.as_str().trim_start()
                            }
                            None => first_line,
                        };
                        let resp = ui.add(egui::Label::new(text).truncate().selectable(false));
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
            if ui.button((dbiji_icons::ic(icon::ARROW_CLOCKWISE), tr("Retry"))).clicked() {
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

/// Read-only, selectable monospace text (DDL, scripts) coloured by `tokens`.
pub fn code_view(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, text: &str, tokens: &[(std::ops::Range<usize>, dbiji_editor::TokenStyle)]) {
    let theme = dbiji_editor::EditorTheme::from_visuals(ui.visuals());
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _wrap: f32| ui.fonts_mut(|f| f.layout_job(crate::sqllang::highlight_job(buf.as_str(), tokens, &theme, font.clone())));
    egui::ScrollArea::both().id_salt(id).auto_shrink([false, false]).show(ui, |ui| {
        let mut t = text;
        ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_width(f32::INFINITY).interactive(true).layouter(&mut layouter));
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
