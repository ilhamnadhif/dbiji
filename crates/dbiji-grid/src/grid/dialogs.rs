//! Dialogs owned by the grid: advanced copy, generated SQL, virtual columns.

use super::frame::GridFrame;
use super::{selection_copy_data_limited, sql_table_name};
use crate::copy::{CopyFormat, QuoteMode, render};
use crate::expr::Expr;
use dbiji_core::{tr, trf};
use egui::{ComboBox, Grid, Key, Modal, ScrollArea, TextEdit, Ui, Window};

impl GridFrame<'_> {
    /// Show open dialogs.
    pub fn dialogs(&mut self, ui: &mut Ui) {
        if self.state.rt.copy_dialog {
            self.copy_dialog(ui);
        }
        if self.state.rt.sql_window.is_some() {
            self.sql_window(ui);
        }
        if self.state.rt.virtual_dialog.is_some() {
            self.virtual_dialog(ui);
        }
    }

    fn copy_dialog(&mut self, ui: &mut Ui) {
        let mut opts = self.state.copy_options.clone();
        if opts.sql_table.is_empty() {
            opts.sql_table = sql_table_name(self.model, self.options);
        }
        let mut do_copy = false;
        let mut close = false;
        let preview = match selection_copy_data_limited(self.model, self.state, self.options, 8) {
            Ok(data) => render(&data, &opts, &self.options.format, self.options.dialect.as_deref()),
            Err(e) => e,
        };
        let resp = Modal::new(self.id.with("copy_dialog")).show(ui.ctx(), |ui| {
            ui.set_width(460.0);
            ui.heading(tr("Advanced copy"));
            ui.add_space(4.0);
            Grid::new(self.id.with("copy_grid")).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label(tr("Format"));
                ComboBox::from_id_salt(self.id.with("copy_format")).selected_text(opts.format.label()).show_ui(
                    ui,
                    |ui| {
                        for f in CopyFormat::ALL {
                            ui.selectable_value(&mut opts.format, f, f.label());
                        }
                    },
                );
                ui.end_row();
                ui.label(tr("Options"));
                ui.horizontal(|ui| {
                    ui.checkbox(&mut opts.include_header, tr("Column names"));
                    ui.checkbox(&mut opts.include_row_numbers, tr("Row numbers"));
                    ui.checkbox(&mut opts.formatted, tr("Formatted values"));
                });
                ui.end_row();
                if opts.format.is_delimited() {
                    ui.label(tr("Delimiter"));
                    ui.horizontal(|ui| {
                        let default = if opts.format == CopyFormat::Csv { "," } else { "\\t" };
                        ui.add(TextEdit::singleline(&mut opts.delimiter).desired_width(60.0).hint_text(default));
                        ui.label(tr("Quote"));
                        ComboBox::from_id_salt(self.id.with("copy_quote"))
                            .selected_text(match opts.quote {
                                QuoteMode::Auto => tr("When needed"),
                                QuoteMode::Always => tr("Always"),
                                QuoteMode::Never => tr("Never"),
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut opts.quote, QuoteMode::Auto, tr("When needed"));
                                ui.selectable_value(&mut opts.quote, QuoteMode::Always, tr("Always"));
                                ui.selectable_value(&mut opts.quote, QuoteMode::Never, tr("Never"));
                            });
                    });
                    ui.end_row();
                }
                ui.label(tr("NULL as"));
                ui.add(TextEdit::singleline(&mut opts.null_text).desired_width(120.0).hint_text(tr("(empty)")));
                ui.end_row();
                if opts.format == CopyFormat::SqlInsert {
                    ui.label(tr("Table"));
                    ui.add(TextEdit::singleline(&mut opts.sql_table).desired_width(240.0));
                    ui.end_row();
                }
            });
            ui.add_space(6.0);
            ui.label(egui::RichText::new(tr("Preview")).strong());
            ScrollArea::both().max_height(160.0).id_salt(self.id.with("copy_preview")).show(ui, |ui| {
                let mut p = preview.as_str();
                ui.add(TextEdit::multiline(&mut p).code_editor().desired_width(f32::INFINITY).desired_rows(4));
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button(tr("Copy")).clicked() || ui.input(|i| i.key_pressed(Key::Enter) && i.modifiers.command) {
                    do_copy = true;
                }
                if ui.button(tr("Cancel")).clicked() {
                    close = true;
                }
            });
        });
        if resp.should_close() {
            close = true;
        }
        self.state.copy_options = opts.clone();
        if do_copy {
            self.copy_selection(&opts);
            close = true;
        }
        if close {
            self.state.rt.copy_dialog = false;
            ui.memory_mut(|m| m.request_focus(self.id));
        }
    }

    fn sql_window(&mut self, ui: &mut Ui) {
        let Some((title, sql)) = self.state.rt.sql_window.clone() else { return };
        let mut open = true;
        let mut copy = false;
        Window::new(title)
            .id(self.id.with("sql_window"))
            .open(&mut open)
            .default_size([560.0, 320.0])
            .resizable(true)
            .collapsible(false)
            .show(ui.ctx(), |ui| {
                ui.horizontal(|ui| {
                    if ui.button(format!("{}  {}", egui_phosphor::regular::COPY, tr("Copy"))).clicked() {
                        copy = true;
                    }
                });
                ScrollArea::both().show(ui, |ui| {
                    let mut s = sql.as_str();
                    ui.add(TextEdit::multiline(&mut s).code_editor().desired_width(f32::INFINITY));
                });
            });
        if copy {
            ui.ctx().copy_text(sql);
            self.notify(tr("SQL copied to the clipboard"), false);
        }
        if !open {
            self.state.rt.sql_window = None;
        }
    }

    fn virtual_dialog(&mut self, ui: &mut Ui) {
        let Some(mut dlg) = self.state.rt.virtual_dialog.take() else { return };
        let names: Vec<String> = self.model.columns().iter().map(|c| c.name.clone()).collect();
        let check = if dlg.expression.trim().is_empty() {
            None
        } else {
            Some(Expr::compile(&dlg.expression, &names).map_err(|e| e.display_with_position(&dlg.expression)))
        };
        let mut apply = false;
        let mut close = false;
        let resp = Modal::new(self.id.with("virtual_dialog")).show(ui.ctx(), |ui| {
            ui.set_width(480.0);
            ui.heading(if dlg.editing.is_some() { tr("Edit virtual column") } else { tr("Add virtual column") });
            Grid::new(self.id.with("virtual_grid")).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label(tr("Name"));
                ui.add(TextEdit::singleline(&mut dlg.name).desired_width(300.0));
                ui.end_row();
                ui.label(tr("Expression"));
                ui.add(
                    TextEdit::multiline(&mut dlg.expression)
                        .code_editor()
                        .desired_rows(3)
                        .desired_width(300.0)
                        .hint_text("price * quantity"),
                );
                ui.end_row();
            });
            match &check {
                Some(Err(e)) => {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                Some(Ok(_)) => {
                    ui.weak(tr("Expression is valid"));
                }
                None => {}
            }
            if let Some(e) = &dlg.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.collapsing(tr("Expression help"), |ui| {
                ui.label(crate::expr::help_text());
                ui.label(trf("Columns: {}", &[&names.join(", ")]));
            });
            ui.horizontal(|ui| {
                let ok = matches!(check, Some(Ok(_)));
                if ui
                    .add_enabled(ok, egui::Button::new(if dlg.editing.is_some() { tr("Save") } else { tr("Add") }))
                    .clicked()
                {
                    apply = true;
                }
                if ui.button(tr("Cancel")).clicked() {
                    close = true;
                }
            });
        });
        if resp.should_close() {
            close = true;
        }
        if apply {
            let result = match dlg.editing {
                Some(i) => self.model.update_virtual_column(i, &dlg.name, &dlg.expression).map(|_| ()),
                None => self.model.add_virtual_column(&dlg.name, &dlg.expression).map(|_| ()),
            };
            match result {
                Ok(()) => {
                    self.state.sync_columns(self.model);
                    close = true;
                }
                Err(e) => dlg.error = Some(e.display_with_position(&dlg.expression)),
            }
        }
        if !close {
            self.state.rt.virtual_dialog = Some(dlg);
        } else {
            ui.memory_mut(|m| m.request_focus(self.id));
        }
    }
}
