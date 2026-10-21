//! DDL generation for SQLite.
//!
//! SQLite's `ALTER TABLE` only supports `RENAME TO`, `RENAME COLUMN`,
//! `ADD COLUMN` and `DROP COLUMN` (with restrictions). [`SqliteDdl::alter_table`]
//! uses those native statements whenever they can express the change and
//! otherwise generates the official 12 step table rebuild
//! (<https://sqlite.org/lang_altertable.html#otheralter>):
//!
//! ```text
//! PRAGMA foreign_keys = OFF            -- has no effect inside a transaction
//! BEGIN
//! CREATE TABLE "_dbiji_new_t" (...)    -- the new definition, columns that are
//!                                      -- renamed keep their old name here
//! INSERT INTO "_dbiji_new_t" (...) SELECT ... FROM "t"
//! DROP TABLE "t"
//! PRAGMA legacy_alter_table = ON       -- views still refer to the dropped "t":
//! ALTER TABLE "_dbiji_new_t" RENAME TO "t"  -- do not validate or rewrite them
//! PRAGMA legacy_alter_table = OFF
//! CREATE INDEX / CREATE TRIGGER ...    -- unchanged indexes and triggers
//! ALTER TABLE "t" RENAME COLUMN ...    -- native renames, so SQLite rewrites
//! ALTER TABLE "t" RENAME TO ...        -- views, triggers and foreign keys
//! CREATE INDEX / CREATE TRIGGER ...    -- new or modified indexes and triggers
//! PRAGMA foreign_key_check('t')        -- reports violations as rows
//! COMMIT
//! PRAGMA foreign_keys = ON
//! ```
//!
//! Rowids of tables without an `INTEGER PRIMARY KEY` are preserved, as is the
//! `AUTOINCREMENT` counter.
//!
//! Information SQLite keeps in the `CREATE TABLE` text but [`TableDef`] has no
//! field for is carried in [`TableDef::options`] (see [`options`]).

use crate::dialect::{qualified, quote_ident, string_literal};
use crate::lexer::{TokKind, significant};
use crate::schema_sql::{
    expression_identifiers, parse_create_index, parse_create_trigger, rename_identifiers,
    rewrite_create, squash_ws,
};
use dbiji_core::config::DbKind;
use dbiji_core::ddl::*;
use dbiji_core::dialect::ansi;
use dbiji_core::value::DataKind;
use std::collections::BTreeMap;

/// [`TableDef::options`] / [`ViewDef::options`] keys used by the SQLite driver.
pub mod options {
    /// `"true"` for `WITHOUT ROWID` tables.
    pub const WITHOUT_ROWID: &str = "without_rowid";
    /// `"true"` for `STRICT` tables.
    pub const STRICT: &str = "strict";
    /// Module of a virtual table (`fts5`, `rtree` ...).
    pub const VIRTUAL_MODULE: &str = "virtual_module";
    /// Module arguments of a virtual table (text inside `USING module(...)`).
    pub const VIRTUAL_ARGS: &str = "virtual_args";
    /// `generated.<column>` = `stored` | `virtual`.
    pub const GENERATED_PREFIX: &str = "generated.";
    /// ON CONFLICT algorithm of the primary key.
    pub const CONFLICT_PK: &str = "on_conflict.primary_key";
    /// `on_conflict.not_null.<column>` = algorithm.
    pub const CONFLICT_NOT_NULL_PREFIX: &str = "on_conflict.not_null.";
    /// `on_conflict.unique.<constraint name or comma separated columns>` = algorithm.
    pub const CONFLICT_UNIQUE_PREFIX: &str = "on_conflict.unique.";
    /// Explicit view column list, e.g. `(a, b)`.
    pub const VIEW_COLUMNS: &str = "column_list";
}

use options::*;

/// Prefix of the temporary table used by table rebuilds.
const REBUILD_PREFIX: &str = "_dbiji_new_";

fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn cols_eq(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| eq_ci(x, y))
}

fn contains_ci(list: &[String], name: &str) -> bool {
    list.iter().any(|c| eq_ci(c, name))
}

fn same_text(a: &str, b: &str) -> bool {
    squash_ws(a) == squash_ws(b)
}

fn opt_text_eq(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => same_text(x, y),
        _ => false,
    }
}

fn is_true(t: &TableDef, key: &str) -> bool {
    t.options.get(key).is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        )
    })
}

/// Option `<prefix><column>` looked up case-insensitively.
fn column_option<'a>(t: &'a TableDef, prefix: &str, column: &str) -> Option<&'a str> {
    t.options
        .iter()
        .find(|(k, _)| {
            k.len() > prefix.len() && k.starts_with(prefix) && eq_ci(&k[prefix.len()..], column)
        })
        .map(|(_, v)| v.as_str())
}

fn generated_stored(t: &TableDef, column: &str) -> bool {
    column_option(t, GENERATED_PREFIX, column).is_some_and(|v| eq_ci(v.trim(), "stored"))
}

/// Validated ON CONFLICT algorithm.
fn conflict_algo(v: Option<&str>) -> Option<&'static str> {
    match v?.trim().to_ascii_uppercase().as_str() {
        "ROLLBACK" => Some("ROLLBACK"),
        "ABORT" => Some("ABORT"),
        "FAIL" => Some("FAIL"),
        "IGNORE" => Some("IGNORE"),
        "REPLACE" => Some("REPLACE"),
        _ => None,
    }
}

fn unique_key_id(k: &KeyDef) -> String {
    k.name.clone().unwrap_or_else(|| k.columns.join(","))
}

fn unique_conflict(t: &TableDef, k: &KeyDef) -> Option<&'static str> {
    let id = unique_key_id(k);
    conflict_algo(column_option(t, CONFLICT_UNIQUE_PREFIX, &id))
}

fn is_virtual(t: &TableDef) -> bool {
    t.options
        .get(VIRTUAL_MODULE)
        .is_some_and(|m| !m.trim().is_empty())
}

/// A literal value or signed number (`DEFAULT` accepts these without parentheses).
fn is_literal(expr: &str) -> bool {
    let toks = significant(expr);
    match toks.as_slice() {
        [t] => matches!(
            t.kind,
            TokKind::Number
                | TokKind::String
                | TokKind::Blob
                | TokKind::QuotedIdent
                | TokKind::Word
        ),
        [sign, n] => {
            sign.kind == TokKind::Operator
                && (sign.text == "-" || sign.text == "+")
                && n.kind == TokKind::Number
        }
        _ => false,
    }
}

/// Default expression that SQLite can evaluate without context: a literal or
/// signed number, but not `CURRENT_TIME` / `CURRENT_DATE` / `CURRENT_TIMESTAMP`
/// (required for `ALTER TABLE ADD COLUMN`).
fn is_constant_default(expr: &str) -> bool {
    is_literal(expr)
        && !significant(expr)
            .iter()
            .any(|t| t.is_any_word(&["CURRENT_TIME", "CURRENT_DATE", "CURRENT_TIMESTAMP"]))
}

fn is_null_default(expr: &Option<String>) -> bool {
    expr.as_deref().is_none_or(|d| eq_ci(d.trim(), "NULL"))
}

/// True when `expr` is a single token, a signed number, a function call or
/// already parenthesised, i.e. needs no extra parentheses.
fn is_atomic_expr(expr: &str) -> bool {
    let toks = significant(expr);
    if toks.is_empty() {
        return false;
    }
    if toks.len() == 1 {
        return true;
    }
    if toks.len() == 2 && toks[0].kind == TokKind::Operator && toks[1].kind == TokKind::Number {
        return true;
    }
    // `( ... )` or `name( ... )` covering the whole expression
    let start = match (toks[0].kind, toks.get(1).map(|t| t.kind)) {
        (TokKind::LParen, _) => 0,
        (TokKind::Word | TokKind::QuotedIdent, Some(TokKind::LParen)) => 1,
        _ => return false,
    };
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate().skip(start) {
        match t.kind {
            TokKind::LParen => depth += 1,
            TokKind::RParen => {
                depth -= 1;
                if depth == 0 {
                    return i == toks.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

fn default_sql(expr: &str) -> String {
    let e = expr.trim();
    let parenthesised = is_atomic_expr(e)
        && significant(e)
            .first()
            .is_some_and(|t| t.kind == TokKind::LParen);
    if is_literal(e) || parenthesised {
        e.to_string()
    } else {
        format!("({e})")
    }
}

fn comment_sql(c: &str) -> String {
    c.replace(['\r', '\n'], " ")
}

fn q_list(cols: &[String]) -> String {
    cols.iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `INTEGER PRIMARY KEY` column (alias of the rowid), if any.
fn rowid_alias(t: &TableDef) -> Option<&str> {
    if is_true(t, WITHOUT_ROWID) || is_virtual(t) {
        return None;
    }
    let pk = t.primary_key.as_ref()?;
    if pk.columns.len() != 1 {
        return None;
    }
    let col = t
        .column(&pk.columns[0])
        .or_else(|| t.columns.iter().find(|c| eq_ci(&c.name, &pk.columns[0])))?;
    let ty = col.data_type.trim();
    (eq_ci(ty, "INTEGER") || col.auto_increment).then_some(col.name.as_str())
}

/// Column using AUTOINCREMENT (must be the single primary key column).
fn autoincrement_column(t: &TableDef) -> Option<&ColumnDef> {
    let pk = t.primary_key.as_ref()?;
    if pk.columns.len() != 1 || is_true(t, WITHOUT_ROWID) {
        return None;
    }
    t.columns
        .iter()
        .find(|c| c.auto_increment && eq_ci(&c.name, &pk.columns[0]))
}

/// A name for `rowid` not shadowed by a column of `t`.
fn rowid_name(t: &TableDef) -> Option<&'static str> {
    ["rowid", "_rowid_", "oid"]
        .into_iter()
        .find(|n| !t.columns.iter().any(|c| eq_ci(&c.name, n)))
}

fn fk_clause(fk: &ForeignKeyDef, with_columns: bool) -> String {
    let mut s = String::new();
    if let Some(n) = fk.name.as_deref().filter(|n| !n.is_empty()) {
        s.push_str(&format!("CONSTRAINT {} ", quote_ident(n)));
    }
    if with_columns {
        s.push_str(&format!("FOREIGN KEY ({}) ", q_list(&fk.columns)));
    }
    s.push_str(&format!("REFERENCES {}", quote_ident(&fk.ref_table)));
    if !fk.ref_columns.is_empty() {
        s.push_str(&format!(" ({})", q_list(&fk.ref_columns)));
    }
    if fk.on_delete != FkAction::NoAction {
        s.push_str(&format!(" ON DELETE {}", fk.on_delete.sql()));
    }
    if fk.on_update != FkAction::NoAction {
        s.push_str(&format!(" ON UPDATE {}", fk.on_update.sql()));
    }
    if fk.deferrable {
        s.push_str(" DEFERRABLE INITIALLY DEFERRED");
    }
    s
}

fn key_sql(kind: &str, k: &KeyDef, conflict: Option<&str>) -> String {
    let mut s = String::new();
    if let Some(n) = k.name.as_deref().filter(|n| !n.is_empty()) {
        s.push_str(&format!("CONSTRAINT {} ", quote_ident(n)));
    }
    s.push_str(&format!("{kind} ({})", q_list(&k.columns)));
    if let Some(c) = conflict {
        s.push_str(&format!(" ON CONFLICT {c}"));
    }
    s
}

fn check_sql(c: &CheckDef) -> String {
    match c.name.as_deref().filter(|n| !n.is_empty()) {
        Some(n) => format!(
            "CONSTRAINT {} CHECK ({})",
            quote_ident(n),
            c.expression.trim()
        ),
        None => format!("CHECK ({})", c.expression.trim()),
    }
}

/// Column definition. `pk` makes it an inline PRIMARY KEY, `fk` adds an inline
/// REFERENCES clause.
fn column_sql(
    t: &TableDef,
    c: &ColumnDef,
    pk: Option<&KeyDef>,
    fk: Option<&ForeignKeyDef>,
) -> String {
    let mut s = quote_ident(&c.name);
    let autoinc = pk.is_some() && c.auto_increment && !is_true(t, WITHOUT_ROWID);
    let ty = if autoinc {
        "INTEGER".to_string()
    } else {
        c.data_type.trim().to_string()
    };
    if !ty.is_empty() {
        s.push(' ');
        s.push_str(&ty);
    }
    if let Some(pk) = pk {
        if let Some(n) = pk.name.as_deref().filter(|n| !n.is_empty()) {
            s.push_str(&format!(" CONSTRAINT {}", quote_ident(n)));
        }
        s.push_str(" PRIMARY KEY");
        if let Some(cf) = conflict_algo(t.options.get(CONFLICT_PK).map(String::as_str)) {
            s.push_str(&format!(" ON CONFLICT {cf}"));
        }
        if autoinc {
            s.push_str(" AUTOINCREMENT");
        }
    }
    if !c.nullable {
        s.push_str(" NOT NULL");
        if let Some(cf) = conflict_algo(column_option(t, CONFLICT_NOT_NULL_PREFIX, &c.name)) {
            s.push_str(&format!(" ON CONFLICT {cf}"));
        }
    }
    if let Some(coll) = c.collation.as_deref().filter(|c| !c.trim().is_empty()) {
        s.push_str(&format!(" COLLATE {}", quote_ident(coll.trim())));
    }
    if let Some(g) = c.generated.as_deref().filter(|g| !g.trim().is_empty()) {
        let kind = if generated_stored(t, &c.name) {
            "STORED"
        } else {
            "VIRTUAL"
        };
        s.push_str(&format!(" GENERATED ALWAYS AS ({}) {kind}", g.trim()));
    } else if let Some(d) = c.default.as_deref().filter(|d| !d.trim().is_empty()) {
        s.push_str(&format!(" DEFAULT {}", default_sql(d)));
    }
    if let Some(fk) = fk {
        s.push(' ');
        s.push_str(&fk_clause(fk, false));
    }
    s
}

/// `CREATE TABLE` statement alone (no indexes / triggers).
fn create_table_sql(t: &TableDef, include_fks: bool) -> String {
    let temp =
        if t.table_kind == TableKind::Temporary && t.schema.as_deref().is_none_or(str::is_empty) {
            "TEMP "
        } else {
            ""
        };
    let target = qualified(t.schema.as_deref(), &t.name);
    if let Some(module) = t
        .options
        .get(VIRTUAL_MODULE)
        .filter(|m| !m.trim().is_empty())
    {
        let args = t
            .options
            .get(VIRTUAL_ARGS)
            .map(String::as_str)
            .unwrap_or("");
        return format!(
            "CREATE VIRTUAL TABLE {target} USING {}({args})",
            quote_ident(module.trim())
        );
    }
    let pk = t.primary_key.as_ref().filter(|k| !k.columns.is_empty());
    let inline_pk = pk.filter(|k| k.columns.len() == 1);
    let mut lines: Vec<(String, Option<String>)> = Vec::new();
    for c in &t.columns {
        let is_pk = inline_pk.filter(|k| eq_ci(&k.columns[0], &c.name));
        lines.push((
            column_sql(t, c, is_pk, None),
            c.comment.clone().filter(|c| !c.trim().is_empty()),
        ));
    }
    if let Some(k) = pk.filter(|k| k.columns.len() > 1) {
        lines.push((
            key_sql(
                "PRIMARY KEY",
                k,
                conflict_algo(t.options.get(CONFLICT_PK).map(String::as_str)),
            ),
            None,
        ));
    }
    for u in &t.unique_keys {
        lines.push((key_sql("UNIQUE", u, unique_conflict(t, u)), None));
    }
    for c in &t.checks {
        lines.push((check_sql(c), None));
    }
    if include_fks {
        for fk in &t.foreign_keys {
            lines.push((fk_clause(fk, true), None));
        }
    }
    let mut body = String::new();
    let n = lines.len();
    for (i, (line, comment)) in lines.into_iter().enumerate() {
        body.push_str("    ");
        body.push_str(&line);
        if i + 1 < n {
            body.push(',');
        }
        if let Some(c) = comment {
            body.push_str(" -- ");
            body.push_str(&comment_sql(c.trim()));
        }
        body.push('\n');
    }
    let mut opts = Vec::new();
    if is_true(t, WITHOUT_ROWID) {
        opts.push("WITHOUT ROWID");
    }
    if is_true(t, STRICT) {
        opts.push("STRICT");
    }
    let suffix = if opts.is_empty() {
        String::new()
    } else {
        format!(" {}", opts.join(", "))
    };
    format!("CREATE {temp}TABLE {target} (\n{body}){suffix}")
}

/// Indexes that are not implied by the primary key or a UNIQUE constraint.
fn secondary_indexes(t: &TableDef) -> impl Iterator<Item = &IndexDef> {
    t.indexes.iter().filter(move |i| {
        if i.primary || i.name.to_ascii_lowercase().starts_with("sqlite_autoindex_") {
            return false;
        }
        if i.unique {
            let cols: Vec<String> = i.columns.iter().map(|c| c.name.clone()).collect();
            let backs = t.unique_keys.iter().any(|u| {
                u.name.as_deref().is_some_and(|n| eq_ci(n, &i.name))
                    || (i.definition.is_none() && cols_eq(&u.columns, &cols))
            });
            if backs {
                return false;
            }
        }
        true
    })
}

/// The stored `CREATE INDEX` text still describes `idx`.
fn index_definition_matches(def: &str, idx: &IndexDef) -> bool {
    let Some(p) = parse_create_index(def) else {
        return false;
    };
    p.unique == idx.unique
        && p.columns.len() == idx.columns.len()
        && p.columns.iter().zip(&idx.columns).all(|(a, b)| {
            a.is_expression == b.is_expression
                && a.descending == b.descending
                && if a.is_expression {
                    same_text(&a.name, &b.name)
                } else {
                    eq_ci(&a.name, &b.name)
                }
        })
        && opt_text_eq(&p.predicate, &idx.predicate)
}

fn index_term_sql(c: &IndexColumn) -> String {
    let mut s = if c.is_expression {
        let e = c.name.trim();
        if is_atomic_expr(e) {
            e.to_string()
        } else {
            format!("({e})")
        }
    } else {
        quote_ident(&c.name)
    };
    if c.descending {
        s.push_str(" DESC");
    }
    s
}

fn create_index_in(schema: Option<&str>, table: &str, idx: &IndexDef) -> String {
    let name = if idx.name.trim().is_empty() {
        let cols: Vec<String> = idx
            .columns
            .iter()
            .map(|c| {
                c.name
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '_')
                    .collect()
            })
            .collect();
        format!("idx_{table}_{}", cols.join("_"))
    } else {
        idx.name.clone()
    };
    let target = qualified(schema, &name);
    if let Some(def) = idx.definition.as_deref()
        && index_definition_matches(def, idx)
        && let Some(sql) = rewrite_create(def, "INDEX", &target, Some(&quote_ident(table)), false)
    {
        return sql;
    }
    let cols = idx
        .columns
        .iter()
        .map(index_term_sql)
        .collect::<Vec<_>>()
        .join(", ");
    let mut s = format!(
        "CREATE {}INDEX {target} ON {} ({cols})",
        if idx.unique { "UNIQUE " } else { "" },
        quote_ident(table)
    );
    if let Some(p) = idx.predicate.as_deref().filter(|p| !p.trim().is_empty()) {
        s.push_str(" WHERE ");
        s.push_str(p.trim());
    }
    s
}

fn create_trigger_in(schema: Option<&str>, table: &str, trigger: &TriggerDef) -> Vec<String> {
    let target = qualified(schema, &trigger.name);
    let def = trigger.definition.trim();
    if !def.is_empty() {
        let def = ansi::strip_trailing_semicolon(def);
        if parse_create_trigger(def).is_some()
            && let Some(sql) = rewrite_create(
                def,
                "TRIGGER",
                &target,
                Some(&quote_ident(table)),
                schema.is_some(),
            )
        {
            return vec![sql];
        }
        return vec![def.to_string()];
    }
    let Some(body) = trigger.body.as_deref().filter(|b| !b.trim().is_empty()) else {
        return Vec::new();
    };
    let timing = if trigger.timing.trim().is_empty() {
        "AFTER"
    } else {
        trigger.timing.trim()
    };
    let events = if trigger.events.is_empty() {
        "INSERT".to_string()
    } else {
        trigger.events.join(" OR ")
    };
    let mut body = body.trim().to_string();
    if !body.ends_with(';') {
        body.push(';');
    }
    vec![format!(
        "CREATE TRIGGER {target} {timing} {events} ON {} FOR EACH ROW BEGIN\n{body}\nEND",
        quote_ident(table)
    )]
}

/// Single-table expression references any of `cols`.
fn expr_mentions(expr: &str, cols: &[String]) -> bool {
    expression_identifiers(expr)
        .iter()
        .any(|i| contains_ci(cols, i))
}

fn index_mentions(idx: &IndexDef, cols: &[String]) -> bool {
    idx.columns.iter().any(|c| {
        if c.is_expression {
            expr_mentions(&c.name, cols)
        } else {
            contains_ci(cols, &c.name)
        }
    }) || idx
        .predicate
        .as_deref()
        .is_some_and(|p| expr_mentions(p, cols))
}

/// Statements renaming columns of `table` (already qualified). Uses
/// temporary names when targets collide with existing names (swaps,
/// case-only renames).
fn rename_columns(table: &str, current: &[String], renames: &[(String, String)]) -> Vec<String> {
    let renames: Vec<&(String, String)> = renames.iter().filter(|(a, b)| a != b).collect();
    if renames.is_empty() {
        return Vec::new();
    }
    let collides = renames.iter().any(|(_, new)| contains_ci(current, new));
    let stmt = |a: &str, b: &str| {
        format!(
            "ALTER TABLE {table} RENAME COLUMN {} TO {}",
            quote_ident(a),
            quote_ident(b)
        )
    };
    if !collides {
        return renames.iter().map(|(a, b)| stmt(a, b)).collect();
    }
    let mut out = Vec::new();
    for (i, (old, _)) in renames.iter().enumerate() {
        out.push(stmt(old, &format!("__dbiji_rn_{i}")));
    }
    for (i, (_, new)) in renames.iter().enumerate() {
        out.push(stmt(&format!("__dbiji_rn_{i}"), new));
    }
    out
}

/// What changes between two versions of a table.
struct Plan<'a> {
    from: &'a TableDef,
    to: &'a TableDef,
    pairs: Vec<(&'a ColumnDef, &'a ColumnDef)>,
    dropped: Vec<&'a ColumnDef>,
    added: Vec<&'a ColumnDef>,
    /// Column renames old → new.
    renames: Vec<(String, String)>,
}

impl<'a> Plan<'a> {
    fn new(from: &'a TableDef, to: &'a TableDef, renamed: &[(String, String)]) -> Self {
        let (pairs, dropped, added) = ansi::match_columns(from, to, renamed);
        let renames = pairs
            .iter()
            .filter(|(f, t)| f.name != t.name)
            .map(|(f, t)| (f.name.clone(), t.name.clone()))
            .collect();
        Plan {
            from,
            to,
            pairs,
            dropped,
            added,
            renames,
        }
    }

    fn schema(&self) -> Option<&'a str> {
        self.from.schema.as_deref().filter(|s| !s.is_empty())
    }

    /// Final name of a column of `from`.
    fn final_name(&self, old: &str) -> String {
        self.renames
            .iter()
            .find(|(o, _)| eq_ci(o, old))
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| old.to_string())
    }

    fn to_has_column(&self, name: &str) -> bool {
        self.to.columns.iter().any(|c| eq_ci(&c.name, name))
    }

    /// Column `t` named in `to` refers to column `f` of `from`: by its final
    /// name, or by its old name when `to` has no column of that name (a
    /// definition that was not updated after the rename).
    fn col_matches(&self, f: &str, t: &str) -> bool {
        eq_ci(&self.final_name(f), t) || (eq_ci(f, t) && !self.to_has_column(t))
    }

    fn cols_match(&self, f: &[String], t: &[String]) -> bool {
        f.len() == t.len() && f.iter().zip(t).all(|(a, b)| self.col_matches(a, b))
    }

    /// A column name of `to`, with old names of renamed columns replaced by
    /// the new ones.
    fn current_name(&self, t: &str) -> String {
        if self.to_has_column(t) {
            return t.to_string();
        }
        self.final_name(t)
    }

    /// `to` index with stale column names fixed (see [`Plan::col_matches`]).
    fn current_index(&self, ti: &IndexDef) -> IndexDef {
        let mut i = ti.clone();
        for c in &mut i.columns {
            if !c.is_expression {
                c.name = self.current_name(&c.name);
            }
        }
        i
    }

    /// Expression of `from` still matches `to`, literally or after the renames.
    fn expr_unchanged(&self, from_expr: &str, to_expr: &str) -> bool {
        same_text(from_expr, to_expr)
            || same_text(
                &rename_identifiers(from_expr, &self.renames, &quote_ident),
                to_expr,
            )
            || same_text(
                &rename_identifiers(from_expr, &self.renames, &|s: &str| s.to_string()),
                to_expr,
            )
    }

    fn opt_expr_unchanged(&self, a: &Option<String>, b: &Option<String>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(x), Some(y)) => self.expr_unchanged(x, y),
            _ => false,
        }
    }

    fn is_self_ref(&self, fk: &ForeignKeyDef) -> bool {
        eq_ci(&fk.ref_table, &self.from.name) || eq_ci(&fk.ref_table, &self.to.name)
    }

    fn fk_same(&self, f: &ForeignKeyDef, t: &ForeignKeyDef) -> bool {
        let self_ref = self.is_self_ref(f);
        let ref_table_ok = eq_ci(&f.ref_table, &t.ref_table) || (self_ref && self.is_self_ref(t));
        let ref_cols_ok = f.ref_columns.is_empty()
            || t.ref_columns.is_empty()
            || if self_ref {
                self.cols_match(&f.ref_columns, &t.ref_columns)
            } else {
                cols_eq(&f.ref_columns, &t.ref_columns)
            };
        f.name.as_deref().unwrap_or("") == t.name.as_deref().unwrap_or("")
            && self.cols_match(&f.columns, &t.columns)
            && ref_table_ok
            && ref_cols_ok
            && f.on_update == t.on_update
            && f.on_delete == t.on_delete
            && f.deferrable == t.deferrable
    }

    fn removed_fks(&self) -> Vec<&'a ForeignKeyDef> {
        self.from
            .foreign_keys
            .iter()
            .filter(|f| !self.to.foreign_keys.iter().any(|t| self.fk_same(f, t)))
            .collect()
    }

    fn added_fks(&self) -> Vec<&'a ForeignKeyDef> {
        self.to
            .foreign_keys
            .iter()
            .filter(|t| !self.from.foreign_keys.iter().any(|f| self.fk_same(f, t)))
            .collect()
    }

    /// New single column FK that can be declared inline in `ADD COLUMN`.
    fn inline_fk_for(&self, c: &ColumnDef) -> Option<&'a ForeignKeyDef> {
        self.added_fks()
            .into_iter()
            .find(|fk| fk.columns.len() == 1 && eq_ci(&fk.columns[0], &c.name))
    }

    fn key_same(&self, f: &KeyDef, t: &KeyDef) -> bool {
        f.name.as_deref().unwrap_or("") == t.name.as_deref().unwrap_or("")
            && self.cols_match(&f.columns, &t.columns)
    }

    fn pk_changed(&self) -> bool {
        let f = self
            .from
            .primary_key
            .as_ref()
            .filter(|k| !k.columns.is_empty());
        let t = self
            .to
            .primary_key
            .as_ref()
            .filter(|k| !k.columns.is_empty());
        let keys_differ = match (f, t) {
            (None, None) => false,
            (Some(f), Some(t)) => !self.key_same(f, t),
            _ => true,
        };
        keys_differ
            || conflict_algo(self.from.options.get(CONFLICT_PK).map(String::as_str))
                != conflict_algo(self.to.options.get(CONFLICT_PK).map(String::as_str))
    }

    fn uniques_changed(&self) -> bool {
        if self.from.unique_keys.len() != self.to.unique_keys.len() {
            return true;
        }
        self.from.unique_keys.iter().any(|f| {
            !self.to.unique_keys.iter().any(|t| {
                self.key_same(f, t) && unique_conflict(self.from, f) == unique_conflict(self.to, t)
            })
        })
    }

    fn checks_changed(&self) -> bool {
        self.from.checks.len() != self.to.checks.len()
            || self.from.checks.iter().any(|f| {
                !self.to.checks.iter().any(|t| {
                    f.name.as_deref().unwrap_or("") == t.name.as_deref().unwrap_or("")
                        && self.expr_unchanged(&f.expression, &t.expression)
                })
            })
    }

    fn column_changed(&self, f: &ColumnDef, t: &ColumnDef) -> bool {
        !eq_ci(&squash_ws(&f.data_type), &squash_ws(&t.data_type))
            || f.nullable != t.nullable
            || !opt_text_eq(&f.default, &t.default)
            || f.auto_increment != t.auto_increment
            || !self.opt_expr_unchanged(&f.generated, &t.generated)
            || f.collation.as_deref().map(str::to_ascii_uppercase)
                != t.collation.as_deref().map(str::to_ascii_uppercase)
            || f.comment.as_deref().map(str::trim).unwrap_or("")
                != t.comment.as_deref().map(str::trim).unwrap_or("")
            || generated_stored(self.from, &f.name) != generated_stored(self.to, &t.name)
            || conflict_algo(column_option(self.from, CONFLICT_NOT_NULL_PREFIX, &f.name))
                != conflict_algo(column_option(self.to, CONFLICT_NOT_NULL_PREFIX, &t.name))
    }

    fn to_index(&self, c: &ColumnDef) -> usize {
        self.to
            .columns
            .iter()
            .position(|x| std::ptr::eq(x, c))
            .unwrap_or(usize::MAX)
    }

    fn order_changed(&self) -> bool {
        let positions: Vec<usize> = self.pairs.iter().map(|(_, t)| self.to_index(t)).collect();
        positions.windows(2).any(|w| w[0] > w[1])
    }

    fn options_changed(&self) -> bool {
        is_true(self.from, WITHOUT_ROWID) != is_true(self.to, WITHOUT_ROWID)
            || is_true(self.from, STRICT) != is_true(self.to, STRICT)
            || self.from.options.get(VIRTUAL_MODULE) != self.to.options.get(VIRTUAL_MODULE)
    }

    fn can_add(&self, c: &ColumnDef) -> bool {
        let last_carried = self.pairs.iter().map(|(_, t)| self.to_index(t)).max();
        if last_carried.is_some_and(|m| self.to_index(c) < m) {
            return false;
        }
        let in_pk = self
            .to
            .primary_key
            .as_ref()
            .is_some_and(|k| contains_ci(&k.columns, &c.name));
        let in_unique = self
            .to
            .unique_keys
            .iter()
            .any(|k| contains_ci(&k.columns, &c.name));
        if in_pk || in_unique || c.auto_increment {
            return false;
        }
        if c.generated.is_some() && generated_stored(self.to, &c.name) {
            return false;
        }
        if let Some(d) = &c.default
            && !is_constant_default(d)
        {
            return false;
        }
        if !c.nullable && is_null_default(&c.default) && c.generated.is_none() {
            return false;
        }
        // foreign keys: only a single-column FK on this column with a NULL default
        for fk in self.added_fks() {
            if contains_ci(&fk.columns, &c.name)
                && (fk.columns.len() != 1 || !is_null_default(&c.default))
            {
                return false;
            }
        }
        true
    }

    fn can_drop(&self, c: &ColumnDef) -> bool {
        let name = std::slice::from_ref(&c.name);
        let in_pk = self
            .from
            .primary_key
            .as_ref()
            .is_some_and(|k| contains_ci(&k.columns, &c.name));
        let in_unique = self
            .from
            .unique_keys
            .iter()
            .any(|k| contains_ci(&k.columns, &c.name));
        let in_fk = self
            .from
            .foreign_keys
            .iter()
            .any(|fk| contains_ci(&fk.columns, &c.name));
        let referenced = self
            .from
            .referenced_by
            .iter()
            .any(|fk| contains_ci(&fk.ref_columns, &c.name));
        let in_check = self
            .from
            .checks
            .iter()
            .any(|ch| expr_mentions(&ch.expression, name));
        let in_generated = self.from.columns.iter().any(|o| {
            !eq_ci(&o.name, &c.name)
                && o.generated
                    .as_deref()
                    .is_some_and(|g| expr_mentions(g, name))
        });
        let kept_index_uses =
            secondary_indexes(self.from).any(|i| self.index_kept(i) && index_mentions(i, name));
        let new_index_uses = secondary_indexes(self.to).any(|i| index_mentions(i, name));
        !(in_pk
            || in_unique
            || in_fk
            || referenced
            || in_check
            || in_generated
            || kept_index_uses
            || new_index_uses)
    }

    /// Index of `to` equal to `fi` of `from` (modulo column renames).
    fn kept_index(&self, fi: &IndexDef) -> Option<&'a IndexDef> {
        secondary_indexes(self.to).find(|ti| {
            eq_ci(&ti.name, &fi.name)
                && ti.unique == fi.unique
                && ti.columns.len() == fi.columns.len()
                && fi.columns.iter().zip(&ti.columns).all(|(a, b)| {
                    a.is_expression == b.is_expression
                        && a.descending == b.descending
                        && if a.is_expression {
                            self.expr_unchanged(&a.name, &b.name)
                        } else {
                            self.col_matches(&a.name, &b.name)
                        }
                })
                && self.opt_expr_unchanged(&fi.predicate, &ti.predicate)
        })
    }

    fn index_kept(&self, fi: &IndexDef) -> bool {
        self.kept_index(fi).is_some()
    }

    /// `to` index that has an unchanged counterpart in `from`.
    fn to_index_kept(&self, ti: &IndexDef) -> bool {
        secondary_indexes(self.from)
            .any(|fi| self.kept_index(fi).is_some_and(|k| std::ptr::eq(k, ti)))
    }

    fn trigger_kept(&self, ft: &TriggerDef) -> bool {
        self.to
            .triggers
            .iter()
            .any(|tt| eq_ci(&tt.name, &ft.name) && same_text(&tt.definition, &ft.definition))
    }

    fn to_trigger_kept(&self, tt: &TriggerDef) -> bool {
        self.from
            .triggers
            .iter()
            .any(|ft| eq_ci(&tt.name, &ft.name) && same_text(&tt.definition, &ft.definition))
    }

    fn needs_rebuild(&self) -> bool {
        self.pk_changed()
            || self.uniques_changed()
            || self.checks_changed()
            || self.options_changed()
            || self.order_changed()
            || self.pairs.iter().any(|(f, t)| self.column_changed(f, t))
            || !self.removed_fks().is_empty()
            || self.added_fks().iter().any(|fk| {
                !(fk.columns.len() == 1
                    && self.added.iter().any(|a| eq_ci(&a.name, &fk.columns[0])))
            })
            || self.added.iter().any(|c| !self.can_add(c))
            || self.dropped.iter().any(|c| !self.can_drop(c))
    }

    fn native(&self) -> Vec<String> {
        let s = self.schema();
        let old_t = qualified(s, &self.from.name);
        let mut out = Vec::new();
        for ft in &self.from.triggers {
            if !self.trigger_kept(ft) {
                out.push(format!("DROP TRIGGER {}", qualified(s, &ft.name)));
            }
        }
        for fi in secondary_indexes(self.from) {
            if !self.index_kept(fi) {
                out.push(format!("DROP INDEX {}", qualified(s, &fi.name)));
            }
        }
        for c in &self.dropped {
            out.push(format!(
                "ALTER TABLE {old_t} DROP COLUMN {}",
                quote_ident(&c.name)
            ));
        }
        let current: Vec<String> = self
            .from
            .columns
            .iter()
            .filter(|c| !self.dropped.iter().any(|d| std::ptr::eq(*d, *c)))
            .map(|c| c.name.clone())
            .collect();
        out.extend(rename_columns(&old_t, &current, &self.renames));
        if self.from.name != self.to.name {
            out.push(format!(
                "ALTER TABLE {old_t} RENAME TO {}",
                quote_ident(&self.to.name)
            ));
        }
        let new_t = qualified(s, &self.to.name);
        for c in &self.added {
            out.push(format!(
                "ALTER TABLE {new_t} ADD COLUMN {}",
                column_sql(self.to, c, None, self.inline_fk_for(c))
            ));
        }
        for ti in secondary_indexes(self.to) {
            if !self.to_index_kept(ti) {
                out.push(create_index_in(s, &self.to.name, &self.current_index(ti)));
            }
        }
        for tt in &self.to.triggers {
            if !self.to_trigger_kept(tt) {
                out.extend(create_trigger_in(s, &self.to.name, tt));
            }
        }
        if out.len() > 1 {
            out.insert(0, "BEGIN".to_string());
            out.push("COMMIT".to_string());
        }
        out
    }

    /// The table definition created by the rebuild: `to`, but with the old
    /// names of renamed columns (renamed natively afterwards so that SQLite
    /// rewrites views and triggers). Returns it with the renames to apply
    /// after the rebuild.
    fn rebuild_definition(&self) -> (TableDef, Vec<(String, String)>) {
        let carried_old: Vec<String> = self.pairs.iter().map(|(f, _)| f.name.clone()).collect();
        let mut final_to_mid: Vec<(String, String)> = self
            .pairs
            .iter()
            .map(|(f, t)| (t.name.clone(), f.name.clone()))
            .collect();
        let mut post_renames = self.renames.clone();
        for (i, a) in self.added.iter().enumerate() {
            if contains_ci(&carried_old, &a.name) {
                let tmp = format!("__dbiji_add_{i}");
                final_to_mid.push((a.name.clone(), tmp.clone()));
                post_renames.push((tmp, a.name.clone()));
            }
        }
        let mid_name = |n: &str| -> String {
            final_to_mid
                .iter()
                .find(|(f, _)| eq_ci(f, n))
                .map(|(_, m)| m.clone())
                .unwrap_or_else(|| n.to_string())
        };
        let map_list =
            |cols: &[String]| -> Vec<String> { cols.iter().map(|c| mid_name(c)).collect() };
        let to_mid_expr = |e: &str| rename_identifiers(e, &final_to_mid, &quote_ident);

        let mut mid = TableDef::new(
            self.schema().map(str::to_string),
            format!("{REBUILD_PREFIX}{}", self.from.name),
        );
        mid.table_kind = self.to.table_kind;
        for tc in &self.to.columns {
            let mut c = tc.clone();
            c.name = mid_name(&tc.name);
            if let Some(g) = &tc.generated {
                let from_g = self
                    .pairs
                    .iter()
                    .find(|(_, t)| std::ptr::eq(*t, tc))
                    .and_then(|(f, _)| f.generated.clone());
                c.generated = Some(match from_g {
                    Some(fg) if self.expr_unchanged(&fg, g) => fg,
                    _ => to_mid_expr(g),
                });
            }
            mid.columns.push(c);
        }
        mid.primary_key = self.to.primary_key.as_ref().map(|k| KeyDef {
            name: k.name.clone(),
            columns: map_list(&k.columns),
        });
        mid.unique_keys = self
            .to
            .unique_keys
            .iter()
            .map(|k| KeyDef {
                name: k.name.clone(),
                columns: map_list(&k.columns),
            })
            .collect();
        mid.checks = self
            .to
            .checks
            .iter()
            .map(|t| {
                let kept = self.from.checks.iter().find(|f| {
                    f.name.as_deref().unwrap_or("") == t.name.as_deref().unwrap_or("")
                        && self.expr_unchanged(&f.expression, &t.expression)
                });
                CheckDef {
                    name: t.name.clone(),
                    expression: kept
                        .map(|f| f.expression.clone())
                        .unwrap_or_else(|| to_mid_expr(&t.expression)),
                }
            })
            .collect();
        mid.foreign_keys = self
            .to
            .foreign_keys
            .iter()
            .map(|fk| {
                let mut m = fk.clone();
                m.columns = map_list(&fk.columns);
                if self.is_self_ref(fk) {
                    m.ref_table = self.from.name.clone();
                    m.ref_columns = map_list(&fk.ref_columns);
                }
                m
            })
            .collect();
        let mut opts = BTreeMap::new();
        for (k, v) in &self.to.options {
            let mapped = if let Some(col) = k.strip_prefix(GENERATED_PREFIX) {
                format!("{GENERATED_PREFIX}{}", mid_name(col))
            } else if let Some(col) = k.strip_prefix(CONFLICT_NOT_NULL_PREFIX) {
                format!("{CONFLICT_NOT_NULL_PREFIX}{}", mid_name(col))
            } else if let Some(id) = k.strip_prefix(CONFLICT_UNIQUE_PREFIX) {
                let named = self
                    .to
                    .unique_keys
                    .iter()
                    .any(|u| u.name.as_deref() == Some(id));
                if named {
                    k.clone()
                } else {
                    let cols: Vec<String> = id.split(',').map(|c| mid_name(c.trim())).collect();
                    format!("{CONFLICT_UNIQUE_PREFIX}{}", cols.join(","))
                }
            } else {
                k.clone()
            };
            opts.insert(mapped, v.clone());
        }
        mid.options = opts;
        (mid, post_renames)
    }

    fn rebuild(&self) -> Vec<String> {
        let s = self.schema();
        let old_t = qualified(s, &self.from.name);
        let (mid, post_renames) = self.rebuild_definition();
        let tmp_t = qualified(s, &mid.name);
        let mut out = vec![
            "PRAGMA foreign_keys = OFF".to_string(),
            "BEGIN".to_string(),
            create_table_sql(&mid, true),
        ];

        // copy the data
        let mut targets = Vec::new();
        let mut sources = Vec::new();
        // keep the rowids unless the new table has an INTEGER PRIMARY KEY (then
        // the rowid is that column, which is copied anyway)
        let keep_rowid = rowid_alias(&mid).is_none()
            && !is_true(self.from, WITHOUT_ROWID)
            && !is_true(&mid, WITHOUT_ROWID)
            && !is_virtual(self.from);
        if keep_rowid && let (Some(src), Some(dst)) = (rowid_name(self.from), rowid_name(&mid)) {
            sources.push(src.to_string());
            targets.push(dst.to_string());
        }
        for (f, t) in &self.pairs {
            if t.generated.as_deref().is_some_and(|g| !g.trim().is_empty()) {
                continue;
            }
            sources.push(quote_ident(&f.name));
            targets.push(quote_ident(&f.name));
        }
        if !self.pairs.is_empty() && !targets.is_empty() {
            out.push(format!(
                "INSERT INTO {tmp_t} ({}) SELECT {} FROM {old_t}",
                targets.join(", "),
                sources.join(", ")
            ));
        }
        if autoincrement_column(&mid).is_some() {
            let seq = qualified(s, "sqlite_sequence");
            out.push(format!(
                "DELETE FROM {seq} WHERE name = {}",
                string_literal(&mid.name)
            ));
            out.push(format!(
                "INSERT INTO {seq} (name, seq) SELECT {}, seq FROM {seq} WHERE name = {}",
                string_literal(&mid.name),
                string_literal(&self.from.name)
            ));
        }
        out.push(format!("DROP TABLE {old_t}"));
        // Legacy mode: the rename must neither validate nor rewrite views and
        // triggers, which still refer to the dropped table at this point.
        out.push("PRAGMA legacy_alter_table = ON".to_string());
        out.push(format!(
            "ALTER TABLE {tmp_t} RENAME TO {}",
            quote_ident(&self.from.name)
        ));
        out.push("PRAGMA legacy_alter_table = OFF".to_string());

        // unchanged indexes and triggers, in their old form
        let dropped: Vec<String> = self.dropped.iter().map(|c| c.name.clone()).collect();
        for fi in secondary_indexes(self.from) {
            if self.index_kept(fi) && !index_mentions(fi, &dropped) {
                out.push(create_index_in(s, &self.from.name, fi));
            }
        }
        for ft in &self.from.triggers {
            if self.trigger_kept(ft) {
                out.extend(create_trigger_in(s, &self.from.name, ft));
            }
        }

        // renames, natively so that SQLite updates every reference
        let mid_names: Vec<String> = mid.columns.iter().map(|c| c.name.clone()).collect();
        out.extend(rename_columns(&old_t, &mid_names, &post_renames));
        if self.from.name != self.to.name {
            out.push(format!(
                "ALTER TABLE {old_t} RENAME TO {}",
                quote_ident(&self.to.name)
            ));
        }

        // new or modified indexes and triggers
        for ti in secondary_indexes(self.to) {
            if !self.to_index_kept(ti) {
                out.push(create_index_in(s, &self.to.name, &self.current_index(ti)));
            }
        }
        for tt in &self.to.triggers {
            if !self.to_trigger_kept(tt) {
                out.extend(create_trigger_in(s, &self.to.name, tt));
            }
        }

        let pragma = match s {
            Some(s) => format!("PRAGMA {}.foreign_key_check", quote_ident(s)),
            None => "PRAGMA foreign_key_check".to_string(),
        };
        let mut checked: Vec<String> = vec![self.to.name.clone()];
        out.push(format!("{pragma}({})", string_literal(&self.to.name)));
        for fk in &self.from.referenced_by {
            let child = if eq_ci(&fk.table, &self.from.name) {
                self.to.name.clone()
            } else {
                fk.table.clone()
            };
            if !child.is_empty() && !contains_ci(&checked, &child) {
                out.push(format!("{pragma}({})", string_literal(&child)));
                checked.push(child);
            }
        }
        out.push("COMMIT".to_string());
        out.push("PRAGMA foreign_keys = ON".to_string());
        out
    }
}

/// `table` with `column` removed together with the keys, indexes, foreign
/// keys and checks that depend on it.
fn without_column(table: &TableDef, column: &str) -> TableDef {
    let col = vec![column.to_string()];
    let mut t = table.clone();
    t.columns.retain(|c| !eq_ci(&c.name, column));
    if t.primary_key
        .as_ref()
        .is_some_and(|k| contains_ci(&k.columns, column))
    {
        t.primary_key = None;
    }
    t.unique_keys.retain(|k| !contains_ci(&k.columns, column));
    t.foreign_keys
        .retain(|fk| !contains_ci(&fk.columns, column));
    t.checks.retain(|c| !expr_mentions(&c.expression, &col));
    t.indexes.retain(|i| !index_mentions(i, &col));
    t
}

/// SQLite DDL generator.
#[derive(Debug, Clone, Copy)]
pub struct SqliteDdl {
    /// The connection enforces foreign keys: a table rebuild switches them back on.
    foreign_keys: bool,
}

impl Default for SqliteDdl {
    fn default() -> Self {
        Self::new()
    }
}

impl SqliteDdl {
    /// Create the generator.
    pub fn new() -> Self {
        SqliteDdl { foreign_keys: true }
    }

    /// The generator for a connection with (or without) foreign key enforcement.
    pub fn with_foreign_keys(foreign_keys: bool) -> Self {
        SqliteDdl { foreign_keys }
    }

    fn script(&self, from: &TableDef, to: &TableDef) -> String {
        self.alter_table(from, to, &[]).join(";\n")
    }
}

/// Upper-cased type name when it is a plain SQL type name (words and an
/// optional numeric parameter list), which SQLite accepts verbatim.
fn plain_type(t: &str) -> Option<String> {
    let ok = t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || " _(),".contains(c))
        && t.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
    if !ok {
        return None;
    }
    if let Some(open) = t.find('(') {
        let inner = t[open + 1..].trim_end_matches(')');
        if !inner
            .split(',')
            .all(|p| p.trim().chars().all(|c| c.is_ascii_digit()) && !p.trim().is_empty())
        {
            return None;
        }
    }
    Some(squash_ws(&t.to_ascii_uppercase()))
}

impl DdlGenerator for SqliteDdl {
    fn create_table(&self, table: &TableDef, include_foreign_keys: bool) -> Vec<String> {
        let mut out = vec![create_table_sql(table, include_foreign_keys)];
        let schema = table.schema.as_deref().filter(|s| !s.is_empty());
        for idx in secondary_indexes(table) {
            out.push(create_index_in(schema, &table.name, idx));
        }
        for t in &table.triggers {
            out.extend(create_trigger_in(schema, &table.name, t));
        }
        out
    }

    fn drop_table(&self, schema: Option<&str>, name: &str, _cascade: bool) -> String {
        format!("DROP TABLE {}", qualified(schema, name))
    }

    fn rename_table(&self, schema: Option<&str>, old_name: &str, new_name: &str) -> String {
        format!(
            "ALTER TABLE {} RENAME TO {}",
            qualified(schema, old_name),
            quote_ident(new_name)
        )
    }

    fn alter_table(
        &self,
        from: &TableDef,
        to: &TableDef,
        renamed_columns: &[(String, String)],
    ) -> Vec<String> {
        if is_virtual(from) || is_virtual(to) {
            // virtual tables can only be renamed
            if from.name != to.name {
                return vec![self.rename_table(from.schema.as_deref(), &from.name, &to.name)];
            }
            return Vec::new();
        }
        let plan = Plan::new(from, to, renamed_columns);
        if plan.needs_rebuild() {
            let mut out = plan.rebuild();
            // Without enforcement on the connection the rebuild leaves it off (as it was).
            if !self.foreign_keys
                && let Some(last) = out
                    .last_mut()
                    .filter(|s| s.as_str() == "PRAGMA foreign_keys = ON")
            {
                *last = "PRAGMA foreign_keys = OFF".to_string();
            }
            out
        } else {
            plan.native()
        }
    }

    fn add_column(&self, table: &TableDef, column: &ColumnDef) -> Vec<String> {
        // `table` may or may not already contain the new column
        let mut from = table.clone();
        from.columns.retain(|c| !eq_ci(&c.name, &column.name));
        let mut to = from.clone();
        to.columns.push(column.clone());
        self.alter_table(&from, &to, &[])
    }

    fn drop_column(&self, table: &TableDef, column: &str) -> String {
        self.script(table, &without_column(table, column))
    }

    fn create_index(&self, table: &TableDef, index: &IndexDef) -> String {
        create_index_in(
            table.schema.as_deref().filter(|s| !s.is_empty()),
            &table.name,
            index,
        )
    }

    fn drop_index(&self, table: &TableDef, index: &IndexDef) -> String {
        format!(
            "DROP INDEX {}",
            qualified(table.schema.as_deref(), &index.name)
        )
    }

    fn add_primary_key(&self, table: &TableDef, key: &KeyDef) -> String {
        let mut to = table.clone();
        to.primary_key = Some(key.clone());
        self.script(table, &to)
    }

    fn drop_primary_key(&self, table: &TableDef, _key: &KeyDef) -> String {
        let mut to = table.clone();
        to.primary_key = None;
        for c in &mut to.columns {
            c.auto_increment = false;
        }
        self.script(table, &to)
    }

    fn add_unique_key(&self, table: &TableDef, key: &KeyDef) -> String {
        let name = key
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("uq_{}_{}", table.name, key.columns.join("_")));
        format!(
            "CREATE UNIQUE INDEX {} ON {} ({})",
            qualified(table.schema.as_deref(), &name),
            quote_ident(&table.name),
            q_list(&key.columns)
        )
    }

    fn drop_unique_key(&self, table: &TableDef, key: &KeyDef) -> String {
        if let Some(n) = key.name.as_deref() {
            let is_index = table.indexes.iter().any(|i| {
                eq_ci(&i.name, n)
                    && i.unique
                    && !i.name.to_ascii_lowercase().starts_with("sqlite_autoindex_")
            }) && !table
                .unique_keys
                .iter()
                .any(|u| u.name.as_deref() == Some(n));
            if is_index {
                return format!("DROP INDEX {}", qualified(table.schema.as_deref(), n));
            }
        }
        let mut to = table.clone();
        to.unique_keys.retain(|u| u != key);
        self.script(table, &to)
    }

    fn add_check(&self, table: &TableDef, check: &CheckDef) -> String {
        let mut to = table.clone();
        to.checks.push(check.clone());
        self.script(table, &to)
    }

    fn drop_check(&self, table: &TableDef, check: &CheckDef) -> String {
        let mut to = table.clone();
        match check.name.as_deref() {
            Some(n) => to.checks.retain(|c| c.name.as_deref() != Some(n)),
            None => to
                .checks
                .retain(|c| !(c.name.is_none() && same_text(&c.expression, &check.expression))),
        }
        self.script(table, &to)
    }

    fn add_foreign_key(&self, table: &TableDef, fk: &ForeignKeyDef) -> String {
        let mut to = table.clone();
        if !to.foreign_keys.contains(fk) {
            to.foreign_keys.push(fk.clone());
        }
        self.script(table, &to)
    }

    fn drop_foreign_key(&self, table: &TableDef, fk: &ForeignKeyDef) -> String {
        let mut to = table.clone();
        to.foreign_keys.retain(|f| {
            let same = match (&fk.name, &f.name) {
                (Some(a), Some(b)) => a == b,
                _ => cols_eq(&f.columns, &fk.columns) && eq_ci(&f.ref_table, &fk.ref_table),
            };
            !same
        });
        self.script(table, &to)
    }

    fn create_trigger(&self, table: &TableDef, trigger: &TriggerDef) -> Vec<String> {
        create_trigger_in(
            table.schema.as_deref().filter(|s| !s.is_empty()),
            &table.name,
            trigger,
        )
    }

    fn drop_trigger(&self, table: &TableDef, trigger: &TriggerDef) -> String {
        format!(
            "DROP TRIGGER {}",
            qualified(table.schema.as_deref(), &trigger.name)
        )
    }

    fn create_view(&self, view: &ViewDef, or_replace: bool) -> Vec<String> {
        let name = qualified(view.schema.as_deref(), &view.name);
        let cols = view
            .options
            .get(VIEW_COLUMNS)
            .map(|c| format!(" {}", c.trim()))
            .unwrap_or_default();
        let mut out = Vec::new();
        if or_replace {
            out.push(format!("DROP VIEW IF EXISTS {name}"));
        }
        out.push(format!(
            "CREATE VIEW {name}{cols} AS\n{}",
            ansi::strip_trailing_semicolon(view.definition.trim())
        ));
        out
    }

    fn drop_view(&self, view: &ViewDef) -> String {
        format!(
            "DROP VIEW {}",
            qualified(view.schema.as_deref(), &view.name)
        )
    }

    fn create_sequence(&self, seq: &SequenceDef) -> String {
        format!(
            "-- SQLite has no sequences ({}); use INTEGER PRIMARY KEY AUTOINCREMENT",
            seq.name
        )
    }

    fn alter_sequence(&self, _from: &SequenceDef, _to: &SequenceDef) -> Vec<String> {
        Vec::new()
    }

    fn drop_sequence(&self, seq: &SequenceDef) -> String {
        format!("-- SQLite has no sequences ({})", seq.name)
    }

    fn create_routine(&self, _routine: &RoutineDef) -> Vec<String> {
        Vec::new()
    }

    fn drop_routine(&self, routine: &RoutineDef) -> String {
        format!("-- SQLite has no stored routines ({})", routine.name)
    }

    fn create_schema(&self, name: &str) -> String {
        format!(
            "ATTACH DATABASE {} AS {}",
            string_literal(&format!("{name}.db")),
            quote_ident(name)
        )
    }

    fn drop_schema(&self, name: &str, _cascade: bool) -> String {
        format!("DETACH DATABASE {}", quote_ident(name))
    }

    fn map_type(&self, source: DbKind, data_type: &str, kind: DataKind) -> String {
        let t = data_type.trim();
        if source == DbKind::Sqlite {
            return t.to_string();
        }
        if t.is_empty() {
            return self.default_type_for(kind);
        }
        let lower = t.to_ascii_lowercase();
        for wrapper in ["nullable(", "lowcardinality("] {
            if lower.starts_with(wrapper) && lower.ends_with(')') {
                return self.map_type(source, &t[wrapper.len()..t.len() - 1], kind);
            }
        }
        let collection = lower.ends_with("[]")
            || lower.starts_with("array")
            || lower.starts_with("list")
            || lower.starts_with("map(")
            || lower.starts_with("tuple(")
            || lower.starts_with("struct")
            || lower.starts_with("union(")
            || lower.starts_with("nested(")
            || (lower.starts_with('_') && source.is_postgres_family());
        if collection {
            return "TEXT".into();
        }
        let base = lower.split(['(', ' ']).next().unwrap_or("");
        let mapped = match base {
            "bool" | "boolean" => Some("INTEGER"),
            "bit" if lower == "bit" || lower == "bit(1)" => Some("INTEGER"),
            "tinyint" if lower == "tinyint(1)" && source.is_mysql_family() => Some("INTEGER"),
            "serial" | "serial2" | "serial4" | "serial8" | "smallserial" | "bigserial" => {
                Some("INTEGER")
            }
            "uuid" | "uniqueidentifier" => Some("TEXT"),
            "json" | "jsonb" => Some("TEXT"),
            "bytea" | "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob"
            | "image" | "raw" | "bytes" | "hierarchyid" | "sql_variant" => Some("BLOB"),
            "money" | "smallmoney" => Some("NUMERIC"),
            "timestamptz" | "datetimeoffset" | "timetz" | "interval" => Some("TEXT"),
            "timestamp" | "time" if lower.contains("with time zone") => Some("TEXT"),
            "xml" | "inet" | "cidr" | "macaddr" | "macaddr8" | "tsvector" | "tsquery"
            | "hstore" | "citext" | "name" | "ltree" | "ipv4" | "ipv6" | "enum" | "enum8"
            | "enum16" | "set" | "string" | "fixedstring" | "geometry" | "geography" | "point"
            | "line" | "lseg" | "box" | "path" | "polygon" | "circle" | "linestring"
            | "multipoint" | "multilinestring" | "multipolygon" | "geometrycollection"
            | "int4range" | "int8range" | "numrange" | "tsrange" | "tstzrange" | "daterange"
            | "varbit" | "oidvector" | "regclass" | "regtype" | "txid_snapshot" | "pg_lsn"
            | "ntext" | "sysname" => Some("TEXT"),
            "year" | "oid" | "xid" => Some("INTEGER"),
            "hugeint" | "uhugeint" | "int128" | "int256" | "uint128" | "uint256" => Some("TEXT"),
            "int8" | "int16" | "int32" | "int64" | "uint8" | "uint16" | "uint32" | "uint64"
            | "utinyint" | "usmallint" | "uinteger" | "ubigint" => Some("INTEGER"),
            "date32" => Some("DATE"),
            "datetime64" | "smalldatetime" | "datetime2" => Some("DATETIME"),
            _ => None,
        };
        if let Some(m) = mapped {
            return m.to_string();
        }
        plain_type(t).unwrap_or_else(|| self.default_type_for(kind))
    }

    fn default_type_for(&self, kind: DataKind) -> String {
        match kind {
            DataKind::Bool | DataKind::Int => "INTEGER",
            DataKind::Float => "REAL",
            DataKind::Decimal => "NUMERIC",
            DataKind::Binary => "BLOB",
            DataKind::Date => "DATE",
            DataKind::Time => "TIME",
            DataKind::DateTime => "DATETIME",
            _ => "TEXT",
        }
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users() -> TableDef {
        let mut t = TableDef::new(Some("main".into()), "users");
        t.columns
            .push(ColumnDef::new("id", "INTEGER").auto_increment());
        t.columns.push(ColumnDef::new("email", "TEXT").not_null());
        t.columns
            .push(ColumnDef::new("age", "INT").default_value("0"));
        t.primary_key = Some(KeyDef {
            name: None,
            columns: vec!["id".into()],
        });
        t.unique_keys.push(KeyDef {
            name: Some("uq_email".into()),
            columns: vec!["email".into()],
        });
        t.checks.push(CheckDef {
            name: None,
            expression: "age >= 0".into(),
        });
        t.indexes.push(IndexDef {
            name: "ix_age".into(),
            columns: vec![IndexColumn::column("age")],
            predicate: Some("age > 0".into()),
            ..Default::default()
        });
        t
    }

    #[test]
    fn create_table_sql_shape() {
        let mut t = users();
        t.columns[1].comment = Some("login\nname".into());
        t.columns[1].collation = Some("NOCASE".into());
        t.foreign_keys.push(ForeignKeyDef {
            name: Some("fk_org".into()),
            columns: vec!["age".into()],
            ref_table: "orgs".into(),
            ref_columns: vec!["id".into()],
            on_delete: FkAction::Cascade,
            deferrable: true,
            ..Default::default()
        });
        t.options.insert(STRICT.into(), "true".into());
        let sql = SqliteDdl::new().create_table(&t, true);
        assert_eq!(sql.len(), 2, "{sql:#?}");
        let c = &sql[0];
        assert!(c.starts_with("CREATE TABLE main.users ("), "{c}");
        assert!(c.contains("id INTEGER PRIMARY KEY AUTOINCREMENT,"), "{c}");
        assert!(
            c.contains("email TEXT NOT NULL COLLATE NOCASE, -- login name"),
            "{c}"
        );
        assert!(c.contains("age INT DEFAULT 0"), "{c}");
        assert!(c.contains("CONSTRAINT uq_email UNIQUE (email)"), "{c}");
        assert!(c.contains("CHECK (age >= 0)"), "{c}");
        assert!(c.contains(
            "CONSTRAINT fk_org FOREIGN KEY (age) REFERENCES orgs (id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED"
        ));
        assert!(c.ends_with(") STRICT"), "{c}");
        assert_eq!(
            sql[1],
            "CREATE INDEX main.ix_age ON users (age) WHERE age > 0"
        );
        let no_fk = SqliteDdl::new().create_table(&t, false);
        assert!(!no_fk[0].contains("FOREIGN KEY"));
    }

    #[test]
    fn defaults_and_generated() {
        let mut t = TableDef::new(None, "t");
        t.columns
            .push(ColumnDef::new("a", "INT").default_value("1 + 2"));
        t.columns
            .push(ColumnDef::new("b", "TEXT").default_value("CURRENT_TIMESTAMP"));
        t.columns
            .push(ColumnDef::new("c", "REAL").default_value("-1.5"));
        t.columns
            .push(ColumnDef::new("d", "TEXT").default_value("datetime('now')"));
        let mut g = ColumnDef::new("e", "INT");
        g.generated = Some("a * 2".into());
        t.columns.push(g);
        t.options
            .insert(format!("{GENERATED_PREFIX}e"), "stored".into());
        let sql = &SqliteDdl::new().create_table(&t, true)[0];
        assert!(sql.contains("a INT DEFAULT (1 + 2)"), "{sql}");
        assert!(sql.contains("b TEXT DEFAULT CURRENT_TIMESTAMP"), "{sql}");
        assert!(sql.contains("c REAL DEFAULT -1.5"), "{sql}");
        assert!(sql.contains("d TEXT DEFAULT (datetime('now'))"), "{sql}");
        assert!(
            sql.contains("e INT GENERATED ALWAYS AS (a * 2) STORED"),
            "{sql}"
        );
    }

    #[test]
    fn native_alter() {
        let from = users();
        let mut to = from.clone();
        to.name = "people".into();
        to.columns[1].name = "mail".into();
        to.unique_keys[0].columns = vec!["mail".into()];
        let mut nick = ColumnDef::new("nick", "TEXT");
        nick.default = Some("'x'".into());
        to.columns.push(nick);
        let sql = SqliteDdl::new().alter_table(&from, &to, &[("email".into(), "mail".into())]);
        assert_eq!(
            sql,
            vec![
                "BEGIN",
                "ALTER TABLE main.users RENAME COLUMN email TO mail",
                "ALTER TABLE main.users RENAME TO people",
                "ALTER TABLE main.people ADD COLUMN nick TEXT DEFAULT 'x'",
                "COMMIT",
            ]
        );
        // dropping a column whose check is removed too needs a rebuild
        let mut to = from.clone();
        to.columns.retain(|c| c.name != "age");
        to.checks.clear();
        to.indexes.clear();
        let sql = SqliteDdl::new().alter_table(&from, &to, &[]);
        assert_eq!(sql[0], "PRAGMA foreign_keys = OFF", "{sql:?}");
        assert!(!sql.iter().any(|s| s.contains("ix_age")), "{sql:?}");
        // dropping an indexed column natively
        let mut from2 = users();
        from2.columns.push(ColumnDef::new("note", "TEXT"));
        from2.indexes.push(IndexDef {
            name: "ix_note".into(),
            columns: vec![IndexColumn::column("note")],
            ..Default::default()
        });
        let to2 = users();
        let sql = SqliteDdl::new().alter_table(&from2, &to2, &[]);
        assert_eq!(
            sql,
            vec![
                "BEGIN",
                "DROP INDEX main.ix_note",
                "ALTER TABLE main.users DROP COLUMN note",
                "COMMIT"
            ]
        );
        assert_eq!(
            SqliteDdl::new().drop_column(&from2, "note"),
            "BEGIN;\nDROP INDEX main.ix_note;\nALTER TABLE main.users DROP COLUMN note;\nCOMMIT"
        );
    }

    #[test]
    fn rebuild_alter() {
        let from = users();
        let mut to = from.clone();
        to.columns[2].data_type = "BIGINT".into();
        let sql = SqliteDdl::new().alter_table(&from, &to, &[]);
        assert_eq!(sql[0], "PRAGMA foreign_keys = OFF");
        assert_eq!(sql[1], "BEGIN");
        assert!(
            sql[2].starts_with("CREATE TABLE main._dbiji_new_users ("),
            "{}",
            sql[2]
        );
        assert!(sql[2].contains("age BIGINT DEFAULT 0"));
        assert_eq!(
            sql[3],
            "INSERT INTO main._dbiji_new_users (id, email, age) SELECT id, email, age FROM main.users"
        );
        assert!(sql.contains(
            &"DELETE FROM main.sqlite_sequence WHERE name = '_dbiji_new_users'".to_string()
        ));
        assert!(sql.contains(&"DROP TABLE main.users".to_string()));
        assert!(sql.contains(&"ALTER TABLE main._dbiji_new_users RENAME TO users".to_string()));
        assert!(sql.contains(&"CREATE INDEX main.ix_age ON users (age) WHERE age > 0".to_string()));
        assert!(sql.contains(&"PRAGMA main.foreign_key_check('users')".to_string()));
        assert_eq!(sql[sql.len() - 2], "COMMIT");
        assert_eq!(sql[sql.len() - 1], "PRAGMA foreign_keys = ON");
    }

    #[test]
    fn rename_with_swap_uses_temporary_names() {
        let current = vec!["a".to_string(), "b".to_string()];
        let sql = rename_columns(
            "t",
            &current,
            &[("a".into(), "b".into()), ("b".into(), "a".into())],
        );
        assert_eq!(sql.len(), 4);
        assert_eq!(sql[0], "ALTER TABLE t RENAME COLUMN a TO __dbiji_rn_0");
        assert_eq!(sql[3], "ALTER TABLE t RENAME COLUMN __dbiji_rn_1 TO a");
    }

    #[test]
    fn type_mapping() {
        let d = SqliteDdl::new();
        assert_eq!(
            d.map_type(DbKind::Postgres, "jsonb", DataKind::Json),
            "TEXT"
        );
        assert_eq!(
            d.map_type(DbKind::Postgres, "bytea", DataKind::Binary),
            "BLOB"
        );
        assert_eq!(d.map_type(DbKind::Postgres, "uuid", DataKind::Uuid), "TEXT");
        assert_eq!(
            d.map_type(DbKind::Postgres, "boolean", DataKind::Bool),
            "INTEGER"
        );
        assert_eq!(
            d.map_type(DbKind::Postgres, "character varying(255)", DataKind::Text),
            "CHARACTER VARYING(255)"
        );
        assert_eq!(
            d.map_type(DbKind::Postgres, "numeric(10,2)", DataKind::Decimal),
            "NUMERIC(10,2)"
        );
        assert_eq!(
            d.map_type(DbKind::Postgres, "integer[]", DataKind::Array),
            "TEXT"
        );
        assert_eq!(
            d.map_type(
                DbKind::Postgres,
                "timestamp with time zone",
                DataKind::DateTimeTz
            ),
            "TEXT"
        );
        assert_eq!(
            d.map_type(DbKind::MySql, "tinyint(1)", DataKind::Bool),
            "INTEGER"
        );
        assert_eq!(
            d.map_type(DbKind::MySql, "enum('a','b')", DataKind::Enum),
            "TEXT"
        );
        assert_eq!(
            d.map_type(DbKind::MySql, "datetime(6)", DataKind::DateTime),
            "DATETIME(6)"
        );
        assert_eq!(
            d.map_type(DbKind::ClickHouse, "Nullable(UInt32)", DataKind::Int),
            "INTEGER"
        );
        assert_eq!(
            d.map_type(DbKind::Postgres, "public.mood", DataKind::Enum),
            "TEXT"
        );
        assert_eq!(
            d.map_type(DbKind::Sqlite, "whatever", DataKind::Other),
            "whatever"
        );
        assert_eq!(d.default_type_for(DataKind::Float), "REAL");
    }

    #[test]
    fn views_and_misc() {
        let d = SqliteDdl::new();
        let mut v = ViewDef {
            schema: Some("main".into()),
            name: "v".into(),
            definition: "SELECT 1;".into(),
            ..Default::default()
        };
        v.options.insert(VIEW_COLUMNS.into(), "(x)".into());
        assert_eq!(
            d.create_view(&v, true),
            vec![
                "DROP VIEW IF EXISTS main.v",
                "CREATE VIEW main.v (x) AS\nSELECT 1"
            ]
        );
        assert_eq!(d.drop_schema("aux", true), "DETACH DATABASE aux");
        assert_eq!(d.create_schema("aux"), "ATTACH DATABASE 'aux.db' AS aux");
        assert_eq!(d.drop_table(Some("main"), "t", true), "DROP TABLE main.t");
        let t = users();
        assert_eq!(
            d.add_unique_key(
                &t,
                &KeyDef {
                    name: None,
                    columns: vec!["age".into()]
                }
            ),
            "CREATE UNIQUE INDEX main.uq_users_age ON users (age)"
        );
    }
}
