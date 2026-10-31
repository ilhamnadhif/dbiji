//! Turns editor text into executable statements (FR-S2, FR-S3, FR-S8):
//! choose the statements for the requested scope, run client commands
//! (`@set`, `@include`, `@export`, `@echo`), substitute `:name` / `${name}`
//! parameters and collect the statements that need a confirmation.

use crate::exec::{ClientAction, StmtPlan};
use dbiji_core::dialect::SqlSyntax;
use dbiji_sql::{ClientCommand, Danger, ParamKind, ParamOptions, SplitOptions, StatementSpan};
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Which part of the editor text to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The statement under the cursor (⌘↩).
    Statement(usize),
    /// The selected text, split into statements.
    Selection(Range<usize>),
    /// The whole script (⌥X).
    Script,
    /// From the statement under the cursor to the end.
    FromCursor(usize),
}

pub struct PrepConfig<'a> {
    pub syntax: &'a SqlSyntax,
    pub split: SplitOptions,
    /// `None` disables parameter substitution.
    pub params: Option<ParamOptions>,
    /// Directory of the script, for relative `@include` paths.
    pub base_dir: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct Prepared {
    pub plans: Vec<StmtPlan>,
    /// Parameters without a value (ask the user, then prepare again).
    pub missing: Vec<(ParamKind, String)>,
    /// Statements that need a confirmation: (plan index, reason).
    pub dangers: Vec<(usize, Danger)>,
    pub has_writes: bool,
    /// Problems that prevent execution (unknown command, unreadable include).
    pub errors: Vec<String>,
    /// Variables after the run's `@set` commands.
    pub variables: BTreeMap<String, String>,
}

fn spans_for(text: &str, scope: &Scope, syntax: &SqlSyntax, split: &SplitOptions) -> Vec<StatementSpan> {
    match scope {
        Scope::Statement(cursor) => dbiji_sql::statement_at(text, (*cursor).min(text.len()), syntax, split).into_iter().collect(),
        Scope::Selection(r) => {
            let r = r.start.min(text.len())..r.end.min(text.len());
            let Some(sub) = text.get(r.clone()) else { return Vec::new() };
            dbiji_sql::split_statements(sub, syntax, split)
                .into_iter()
                .map(|mut s| {
                    s.start += r.start;
                    s.end += r.start;
                    s.outer_start += r.start;
                    s.outer_end += r.start;
                    s
                })
                .collect()
        }
        Scope::Script => dbiji_sql::split_statements(text, syntax, split),
        Scope::FromCursor(cursor) => dbiji_sql::split_statements(text, syntax, split).into_iter().filter(|s| s.end >= *cursor).collect(),
    }
}

/// Prepare the statements of `scope`. `values` holds parameter values the
/// user entered (SQL text, used verbatim) and variables set earlier.
pub fn prepare(text: &str, scope: Scope, cfg: &PrepConfig, values: &BTreeMap<String, String>) -> Prepared {
    let mut out = Prepared { variables: values.clone(), ..Default::default() };
    let spans = spans_for(text, &scope, cfg.syntax, &cfg.split);
    let mut index = 0usize;
    add_spans(text, &spans, cfg, &mut out, &mut index, 0, true);
    out.missing.dedup();
    out
}

fn add_spans(text: &str, spans: &[StatementSpan], cfg: &PrepConfig, out: &mut Prepared, index: &mut usize, depth: usize, editor_ranges: bool) {
    for span in spans {
        let stmt = span.text(text);
        if stmt.trim().is_empty() {
            continue;
        }
        let range = if editor_ranges { span.start..span.end } else { 0..0 };
        if let Some(cmd) = dbiji_sql::parse_client_command(stmt) {
            match cmd {
                Err(e) => out.errors.push(e),
                Ok(ClientCommand::Set { name, value }) => {
                    let value = substitute(&value, cfg, &out.variables, &mut Vec::new());
                    out.variables.insert(name.clone(), value.clone());
                    push(out, index, stmt, String::new(), range, false, Some(ClientAction::Set { name, value }));
                }
                Ok(ClientCommand::Echo(t)) => {
                    let t = substitute(&t, cfg, &out.variables, &mut Vec::new());
                    push(out, index, stmt, String::new(), range, false, Some(ClientAction::Echo(t)));
                }
                Ok(ClientCommand::Export { options }) => push(out, index, stmt, String::new(), range, false, Some(ClientAction::Export(options))),
                Ok(ClientCommand::Include { path }) => {
                    if depth >= 8 {
                        out.errors.push(format!("@include nested too deeply: {path}"));
                        continue;
                    }
                    let p = Path::new(&path);
                    let full = if p.is_absolute() { p.to_path_buf() } else { cfg.base_dir.clone().unwrap_or_default().join(p) };
                    match std::fs::read_to_string(&full) {
                        Ok(inner) => {
                            let inner_spans = dbiji_sql::split_statements(&inner, cfg.syntax, &cfg.split);
                            let sub = PrepConfig { syntax: cfg.syntax, split: cfg.split.clone(), params: cfg.params, base_dir: full.parent().map(Path::to_path_buf) };
                            add_spans(&inner, &inner_spans, &sub, out, index, depth + 1, false);
                        }
                        Err(e) => out.errors.push(format!("@include {}: {e}", full.display())),
                    }
                }
                Ok(ClientCommand::Other { name, .. }) => out.errors.push(format!("Unknown client command @{name}")),
            }
            continue;
        }
        let info = dbiji_sql::classify(stmt, cfg.syntax);
        let mut missing = Vec::new();
        // A shell language (MongoDB, Redis) uses `:` and `$` itself: its text
        // is sent as written, never substituted.
        let sql = if cfg.syntax.shell.is_some() { stmt.to_string() } else { substitute(stmt, cfg, &out.variables, &mut missing) };
        for m in missing {
            if !out.missing.contains(&m) {
                out.missing.push(m);
            }
        }
        if let Some(d) = info.danger {
            out.dangers.push((*index, d));
        }
        out.has_writes |= info.is_write;
        push(out, index, stmt, sql, range, info.is_write, None);
    }
}

fn push(out: &mut Prepared, index: &mut usize, original: &str, sql: String, range: Range<usize>, is_write: bool, client: Option<ClientAction>) {
    out.plans.push(StmtPlan { index: *index, sql, original: original.to_string(), range, is_write, client });
    *index += 1;
}

/// Replace parameters with their values; unknown ones are collected in `missing`
/// and left in place.
fn substitute(stmt: &str, cfg: &PrepConfig, vars: &BTreeMap<String, String>, missing: &mut Vec<(ParamKind, String)>) -> String {
    let Some(opts) = cfg.params else { return stmt.to_string() };
    let refs = dbiji_sql::find_parameters(stmt, cfg.syntax, &opts);
    if refs.is_empty() {
        return stmt.to_string();
    }
    match dbiji_sql::substitute(stmt, cfg.syntax, &opts, &|r| vars.get(&r.name).cloned()) {
        Ok(s) => s,
        Err(unresolved) => {
            for r in unresolved {
                let k = (r.kind, r.name.clone());
                if !missing.contains(&k) {
                    missing.push(k);
                }
            }
            stmt.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(syntax: &SqlSyntax) -> PrepConfig<'_> {
        PrepConfig { syntax, split: SplitOptions::default(), params: Some(ParamOptions::default()), base_dir: None }
    }

    #[test]
    fn statement_under_cursor_and_script() {
        let syntax = SqlSyntax::ansi();
        let text = "select 1;\nselect 2;\n\nupdate t set a = 1;";
        let p = prepare(text, Scope::Statement(12), &cfg(&syntax), &BTreeMap::new());
        assert_eq!(p.plans.len(), 1);
        assert_eq!(p.plans[0].sql, "select 2");
        let p = prepare(text, Scope::Script, &cfg(&syntax), &BTreeMap::new());
        assert_eq!(p.plans.len(), 3);
        assert!(p.has_writes);
        assert_eq!(p.dangers, vec![(2, Danger::UpdateWithoutWhere)]);
    }

    #[test]
    fn variables_and_parameters() {
        let syntax = SqlSyntax::ansi();
        let text = "@set lim = 10\nselect * from t where id = :id limit ${lim}";
        let p = prepare(text, Scope::Script, &cfg(&syntax), &BTreeMap::new());
        assert_eq!(p.missing, vec![(ParamKind::Named, "id".to_string())]);
        let mut v = BTreeMap::new();
        v.insert("id".to_string(), "42".to_string());
        let p = prepare(text, Scope::Script, &cfg(&syntax), &v);
        assert!(p.missing.is_empty());
        assert!(matches!(p.plans[0].client, Some(ClientAction::Set { .. })));
        assert_eq!(p.plans[1].sql, "select * from t where id = 42 limit 10");
    }

    #[test]
    fn include_reads_relative_files() {
        let dir = std::env::temp_dir().join(format!("dbiji-prep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("inc.sql"), "select 'inc';\nselect 'inc2';").unwrap();
        let syntax = SqlSyntax::ansi();
        let mut c = cfg(&syntax);
        c.base_dir = Some(dir.clone());
        let p = prepare("@include inc.sql\nselect 3;", Scope::Script, &c, &BTreeMap::new());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        let sqls: Vec<&str> = p.plans.iter().map(|p| p.sql.as_str()).collect();
        assert_eq!(sqls, vec!["select 'inc'", "select 'inc2'", "select 3"]);
    }
}
