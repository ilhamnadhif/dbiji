//! Semantic diagnostics for the editor (FR-S6): unknown tables and unknown
//! qualified columns, checked against the metadata catalog.
//!
//! The checks are deliberately conservative — a false warning is worse than
//! a missed one: tables are only reported when their schema is loaded, CTEs,
//! derived tables, table functions, temporary tables (`#t`), system objects
//! and objects created earlier in the script are never reported, and columns
//! are only checked for real tables whose column list is loaded.

use crate::analyze::{ColRef, Model, RefRole, SourceKind, build_model};
use crate::catalog::CompletionCatalog;
use crate::lexicon::ident_eq;
use crate::split::{SplitOptions, split_tokens};
use crate::token::{Token, tokenize};
use dbiji_core::{SqlSyntax, trf};
use serde::{Deserialize, Serialize};

/// Severity of a [`Diagnostic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Severity {
    /// The statement will fail.
    Error,
    /// The statement is probably wrong.
    Warning,
    /// Informational hint.
    Info,
}

/// A problem found in the SQL text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Byte range of the offending name.
    pub start: usize,
    /// End of the range (exclusive).
    pub end: usize,
    /// How serious the problem is.
    pub severity: Severity,
    /// Localised message.
    pub message: String,
}

const SYSTEM_SCHEMAS: &[&str] = &[
    "information_schema",
    "pg_catalog",
    "pg_toast",
    "sys",
    "mysql",
    "performance_schema",
    "sqlite_temp",
    "temp",
];
const SYSTEM_TABLE_PREFIXES: &[&str] = &["pg_", "sqlite_", "sys", "information_schema"];
const SYSTEM_TABLES: &[&str] = &["dual"];
const SYSTEM_COLUMNS: &[&str] = &[
    "ctid", "xmin", "xmax", "cmin", "cmax", "tableoid", "oid", "rowid", "_rowid_", "_rowid", "oid$",
];

fn is_system_table(name: &str) -> bool {
    let l = name.to_lowercase();
    SYSTEM_TABLES.contains(&l.as_str()) || SYSTEM_TABLE_PREFIXES.iter().any(|p| l.starts_with(p))
}

/// Diagnostics for one statement (offsets relative to `stmt`).
pub fn diagnostics(
    stmt: &str,
    syntax: &SqlSyntax,
    catalog: &dyn CompletionCatalog,
) -> Vec<Diagnostic> {
    // The analysis is SQL: a shell language (MongoDB, Redis) has no tables to check.
    if syntax.shell.is_some() {
        return Vec::new();
    }
    let tokens = tokenize(stmt, syntax);
    check(stmt, &tokens, catalog, &[])
}

/// Diagnostics for a whole script (absolute offsets). Tables and views
/// created by earlier statements of the script are known.
pub fn script_diagnostics(
    src: &str,
    syntax: &SqlSyntax,
    catalog: &dyn CompletionCatalog,
    opts: &SplitOptions,
) -> Vec<Diagnostic> {
    if syntax.shell.is_some() {
        return Vec::new();
    }
    let split = split_tokens(src, syntax, opts);
    let mut created: Vec<(Option<String>, String)> = Vec::new();
    let mut out = Vec::new();
    for s in &split.statements {
        if s.span.is_client_command {
            continue;
        }
        let toks = split.tokens.get(s.tokens.clone()).unwrap_or(&[]);
        out.extend(check(src, toks, catalog, &created));
        // Remember objects this statement creates.
        let m = build_model(src, toks, None, None, s.span.end);
        for src_ref in &m.sources {
            if let SourceKind::Table { schema, name, .. } = &src_ref.kind
                && src_ref.role == RefRole::Create
            {
                created.push((schema.clone(), name.clone()));
            }
        }
        // SELECT ... INTO new_table
        let sig: Vec<&Token> = toks.iter().filter(|t| !t.is_trivia()).collect();
        for w in sig.windows(2) {
            if crate::token::is_word(w[0], src, "INTO") && w[1].kind.is_word() {
                created.push((
                    None,
                    crate::token::unquote_ident(w[1].text(src)).into_owned(),
                ));
            }
        }
    }
    out
}

fn check(
    src: &str,
    tokens: &[Token],
    catalog: &dyn CompletionCatalog,
    created: &[(Option<String>, String)],
) -> Vec<Diagnostic> {
    let end = tokens.last().map(|t| t.end).unwrap_or(0);
    let m = build_model(src, tokens, None, None, end);
    let mut out = Vec::new();
    unknown_tables(&m, catalog, created, &mut out);
    unknown_columns(&m, catalog, &mut out);
    out
}

fn table_known(catalog: &dyn CompletionCatalog, schema: Option<&str>, name: &str) -> Option<bool> {
    if !catalog.is_schema_loaded(schema) {
        return None;
    }
    if catalog.find_table(schema, name).is_some() {
        return Some(true);
    }
    if schema.is_none() {
        // Unqualified names may resolve through the search path.
        for s in catalog.schemas() {
            if catalog.is_schema_loaded(Some(&s)) && catalog.find_table(Some(&s), name).is_some() {
                return Some(true);
            }
        }
    }
    Some(false)
}

fn unknown_tables(
    m: &Model,
    catalog: &dyn CompletionCatalog,
    created: &[(Option<String>, String)],
    out: &mut Vec<Diagnostic>,
) {
    for s in &m.sources {
        if !matches!(s.role, RefRole::Source | RefRole::Target) {
            continue;
        }
        let SourceKind::Table {
            catalog: cat,
            schema,
            name,
        } = &s.kind
        else {
            continue;
        };
        if cat.is_some() || name.is_empty() || name.starts_with(['#', '@']) || is_system_table(name)
        {
            continue;
        }
        if schema
            .as_deref()
            .is_some_and(|sc| SYSTEM_SCHEMAS.iter().any(|x| ident_eq(x, sc)))
        {
            continue;
        }
        if created.iter().any(|(cs, cn)| {
            ident_eq(cn, name)
                && (cs.is_none()
                    || schema.is_none()
                    || cs
                        .as_deref()
                        .zip(schema.as_deref())
                        .is_some_and(|(a, b)| ident_eq(a, b)))
        }) {
            continue;
        }
        if m.ctes.iter().any(|c| ident_eq(&c.name, name)) {
            continue;
        }
        if table_known(catalog, schema.as_deref(), name) == Some(false) {
            let (start, end) = s
                .parts
                .last()
                .map(|p| (p.1, p.2))
                .unwrap_or((s.start, s.end));
            let message = match schema {
                Some(sc) => trf("Unknown table: {0}.{1}", &[sc, name]),
                None => trf("Unknown table: {0}", &[name]),
            };
            out.push(Diagnostic {
                start,
                end,
                severity: Severity::Warning,
                message,
            });
        }
    }
}

fn unknown_columns(m: &Model, catalog: &dyn CompletionCatalog, out: &mut Vec<Diagnostic>) {
    for c in &m.cols {
        if c.call {
            continue;
        }
        let Some((src, col)) = column_target(m, c) else {
            continue;
        };
        let Some(s) = m.sources.get(src) else {
            continue;
        };
        let SourceKind::Table {
            catalog: cat,
            schema,
            name,
        } = &s.kind
        else {
            continue;
        };
        if cat.is_some() || !s.alias_columns.is_empty() {
            continue;
        }
        let Some(cols) = catalog.columns(schema.as_deref(), name) else {
            continue;
        };
        if cols.is_empty() {
            continue; // not loaded
        }
        let (cname, start, end) = col;
        if SYSTEM_COLUMNS.iter().any(|x| ident_eq(x, cname))
            || cols.iter().any(|k| ident_eq(&k.name, cname))
        {
            continue;
        }
        out.push(Diagnostic {
            start,
            end,
            severity: Severity::Warning,
            message: trf("Unknown column {0} in {1}", &[&cname, name]),
        });
    }
}

/// The source a column reference belongs to, when it can be determined.
fn column_target<'a>(m: &Model, c: &'a ColRef) -> Option<(usize, (&'a str, usize, usize))> {
    let last = c.parts.last()?;
    let col = (last.0.as_str(), last.1, last.2);
    if let Some(s) = c.implicit {
        return Some((s, col));
    }
    match c.parts.len() {
        2 => {
            let q = &c.parts[0].0;
            m.resolve_qualifier(c.scope, q).map(|s| (s, col))
        }
        3 => {
            // schema.table.column — only when the first part is not an alias
            // (alias.composite.field) and the table is in scope.
            let (sch, tab) = (&c.parts[0].0, &c.parts[1].0);
            if m.resolve_qualifier(c.scope, sch).is_some() {
                return None;
            }
            let vis = m.visible_sources(c.scope);
            vis.into_iter().map(|(i, _)| i).find(|&i| {
                let s = &m.sources[i];
                s.alias.is_none()
                    && matches!(&s.kind, SourceKind::Table { schema: Some(ss), name, .. } if ident_eq(ss, sch) && ident_eq(name, tab))
            })
            .map(|s| (s, col))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::test_catalog::catalog;
    use crate::testutil::*;

    fn d(sql: &str) -> Vec<(String, String)> {
        diagnostics(sql, &pg(), &catalog())
            .into_iter()
            .map(|x| (sql[x.start..x.end].to_string(), x.message))
            .collect()
    }

    #[test]
    fn unknown_tables() {
        assert_eq!(
            d("SELECT * FROM usres"),
            vec![("usres".to_string(), "Unknown table: usres".to_string())]
        );
        assert!(d("SELECT * FROM users u JOIN orders o ON o.user_id = u.id").is_empty());
        assert_eq!(d("SELECT * FROM sales.nope").len(), 1);
        assert!(d("SELECT * FROM sales.invoices").is_empty());
        assert!(
            d("SELECT * FROM archive.anything").is_empty(),
            "schema not loaded"
        );
        assert!(
            d("SELECT * FROM elsewhere.anything").is_empty(),
            "unknown schema"
        );
        // unqualified names found in another loaded schema (search path)
        assert!(d("SELECT * FROM invoices").is_empty());
        assert_eq!(d("UPDATE nope SET a = 1").len(), 1);
        assert_eq!(d("INSERT INTO nope (a) VALUES (1)").len(), 1);
        assert_eq!(d("DELETE FROM nope").len(), 1);
    }

    #[test]
    fn no_false_positives() {
        for sql in [
            "WITH x AS (SELECT 1 AS a) SELECT x.a FROM x",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) SELECT * FROM t",
            "SELECT s.a FROM (SELECT 1 AS a) s",
            "SELECT * FROM generate_series(1, 3) g(n)",
            "SELECT 1 FROM dual",
            "SELECT * FROM pg_class",
            "SELECT * FROM information_schema.tables",
            "SELECT * FROM db.public.users",
            "SELECT x.y FROM users u",
            "SELECT u.ctid, u.xmin FROM users u",
            "SELECT * FROM users WHERE id IN (SELECT user_id FROM orders o WHERE o.total > 1)",
            "SELECT NEW.id",
            "DROP TABLE IF EXISTS nope",
            "ALTER TABLE nope ADD COLUMN x int",
            "CREATE TABLE brand_new (id int)",
            "SELECT \"MixedCase\".\"Id\" FROM \"MixedCase\"",
            "SELECT u.name, count(*) FROM users u GROUP BY u.name",
            "SELECT public.users.email FROM public.users",
            "SELECT q.x FROM (SELECT 1) AS q(x)",
            "",
            "SELECT",
            "SELECT u. FROM users u",
        ] {
            assert!(d(sql).is_empty(), "{sql}: {:?}", d(sql));
        }
        assert!(diagnostics("SELECT * FROM #tmp", &tsql(), &catalog()).is_empty());
    }

    #[test]
    fn unknown_columns() {
        assert_eq!(
            d("SELECT u.nme FROM users u"),
            vec![("nme".to_string(), "Unknown column nme in users".to_string())]
        );
        assert!(d("SELECT u.NAME FROM users u").is_empty());
        assert_eq!(
            d("SELECT o.total, o.totl FROM orders o WHERE o.user_id = 1").len(),
            1
        );
        assert_eq!(d("SELECT users.nme FROM users").len(), 1);
        assert_eq!(d("SELECT public.users.nme FROM public.users").len(), 1);
        assert_eq!(d("INSERT INTO users (nme) VALUES ('x')").len(), 1);
        assert_eq!(d("SELECT * FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.idd)").len(), 1);
        // unresolved qualifiers and CTE columns are not checked
        assert!(d("WITH c AS (SELECT 1 AS a) SELECT c.zzz FROM c").is_empty());
    }

    #[test]
    fn script_level() {
        let src = "CREATE TABLE tmp (a int);\nSELECT * FROM tmp;\nSELECT * INTO tmp2 FROM users;\nSELECT * FROM tmp2;\nSELECT * FROM usres;";
        let ds = script_diagnostics(src, &pg(), &catalog(), &SplitOptions::default());
        assert_eq!(ds.len(), 1, "{ds:?}");
        assert_eq!(&src[ds[0].start..ds[0].end], "usres");
        assert_eq!(ds[0].severity, Severity::Warning);
    }

    #[test]
    fn random_inputs_never_panic() {
        let cat = catalog();
        let mut rng = Rng::new(31);
        for _ in 0..300 {
            let src = random_sql(&mut rng, 30);
            for syn in all_syntaxes() {
                for x in script_diagnostics(&src, &syn, &cat, &SplitOptions::default()) {
                    assert!(x.start <= x.end && x.end <= src.len());
                }
            }
        }
    }
}
