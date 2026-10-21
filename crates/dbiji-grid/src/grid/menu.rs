//! Context menus of the body and the header.

use super::frame::GridFrame;
use super::state::VirtualColumnDialog;
use super::{GridAction, SqlKind, apply_sort, apply_value_filter, sql_table_name};
use crate::copy::{CopyFormat, CopyOptions};
use crate::format::ColumnFormat;
use crate::model::ReadOnlyReason;
use crate::view::{FilterOp, SortKey};
use dbiji_core::dialect::GenericDialect;
use dbiji_core::{Dialect, Value, tr, trf};
use egui::{Button, Key, KeyboardShortcut, Modifiers, Popup, PopupCloseBehavior, PopupKind, Response, RichText, SetOpenCommand, Ui};
use egui_phosphor::regular as icon;

fn item(ui: &mut Ui, icon: &str, text: &str, shortcut: Option<KeyboardShortcut>) -> Response {
    let mut b = Button::new(format!("{icon}  {text}"));
    if let Some(s) = shortcut {
        b = b.shortcut_text(ui.ctx().format_shortcut(&s));
    }
    ui.add(b)
}

fn short_value(v: &Value) -> String {
    if v.is_null() {
        return "NULL".into();
    }
    let t = v.to_text();
    let first = t.lines().next().unwrap_or("");
    if first.chars().count() > 32 || t.contains('\n') { format!("{}…", first.chars().take(32).collect::<String>()) } else { first.to_string() }
}

impl GridFrame<'_> {
    /// Context menu of the body (and row numbers).
    pub fn context_menu(&mut self, resp: &Response) {
        Popup::context_menu(resp).show(|ui| {
            ui.set_min_width(220.0);
            self.menu_items(ui);
        });
    }

    /// Context menu of the header.
    pub fn header_menu(&mut self, resp: &Response) {
        if resp.secondary_clicked()
            && let Some(p) = resp.interact_pointer_pos()
            && let Some(pos) = self.geo.col_at(p.x, self.state.rt.scroll_x, false)
            && !self.state.rt.selection.is_column_selected(pos)
        {
            self.state.rt.selection.select_columns(pos, pos, false);
        }
        Popup::context_menu(resp).show(|ui| {
            ui.set_min_width(220.0);
            let Some(col) = self.active().map(|(_, c)| c).or_else(|| self.geo.cols.first().copied()) else {
                ui.label(tr("No columns"));
                return;
            };
            self.column_items(ui, col);
            ui.separator();
            if item(ui, icon::COPY, tr("Copy column name"), None).clicked() {
                self.ctx.copy_text(self.model.column_name(col).to_string());
                ui.close();
            }
            if item(ui, icon::COPY, tr("Copy column values"), None).clicked() {
                let nrows = self.geo.nrows;
                if let Some(pos) = self.pos_of(col) {
                    self.state.rt.selection.select_columns(pos, pos, false);
                    let _ = nrows;
                    self.copy_selection(&CopyOptions::plain());
                }
                ui.close();
            }
        });
    }

    /// The column menu of the header's ▾ button: ordering, filters by the
    /// current cell's value, filter templates, default filter.
    pub fn header_popup(&mut self, ui: &Ui) {
        let Some((col, anchor, open)) = self.state.rt.header_popup else { return };
        self.state.rt.header_popup = Some((col, anchor, false));
        let set = open.then_some(SetOpenCommand::Bool(true));
        let shown = Popup::new(self.id.with("header_popup"), self.ctx.clone(), anchor, ui.layer_id())
            .kind(PopupKind::Menu)
            .layout(egui::Layout::top_down_justified(egui::Align::Min))
            .style(egui::containers::menu::menu_style)
            .gap(0.0)
            .open_memory(set)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.set_min_width(240.0);
                self.column_popup_items(ui, col);
            });
        if shown.is_none() {
            self.state.rt.header_popup = None;
        }
    }

    fn column_popup_items(&mut self, ui: &mut Ui, col: usize) {
        let name = self.model.column_name(col).to_string();
        let current = self.state.sort.iter().find(|k| k.column == col).copied();
        let check = |on: bool| if on { icon::CHECK } else { "" };
        // --- ordering
        for desc in [false, true] {
            let on = current.is_some_and(|k| k.descending == desc);
            let (arrow, label) = if desc { (icon::ARROW_DOWN, trf("Order by {} DESC", &[&name])) } else { (icon::ARROW_UP, trf("Order by {} ASC", &[&name])) };
            if ui.add(Button::new(format!("{arrow}  {label}")).right_text(check(on))).clicked() {
                let key = if desc { SortKey::desc(col) } else { SortKey::asc(col) };
                // A column already in the ordering keeps its place.
                let keys = if current.is_some() { self.state.sort.iter().map(|k| if k.column == col { key } else { *k }).collect() } else { vec![key] };
                apply_sort(self.model, self.state, self.options, keys, &self.ctx, &mut self.actions);
                ui.close();
            }
        }
        if ui.add_enabled(current.is_some(), Button::new(format!("{}  {}", icon::ARROWS_DOWN_UP, trf("Disable order by {}", &[&name])))).clicked() {
            let keys = self.state.sort.iter().copied().filter(|k| k.column != col).collect();
            apply_sort(self.model, self.state, self.options, keys, &self.ctx, &mut self.actions);
            ui.close();
        }
        if !self.state.sort.is_empty() && current.is_none() {
            ui.menu_button(format!("{}  {}", icon::LIST_PLUS, tr("Add to the ordering")), |ui| {
                for (desc, label) in [(false, "ASC"), (true, "DESC")] {
                    if ui.button(format!("{name} {label}")).clicked() {
                        let mut keys = self.state.sort.clone();
                        keys.push(if desc { SortKey::desc(col) } else { SortKey::asc(col) });
                        apply_sort(self.model, self.state, self.options, keys, &self.ctx, &mut self.actions);
                        ui.close();
                    }
                }
            });
        }
        // --- filters
        let server = self.state.mode == super::DataMode::Server;
        let filterable = !(self.model.is_virtual_column(col) && server);
        let row = self.active().map(|(r, _)| r).or((self.geo.nrows > 0).then_some(0));
        if filterable && let Some(row) = row {
            let value = self.model.cell(row, col).unwrap_or_default();
            ui.separator();
            ui.label(RichText::new(tr("Cell value")).weak().small());
            let v = short_value(&value);
            let ops: &[FilterOp] = if value.is_null() { &[FilterOp::Eq, FilterOp::NotEq] } else { &[FilterOp::Eq, FilterOp::NotEq, FilterOp::Gt, FilterOp::Lt] };
            for &op in ops {
                let label = if value.is_null() { format!("{name} {}", if op == FilterOp::Eq { "IS NULL" } else { "IS NOT NULL" }) } else { format!("{name} {} '{v}'", op.sql()) };
                if ui.button(label).clicked() {
                    apply_value_filter(self.model, self.state.mode, self.options, (col, op, &value), &self.ctx, &mut self.actions);
                    ui.close();
                }
            }
        }
        if filterable {
            ui.separator();
            ui.label(RichText::new(tr("Custom")).weak().small());
            let column = if server { super::server_column(self.model, self.options, col) } else { name.clone() };
            for op in [FilterOp::Eq, FilterOp::NotEq, FilterOp::Gt, FilterOp::Lt] {
                if ui.button(format!("{name} {} …", op.sql())).on_hover_text(tr("Type the value in the filter box")).clicked() {
                    self.actions.push(GridAction::EditFilter(format!("{column} {} ", op.sql())));
                    ui.close();
                }
            }
            for op in [FilterOp::IsNull, FilterOp::IsNotNull] {
                if ui.button(format!("{name} {}", op.sql())).clicked() {
                    apply_value_filter(self.model, self.state.mode, self.options, (col, op, &Value::Null), &self.ctx, &mut self.actions);
                    ui.close();
                }
            }
        }
        // --- default filter, reset
        ui.separator();
        if crate::model::source_table(self.model.columns()).is_ok() {
            if item(ui, icon::PUSH_PIN, tr("Save as default filter"), None).on_hover_text(tr("Applied whenever this table's data is opened")).clicked() {
                self.actions.push(GridAction::SaveDefaultFilter);
                ui.close();
            }
            if item(ui, icon::PUSH_PIN_SLASH, tr("Reset default filter"), None).clicked() {
                self.actions.push(GridAction::DefaultFilter { where_clause: None, order_by: None });
                ui.close();
            }
        }
        if item(ui, icon::FUNNEL_X, tr("Remove all filters and orderings"), None).clicked() {
            self.state.sort.clear();
            match self.state.mode {
                super::DataMode::Client => self.model.clear_view(),
                super::DataMode::Server => self.actions.push(GridAction::ServerQueryChanged { where_clause: Some(String::new()), order_by: Some(String::new()) }),
            }
            self.actions.push(GridAction::ClearFilterText);
            ui.close();
        }
        if item(ui, icon::FUNNEL, tr("Edit filter…"), None).clicked() {
            self.actions.push(GridAction::EditFilter(String::new()));
            ui.close();
        }
    }

    fn menu_items(&mut self, ui: &mut Ui) {
        let active = self.active();
        let editable = self.model.is_editable() && self.options.allow_editing;
        let cmd = Modifiers::COMMAND;
        // --- clipboard
        if item(ui, icon::COPY, tr("Copy"), Some(KeyboardShortcut::new(cmd, Key::C))).clicked() {
            self.copy_selection(&CopyOptions::plain());
            ui.close();
        }
        if item(ui, icon::CLIPBOARD, tr("Advanced copy…"), Some(KeyboardShortcut::new(cmd | Modifiers::SHIFT, Key::C))).clicked() {
            self.state.rt.copy_dialog = true;
            ui.close();
        }
        ui.menu_button(format!("{}  {}", icon::COPY_SIMPLE, tr("Copy as")), |ui| {
            for f in CopyFormat::ALL {
                if ui.button(f.label()).clicked() {
                    let opts = CopyOptions { format: f, include_header: true, ..self.state.copy_options.clone() };
                    self.copy_selection(&opts);
                    ui.close();
                }
            }
        });
        if editable && item(ui, icon::CLIPBOARD, tr("Paste"), Some(KeyboardShortcut::new(cmd, Key::V))).clicked() {
            self.actions.push(GridAction::PasteRequested);
            ui.close();
        }
        ui.separator();
        // --- editing
        if let Some((row, col)) = active {
            let cell_ok = editable && self.model.can_edit_cell(row, col).is_ok();
            ui.add_enabled_ui(cell_ok, |ui| {
                if item(ui, icon::PENCIL_SIMPLE, tr("Edit cell"), Some(KeyboardShortcut::new(Modifiers::NONE, Key::Enter))).clicked() {
                    self.start_edit(row, col, None);
                    ui.close();
                }
                if item(ui, icon::PROHIBIT, tr("Set NULL"), Some(KeyboardShortcut::new(Modifiers::SHIFT, Key::Delete))).clicked() {
                    self.set_selected_null();
                    ui.close();
                }
                if item(ui, icon::ARROW_COUNTER_CLOCKWISE, tr("Set DEFAULT"), None).clicked() {
                    self.set_selected_default();
                    ui.close();
                }
            });
        }
        ui.add_enabled_ui(editable, |ui| {
            if item(ui, icon::ROWS_PLUS_BOTTOM, tr("Add row"), Some(KeyboardShortcut::new(Modifiers::NONE, Key::Insert))).clicked() {
                self.add_row_after_active();
                ui.close();
            }
            if item(ui, icon::ROWS_PLUS_TOP, tr("Add row before"), Some(KeyboardShortcut::new(Modifiers::SHIFT, Key::Insert))).clicked() {
                self.add_row_before_active();
                ui.close();
            }
            if item(ui, icon::COPY_SIMPLE, tr("Duplicate row(s)"), Some(KeyboardShortcut::new(cmd, Key::D))).clicked() {
                self.duplicate_selected();
                ui.close();
            }
            if active.is_some() {
                if item(ui, icon::ARROW_FAT_LINE_DOWN, tr("Copy from row above"), Some(KeyboardShortcut::new(cmd | Modifiers::ALT, Key::ArrowUp))).clicked() {
                    self.copy_from_adjacent(true);
                    ui.close();
                }
                if item(ui, icon::ARROW_FAT_LINE_UP, tr("Copy from row below"), Some(KeyboardShortcut::new(cmd | Modifiers::ALT, Key::ArrowDown))).clicked() {
                    self.copy_from_adjacent(false);
                    ui.close();
                }
                if item(ui, icon::FINGERPRINT, tr("Generate UUID"), None).clicked() {
                    self.generate_uuid();
                    ui.close();
                }
            }
            if item(ui, icon::TRASH, tr("Delete row(s)"), Some(KeyboardShortcut::new(Modifiers::NONE, Key::Delete))).clicked() {
                self.delete_selected();
                ui.close();
            }
        });
        if self.model.can_undo() && item(ui, icon::ARROW_U_UP_LEFT, tr("Undo"), Some(KeyboardShortcut::new(cmd, Key::Z))).clicked() {
            self.model.undo();
            ui.close();
        }
        if !editable && let Some(reason) = self.model.read_only_reason().cloned() {
            ui.add_enabled(false, Button::new(format!("{}  {}", icon::LOCK_SIMPLE, reason.short_label()))).on_disabled_hover_text(reason.message());
            if reason == ReadOnlyReason::NoKey && item(ui, icon::KEY, tr("Define virtual key…"), None).clicked() {
                let table = crate::model::source_table(self.model.columns()).ok();
                self.actions.push(GridAction::DefineVirtualKey { table });
                ui.close();
            }
        }
        ui.separator();
        // --- filtering / ordering / columns
        if let Some((row, col)) = active {
            let value = self.model.cell(row, col).unwrap_or_default();
            let name = self.model.column_name(col).to_string();
            let virtual_server = self.model.is_virtual_column(col) && self.state.mode == super::DataMode::Server;
            ui.add_enabled_ui(!virtual_server, |ui| {
                ui.menu_button(format!("{}  {}", icon::FUNNEL, tr("Filter by value")), |ui| {
                    let v = short_value(&value);
                    for op in FilterOp::ALL {
                        if value.is_null() && !op.is_unary() && op != FilterOp::Eq && op != FilterOp::NotEq {
                            continue;
                        }
                        let label = match op {
                            FilterOp::IsNull | FilterOp::IsNotNull => format!("{name} {}", op.sql()),
                            FilterOp::Contains => trf("{} contains '{}'", &[&name, &v]),
                            _ if value.is_null() => {
                                format!("{name} {}", if op == FilterOp::Eq { "IS NULL" } else { "IS NOT NULL" })
                            }
                            _ => format!("{name} {} '{v}'", op.sql()),
                        };
                        if ui.button(label).clicked() {
                            apply_value_filter(self.model, self.state.mode, self.options, (col, op, &value), &self.ctx, &mut self.actions);
                            ui.close();
                        }
                    }
                });
            });
            ui.menu_button(format!("{}  {}", icon::PAINT_BUCKET, tr("Highlight rows with this value")), |ui| {
                let text = (!value.is_null()).then(|| value.to_text().into_owned());
                for (label, rgb) in super::state::HIGHLIGHT_COLORS {
                    let swatch = egui::RichText::new("■").color(egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]));
                    if ui.button(egui::WidgetText::from(swatch)).on_hover_text(tr(label)).clicked() {
                        self.state.set_highlight(&name, text.clone(), Some(rgb));
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button(tr("Remove this highlight")).clicked() {
                    self.state.set_highlight(&name, text.clone(), None);
                    ui.close();
                }
                if !self.state.highlights.is_empty() && ui.button(tr("Remove all highlights")).clicked() {
                    self.state.highlights.clear();
                    ui.close();
                }
            });
            let has_filter = self.model.requested_view_spec().filter.is_some();
            if has_filter && item(ui, icon::FUNNEL_X, tr("Clear client filter"), None).clicked() {
                crate::grid::apply_client_filter(self.model, None, &self.ctx);
                ui.close();
            }
            self.column_items(ui, col);
            ui.separator();
            // --- navigation / values
            if let Some(i) = self.model.foreign_key_for_column(col) {
                let target = self.model.foreign_keys()[i].ref_table.display_path();
                if item(ui, icon::ARROW_SQUARE_OUT, &trf("Navigate to {}", &[&target]), None).on_hover_text(tr("Cmd+click a link cell to navigate")).clicked() {
                    self.navigate_fk(row, col);
                    ui.close();
                }
            }
            if item(ui, icon::EYE, tr("View value"), Some(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))).clicked() {
                self.actions.push(GridAction::OpenValuePanel { row, col });
                ui.close();
            }
            ui.menu_button(format!("{}  {}", icon::FILE_SQL, tr("Generate SQL")), |ui| {
                for (kind, label) in [(SqlKind::Select, "SELECT"), (SqlKind::Insert, "INSERT"), (SqlKind::Update, "UPDATE"), (SqlKind::Delete, "DELETE")] {
                    if ui.button(label).clicked() {
                        self.generate_sql(kind);
                        ui.close();
                    }
                }
            });
        }
        ui.menu_button(format!("{}  {}", icon::FUNCTION, tr("Virtual columns")), |ui| {
            if ui.button(tr("Add virtual column…")).clicked() {
                self.state.rt.virtual_dialog = Some(VirtualColumnDialog::default());
                ui.close();
            }
            if let Some((_, col)) = active
                && self.model.is_virtual_column(col)
            {
                let idx = col - self.model.base_column_count();
                if ui.button(tr("Edit virtual column…")).clicked() {
                    let v = &self.model.virtual_columns()[idx];
                    self.state.rt.virtual_dialog =
                        Some(VirtualColumnDialog { name: v.name.clone(), expression: v.expression.source().to_string(), error: None, editing: Some(idx) });
                    ui.close();
                }
                if ui.button(tr("Remove virtual column")).clicked() {
                    self.model.remove_virtual_column(idx);
                    ui.close();
                }
            }
        });
        ui.menu_button(format!("{}  {}", icon::MAGNIFYING_GLASS_PLUS, tr("Zoom")), |ui| {
            if ui.button(tr("Zoom in")).clicked() {
                self.state.zoom_by(Some(0.1));
            }
            if ui.button(tr("Zoom out")).clicked() {
                self.state.zoom_by(Some(-0.1));
            }
            if ui.button(tr("Reset zoom")).clicked() {
                self.state.zoom_by(None);
                ui.close();
            }
        });
        ui.separator();
        if item(ui, icon::EXPORT, tr("Export selection…"), None).clicked() {
            let rows = self.state.rt.selection.rows(self.geo.nrows, usize::MAX);
            let columns = self.state.selected_columns();
            let all = self.state.rt.selection.is_all();
            self.actions.push(GridAction::Export { rows: (!all).then_some(rows), columns });
            ui.close();
        }
        if item(ui, icon::ARROWS_CLOCKWISE, tr("Refresh"), Some(KeyboardShortcut::new(cmd, Key::R))).clicked() {
            self.actions.push(GridAction::Refresh);
            ui.close();
        }
    }

    /// Column related items (ordering, visibility, size, format).
    fn column_items(&mut self, ui: &mut Ui, col: usize) {
        ui.menu_button(format!("{}  {}", icon::SORT_ASCENDING, tr("Order by")), |ui| {
            let name = self.model.column_name(col).to_string();
            if ui.button(format!("{name} ASC")).clicked() {
                apply_sort(self.model, self.state, self.options, vec![SortKey::asc(col)], &self.ctx, &mut self.actions);
                ui.close();
            }
            if ui.button(format!("{name} DESC")).clicked() {
                apply_sort(self.model, self.state, self.options, vec![SortKey::desc(col)], &self.ctx, &mut self.actions);
                ui.close();
            }
            if !self.state.sort.is_empty() {
                if ui.button(tr("Add as secondary ASC")).clicked() {
                    let mut keys = self.state.sort.clone();
                    keys.retain(|k| k.column != col);
                    keys.push(SortKey::asc(col));
                    apply_sort(self.model, self.state, self.options, keys, &self.ctx, &mut self.actions);
                    ui.close();
                }
                if ui.button(tr("Clear sorting")).clicked() {
                    apply_sort(self.model, self.state, self.options, Vec::new(), &self.ctx, &mut self.actions);
                    ui.close();
                }
            }
        });
        if item(ui, icon::EYE_SLASH, tr("Hide column"), None).clicked() {
            self.state.set_column_visible(col, false);
            ui.close();
        }
        ui.menu_button(format!("{}  {}", icon::COLUMNS, tr("Columns")), |ui| {
            if ui.button(tr("Show all")).clicked() {
                self.state.show_all_columns();
            }
            ui.separator();
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for i in 0..self.state.order.len() {
                    let c = self.state.order[i];
                    let mut visible = self.state.columns.get(c).is_some_and(|s| s.visible);
                    if ui.checkbox(&mut visible, self.model.column_name(c)).changed() {
                        self.state.set_column_visible(c, visible);
                    }
                }
            });
        });
        if item(ui, icon::ARROWS_OUT_LINE_HORIZONTAL, tr("Auto-size column"), None).clicked() {
            let ctx = self.ctx.clone();
            let painter = egui::Painter::new(ctx, ui.layer_id(), ui.clip_rect());
            self.autosize_with(&painter, col);
            ui.close();
        }
        ui.menu_button(format!("{}  {}", icon::COLUMNS_PLUS_RIGHT, tr("Column layout")), |ui| {
            let cmd_alt = Modifiers::COMMAND | Modifiers::ALT;
            if item(ui, icon::ARROW_LEFT, tr("Move column left"), Some(KeyboardShortcut::new(cmd_alt, Key::ArrowLeft))).clicked() {
                self.move_active_column(-1);
                ui.close();
            }
            if item(ui, icon::ARROW_RIGHT, tr("Move column right"), Some(KeyboardShortcut::new(cmd_alt, Key::ArrowRight))).clicked() {
                self.move_active_column(1);
                ui.close();
            }
            ui.separator();
            if item(ui, icon::ARROWS_OUT_LINE_HORIZONTAL, tr("Fit columns to values"), None).clicked() {
                let ctx = self.ctx.clone();
                let painter = egui::Painter::new(ctx, ui.layer_id(), ui.clip_rect());
                self.fit_columns_to_values(&painter);
                ui.close();
            }
            if item(ui, icon::ARROWS_HORIZONTAL, tr("Fit columns to screen"), None).clicked() {
                self.fit_columns_to_screen();
                ui.close();
            }
            if item(ui, icon::EYE_SLASH, tr("Hide columns without data"), None).clicked() {
                self.hide_empty_columns();
                ui.close();
            }
        });
        ui.menu_button(format!("{}  {}", icon::TEXT_AA, tr("Format")), |ui| {
            let mut f = self.state.column_format(col).cloned().unwrap_or_default();
            let mut changed = false;
            if self.model.column_kind(col).is_numeric() || self.model.is_virtual_column(col) {
                ui.label(tr("Decimals"));
                ui.horizontal(|ui| {
                    if ui.selectable_label(f.decimals.is_none(), tr("Auto")).clicked() {
                        f.decimals = None;
                        changed = true;
                    }
                    for d in 0..=4usize {
                        if ui.selectable_label(f.decimals == Some(d), d.to_string()).clicked() {
                            f.decimals = Some(d);
                            changed = true;
                        }
                    }
                });
                let mut grouped = f.thousands_separator.unwrap_or(self.options.format.thousands_separator);
                if ui.checkbox(&mut grouped, tr("Thousands separator")).changed() {
                    f.thousands_separator = Some(grouped);
                    changed = true;
                }
            }
            if self.model.column_kind(col).is_temporal() {
                ui.label(tr("Date pattern"));
                for p in ["%Y-%m-%d", "%d.%m.%Y", "%d/%m/%Y", "%m/%d/%Y", "%e %b %Y"] {
                    if ui.selectable_label(f.date_format.as_deref() == Some(p), p).clicked() {
                        f.date_format = Some(p.to_string());
                        changed = true;
                    }
                }
                ui.label(tr("Timestamp pattern"));
                for p in ["%Y-%m-%d %H:%M:%S", "%d.%m.%Y %H:%M", "%Y-%m-%dT%H:%M:%S%.3f"] {
                    if ui.selectable_label(f.datetime_format.as_deref() == Some(p), p).clicked() {
                        f.datetime_format = Some(p.to_string());
                        changed = true;
                    }
                }
            }
            ui.separator();
            if ui.button(tr("Reset format")).clicked() {
                f = ColumnFormat::default();
                changed = true;
            }
            if changed {
                self.state.set_column_format(col, Some(f));
            }
        });
    }

    /// Auto-size `col` using a painter for measurement.
    pub fn autosize_with(&mut self, painter: &egui::Painter, col: usize) {
        let header = self.model.column_name(col).to_string();
        let mut w = self.measure(painter, &header) + 2.0 * super::frame::PAD + super::input::SORT_ZONE + super::input::MENU_ZONE + 4.0;
        let rows = self.geo.visible_rows(self.state.rt.scroll_y);
        let fmt = self.options.format.clone();
        let prepared = crate::format::PreparedFormat::new(&fmt, self.state.column_format(col));
        let mut texts = Vec::new();
        self.model.visit_rows(rows, |_, cells| {
            let v = cells.value_of(col).unwrap_or_default();
            texts.push(if v.is_null() { fmt.null_text.clone() } else { prepared.format(&v).into_owned() });
        });
        for t in &texts {
            w = w.max(self.measure(painter, t) + 2.0 * super::frame::PAD + 2.0);
        }
        self.state.set_column_width(col, w.clamp(super::state::MIN_COLUMN_WIDTH, super::state::MAX_AUTO_WIDTH));
    }

    /// Generate SQL for the selected rows and show it.
    pub fn generate_sql(&mut self, kind: SqlKind) {
        let rows = self.state.rt.selection.rows(self.geo.nrows, 10_000);
        if rows.is_empty() {
            return;
        }
        let generic = GenericDialect::new();
        let dialect: &dyn Dialect = self.options.dialect.as_deref().unwrap_or(&generic);
        let table = sql_table_name(self.model, self.options);
        let sql = self.model.generate_rows_sql(kind, &rows, dialect, &table);
        self.state.rt.sql_window = Some((trf("Generated SQL ({} rows)", &[&rows.len()]), sql.clone()));
        self.actions.push(GridAction::GeneratedSql { kind, sql });
    }
}
