//! Interactive demo of the DBiji SQL editor.
//!
//! ```sh
//! cargo run -p dbiji-editor --example editor_demo
//! ```
//!
//! Shows the editor with the built-in SQL highlighter, a fake schema for
//! completion (`users`, `orders`, `products`; try `SELECT u.` after
//! `FROM users u`), command-click hyperlinks on table names, statement
//! highlighting, demo diagnostics and spell checking, and a read-only DDL
//! viewer.

use std::collections::HashSet;
use std::ops::Range;

use dbiji_editor::{
    BasicSqlLanguage, CodeView, CompletionList, ContextAction, Diagnostic, EditorEvent,
    EditorLanguage, EditorOptions, EditorState, EditorTheme, Severity, SqlEditor, TableInfo,
    TokenStyle,
};
use eframe::egui;

/// Wraps [`BasicSqlLanguage`] and adds a toy spelling dictionary.
struct DemoLanguage {
    sql: BasicSqlLanguage,
}

impl EditorLanguage for DemoLanguage {
    fn highlight(&self, text: &str) -> Vec<(Range<usize>, TokenStyle)> {
        self.sql.highlight(text)
    }
    fn complete(&self, text: &str, cursor: usize) -> Option<CompletionList> {
        self.sql.complete(text, cursor)
    }
    fn hover(&self, text: &str, offset: usize) -> Option<String> {
        self.sql.hover(text, offset)
    }
    fn link_at(&self, text: &str, offset: usize) -> Option<Range<usize>> {
        self.sql.link_at(text, offset)
    }
    fn spell_suggestions(&self, word: &str) -> Vec<String> {
        match word.to_ascii_lowercase().as_str() {
            "teh" => vec!["the".into(), "ten".into()],
            "recieve" => vec!["receive".into()],
            "qeury" => vec!["query".into(), "queue".into()],
            _ => Vec::new(),
        }
    }
}

const SAMPLE: &str = "\
-- DBiji editor demo: try Ctrl+Space, Cmd+/, Cmd+F, Cmd+L, Alt+Up/Down, Cmd+D
-- Command-click a table name to follow the hyperlink. Teh qeury below is demo text.
SELECT u.id, u.name, count(o.id) AS order_count
FROM users u
LEFT JOIN orders o ON o.user_id = u.id
WHERE u.created_at >= '2026-01-01'
  AND u.email LIKE '%@example.com'
GROUP BY u.id, u.name
ORDER BY order_count DESC
LIMIT 50;

/* Multi-line comment
   spanning lines */
UPDATE products SET price = price * 1.05 WHERE category_id = :category;

select x frm orders;
";

const DDL: &str = "\
CREATE TABLE users (
    id         BIGSERIAL PRIMARY KEY,
    name       VARCHAR(200) NOT NULL,
    email      TEXT UNIQUE,
    created_at TIMESTAMPTZ DEFAULT now()
);

CREATE INDEX users_email_idx ON users (email);
";

struct Demo {
    editor: EditorState,
    lang: DemoLanguage,
    options: EditorOptions,
    dark: bool,
    show_viewer: bool,
    log: Vec<String>,
    dictionary: HashSet<String>,
    analyzed_version: Option<u64>,
}

impl Demo {
    fn new() -> Self {
        let tables = vec![
            TableInfo {
                name: "users".into(),
                columns: ["id", "name", "email", "created_at"]
                    .map(String::from)
                    .to_vec(),
            },
            TableInfo {
                name: "orders".into(),
                columns: ["id", "user_id", "total", "created_at"]
                    .map(String::from)
                    .to_vec(),
            },
            TableInfo {
                name: "products".into(),
                columns: ["id", "name", "price", "category_id"]
                    .map(String::from)
                    .to_vec(),
            },
        ];
        Self {
            editor: EditorState::new("demo", SAMPLE.to_owned()),
            lang: DemoLanguage {
                sql: BasicSqlLanguage::new().with_tables(tables),
            },
            options: EditorOptions::default(),
            dark: true,
            show_viewer: true,
            log: Vec::new(),
            dictionary: HashSet::new(),
            analyzed_version: None,
        }
    }

    fn log(&mut self, s: impl Into<String>) {
        self.log.push(s.into());
        if self.log.len() > 50 {
            self.log.remove(0);
        }
    }

    /// Naive statement range at the cursor (split on `;`).
    fn statement_at_cursor(&self) -> Option<Range<usize>> {
        let text = self.editor.text();
        let c = self.editor.cursor();
        let start = text[..c].rfind(';').map_or(0, |p| p + 1);
        let end = text[c..].find(';').map_or(text.len(), |p| c + p + 1);
        let s = &text[start..end];
        let lead = s.len() - s.trim_start().len();
        let trimmed = s.trim();
        (!trimmed.is_empty()).then(|| start + lead..start + lead + trimmed.len())
    }

    /// Demo "analysis": misspelled keywords become diagnostics, unknown
    /// words in comments become spelling issues.
    fn analyze(&mut self) {
        let text = self.editor.text().to_owned();
        let mut diags = Vec::new();
        let mut spell = Vec::new();
        for (range, style) in self.lang.highlight(&text) {
            let word = &text[range.clone()];
            if style == TokenStyle::Identifier {
                let fix = match word.to_ascii_lowercase().as_str() {
                    "frm" => Some("FROM"),
                    "selct" => Some("SELECT"),
                    "wher" => Some("WHERE"),
                    _ => None,
                };
                if let Some(fix) = fix {
                    diags.push(Diagnostic::new(
                        range.clone(),
                        Severity::Error,
                        format!("Unknown word \"{word}\". Did you mean {fix}?"),
                    ));
                }
            }
            if style == TokenStyle::Comment {
                let mut pos = 0;
                for w in word.split(|c: char| !c.is_alphabetic()) {
                    let start = range.start + pos;
                    pos += w.len() + 1;
                    let lw = w.to_ascii_lowercase();
                    if !w.is_empty()
                        && !self.lang.spell_suggestions(&lw).is_empty()
                        && !self.dictionary.contains(&lw)
                    {
                        spell.push(start..start + w.len());
                    }
                }
            }
        }
        if let Some(p) = text.find("select x") {
            diags.push(Diagnostic::new(
                p + 7..p + 8,
                Severity::Warning,
                "Column x is not qualified",
            ));
        }
        self.editor.set_diagnostics(diags);
        self.editor.set_spell_issues(spell);
        self.analyzed_version = Some(self.editor.version());
    }

    /// Demo formatter: upper-case keywords (one undo step).
    fn format(&mut self, ctx: &egui::Context) {
        let text = self.editor.text().to_owned();
        let mut out = text.clone();
        for (range, style) in self.lang.highlight(&text).into_iter().rev() {
            if matches!(style, TokenStyle::Keyword | TokenStyle::DataType) {
                out.replace_range(range.clone(), &text[range].to_ascii_uppercase());
            }
        }
        self.editor.set_text(ctx, out);
    }

    fn large_sample(lines: usize) -> String {
        let mut s = String::with_capacity(lines * 48);
        for i in 0..lines {
            match i % 6 {
                0 => s.push_str(&format!(
                    "SELECT id, name, email FROM users WHERE id = {i};\n"
                )),
                1 => s.push_str("-- comment line explaining the next statement\n"),
                2 => s.push_str(&format!(
                    "INSERT INTO orders (user_id, total) VALUES ({i}, {}.50);\n",
                    i % 97
                )),
                3 => {
                    s.push_str("UPDATE products SET price = price * 1.05 WHERE category_id = 7;\n")
                }
                4 => s.push_str("  /* block comment */ DELETE FROM orders WHERE total < 0;\n"),
                _ => s.push('\n'),
            }
        }
        s
    }
}

impl eframe::App for Demo {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.global_style().visuals.dark_mode != self.dark {
            ctx.set_visuals(if self.dark {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            });
        }
        let theme = EditorTheme::from_visuals(ui.visuals());
        if self.analyzed_version != Some(self.editor.version()) {
            self.analyze();
        }

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                use egui_phosphor::regular as ph;
                if ui
                    .button(format!(
                        "{} Theme",
                        if self.dark { ph::SUN } else { ph::MOON }
                    ))
                    .clicked()
                {
                    self.dark = !self.dark;
                }
                ui.checkbox(&mut self.options.word_wrap, "Word wrap");
                ui.checkbox(&mut self.options.line_numbers, "Line numbers");
                ui.checkbox(&mut self.options.auto_close_brackets, "Auto close");
                ui.checkbox(
                    &mut self.options.auto_complete_on_letters,
                    "Complete while typing",
                );
                ui.checkbox(&mut self.options.read_only, "Read only");
                ui.checkbox(&mut self.options.cursor_blink, "Blink");
                ui.checkbox(&mut self.show_viewer, "DDL viewer");
                ui.separator();
                if ui.button(ph::MINUS).on_hover_text("Smaller font").clicked() {
                    self.options.font_size = (self.options.font_size - 1.0).max(8.0);
                }
                ui.label(format!("{:.0}pt", self.options.font_size));
                if ui.button(ph::PLUS).on_hover_text("Larger font").clicked() {
                    self.options.font_size = (self.options.font_size + 1.0).min(32.0);
                }
                ui.separator();
                if ui
                    .button(format!("{} Find", ph::MAGNIFYING_GLASS))
                    .clicked()
                {
                    self.editor.open_find(false);
                }
                if ui.button("Go to line").clicked() {
                    self.editor.open_goto_line();
                }
                if ui.button("Format").clicked() {
                    self.format(&ctx);
                }
                if ui.button("Load 10k lines").clicked() {
                    self.editor.reset_text(&ctx, Self::large_sample(10_000));
                }
                if ui.button("Load 50k lines").clicked() {
                    self.editor.reset_text(&ctx, Self::large_sample(50_000));
                }
                if ui.button("Reset").clicked() {
                    self.editor.reset_text(&ctx, SAMPLE.to_owned());
                }
            });
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                let (line, col) = self.editor.line_col();
                ui.label(format!("Ln {line}, Col {col}"));
                ui.separator();
                ui.label(format!(
                    "{} lines, {} bytes",
                    self.editor.line_count(),
                    self.editor.text().len()
                ));
                ui.separator();
                ui.label(if self.editor.is_modified() {
                    "modified"
                } else {
                    "saved"
                });
                ui.separator();
                ui.label(self.log.last().cloned().unwrap_or_default());
            });
        });

        if self.show_viewer {
            egui::Panel::right("viewer")
                .default_size(360.0)
                .resizable(true)
                .show(ui, |ui| {
                    ui.strong("DDL (read-only code_view)");
                    CodeView::new(DDL, &self.lang, &theme)
                        .id_salt("ddl")
                        .show(ui);
                });
        }

        egui::CentralPanel::default().show(ui, |ui| {
            let cursor_before = self.editor.cursor();
            let out = SqlEditor::new(&mut self.editor, &self.lang, &self.options, &theme).show(ui);
            for event in out.events {
                match event {
                    EditorEvent::ContextMenu(ContextAction::Format) => self.format(&ctx),
                    EditorEvent::ContextMenu(ContextAction::AddSpellingWord(w)) => {
                        self.dictionary.insert(w.to_ascii_lowercase());
                        self.analyzed_version = None;
                        self.log(format!("added \"{w}\" to the dictionary"));
                    }
                    EditorEvent::ContextMenu(
                        a @ (ContextAction::Execute
                        | ContextAction::ExecuteScript
                        | ContextAction::ExecuteNewTab
                        | ContextAction::Explain),
                    ) => {
                        let sql = self.editor.selected_text().map(str::to_owned).or_else(|| {
                            self.statement_at_cursor()
                                .map(|r| self.editor.text()[r].to_owned())
                        });
                        self.log(format!(
                            "{}: {}",
                            a.label(),
                            sql.unwrap_or_default().lines().next().unwrap_or("")
                        ));
                    }
                    EditorEvent::LinkClicked { text, .. } => {
                        self.log(format!("open object editor for {text}"))
                    }
                    EditorEvent::CompletionAccepted(c) => {
                        self.log(format!("completed {}", c.label))
                    }
                    EditorEvent::SaveRequested => {
                        self.editor.mark_saved();
                        self.log("saved");
                    }
                    EditorEvent::CursorMoved(_)
                    | EditorEvent::FocusChanged(_)
                    | EditorEvent::ContextMenu(_) => {}
                }
            }
            if out.changed || self.editor.cursor() != cursor_before {
                let st = self.statement_at_cursor();
                self.editor.set_statement_highlight(st);
            }
        });
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 760.0])
            .with_title("DBiji SQL editor demo"),
        ..Default::default()
    };
    eframe::run_native(
        "dbiji-editor-demo",
        options,
        Box::new(|cc| {
            let mut fonts = egui::FontDefinitions::default();
            egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
            cc.egui_ctx.set_fonts(fonts);
            Ok(Box::new(Demo::new()))
        }),
    )
}
