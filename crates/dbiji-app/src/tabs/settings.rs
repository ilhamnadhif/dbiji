//! Preferences tab: every section of `settings.json`, the keymap editor,
//! toolbar customisation, connection types, AI and secret storage.

use crate::command::AppCommand;
use crate::cx::AppContext;
use crate::keys::{Action, IS_MAC, Keymap, Shortcut};
use crate::tabs::TabView;
use dbiji_config::settings::{AiEffort, AiProvider, BlankLineDelimiter, KeywordCase, NavigatorSort, ResultsLayout, SecretBackend, SortFilterMode, Theme};
use dbiji_config::{KeymapPreset, SecretsManager};
use dbiji_core::config::{ConnectionType, NavigatorView};
use dbiji_core::i18n::Locale;
use dbiji_core::value::{BinaryFormat, BoolStyle};
use dbiji_core::{Secret, tr, trf};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    General,
    Editor,
    Results,
    Execution,
    History,
    Navigator,
    Keyboard,
    Toolbar,
    ConnectionTypes,
    NetworkProfiles,
    Ai,
    Security,
    Accessibility,
}

impl Section {
    const ALL: [Section; 13] = [
        Section::General,
        Section::Editor,
        Section::Results,
        Section::Execution,
        Section::History,
        Section::Navigator,
        Section::Keyboard,
        Section::Toolbar,
        Section::ConnectionTypes,
        Section::NetworkProfiles,
        Section::Ai,
        Section::Security,
        Section::Accessibility,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::General => tr("General"),
            Section::Editor => tr("SQL editor"),
            Section::Results => tr("Result sets"),
            Section::Execution => tr("Execution"),
            Section::History => tr("Query history"),
            Section::Navigator => tr("Navigator"),
            Section::Keyboard => tr("Keyboard shortcuts"),
            Section::Toolbar => tr("Toolbar"),
            Section::ConnectionTypes => tr("Connection types"),
            Section::NetworkProfiles => tr("Network profiles"),
            Section::Ai => tr("AI assistant"),
            Section::Security => tr("Security"),
            Section::Accessibility => tr("Accessibility"),
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Section::General => icon::SLIDERS,
            Section::Editor => icon::FILE_SQL,
            Section::Results => icon::TABLE,
            Section::Execution => icon::PLAY,
            Section::History => icon::CLOCK_COUNTER_CLOCKWISE,
            Section::Navigator => icon::TREE_STRUCTURE,
            Section::Keyboard => icon::KEYBOARD,
            Section::Toolbar => icon::TOOLBOX,
            Section::ConnectionTypes => icon::TAG,
            Section::NetworkProfiles => icon::SHARE_NETWORK,
            Section::Ai => icon::SPARKLE,
            Section::Security => icon::LOCK_KEY,
            Section::Accessibility => icon::EYE,
        }
    }
}

pub struct SettingsTab {
    section: Section,
    keymap: Option<Keymap>,
    key_filter: String,
    recording: Option<Action>,
    ai_key: String,
    ai_key_loaded: Option<AiProvider>,
    ai_key_saved: bool,
    selected_type: usize,
    profile: Option<dbiji_core::config::NetworkProfile>,
    profile_ssh_password: String,
    profile_ssh_passphrase: String,
    profile_proxy_password: String,
    new_master: String,
    new_master2: String,
    old_master: String,
    message: Option<Result<String, String>>,
    /// Asking whether to reset every setting.
    confirm_reset: bool,
}

/// Every setting back to its default, except the language and the state
/// of the update checks (seen version, skipped release).
pub fn reset_settings(settings: &mut dbiji_config::settings::AppSettings) {
    let locale = settings.general.locale;
    let updates = settings.updates.clone();
    *settings = Default::default();
    settings.general.locale = locale;
    settings.updates = updates;
}

impl Default for SettingsTab {
    fn default() -> Self {
        SettingsTab {
            section: Section::General,
            keymap: None,
            key_filter: String::new(),
            recording: None,
            ai_key: String::new(),
            ai_key_loaded: None,
            ai_key_saved: false,
            selected_type: 0,
            profile: None,
            profile_ssh_password: String::new(),
            profile_ssh_passphrase: String::new(),
            profile_proxy_password: String::new(),
            new_master: String::new(),
            new_master2: String::new(),
            old_master: String::new(),
            message: None,
            confirm_reset: false,
        }
    }
}

fn row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> bool) -> bool {
    ui.label(label);
    let changed = add(ui);
    ui.end_row();
    changed
}

fn check(ui: &mut Ui, value: &mut bool, label: &str) -> bool {
    let c = ui.checkbox(value, label).changed();
    ui.end_row();
    c
}

fn grid(ui: &mut Ui, id: &str, add: impl FnOnce(&mut Ui) -> bool) -> bool {
    let mut changed = false;
    egui::Grid::new(id).num_columns(2).spacing([16.0, 8.0]).min_col_width(220.0).show(ui, |ui| {
        changed = add(ui);
    });
    changed
}

fn combo<T: PartialEq + Copy>(ui: &mut Ui, id: &str, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    let current = options.iter().find(|(v, _)| v == value).map(|(_, l)| *l).unwrap_or("");
    egui::ComboBox::from_id_salt(id).selected_text(current).show_ui(ui, |ui| {
        for (v, l) in options {
            if ui.selectable_value(value, *v, *l).changed() {
                changed = true;
            }
        }
    });
    changed
}

fn num<T: egui::emath::Numeric>(ui: &mut Ui, value: &mut T, range: std::ops::RangeInclusive<T>, suffix: &str) -> bool {
    ui.add(egui::DragValue::new(value).range(range).suffix(suffix)).changed()
}

fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(text).strong());
    ui.add_space(2.0);
}

impl SettingsTab {
    fn general(&mut self, ui: &mut Ui, cx: &mut AppContext) -> bool {
        let s = &mut cx.settings.general;
        grid(ui, "set_general", |ui| {
            let mut c = false;
            c |= row(ui, tr("Language"), |ui| {
                let mut v = s.locale;
                let ch = combo(ui, "set_locale", &mut v, &[(None, tr("System")), (Some(Locale::En), "English"), (Some(Locale::Id), "Bahasa Indonesia")]);
                s.locale = v;
                ch
            });
            c |= row(ui, tr("Theme"), |ui| combo(ui, "set_theme", &mut s.theme, &[(Theme::System, tr("System")), (Theme::Light, tr("Light")), (Theme::Dark, tr("Dark"))]));
            c |= row(ui, tr("Font size"), |ui| num(ui, &mut s.ui_font_size, 8.0..=32.0, " pt"));
            c |= row(ui, tr("Zoom"), |ui| {
                let mut pct = (s.ui_scale * 100.0).round();
                let ch = num(ui, &mut pct, 50.0..=300.0, " %");
                s.ui_scale = pct / 100.0;
                ch
            });
            ui.label("");
            c |= check(ui, &mut s.restore_session, tr("Reopen the editors of the last session on start"));
            ui.label("");
            c |= check(ui, &mut s.confirm_exit_with_unsaved, tr("Ask before quitting with unsaved changes"));
            ui.label("");
            let u = &mut cx.settings.updates;
            let mut auto = u.check_automatically == Some(true);
            if check(ui, &mut auto, tr("Check for updates automatically (once a day)")) {
                u.check_automatically = Some(auto);
                c = true;
            }
            c
        })
    }

    fn editor(&mut self, ui: &mut Ui, cx: &mut AppContext) -> bool {
        let e = &mut cx.settings.editor;
        let case_opts = [(KeywordCase::Upper, tr("UPPER")), (KeywordCase::Lower, tr("lower")), (KeywordCase::Preserve, tr("As typed"))];
        let mut c = grid(ui, "set_editor", |ui| {
            let mut c = false;
            c |= row(ui, tr("Font size"), |ui| num(ui, &mut e.font_size, 8.0..=40.0, " pt"));
            c |= row(ui, tr("Tab size"), |ui| num(ui, &mut e.tab_size, 1..=16, ""));
            c |= row(ui, tr("Statement delimiter"), |ui| ui.add(egui::TextEdit::singleline(&mut e.statement_delimiter).desired_width(60.0)).changed());
            c |= row(ui, tr("Blank line ends a statement"), |ui| {
                combo(
                    ui,
                    "set_blank",
                    &mut e.blank_line_delimiter,
                    &[(BlankLineDelimiter::Never, tr("Never")), (BlankLineDelimiter::Smart, tr("Smart")), (BlankLineDelimiter::Always, tr("Always"))],
                )
            });
            c |= row(ui, tr("Auto-save interval"), |ui| num(ui, &mut e.auto_save_interval_secs, 0..=3600, " s"));
            for (v, l) in [
                (&mut e.insert_spaces, tr("Insert spaces instead of tabs")),
                (&mut e.line_numbers, tr("Show line numbers")),
                (&mut e.folding, tr("Code folding (statements, block comments)")),
                (&mut e.mark_occurrences, tr("Highlight occurrences of the word under the cursor")),
                (&mut e.highlight_current_line, tr("Highlight the current line")),
                (&mut e.word_wrap, tr("Wrap long lines")),
                (&mut e.auto_close_quotes, tr("Close quotes automatically")),
                (&mut e.auto_close_brackets, tr("Close brackets automatically")),
                (&mut e.spell_check, tr("Check spelling in comments and strings")),
            ] {
                ui.label("");
                c |= check(ui, v, l);
            }
            c
        });
        heading(ui, tr("Autocomplete"));
        let a = &mut e.autocomplete;
        c |= grid(ui, "set_autocomplete", |ui| {
            let mut c = false;
            ui.label("");
            c |= check(ui, &mut a.enabled, tr("Enable autocomplete"));
            ui.label("");
            c |= check(ui, &mut a.auto_activate_on_letters, tr("Open automatically while typing"));
            ui.label("");
            c |= check(ui, &mut a.insert_single_proposal, tr("Insert a single proposal directly"));
            c |= row(ui, tr("Delay"), |ui| num(ui, &mut a.delay_ms, 0..=2000, " ms"));
            c |= row(ui, tr("Minimum characters"), |ui| num(ui, &mut a.min_chars, 0..=5, ""));
            c |= row(ui, tr("Maximum proposals"), |ui| num(ui, &mut a.max_items, 10..=1000, ""));
            c |= row(ui, tr("Keyword case"), |ui| combo(ui, "set_ac_case", &mut a.keyword_case, &case_opts));
            c
        });
        heading(ui, tr("Formatter"));
        let f = &mut e.format;
        c |= grid(ui, "set_format", |ui| {
            let mut c = false;
            {
                use dbiji_config::settings::FormatterKind as K;
                c |= row(ui, tr("Formatter"), |ui| {
                    combo(ui, "set_fmt_kind", &mut f.formatter, &[(K::Default, tr("Default")), (K::Compact, tr("Compact")), (K::External, tr("External command"))])
                });
                if f.formatter == K::External {
                    c |= row(ui, tr("Command"), |ui| {
                        ui.add(egui::TextEdit::singleline(&mut f.external_command).hint_text("pg_format -").desired_width(320.0))
                            .on_hover_text(tr("Gets the SQL on standard input and writes the formatted SQL to standard output"))
                            .changed()
                    });
                }
            }
            c |= row(ui, tr("Keyword case"), |ui| combo(ui, "set_fmt_case", &mut f.keyword_case, &case_opts));
            c |= row(ui, tr("Indent width"), |ui| num(ui, &mut f.indent_width, 1..=8, ""));
            c |= row(ui, tr("Lines between statements"), |ui| num(ui, &mut f.lines_between_statements, 0..=4, ""));
            c |= row(ui, tr("Maximum line length"), |ui| num(ui, &mut f.max_line_length, 40..=400, ""));
            ui.label("");
            c |= check(ui, &mut f.use_tabs, tr("Indent with tabs"));
            ui.label("");
            c |= check(ui, &mut f.comma_first, tr("Commas at the start of lines"));
            ui.label("");
            c |= check(ui, &mut f.newline_before_and_or, tr("New line before AND / OR"));
            ui.label("");
            c |= check(ui, &mut f.statement_only, tr("Without a selection, format only the statement under the cursor"));
            c
        });
        heading(ui, tr("Parameters and variables"));
        let p = &mut e.parameters;
        c |= grid(ui, "set_params", |ui| {
            let mut c = false;
            ui.label("");
            c |= check(ui, &mut p.enabled, tr("Enable parameters"));
            ui.label("");
            c |= check(ui, &mut p.named_colon, tr(":name parameters"));
            ui.label("");
            c |= check(ui, &mut p.dollar_brace, tr("${name} variables"));
            ui.label("");
            c |= check(ui, &mut p.question_mark, tr("? positional parameters"));
            ui.label("");
            c |= check(ui, &mut p.prompt_for_values, tr("Ask for values before executing"));
            c
        });
        c
    }

    fn results(&mut self, ui: &mut Ui, cx: &mut AppContext) -> bool {
        let r = &mut cx.settings.results;
        let mut c = grid(ui, "set_results", |ui| {
            let mut c = false;
            c |= row(ui, tr("Rows fetched per page"), |ui| num(ui, &mut r.fetch_size, 10..=100_000, ""));
            c |= row(ui, tr("Results position"), |ui| {
                combo(ui, "set_layout", &mut r.layout, &[(ResultsLayout::Bottom, tr("Below the editor")), (ResultsLayout::Right, tr("Right of the editor"))])
            });
            c |= row(ui, tr("Sorting and filtering"), |ui| {
                combo(ui, "set_sortmode", &mut r.sort_filter_mode, &[(SortFilterMode::Server, tr("On the server (re-query)")), (SortFilterMode::Client, tr("On fetched rows"))])
            });
            c |= row(ui, tr("Maximum result tabs"), |ui| num(ui, &mut r.max_result_tabs, 1..=100, ""));
            c |= row(ui, tr("Auto-refresh interval"), |ui| num(ui, &mut r.auto_refresh_secs, 0..=3600, " s"));
            c |= row(ui, tr("Memory per result set"), |ui| num(ui, &mut r.store_memory_limit_mb, 16..=16_384, " MB"));
            c |= row(ui, tr("Memory for all result sets"), |ui| num(ui, &mut r.global_memory_limit_mb, 64..=65_536, " MB"));
            c |= row(ui, tr("Confirm \"fetch all\" above"), |ui| num(ui, &mut r.confirm_fetch_all_above, 0..=100_000_000, tr(" rows")));
            c |= row(ui, tr("Maximum text length"), |ui| num(ui, &mut r.max_text_length, 64..=10_000_000, ""));
            ui.label("");
            c |= check(ui, &mut r.open_each_query_in_new_tab, tr("Open each query in a new result tab"));
            ui.label("");
            c |= check(ui, &mut r.show_row_numbers, tr("Show row numbers"));
            c
        });
        heading(ui, tr("Value formats"));
        let f = &mut r.format;
        c |= grid(ui, "set_valfmt", |ui| {
            let mut c = false;
            c |= row(ui, tr("NULL text"), |ui| ui.add(egui::TextEdit::singleline(&mut f.null_text).desired_width(120.0)).changed());
            c |= row(ui, tr("Date format"), |ui| ui.add(egui::TextEdit::singleline(&mut f.date_format).desired_width(160.0)).changed());
            c |= row(ui, tr("Time format"), |ui| ui.add(egui::TextEdit::singleline(&mut f.time_format).desired_width(160.0)).changed());
            c |= row(ui, tr("Timestamp format"), |ui| ui.add(egui::TextEdit::singleline(&mut f.datetime_format).desired_width(200.0)).changed());
            c |= row(ui, tr("Binary values"), |ui| {
                combo(ui, "set_bin", &mut f.binary_format, &[(BinaryFormat::Hex, tr("Hexadecimal")), (BinaryFormat::Base64, "Base64"), (BinaryFormat::Text, tr("Text"))])
            });
            c |= row(ui, tr("Booleans"), |ui| {
                combo(ui, "set_bool", &mut f.bool_style, &[(BoolStyle::TrueFalse, "true / false"), (BoolStyle::Checkbox, "☑ / ☐"), (BoolStyle::OneZero, "1 / 0")])
            });
            c |= row(ui, tr("Float decimals"), |ui| {
                let mut on = f.float_max_decimals.is_some();
                let mut n = f.float_max_decimals.unwrap_or(6);
                let mut ch = ui.checkbox(&mut on, tr("Limit")).changed();
                if on {
                    ch |= num(ui, &mut n, 0..=20, "");
                }
                f.float_max_decimals = on.then_some(n);
                ch
            });
            ui.label("");
            c |= check(ui, &mut f.show_fractional_seconds, tr("Show fractional seconds"));
            ui.label("");
            c |= check(ui, &mut f.thousands_separator, tr("Thousands separator"));
            c
        });
        c
    }

    fn execution(&mut self, ui: &mut Ui, cx: &mut AppContext) -> bool {
        let e = &mut cx.settings.execution;
        grid(ui, "set_exec", |ui| {
            let mut c = false;
            c |= row(ui, tr("Query timeout"), |ui| num(ui, &mut e.query_timeout_secs, 0..=86_400, tr(" s (0 = none)")));
            ui.label("");
            c |= check(ui, &mut e.stop_on_error, tr("Stop the script on the first error"));
            ui.label("");
            c |= check(ui, &mut e.confirm_dangerous_statements, tr("Confirm DELETE / UPDATE without WHERE, DROP and TRUNCATE"));
            ui.label("");
            c |= check(ui, &mut e.smart_commit, tr("Smart commit: switch to manual commit before data changes"));
            c
        })
    }

    fn history(&mut self, ui: &mut Ui, cx: &mut AppContext) -> bool {
        let h = &mut cx.settings.history;
        let c = grid(ui, "set_hist", |ui| {
            let mut c = false;
            ui.label("");
            c |= check(ui, &mut h.enabled, tr("Record executed queries"));
            ui.label("");
            c |= check(ui, &mut h.redact_secrets, tr("Mask passwords in recorded statements"));
            c |= row(ui, tr("Keep at most"), |ui| num(ui, &mut h.max_entries, 0..=10_000_000, tr(" entries")));
            c |= row(ui, tr("Keep for"), |ui| num(ui, &mut h.max_age_days, 0..=36_500, tr(" days")));
            c
        });
        ui.add_space(8.0);
        if let Some(store) = cx.services.history.clone()
            && ui.button((dbiji_icons::ic(icon::TRASH), tr("Clear the whole history"))).clicked()
        {
            match store.lock().clear(None) {
                Ok(n) => cx.toasts.success(trf("Deleted {} history entries", &[&n])),
                Err(e) => cx.toasts.error(tr("Cannot clear the history"), Some(e.to_string())),
            }
        }
        c
    }

    fn navigator(&mut self, ui: &mut Ui, cx: &mut AppContext) -> bool {
        let n = &mut cx.settings.navigator;
        grid(ui, "set_nav", |ui| {
            let mut c = false;
            c |= row(ui, tr("Order"), |ui| {
                combo(ui, "set_navsort", &mut n.sort, &[(NavigatorSort::Alphabetical, tr("Alphabetical")), (NavigatorSort::Natural, tr("As reported by the database"))])
            });
            c |= row(ui, tr("Default view of new connections"), |ui| {
                combo(ui, "set_navview", &mut n.default_view, &[(NavigatorView::Simple, tr("Simple")), (NavigatorView::Advanced, tr("Advanced"))])
            });
            ui.label("");
            c |= check(ui, &mut n.show_system_objects, tr("Show system schemas and objects"));
            ui.label("");
            c |= check(ui, &mut n.show_connection_details, tr("Show user@host next to connection names"));
            ui.label("");
            c |= check(ui, &mut n.expand_on_connect, tr("Expand a connection after connecting"));
            c
        })
    }

    fn keyboard(&mut self, ui: &mut Ui, cx: &mut AppContext) -> bool {
        let km = self.keymap.get_or_insert_with(|| cx.keymap.clone());
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label(tr("Preset"));
            let mut preset = km.preset();
            if combo(ui, "set_preset", &mut preset, &[(KeymapPreset::Dbiji, "DBiji"), (KeymapPreset::Dbeaver, "DBeaver")]) {
                km.set_preset(preset);
                changed = true;
            }
            if ui.button(tr("Reset all")).clicked() {
                km.reset_all();
                changed = true;
            }
            ui.add(egui::TextEdit::singleline(&mut self.key_filter).hint_text(tr("Filter…")).desired_width(180.0));
        });
        let conflicts = km.conflicts();
        if !conflicts.is_empty() {
            let color = crate::theme::warning_color(ui.visuals().dark_mode);
            for cf in conflicts.iter().take(5) {
                let names: Vec<&str> = cf.actions.iter().map(|a| crate::keys::action_label(*a)).collect();
                ui.label(RichText::new(format!("{} {} — {}", icon::WARNING, cf.shortcut.format(IS_MAC), names.join(", "))).color(color));
            }
        }
        // Recording a new shortcut: the next key press with a non-modifier key.
        if let Some(action) = self.recording {
            let ev = ui.ctx().input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key { key, pressed: true, modifiers, .. } => Some((*key, *modifiers)),
                    _ => None,
                })
            });
            if let Some((key, m)) = ev {
                if key == egui::Key::Escape && m.is_none() {
                    self.recording = None;
                } else if let Some(sc) = crate::keys::shortcut_of(key, m) {
                    km.set(action, vec![sc]);
                    self.recording = None;
                    changed = true;
                }
                ui.ctx().input_mut(|i| i.consume_key(m, key));
            }
        }
        let filter = self.key_filter.to_lowercase();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("set_keys").num_columns(4).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                ui.strong(tr("Action"));
                ui.strong(tr("Category"));
                ui.strong(tr("Shortcut"));
                ui.label("");
                ui.end_row();
                for a in Action::ALL {
                    let label = crate::keys::action_label(*a);
                    let keys: Vec<String> = km.shortcuts(*a).iter().map(|s| s.format(IS_MAC)).collect();
                    if !filter.is_empty() && !label.to_lowercase().contains(&filter) && !keys.join(" ").to_lowercase().contains(&filter) {
                        continue;
                    }
                    ui.label(label);
                    ui.weak(tr(a.category().label()));
                    if self.recording == Some(*a) {
                        ui.label(RichText::new(tr("Press a key combination… (Esc cancels)")).italics());
                    } else {
                        ui.monospace(if keys.is_empty() { "—".to_string() } else { keys.join("   ") });
                    }
                    ui.horizontal(|ui| {
                        if ui.small_button(dbiji_icons::ic(icon::RECORD)).on_hover_text(tr("Record a shortcut")).clicked() {
                            self.recording = Some(*a);
                        }
                        if ui.small_button(dbiji_icons::ic(icon::ARROW_COUNTER_CLOCKWISE)).on_hover_text(tr("Reset to default")).clicked() {
                            km.reset(*a);
                            changed = true;
                        }
                        if ui.small_button(dbiji_icons::ic(icon::X)).on_hover_text(tr("Remove the shortcut")).clicked() {
                            km.unbind(*a);
                            changed = true;
                        }
                    });
                    ui.end_row();
                }
            });
        });
        if changed {
            cx.settings.keymap = km.to_config();
        }
        let _ = Shortcut::parse;
        changed
    }

    fn toolbar(&mut self, ui: &mut Ui, cx: &mut AppContext) -> bool {
        let t = &mut cx.settings.toolbar;
        let mut changed = false;
        ui.label(tr("Choose and order the actions shown in the main toolbar."));
        let mut swap: Option<(usize, usize)> = None;
        let mut remove: Option<usize> = None;
        egui::Grid::new("set_toolbar").num_columns(3).striped(true).show(ui, |ui| {
            for (i, id) in t.items.iter().enumerate() {
                let label =
                    if id == "|" { tr("Separator").to_string() } else { Action::from_id(id).map(|a| crate::keys::action_label(a).to_string()).unwrap_or_else(|| id.clone()) };
                let mut visible = !t.hidden.contains(id);
                if ui.checkbox(&mut visible, label).changed() {
                    if visible {
                        t.hidden.retain(|h| h != id);
                    } else {
                        t.hidden.push(id.clone());
                    }
                    changed = true;
                }
                ui.horizontal(|ui| {
                    if ui.add_enabled(i > 0, egui::Button::new(dbiji_icons::ic(icon::ARROW_UP)).small()).clicked() {
                        swap = Some((i, i - 1));
                    }
                    if ui.add_enabled(i + 1 < t.items.len(), egui::Button::new(dbiji_icons::ic(icon::ARROW_DOWN)).small()).clicked() {
                        swap = Some((i, i + 1));
                    }
                    if ui.small_button(dbiji_icons::ic(icon::TRASH)).clicked() {
                        remove = Some(i);
                    }
                });
                ui.end_row();
            }
        });
        if let Some((a, b)) = swap {
            t.items.swap(a, b);
            changed = true;
        }
        if let Some(i) = remove {
            t.items.remove(i);
            changed = true;
        }
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("set_tb_add").selected_text(tr("Add…")).show_ui(ui, |ui| {
                if ui.selectable_label(false, tr("Separator")).clicked() {
                    t.items.push("|".into());
                    changed = true;
                }
                for a in Action::ALL {
                    if t.items.iter().any(|i| i == a.id()) {
                        continue;
                    }
                    if ui.selectable_label(false, crate::keys::action_label(*a)).clicked() {
                        t.items.push(a.id().to_string());
                        changed = true;
                    }
                }
            });
            if ui.button(tr("Reset")).clicked() {
                *t = Default::default();
                changed = true;
            }
        });
        changed
    }

    fn connection_types(&mut self, ui: &mut Ui, cx: &mut AppContext) -> bool {
        let types = cx.conns.connection_types();
        let mut changed = false;
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(180.0);
                for (i, t) in types.iter().enumerate() {
                    let marker = crate::theme::rgb(t.color);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("■").color(marker));
                        if ui.selectable_label(self.selected_type == i, &t.name).clicked() {
                            self.selected_type = i;
                        }
                    });
                }
                if ui.button((dbiji_icons::ic(icon::PLUS), tr("New type"))).clicked() {
                    let mut t = ConnectionType::development();
                    t.id = format!("custom-{}", uuid::Uuid::new_v4().simple());
                    t.name = tr("Custom").to_string();
                    cx.conns.store.set_connection_type(t);
                    cx.conns.dirty = true;
                    self.selected_type = types.len();
                }
            });
            ui.separator();
            let Some(mut t) = types.get(self.selected_type).cloned() else { return };
            ui.vertical(|ui| {
                let builtin = ConnectionType::builtins().iter().any(|b| b.id == t.id);
                let mut c = grid(ui, "set_ctype", |ui| {
                    let mut c = false;
                    c |= row(ui, tr("Name"), |ui| ui.add(egui::TextEdit::singleline(&mut t.name).desired_width(200.0)).changed());
                    c |= row(ui, tr("Colour"), |ui| ui.color_edit_button_srgb(&mut t.color).changed());
                    ui.label("");
                    c |= check(ui, &mut t.auto_commit, tr("Auto-commit by default"));
                    ui.label("");
                    c |= check(ui, &mut t.confirm_execute, tr("Confirm statements that change data or schema"));
                    ui.label("");
                    c |= check(ui, &mut t.confirm_data_change, tr("Confirm saving data changes"));
                    ui.label("");
                    c |= check(ui, &mut t.confirm_dangerous, tr("Confirm dangerous statements"));
                    c
                });
                ui.add_space(6.0);
                let label = if builtin { tr("Reset to default") } else { tr("Delete type") };
                if ui.button(label).clicked() {
                    let n = cx.conns.store.remove_connection_type(&t.id);
                    if n > 0 {
                        cx.toasts.info(trf("{} connection(s) now use the Development type", &[&n]));
                    }
                    cx.conns.dirty = true;
                    c = false;
                    self.selected_type = 0;
                }
                if c {
                    cx.conns.store.set_connection_type(t.clone());
                    cx.conns.dirty = true;
                    changed = true;
                }
            });
        });
        let _ = changed;
        false
    }

    fn network_profiles(&mut self, ui: &mut Ui, cx: &mut AppContext) -> bool {
        use dbiji_core::config::{NetworkProfile, ProxyConfig, SecretSlot, SshConfig};
        ui.label(tr("Reusable SSH tunnel and proxy settings; connections pick a profile on their SSH page."));
        ui.add_space(4.0);
        let profiles = cx.conns.store.network_profiles.clone();
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(200.0);
                for p in &profiles {
                    let used = cx.conns.configs().iter().filter(|c| c.network_profile.as_deref() == Some(p.id.as_str())).count();
                    let sel = self.profile.as_ref().is_some_and(|x| x.id == p.id);
                    if ui.selectable_label(sel, format!("{} {}  ({used})", icon::SHARE_NETWORK, p.name)).clicked() {
                        self.profile = Some(p.clone());
                        self.profile_ssh_password.clear();
                        self.profile_ssh_passphrase.clear();
                        self.profile_proxy_password.clear();
                    }
                }
                if ui.button((dbiji_icons::ic(icon::PLUS), tr("New profile"))).clicked() {
                    self.profile = Some(NetworkProfile {
                        id: uuid::Uuid::new_v4().to_string(),
                        name: tr("New profile").to_string(),
                        ssh: Some(SshConfig { enabled: true, ..Default::default() }),
                        proxy: None,
                    });
                }
            });
            ui.separator();
            let Some(mut profile) = self.profile.take() else {
                ui.weak(tr("Select or create a profile."));
                return;
            };
            let p = &mut profile;
            let mut deleted = false;
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(tr("Name"));
                    ui.add(egui::TextEdit::singleline(&mut p.name).desired_width(240.0));
                });
                heading(ui, "SSH");
                let ssh = p.ssh.get_or_insert_with(SshConfig::default);
                crate::dialogs::connection::ssh_form(ui, "profile_ssh", ssh, &mut self.profile_ssh_password, &mut self.profile_ssh_passphrase);
                heading(ui, tr("Proxy"));
                let proxy = p.proxy.get_or_insert_with(ProxyConfig::default);
                crate::dialogs::connection::proxy_form(ui, "profile_proxy", proxy, &mut self.profile_proxy_password);
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new(tr("Save profile")).strong()).clicked() {
                        let id = p.id.clone();
                        let ssh_save = p.ssh.as_ref().is_some_and(|s| s.enabled && s.save_credentials);
                        let proxy_save = p.proxy.as_ref().is_some_and(|s| s.enabled && s.save_credentials);
                        let pairs = [
                            (SecretSlot::SshPassword, &self.profile_ssh_password, ssh_save),
                            (SecretSlot::SshPassphrase, &self.profile_ssh_passphrase, ssh_save),
                            (SecretSlot::ProxyPassword, &self.profile_proxy_password, proxy_save),
                        ];
                        for (slot, value, save) in pairs {
                            let key = dbiji_config::profile_secret_key(&id, slot);
                            let r = if save && !value.is_empty() {
                                cx.conns.secrets.set(&key, &Secret::new(value.clone()))
                            } else if !save {
                                cx.conns.secrets.delete(&key)
                            } else {
                                Ok(())
                            };
                            if let Err(e) = r {
                                cx.toasts.error(tr("Cannot store the password securely"), Some(e.to_string()));
                            }
                        }
                        cx.conns.store.set_network_profile(p.clone());
                        cx.conns.dirty = true;
                        cx.toasts.success(tr("Profile saved"));
                    }
                    if ui.button(tr("Delete profile")).clicked() {
                        let n = cx.conns.store.remove_network_profile(&p.id);
                        for slot in [SecretSlot::SshPassword, SecretSlot::SshPassphrase, SecretSlot::ProxyPassword] {
                            let _ = cx.conns.secrets.delete(&dbiji_config::profile_secret_key(&p.id, slot));
                        }
                        cx.conns.dirty = true;
                        if n > 0 {
                            cx.toasts.info(trf("{} connection(s) now use their own network settings", &[&n]));
                        }
                        deleted = true;
                    }
                });
            });
            if !deleted {
                self.profile = Some(profile);
            }
        });
        false
    }

    fn ai(&mut self, ui: &mut Ui, cx: &mut AppContext) -> bool {
        let provider = cx.settings.ai.provider;
        if self.ai_key_loaded != Some(provider) {
            self.ai_key = String::new();
            self.ai_key_saved = cx.services.secrets.read().ai_api_key(provider.id()).ok().flatten().is_some();
            self.ai_key_loaded = Some(provider);
        }
        ui.label(tr("The assistant sends your question, and optionally schema details, to the provider you configure. Nothing is sent while it is disabled."));
        ui.add_space(4.0);
        let a = &mut cx.settings.ai;
        let mut c = grid(ui, "set_ai", |ui| {
            let mut c = false;
            ui.label("");
            c |= check(ui, &mut a.enabled, tr("Enable the AI assistant"));
            c |= row(ui, tr("Provider"), |ui| {
                combo(ui, "set_ai_prov", &mut a.provider, &[(AiProvider::Anthropic, "Anthropic Claude"), (AiProvider::OpenAiCompatible, tr("OpenAI-compatible server"))])
            });
            c |= row(ui, tr("Model"), |ui| ui.add(egui::TextEdit::singleline(&mut a.model).desired_width(240.0)).changed());
            c |= row(ui, tr("Base URL"), |ui| {
                let mut v = a.base_url.clone().unwrap_or_default();
                let ch = ui.add(egui::TextEdit::singleline(&mut v).hint_text(tr("(provider default)")).desired_width(300.0)).changed();
                a.base_url = if v.trim().is_empty() { None } else { Some(v.trim().to_string()) };
                ch
            });
            c |= row(ui, tr("Maximum answer length"), |ui| num(ui, &mut a.max_tokens, 256..=128_000, tr(" tokens")));
            c |= row(ui, tr("Effort"), |ui| {
                combo(
                    ui,
                    "set_ai_eff",
                    &mut a.effort,
                    &[
                        (AiEffort::Low, tr("Low")),
                        (AiEffort::Medium, tr("Medium")),
                        (AiEffort::High, tr("High")),
                        (AiEffort::Xhigh, tr("Very high")),
                        (AiEffort::Max, tr("Maximum")),
                    ],
                )
            });
            ui.label("");
            c |= check(ui, &mut a.include_schema_context, tr("Send table and column names as context"));
            ui.label("");
            c |= check(ui, &mut a.send_sample_data, tr("Allow sending sample rows"));
            c
        });
        heading(ui, tr("API key"));
        ui.horizontal(|ui| {
            let hint = if self.ai_key_saved { tr("(saved — enter a new key to replace it)") } else { tr("(not set)") };
            ui.add(egui::TextEdit::singleline(&mut self.ai_key).password(true).hint_text(hint).desired_width(300.0));
            if ui.add_enabled(!self.ai_key.trim().is_empty(), egui::Button::new(tr("Save key"))).clicked() {
                let key = Secret::new(self.ai_key.trim().to_string());
                match cx.services.secrets.read().set_ai_api_key(provider.id(), Some(&key)) {
                    Ok(()) => {
                        self.ai_key.clear();
                        self.ai_key_saved = true;
                        cx.toasts.success(tr("API key saved securely"));
                    }
                    Err(e) => cx.toasts.error(tr("Cannot store the API key"), Some(e.to_string())),
                }
            }
            if self.ai_key_saved && ui.button(tr("Remove key")).clicked() && cx.services.secrets.read().set_ai_api_key(provider.id(), None).is_ok() {
                self.ai_key_saved = false;
            }
        });
        c |= false;
        c
    }

    fn security(&mut self, ui: &mut Ui, cx: &mut AppContext) -> bool {
        let current = cx.services.secrets.read().backend();
        ui.label(tr("Passwords and API keys are never written to configuration files."));
        ui.add_space(4.0);
        let mut changed = grid(ui, "set_sec", |ui| {
            let mut c = false;
            ui.label(tr("Secret storage"));
            ui.label(match current {
                SecretBackend::Keychain => tr("Operating system keychain"),
                SecretBackend::EncryptedFile => tr("Encrypted file with master password"),
            });
            ui.end_row();
            c |= row(ui, tr("Lock the vault after"), |ui| num(ui, &mut cx.settings.security.auto_lock_minutes, 0..=1440, tr(" min idle (0 = never)")));
            c
        });
        ui.add_space(8.0);
        let target = match current {
            SecretBackend::Keychain => SecretBackend::EncryptedFile,
            SecretBackend::EncryptedFile => SecretBackend::Keychain,
        };
        let need_master = target == SecretBackend::EncryptedFile;
        heading(
            ui,
            match target {
                SecretBackend::EncryptedFile => tr("Move secrets to an encrypted file"),
                SecretBackend::Keychain => tr("Move secrets to the keychain"),
            },
        );
        if need_master {
            grid(ui, "set_sec_new", |ui| {
                row(ui, tr("New master password"), |ui| ui.add(egui::TextEdit::singleline(&mut self.new_master).password(true)).changed());
                row(ui, tr("Repeat password"), |ui| ui.add(egui::TextEdit::singleline(&mut self.new_master2).password(true)).changed());
                false
            });
        }
        if ui.button(tr("Move secrets")).clicked() {
            let keys = {
                let mut k = cx.conns.store.all_secret_keys();
                for p in [AiProvider::Anthropic, AiProvider::OpenAiCompatible] {
                    k.push(dbiji_config::ai_key(p.id()));
                }
                k
            };
            let result: Result<String, String> = (|| {
                let mut mgr = cx.services.secrets.write();
                if need_master {
                    if self.new_master.is_empty() || self.new_master != self.new_master2 {
                        return Err(tr("The passwords do not match.").to_string());
                    }
                    let vault = std::sync::Arc::new(dbiji_config::VaultStore::new(cx.services.paths.vault_file()));
                    if !vault.exists() {
                        vault.create(&Secret::new(self.new_master.clone())).map_err(|e| e.to_string())?;
                    } else {
                        vault.unlock(&Secret::new(self.new_master.clone())).map_err(|e| e.to_string())?;
                    }
                    let r = mgr.switch_to_vault(vault, &keys, true).map_err(|e| e.to_string())?;
                    Ok(trf("{} secret(s) moved", &[&r.migrated.len()]))
                } else {
                    let r = mgr
                        .switch_backend(SecretBackend::Keychain, std::sync::Arc::new(dbiji_config::BundledStore::new(dbiji_config::KeychainStore::new())), &keys, true)
                        .map_err(|e| e.to_string())?;
                    Ok(trf("{} secret(s) moved", &[&r.migrated.len()]))
                }
            })();
            if result.is_ok() {
                cx.settings.security.secret_backend = target;
                changed = true;
                self.new_master.clear();
                self.new_master2.clear();
            }
            self.message = Some(result);
        }
        if current == SecretBackend::EncryptedFile {
            heading(ui, tr("Change master password"));
            grid(ui, "set_sec_chg", |ui| {
                row(ui, tr("Current password"), |ui| ui.add(egui::TextEdit::singleline(&mut self.old_master).password(true)).changed());
                row(ui, tr("New master password"), |ui| ui.add(egui::TextEdit::singleline(&mut self.new_master).password(true)).changed());
                row(ui, tr("Repeat password"), |ui| ui.add(egui::TextEdit::singleline(&mut self.new_master2).password(true)).changed());
                false
            });
            if ui.button(tr("Change password")).clicked() {
                let mgr: parking_lot::RwLockReadGuard<'_, SecretsManager> = cx.services.secrets.read();
                let r = if self.new_master.is_empty() || self.new_master != self.new_master2 {
                    Err(tr("The passwords do not match.").to_string())
                } else if let Some(v) = mgr.vault() {
                    v.change_master_password(&Secret::new(self.old_master.clone()), &Secret::new(self.new_master.clone()))
                        .map(|_| tr("Master password changed").to_string())
                        .map_err(|e| e.to_string())
                } else {
                    Err(tr("The vault is not active.").to_string())
                };
                if r.is_ok() {
                    self.old_master.clear();
                    self.new_master.clear();
                    self.new_master2.clear();
                }
                self.message = Some(r);
            }
            if ui.button((dbiji_icons::ic(icon::LOCK), tr("Lock now"))).clicked()
                && let Some(v) = cx.services.secrets.read().vault()
            {
                v.lock();
            }
        }
        match &self.message {
            Some(Ok(m)) => {
                ui.label(RichText::new(m).color(crate::theme::success_color(ui.visuals().dark_mode)));
            }
            Some(Err(e)) => crate::widgets::error_box(ui, e),
            None => {}
        }
        changed
    }

    fn accessibility(&mut self, ui: &mut Ui, cx: &mut AppContext) -> bool {
        let a = &mut cx.settings.accessibility;
        grid(ui, "set_a11y", |ui| {
            let mut c = false;
            ui.label("");
            c |= check(ui, &mut a.high_contrast, tr("High contrast"));
            ui.label("");
            c |= check(ui, &mut a.reduce_motion, tr("Reduce motion"));
            ui.label("");
            c |= check(ui, &mut a.focus_indicator, tr("Always show the keyboard focus"));
            ui.label("");
            c |= check(ui, &mut cx.settings.tasks.scheduler_enabled, tr("Run scheduled tasks while DBiji is open"));
            c
        })
    }
}

impl TabView for SettingsTab {
    fn title(&self) -> String {
        tr("Settings").to_string()
    }

    fn icon(&self) -> &'static str {
        icon::GEAR
    }

    fn dedupe_key(&self) -> Option<String> {
        Some("settings".into())
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        let mut changed = false;
        egui::Panel::left("settings_sections").resizable(false).exact_size(200.0).show(ui, |ui| {
            ui.add_space(6.0);
            for s in Section::ALL {
                if ui.selectable_label(self.section == s, format!("{}  {}", s.icon(), s.label())).clicked() {
                    self.section = s;
                    self.message = None;
                    self.recording = None;
                }
            }
            ui.add_space(12.0);
            let dir = cx.services.paths.config_dir().to_path_buf();
            if ui.add(egui::Button::new((dbiji_icons::ic(icon::FOLDER_OPEN), tr("Open settings folder"))).frame(false)).on_hover_text(dir.display().to_string()).clicked() {
                let _ = open::that_detached(&dir);
            }
            if self.confirm_reset {
                ui.label(tr("Reset every setting to its default? Connections, scripts and passwords are kept."));
                ui.horizontal(|ui| {
                    if ui.button(RichText::new(tr("Reset")).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
                        reset_settings(cx.settings);
                        self.keymap = None;
                        self.confirm_reset = false;
                        self.message = Some(Ok(tr("The settings were reset to their defaults.").to_string()));
                        changed = true;
                    }
                    if ui.button(tr("Cancel")).clicked() {
                        self.confirm_reset = false;
                    }
                });
            } else if ui.button((dbiji_icons::ic(icon::ARROW_COUNTER_CLOCKWISE), tr("Reset all settings…"))).clicked() {
                self.confirm_reset = true;
            }
        });
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading(self.section.label());
            ui.add_space(4.0);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                changed = match self.section {
                    Section::General => self.general(ui, cx),
                    Section::Editor => self.editor(ui, cx),
                    Section::Results => self.results(ui, cx),
                    Section::Execution => self.execution(ui, cx),
                    Section::History => self.history(ui, cx),
                    Section::Navigator => self.navigator(ui, cx),
                    Section::Keyboard => self.keyboard(ui, cx),
                    Section::Toolbar => self.toolbar(ui, cx),
                    Section::ConnectionTypes => self.connection_types(ui, cx),
                    Section::NetworkProfiles => self.network_profiles(ui, cx),
                    Section::Ai => self.ai(ui, cx),
                    Section::Security => self.security(ui, cx),
                    Section::Accessibility => self.accessibility(ui, cx),
                };
            });
        });
        if changed {
            cx.push(AppCommand::SettingsChanged);
        }
    }

    crate::impl_tab_any!();
}

#[doc(hidden)]
impl SettingsTab {
    /// Cycle through the sections (headless smoke tests).
    pub fn next_section_for_tests(&mut self) {
        let i = Section::ALL.iter().position(|s| *s == self.section).unwrap_or(0);
        self.section = Section::ALL[(i + 1) % Section::ALL.len()];
    }
}
