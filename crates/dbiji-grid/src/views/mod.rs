//! Alternative result views (FR-D5): record, text, JSON and XML.

mod docs;
mod record;
mod text;

pub use docs::{DocumentKind, DocumentView, DocumentViewState};
pub use record::{RecordView, RecordViewState};
pub use text::{MAX_TEXT_ROWS, TextView, TextViewState, render_text_table};

use dbiji_core::tr;
use egui::{ScrollArea, TextStyle, Ui};
use std::sync::Arc;

/// Show pre-rendered lines in a virtualised, monospace scroll area.
pub(crate) fn show_lines(ui: &mut Ui, id_salt: impl std::hash::Hash + std::fmt::Debug, lines: &Arc<Vec<String>>, max_chars: usize) {
    let font = TextStyle::Monospace.resolve(ui.style());
    let row_h = ui.fonts_mut(|f| f.row_height(&font));
    let char_w = ui.fonts_mut(|f| f.glyph_width(&font, 'M'));
    let width = (max_chars as f32 * char_w + 16.0).max(ui.available_width());
    ScrollArea::both().id_salt(id_salt).auto_shrink([false, false]).show_rows(ui, row_h, lines.len(), |ui, range| {
        ui.set_min_width(width);
        for i in range {
            if let Some(line) = lines.get(i) {
                ui.add(egui::Label::new(egui::RichText::new(line.as_str()).monospace()).extend());
            }
        }
    });
    if lines.is_empty() {
        ui.weak(tr("(empty)"));
    }
}
