//! T-SQL DDL generation (`MsSqlDdl`).
//!
//! SQL Server specifics that [`TableDef`] has no dedicated field for travel
//! in [`TableDef::options`]:
//! * `default_constraint:<column>` – name of the column's default constraint;
//! * `identity:<column>` – `seed,increment` when not `1,1`;
//! * `filegroup` / `data_compression` – storage options.
//!
//! Computed columns are persisted when [`ColumnDef::generated`] ends with
//! `PERSISTED`. Index kinds (`CLUSTERED`, `NONCLUSTERED`, `CLUSTERED
//! COLUMNSTORE`, `XML`, `SPATIAL` ...) are carried by [`IndexDef::method`];
//! the index backing the primary key has `primary = true`.

use crate::dialect::{ident, nstring, qualified, quote};
use crate::sqltext::strip_outer_parens;
use dbiji_core::DbKind;
use dbiji_core::ddl::*;
use dbiji_core::dialect::ansi::{match_columns, same_index, secondary_indexes, strip_trailing_semicolon};
use dbiji_core::meta::ObjectKind;
use dbiji_core::value::DataKind;

/// DDL generator for SQL Server.
#[derive(Debug, Default, Clone)]
pub struct MsSqlDdl;

impl MsSqlDdl {
    /// Create the generator.
    pub fn new() -> Self {
        MsSqlDdl
    }
}

/// Option key holding the default constraint name of `column`.
pub fn default_constraint_key(column: &str) -> String {
    format!("default_constraint:{column}")
}

/// Option key holding the identity seed/increment of `column`.
pub fn identity_key(column: &str) -> String {
    format!("identity:{column}")
}

fn schema_or_dbo(schema: Option<&str>) -> &str {
    match schema {
        Some(s) if !s.is_empty() => s,
        _ => "dbo",
    }
}

fn tname(t: &TableDef) -> String {
    qualified(t.schema.as_deref(), &t.name)
}

/// `[schema].[table]` always quoted (for OBJECT_ID / sp_rename arguments).
fn full_quoted(schema: Option<&str>, name: &str) -> String {
    format!("{}.{}", quote(schema_or_dbo(schema)), quote(name))
}

/// Deterministic short suffix for T-SQL variable names.
fn var_suffix(parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.bytes().chain(std::iter::once(0u8)) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{:08x}", h as u32)
}

fn is_persisted(generated: &str) -> bool {
    generated.trim_end().to_ascii_uppercase().ends_with("PERSISTED")
}

fn generated_expr(generated: &str) -> String {
    let g = generated.trim();
    let g = if is_persisted(g) { g[..g.len() - "PERSISTED".len()].trim_end() } else { g };
    strip_outer_parens(g)
}

fn identity_clause(t: &TableDef, c: &ColumnDef) -> String {
    let spec = t.options.get(&identity_key(&c.name)).map(String::as_str).unwrap_or("1,1");
    let mut parts = spec.split(',').map(str::trim);
    let seed = parts.next().filter(|s| !s.is_empty()).unwrap_or("1");
    let inc = parts.next().filter(|s| !s.is_empty()).unwrap_or("1");
    format!(" IDENTITY({seed},{inc})")
}

fn default_name(t: &TableDef, column: &str) -> String {
    t.options.get(&default_constraint_key(column)).cloned().unwrap_or_else(|| format!("DF_{}_{}", t.name, column))
}

/// Column definition for `CREATE TABLE` / `ADD`.
pub fn column_definition(t: &TableDef, c: &ColumnDef) -> String {
    if let Some(g) = c.generated.as_deref().filter(|g| !g.trim().is_empty()) {
        let mut s = format!("{} AS ({})", ident(&c.name), generated_expr(g));
        if is_persisted(g) {
            s.push_str(" PERSISTED");
            if !c.nullable {
                s.push_str(" NOT NULL");
            }
        }
        return s;
    }
    let mut s = format!("{} {}", ident(&c.name), c.data_type);
    if let Some(col) = c.collation.as_deref().filter(|c| !c.is_empty()) {
        s.push_str(&format!(" COLLATE {col}"));
    }
    if c.auto_increment {
        s.push_str(&identity_clause(t, c));
    }
    s.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
    if let Some(d) = c.default.as_deref().filter(|d| !d.trim().is_empty()) {
        s.push_str(&format!(" CONSTRAINT {} DEFAULT ({})", quote(&default_name(t, &c.name)), strip_outer_parens(d)));
    }
    s
}

fn col_list(cols: &[String]) -> String {
    cols.iter().map(|c| ident(c)).collect::<Vec<_>>().join(", ")
}

/// Clustered / nonclustered keyword of the index backing `key`.
fn key_kind(t: &TableDef, key: &KeyDef, primary: bool) -> &'static str {
    let backing = t.indexes.iter().find(|i| if primary { i.primary } else { i.unique && (key.name.as_deref() == Some(i.name.as_str())) });
    match backing.and_then(|i| i.method.as_deref()).map(str::to_ascii_uppercase).as_deref() {
        Some("CLUSTERED") => " CLUSTERED",
        Some("NONCLUSTERED") => " NONCLUSTERED",
        _ => "",
    }
}

fn pk_clause(t: &TableDef, k: &KeyDef) -> String {
    let name = k.name.clone().unwrap_or_else(|| format!("PK_{}", t.name));
    format!("CONSTRAINT {} PRIMARY KEY{} ({})", quote(&name), key_kind(t, k, true), col_list(&k.columns))
}

fn unique_clause(t: &TableDef, k: &KeyDef) -> String {
    let name = k.name.clone().unwrap_or_else(|| format!("UQ_{}_{}", t.name, k.columns.join("_")));
    format!("CONSTRAINT {} UNIQUE{} ({})", quote(&name), key_kind(t, k, false), col_list(&k.columns))
}

fn check_clause(t: &TableDef, c: &CheckDef) -> String {
    let name = c.name.clone().unwrap_or_else(|| format!("CK_{}", t.name));
    format!("CONSTRAINT {} CHECK ({})", quote(&name), strip_outer_parens(&c.expression))
}

fn fk_action(a: FkAction) -> Option<&'static str> {
    match a {
        FkAction::NoAction | FkAction::Restrict => None,
        FkAction::Cascade => Some("CASCADE"),
        FkAction::SetNull => Some("SET NULL"),
        FkAction::SetDefault => Some("SET DEFAULT"),
    }
}

fn fk_clause(t: &TableDef, fk: &ForeignKeyDef) -> String {
    let name = fk.name.clone().unwrap_or_else(|| format!("FK_{}_{}", t.name, fk.ref_table));
    let mut s = format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
        quote(&name),
        col_list(&fk.columns),
        qualified(Some(schema_or_dbo(fk.ref_schema.as_deref().or(t.schema.as_deref()))), &fk.ref_table),
        col_list(&fk.ref_columns)
    );
    if let Some(a) = fk_action(fk.on_delete) {
        s.push_str(&format!(" ON DELETE {a}"));
    }
    if let Some(a) = fk_action(fk.on_update) {
        s.push_str(&format!(" ON UPDATE {a}"));
    }
    s
}

/// `EXEC sys.sp_<op>extendedproperty` for `MS_Description` on a table
/// (`column = None`), column or index.
fn ext_property(op: &str, schema: Option<&str>, level1_type: &str, level1: &str, level2: Option<(&str, &str)>, value: Option<&str>) -> String {
    let mut s = format!("EXEC sys.sp_{op}extendedproperty @name = N'MS_Description'");
    if let Some(v) = value {
        s.push_str(&format!(", @value = {}", nstring(v)));
    }
    s.push_str(&format!(", @level0type = N'SCHEMA', @level0name = {}, @level1type = N'{level1_type}', @level1name = {}", nstring(schema_or_dbo(schema)), nstring(level1)));
    if let Some((ty, name)) = level2 {
        s.push_str(&format!(", @level2type = N'{ty}', @level2name = {}", nstring(name)));
    }
    s
}

/// Comment statement transforming `from` into `to`.
fn comment_change(schema: Option<&str>, l1_type: &str, l1: &str, l2: Option<(&str, &str)>, from: Option<&str>, to: Option<&str>) -> Option<String> {
    let from = from.filter(|c| !c.is_empty());
    let to = to.filter(|c| !c.is_empty());
    match (from, to) {
        (None, None) => None,
        (None, Some(v)) => Some(ext_property("add", schema, l1_type, l1, l2, Some(v))),
        (Some(_), Some(v)) if from != Some(v) => Some(ext_property("update", schema, l1_type, l1, l2, Some(v))),
        (Some(_), Some(_)) => None,
        (Some(_), None) => Some(ext_property("drop", schema, l1_type, l1, l2, None)),
    }
}

/// Dynamic SQL dropping the default constraint of a column, whatever its name.
pub fn drop_default_sql(t: &TableDef, column: &str) -> String {
    let v = format!("@dbiji_df_{}", var_suffix(&[schema_or_dbo(t.schema.as_deref()), &t.name, column]));
    let obj = nstring(&full_quoted(t.schema.as_deref(), &t.name));
    let stmt = nstring(&format!("ALTER TABLE {} DROP CONSTRAINT ", tname(t)));
    format!(
        "DECLARE {v} sysname\n\
         SELECT {v} = dc.name FROM sys.default_constraints dc \
         JOIN sys.columns c ON c.object_id = dc.parent_object_id AND c.column_id = dc.parent_column_id \
         WHERE dc.parent_object_id = OBJECT_ID({obj}) AND c.name = {}\n\
         IF {v} IS NOT NULL EXEC ({stmt} + QUOTENAME({v}))",
        nstring(column)
    )
}

fn drop_key_dynamic(t: &TableDef, kind: &str) -> String {
    let v = format!("@dbiji_pk_{}", var_suffix(&[schema_or_dbo(t.schema.as_deref()), &t.name, kind]));
    let obj = nstring(&full_quoted(t.schema.as_deref(), &t.name));
    let stmt = nstring(&format!("ALTER TABLE {} DROP CONSTRAINT ", tname(t)));
    format!(
        "DECLARE {v} sysname\n\
         SELECT {v} = name FROM sys.key_constraints WHERE parent_object_id = OBJECT_ID({obj}) AND type = '{kind}'\n\
         IF {v} IS NOT NULL EXEC ({stmt} + QUOTENAME({v}))"
    )
}

fn index_kind(idx: &IndexDef) -> String {
    idx.method.as_deref().map(|m| m.trim().to_ascii_uppercase()).unwrap_or_default()
}

/// `CREATE INDEX` statement for SQL Server index kinds.
pub fn create_index_sql(t: &TableDef, idx: &IndexDef) -> String {
    let kind = index_kind(idx);
    let cols = idx
        .columns
        .iter()
        .map(|c| {
            let mut s = if c.is_expression { c.name.clone() } else { ident(&c.name) };
            if c.descending {
                s.push_str(" DESC");
            }
            s
        })
        .collect::<Vec<_>>()
        .join(", ");
    let table = tname(t);
    let name = quote(&idx.name);
    if kind.contains("COLUMNSTORE") {
        let clustered = kind.starts_with("CLUSTERED");
        return if clustered || idx.columns.is_empty() {
            format!("CREATE CLUSTERED COLUMNSTORE INDEX {name} ON {table}")
        } else {
            format!("CREATE NONCLUSTERED COLUMNSTORE INDEX {name} ON {table} ({cols})")
        };
    }
    if kind.contains("XML") {
        return format!("CREATE PRIMARY XML INDEX {name} ON {table} ({cols})");
    }
    if kind.contains("SPATIAL") {
        return format!("CREATE SPATIAL INDEX {name} ON {table} ({cols})");
    }
    let clustered = match kind.as_str() {
        "CLUSTERED" => "CLUSTERED ",
        "NONCLUSTERED" => "NONCLUSTERED ",
        _ => "",
    };
    let mut s = format!("CREATE {}{clustered}INDEX {name} ON {table} ({cols})", if idx.unique { "UNIQUE " } else { "" });
    if !idx.include.is_empty() {
        s.push_str(&format!(" INCLUDE ({})", col_list(&idx.include)));
    }
    if let Some(p) = idx.predicate.as_deref().filter(|p| !p.trim().is_empty()) {
        s.push_str(&format!(" WHERE {}", strip_outer_parens(p)));
    }
    s
}

fn table_comments(t: &TableDef) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(c) = t.comment.as_deref().filter(|c| !c.is_empty()) {
        out.push(ext_property("add", t.schema.as_deref(), "TABLE", &t.name, None, Some(c)));
    }
    for col in &t.columns {
        if let Some(c) = col.comment.as_deref().filter(|c| !c.is_empty()) {
            out.push(ext_property("add", t.schema.as_deref(), "TABLE", &t.name, Some(("COLUMN", &col.name)), Some(c)));
        }
    }
    out
}

/// Indexes of `t` whose key or included columns contain `column`.
fn indexes_using<'a>(t: &'a TableDef, column: &str) -> Vec<&'a IndexDef> {
    secondary_indexes(t).filter(|i| i.columns.iter().any(|c| c.name == column) || i.include.iter().any(|c| c == column)).collect()
}

fn check_mentions(check: &CheckDef, column: &str) -> bool {
    expr_mentions(&check.expression, column)
}

/// Does a T-SQL expression reference `column` (bracketed or bare word)?
fn expr_mentions(expr: &str, column: &str) -> bool {
    let e = expr.to_ascii_lowercase();
    let c = column.to_ascii_lowercase();
    e.contains(&format!("[{c}]")) || e.split(|ch: char| !(ch.is_alphanumeric() || ch == '_')).any(|w| w == c)
}

impl DdlGenerator for MsSqlDdl {
    fn create_table(&self, t: &TableDef, include_foreign_keys: bool) -> Vec<String> {
        let mut lines: Vec<String> = t.columns.iter().map(|c| column_definition(t, c)).collect();
        if let Some(pk) = t.primary_key.as_ref().filter(|k| !k.columns.is_empty()) {
            lines.push(pk_clause(t, pk));
        }
        for u in &t.unique_keys {
            lines.push(unique_clause(t, u));
        }
        for c in &t.checks {
            lines.push(check_clause(t, c));
        }
        if include_foreign_keys {
            for fk in &t.foreign_keys {
                lines.push(fk_clause(t, fk));
            }
        }
        let mut sql = format!("CREATE TABLE {} (\n    {}\n)", tname(t), lines.join(",\n    "));
        if let Some(fg) = t.options.get("filegroup").filter(|f| !f.is_empty()) {
            sql.push_str(&format!(" ON {}", quote(fg)));
        }
        if let Some(dc) = t.options.get("data_compression").filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case("NONE")) {
            sql.push_str(&format!(" WITH (DATA_COMPRESSION = {dc})"));
        }
        let mut out = vec![sql];
        for idx in secondary_indexes(t) {
            out.push(create_index_sql(t, idx));
        }
        out.extend(table_comments(t));
        for idx in secondary_indexes(t) {
            if let Some(c) = idx.comment.as_deref().filter(|c| !c.is_empty()) {
                out.push(ext_property("add", t.schema.as_deref(), "TABLE", &t.name, Some(("INDEX", &idx.name)), Some(c)));
            }
        }
        for trg in &t.triggers {
            out.extend(self.create_trigger(t, trg));
        }
        out
    }

    fn drop_table(&self, schema: Option<&str>, name: &str, _cascade: bool) -> String {
        format!("DROP TABLE {}", qualified(schema, name))
    }

    fn rename_table(&self, schema: Option<&str>, old_name: &str, new_name: &str) -> String {
        format!("EXEC sp_rename {}, {}", nstring(&full_quoted(schema, old_name)), nstring(new_name))
    }

    fn alter_table(&self, from: &TableDef, to: &TableDef, renamed: &[(String, String)]) -> Vec<String> {
        let mut out = Vec::new();
        if from.name != to.name {
            out.push(self.rename_table(from.schema.as_deref(), &from.name, &to.name));
        }
        // From here on the table has its new name.
        let mut cur = from.clone();
        cur.name = to.name.clone();
        let t = tname(to);
        let (pairs, dropped, added) = match_columns(&cur, to, renamed);

        // Columns whose type / nullability / collation change, or are dropped.
        let altered: Vec<&str> = pairs
            .iter()
            .filter(|(f, tc)| f.data_type != tc.data_type || f.nullable != tc.nullable || f.collation != tc.collation || f.generated != tc.generated)
            .map(|(f, _)| f.name.as_str())
            .collect();
        let mut touched: Vec<&str> = altered.iter().copied().chain(dropped.iter().map(|c| c.name.as_str())).collect();
        // Computed columns referencing a touched column must be dropped and
        // re-created around the change (SQL Server refuses ALTER COLUMN).
        let dependents: Vec<&ColumnDef> = pairs
            .iter()
            .filter(|(f, tc)| f.generated.is_some() && f.generated == tc.generated && !touched.contains(&f.name.as_str()))
            .filter(|(f, _)| {
                let expr = f.generated.as_deref().unwrap_or("");
                touched.iter().any(|c| expr_mentions(expr, c))
            })
            .map(|(_, tc)| *tc)
            .collect();
        for d in &dependents {
            if let Some((f, _)) = pairs.iter().find(|(_, tc)| std::ptr::eq(*tc, *d)) {
                touched.push(f.name.as_str());
            }
        }

        // Keys this alter drops (and maybe re-adds): foreign keys referencing them
        // must go first, or SQL Server refuses to drop the key.
        let same_set = |a: &[String], b: &[String]| a.len() == b.len() && a.iter().all(|x| b.iter().any(|y| x.eq_ignore_ascii_case(y)));
        let mut dropped_keys: Vec<&[String]> = Vec::new();
        if let Some(pk) = cur.primary_key.as_ref().filter(|k| cur.primary_key != to.primary_key || k.columns.iter().any(|c| touched.contains(&c.as_str()))) {
            dropped_keys.push(&pk.columns);
        }
        for u in &cur.unique_keys {
            if !to.unique_keys.contains(u) || u.columns.iter().any(|c| touched.contains(&c.as_str())) {
                dropped_keys.push(&u.columns);
            }
        }
        let unique_index_cols: Vec<Vec<String>> = secondary_indexes(&cur)
            .filter(|i| i.unique)
            .filter(|i| {
                let kept = secondary_indexes(to).any(|x| same_index(x, i) && x.include == i.include && x.method == i.method);
                !kept || touched.iter().any(|c| indexes_using(&cur, c).iter().any(|x| x.name == i.name))
            })
            .map(|i| i.columns.iter().map(|c| c.name.clone()).collect())
            .collect();
        dropped_keys.extend(unique_index_cols.iter().map(Vec::as_slice));
        let on_dropped_key = |fk: &ForeignKeyDef| dropped_keys.iter().any(|k| same_set(k, &fk.ref_columns));
        let self_ref = |fk: &ForeignKeyDef| fk.table.eq_ignore_ascii_case(&from.name) || fk.table.eq_ignore_ascii_case(&to.name);
        let referencing: Vec<&ForeignKeyDef> = cur.referenced_by.iter().filter(|fk| !self_ref(fk) && on_dropped_key(fk)).collect();
        let child = |fk: &ForeignKeyDef| TableDef::new(fk.schema.clone().or(cur.schema.clone()), fk.table.clone());
        let renamed_col = |c: &String| pairs.iter().find(|(f, _)| f.name.eq_ignore_ascii_case(c)).map(|(_, tc)| tc.name.clone()).unwrap_or_else(|| c.clone());
        let final_keys: Vec<Vec<String>> = to
            .primary_key
            .iter()
            .chain(to.unique_keys.iter())
            .map(|k| k.columns.clone())
            .chain(secondary_indexes(to).filter(|i| i.unique).map(|i| i.columns.iter().map(|c| c.name.clone()).collect()))
            .collect();
        // Another table's foreign key that cannot be added back (its key is removed, or a key
        // column changes type under it) would silently disappear: refuse the change instead.
        let retyped = |c: &String| pairs.iter().any(|(f, tc)| f.name.eq_ignore_ascii_case(c) && f.data_type != tc.data_type);
        let lost: Vec<String> = referencing
            .iter()
            .filter(|fk| {
                let cols: Vec<String> = fk.ref_columns.iter().map(renamed_col).collect();
                !final_keys.iter().any(|k| same_set(k, &cols)) || fk.ref_columns.iter().any(retyped)
            })
            .map(|fk| format!("{} ({})", fk.name.as_deref().unwrap_or("?"), fk.table))
            .collect();
        if !lost.is_empty() {
            let msg = format!("Foreign keys of other tables reference a key this change removes or changes the type of: {}. Drop or change them first.", lost.join(", "));
            out.insert(0, format!("THROW 50000, {}, 1", nstring(&msg)));
        }
        for fk in &referencing {
            out.push(self.drop_foreign_key(&child(fk), fk));
        }

        // 1. foreign keys removed / changed / using touched columns (or a key dropped below)
        let mut dropped_fks: Vec<&ForeignKeyDef> = Vec::new();
        for fk in &cur.foreign_keys {
            let uses = fk.columns.iter().any(|c| touched.contains(&c.as_str()));
            let on_own_key = (fk.ref_table.eq_ignore_ascii_case(&from.name) || fk.ref_table.eq_ignore_ascii_case(&to.name)) && on_dropped_key(fk);
            if !to.foreign_keys.contains(fk) || uses || on_own_key {
                out.push(self.drop_foreign_key(&cur, fk));
                dropped_fks.push(fk);
            }
        }
        // 2. secondary indexes removed / changed / using touched columns
        let from_idx: Vec<&IndexDef> = secondary_indexes(&cur).collect();
        let to_idx: Vec<&IndexDef> = secondary_indexes(to).collect();
        let mut recreate_idx: Vec<&IndexDef> = Vec::new();
        for i in &from_idx {
            let kept = to_idx.iter().find(|x| same_index(x, i) && x.include == i.include && x.method == i.method);
            let uses = touched.iter().any(|c| indexes_using(&cur, c).iter().any(|x| x.name == i.name));
            if kept.is_none() || uses {
                out.push(self.drop_index(&cur, i));
                if let (Some(k), true) = (kept, uses) {
                    recreate_idx.push(k);
                }
            }
        }
        // 3. keys and checks removed / changed / using touched columns
        let pk_changed = cur.primary_key != to.primary_key;
        let pk_uses = cur.primary_key.as_ref().is_some_and(|k| k.columns.iter().any(|c| touched.contains(&c.as_str())));
        if let Some(pk) = cur.primary_key.as_ref().filter(|_| pk_changed || pk_uses) {
            out.push(self.drop_primary_key(&cur, pk));
        }
        let mut readd_uniques: Vec<&KeyDef> = Vec::new();
        for u in &cur.unique_keys {
            let uses = u.columns.iter().any(|c| touched.contains(&c.as_str()));
            if !to.unique_keys.contains(u) || uses {
                out.push(self.drop_unique_key(&cur, u));
                if uses && to.unique_keys.contains(u) {
                    readd_uniques.push(u);
                }
            }
        }
        let mut readd_checks: Vec<&CheckDef> = Vec::new();
        for c in &cur.checks {
            let uses = touched.iter().any(|col| check_mentions(c, col));
            if !to.checks.contains(c) || uses {
                out.push(self.drop_check(&cur, c));
                if uses && to.checks.contains(c) {
                    readd_checks.push(c);
                }
            }
        }
        // 4. defaults of dropped / altered / default-changed columns
        for c in &dropped {
            if c.default.is_some() {
                out.push(drop_default_sql(&cur, &c.name));
            }
        }
        for (f, tc) in &pairs {
            if f.default.is_some() && (f.default != tc.default || altered.contains(&f.name.as_str())) {
                out.push(drop_default_sql(&cur, &f.name));
            }
        }
        // 5. dropped columns (and computed dependents, re-added below)
        for c in &dropped {
            out.push(format!("ALTER TABLE {t} DROP COLUMN {}", ident(&c.name)));
        }
        for (f, tc) in &pairs {
            if dependents.iter().any(|d| std::ptr::eq(*d, *tc)) {
                out.push(format!("ALTER TABLE {t} DROP COLUMN {}", ident(&f.name)));
            }
        }
        // 6. renamed columns
        for (f, tc) in &pairs {
            if f.name != tc.name {
                out.push(format!("EXEC sp_rename {}, {}, N'COLUMN'", nstring(&format!("{}.{}", full_quoted(to.schema.as_deref(), &to.name), quote(&f.name))), nstring(&tc.name)));
            }
        }
        // 7. altered columns
        for (f, tc) in &pairs {
            if f.generated != tc.generated && (f.generated.is_some() || tc.generated.is_some()) {
                // computed columns cannot be altered: drop and re-add
                out.push(format!("ALTER TABLE {t} DROP COLUMN {}", ident(&tc.name)));
                out.push(format!("ALTER TABLE {t} ADD {}", column_definition(to, tc)));
                continue;
            }
            if f.auto_increment != tc.auto_increment {
                out.push(format!("-- SQL Server cannot add or remove IDENTITY on the existing column {}; recreate the table", ident(&tc.name)));
            }
            if f.data_type != tc.data_type || f.nullable != tc.nullable || f.collation != tc.collation {
                let mut s = format!("ALTER TABLE {t} ALTER COLUMN {} {}", ident(&tc.name), tc.data_type);
                if let Some(col) = tc.collation.as_deref().filter(|c| !c.is_empty()) {
                    s.push_str(&format!(" COLLATE {col}"));
                }
                s.push_str(if tc.nullable { " NULL" } else { " NOT NULL" });
                out.push(s);
            }
        }
        for d in &dependents {
            out.push(format!("ALTER TABLE {t} ADD {}", column_definition(to, d)));
        }
        // 8. new defaults
        for (f, tc) in &pairs {
            if tc.generated.is_some() {
                continue;
            }
            if let Some(d) = tc.default.as_deref().filter(|d| !d.trim().is_empty())
                && (f.default != tc.default || altered.contains(&f.name.as_str()))
            {
                out.push(format!("ALTER TABLE {t} ADD CONSTRAINT {} DEFAULT ({}) FOR {}", quote(&default_name(to, &tc.name)), strip_outer_parens(d), ident(&tc.name)));
            }
        }
        // 9. added columns
        for c in &added {
            out.push(format!("ALTER TABLE {t} ADD {}", column_definition(to, c)));
        }
        // 10. keys and checks
        if let Some(pk) = to.primary_key.as_ref().filter(|_| pk_changed || pk_uses).filter(|k| !k.columns.is_empty()) {
            out.push(self.add_primary_key(to, pk));
        }
        for u in &to.unique_keys {
            if !cur.unique_keys.contains(u) || readd_uniques.contains(&u) {
                out.push(self.add_unique_key(to, u));
            }
        }
        for c in &to.checks {
            if !cur.checks.contains(c) || readd_checks.contains(&c) {
                out.push(self.add_check(to, c));
            }
        }
        // 11. indexes
        for i in &to_idx {
            let existed = from_idx.iter().any(|x| same_index(x, i) && x.include == i.include && x.method == i.method);
            if !existed || recreate_idx.iter().any(|r| r.name == i.name) {
                out.push(self.create_index(to, i));
            }
        }
        // 12. foreign keys
        for fk in &to.foreign_keys {
            if !cur.foreign_keys.contains(fk) || dropped_fks.contains(&fk) {
                out.push(self.add_foreign_key(to, fk));
            }
        }
        // Other tables' foreign keys on a key that exists again (columns renamed).
        for fk in &referencing {
            let mut nf = (*fk).clone();
            nf.ref_table = to.name.clone();
            nf.ref_schema = to.schema.clone().or(nf.ref_schema);
            nf.ref_columns = fk.ref_columns.iter().map(renamed_col).collect();
            if final_keys.iter().any(|k| same_set(k, &nf.ref_columns)) {
                out.push(self.add_foreign_key(&child(fk), &nf));
            }
        }
        // 13. comments
        if let Some(s) = comment_change(to.schema.as_deref(), "TABLE", &to.name, None, cur.comment.as_deref(), to.comment.as_deref()) {
            out.push(s);
        }
        for (f, tc) in &pairs {
            let recreated = f.generated != tc.generated && (f.generated.is_some() || tc.generated.is_some());
            let from_comment = if recreated || dependents.iter().any(|d| std::ptr::eq(*d, *tc)) {
                None // the column was re-created
            } else {
                f.comment.as_deref()
            };
            if let Some(s) = comment_change(to.schema.as_deref(), "TABLE", &to.name, Some(("COLUMN", &tc.name)), from_comment, tc.comment.as_deref()) {
                out.push(s);
            }
        }
        for c in &added {
            if let Some(cm) = c.comment.as_deref().filter(|c| !c.is_empty()) {
                out.push(ext_property("add", to.schema.as_deref(), "TABLE", &to.name, Some(("COLUMN", &c.name)), Some(cm)));
            }
        }
        out
    }

    fn add_column(&self, table: &TableDef, column: &ColumnDef) -> Vec<String> {
        let mut out = vec![format!("ALTER TABLE {} ADD {}", tname(table), column_definition(table, column))];
        if let Some(c) = column.comment.as_deref().filter(|c| !c.is_empty()) {
            out.push(ext_property("add", table.schema.as_deref(), "TABLE", &table.name, Some(("COLUMN", &column.name)), Some(c)));
        }
        out
    }

    fn drop_column(&self, table: &TableDef, column: &str) -> String {
        let has_default = table.column(column).map(|c| c.default.is_some()).unwrap_or(true);
        let drop = format!("ALTER TABLE {} DROP COLUMN {}", tname(table), ident(column));
        if has_default { format!("{}\n{drop}", drop_default_sql(table, column)) } else { drop }
    }

    fn create_index(&self, table: &TableDef, index: &IndexDef) -> String {
        create_index_sql(table, index)
    }

    fn drop_index(&self, table: &TableDef, index: &IndexDef) -> String {
        format!("DROP INDEX {} ON {}", quote(&index.name), tname(table))
    }

    fn add_primary_key(&self, table: &TableDef, key: &KeyDef) -> String {
        format!("ALTER TABLE {} ADD {}", tname(table), pk_clause(table, key))
    }

    fn drop_primary_key(&self, table: &TableDef, key: &KeyDef) -> String {
        match &key.name {
            Some(n) => format!("ALTER TABLE {} DROP CONSTRAINT {}", tname(table), quote(n)),
            None => drop_key_dynamic(table, "PK"),
        }
    }

    fn add_unique_key(&self, table: &TableDef, key: &KeyDef) -> String {
        format!("ALTER TABLE {} ADD {}", tname(table), unique_clause(table, key))
    }

    fn drop_unique_key(&self, table: &TableDef, key: &KeyDef) -> String {
        match &key.name {
            Some(n) => format!("ALTER TABLE {} DROP CONSTRAINT {}", tname(table), quote(n)),
            None => drop_key_dynamic(table, "UQ"),
        }
    }

    fn add_check(&self, table: &TableDef, check: &CheckDef) -> String {
        format!("ALTER TABLE {} ADD {}", tname(table), check_clause(table, check))
    }

    fn drop_check(&self, table: &TableDef, check: &CheckDef) -> String {
        let name = check.name.clone().unwrap_or_else(|| format!("CK_{}", table.name));
        format!("ALTER TABLE {} DROP CONSTRAINT {}", tname(table), quote(&name))
    }

    fn add_foreign_key(&self, table: &TableDef, fk: &ForeignKeyDef) -> String {
        format!("ALTER TABLE {} ADD {}", tname(table), fk_clause(table, fk))
    }

    fn drop_foreign_key(&self, table: &TableDef, fk: &ForeignKeyDef) -> String {
        let name = fk.name.clone().unwrap_or_else(|| format!("FK_{}_{}", table.name, fk.ref_table));
        format!("ALTER TABLE {} DROP CONSTRAINT {}", tname(table), quote(&name))
    }

    fn create_trigger(&self, _table: &TableDef, trigger: &TriggerDef) -> Vec<String> {
        let def = trigger.definition.trim();
        if def.is_empty() { Vec::new() } else { vec![strip_trailing_semicolon(def).to_string()] }
    }

    fn drop_trigger(&self, table: &TableDef, trigger: &TriggerDef) -> String {
        format!("DROP TRIGGER {}", qualified(Some(schema_or_dbo(table.schema.as_deref())), &trigger.name))
    }

    fn create_view(&self, view: &ViewDef, or_replace: bool) -> Vec<String> {
        let mut out = vec![format!(
            "CREATE {}VIEW {} AS\n{}",
            if or_replace { "OR ALTER " } else { "" },
            qualified(view.schema.as_deref(), &view.name),
            strip_trailing_semicolon(&view.definition)
        )];
        if let Some(c) = view.comment.as_deref().filter(|c| !c.is_empty()) {
            out.push(ext_property("add", view.schema.as_deref(), "VIEW", &view.name, None, Some(c)));
        }
        out
    }

    fn drop_view(&self, view: &ViewDef) -> String {
        format!("DROP VIEW {}", qualified(view.schema.as_deref(), &view.name))
    }

    fn create_sequence(&self, seq: &SequenceDef) -> String {
        let mut s = format!("CREATE SEQUENCE {}", qualified(seq.schema.as_deref(), &seq.name));
        if let Some(t) = seq.data_type.as_deref().filter(|t| !t.is_empty()) {
            s.push_str(&format!(" AS {t}"));
        }
        if let Some(v) = seq.start {
            s.push_str(&format!(" START WITH {v}"));
        }
        if let Some(v) = seq.increment {
            s.push_str(&format!(" INCREMENT BY {v}"));
        }
        if let Some(v) = seq.min_value {
            s.push_str(&format!(" MINVALUE {v}"));
        }
        if let Some(v) = seq.max_value {
            s.push_str(&format!(" MAXVALUE {v}"));
        }
        s.push_str(if seq.cycle { " CYCLE" } else { " NO CYCLE" });
        match seq.cache {
            Some(c) if c > 0 => s.push_str(&format!(" CACHE {c}")),
            Some(_) => s.push_str(" NO CACHE"),
            None => {}
        }
        s
    }

    fn alter_sequence(&self, from: &SequenceDef, to: &SequenceDef) -> Vec<String> {
        let mut parts = Vec::new();
        if from.start != to.start
            && let Some(v) = to.start
        {
            parts.push(format!("RESTART WITH {v}"));
        }
        if from.increment != to.increment
            && let Some(v) = to.increment
        {
            parts.push(format!("INCREMENT BY {v}"));
        }
        if from.min_value != to.min_value {
            parts.push(to.min_value.map(|v| format!("MINVALUE {v}")).unwrap_or_else(|| "NO MINVALUE".into()));
        }
        if from.max_value != to.max_value {
            parts.push(to.max_value.map(|v| format!("MAXVALUE {v}")).unwrap_or_else(|| "NO MAXVALUE".into()));
        }
        if from.cycle != to.cycle {
            parts.push(if to.cycle { "CYCLE".into() } else { "NO CYCLE".into() });
        }
        if from.cache != to.cache {
            parts.push(match to.cache {
                Some(c) if c > 0 => format!("CACHE {c}"),
                _ => "NO CACHE".into(),
            });
        }
        if parts.is_empty() { Vec::new() } else { vec![format!("ALTER SEQUENCE {} {}", qualified(to.schema.as_deref(), &to.name), parts.join(" "))] }
    }

    fn drop_sequence(&self, seq: &SequenceDef) -> String {
        format!("DROP SEQUENCE {}", qualified(seq.schema.as_deref(), &seq.name))
    }

    fn create_routine(&self, routine: &RoutineDef) -> Vec<String> {
        let def = routine.definition.trim();
        if def.is_empty() { Vec::new() } else { vec![strip_trailing_semicolon(def).to_string()] }
    }

    fn drop_routine(&self, routine: &RoutineDef) -> String {
        let kind = match routine.kind {
            Some(ObjectKind::Procedure) => "PROCEDURE",
            Some(ObjectKind::Aggregate) => "AGGREGATE",
            _ => "FUNCTION",
        };
        format!("DROP {kind} {}", qualified(routine.schema.as_deref(), &routine.name))
    }

    fn create_type(&self, ty: &TypeDef) -> Vec<String> {
        if ty.definition.trim().is_empty() { Vec::new() } else { vec![ty.definition.trim().to_string()] }
    }

    fn drop_type(&self, ty: &TypeDef) -> Option<String> {
        Some(format!("DROP TYPE {}", qualified(ty.schema.as_deref(), &ty.name)))
    }

    fn create_schema(&self, name: &str) -> String {
        format!("CREATE SCHEMA {}", quote(name))
    }

    fn drop_schema(&self, name: &str, _cascade: bool) -> String {
        format!("DROP SCHEMA {}", quote(name))
    }

    fn map_type(&self, source: DbKind, data_type: &str, kind: DataKind) -> String {
        if source == DbKind::SqlServer {
            return data_type.to_string();
        }
        map_foreign_type(source, data_type, kind).unwrap_or_else(|| self.default_type_for(kind))
    }

    fn default_type_for(&self, kind: DataKind) -> String {
        match kind {
            DataKind::Bool => "bit",
            DataKind::Int => "bigint",
            DataKind::Float => "float",
            DataKind::Decimal => "decimal(38,10)",
            DataKind::Binary => "varbinary(max)",
            DataKind::Date => "date",
            DataKind::Time | DataKind::TimeTz => "time",
            DataKind::DateTime => "datetime2",
            DataKind::DateTimeTz => "datetimeoffset",
            DataKind::Uuid => "uniqueidentifier",
            DataKind::Xml => "xml",
            DataKind::Geometry => "geometry",
            _ => "nvarchar(max)",
        }
        .to_string()
    }
}

/// Split type name of another product.
struct Foreign {
    base: String,
    args: Vec<String>,
    array: bool,
    unsigned: bool,
}

fn parse_foreign(data_type: &str) -> Foreign {
    let mut t = data_type.trim().to_ascii_lowercase();
    let mut array = false;
    while let Some(stripped) = t.trim_end().strip_suffix(']') {
        match stripped.rfind('[') {
            Some(i) => {
                t = stripped[..i].to_string();
                array = true;
            }
            None => break,
        }
    }
    if t.starts_with('_') && t.len() > 1 {
        array = true;
        t = t[1..].to_string();
    }
    if t.starts_with("array") || t.starts_with("list") {
        array = true;
    }
    let mut args = Vec::new();
    let mut words = t.clone();
    if let Some(open) = t.find('(') {
        let close = t[open..].find(')').map(|c| open + c).unwrap_or(t.len());
        args = t[open + 1..close].split(',').map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect();
        words = format!("{} {}", &t[..open], if close < t.len() { &t[close + 1..] } else { "" });
    }
    let mut unsigned = false;
    let parts: Vec<&str> = words
        .split_whitespace()
        .filter(|w| {
            if *w == "unsigned" {
                unsigned = true;
                false
            } else {
                !matches!(*w, "signed" | "zerofill")
            }
        })
        .collect();
    Foreign { base: parts.join(" "), args, array, unsigned }
}

fn arg(f: &Foreign, i: usize) -> Option<u32> {
    f.args.get(i).and_then(|a| a.parse().ok())
}

/// Length argument for character types: `max`, a number, or none.
fn char_len(f: &Foreign, limit: u32) -> String {
    match f.args.first().map(String::as_str) {
        Some("max") => "max".into(),
        Some(n) => match n.parse::<u32>() {
            Ok(n) if n > 0 && n <= limit => n.to_string(),
            _ => "max".into(),
        },
        None => "max".into(),
    }
}

/// Map a type of another product to SQL Server; `None` = use the kind default.
pub fn map_foreign_type(source: DbKind, data_type: &str, kind: DataKind) -> Option<String> {
    let f = parse_foreign(data_type);
    if f.array || kind == DataKind::Array {
        return Some("nvarchar(max)".into());
    }
    let base = f.base.as_str();
    // ClickHouse wrappers
    if source == DbKind::ClickHouse {
        let inner = data_type.trim();
        for w in ["Nullable(", "LowCardinality("] {
            if let Some(rest) = inner.strip_prefix(w).and_then(|r| r.strip_suffix(')')) {
                return map_foreign_type(source, rest, kind);
            }
        }
        let mapped = match base {
            "uint8" => "tinyint",
            "int8" | "int16" => "smallint",
            "uint16" | "int32" => "int",
            "uint32" | "int64" => "bigint",
            "uint64" => "decimal(20,0)",
            "int128" | "uint128" | "int256" | "uint256" => "decimal(38,0)",
            "float32" => "real",
            "float64" => "float",
            "string" => "nvarchar(max)",
            "uuid" => "uniqueidentifier",
            "date" | "date32" => "date",
            "datetime" => "datetime2(0)",
            "bool" => "bit",
            "ipv4" | "ipv6" => "varchar(45)",
            "enum8" | "enum16" => "nvarchar(255)",
            _ => "",
        };
        if !mapped.is_empty() {
            return Some(mapped.into());
        }
        if base == "datetime64" {
            let p = arg(&f, 0).unwrap_or(3).min(7);
            return Some(if f.args.len() > 1 { format!("datetimeoffset({p})") } else { format!("datetime2({p})") });
        }
        if base == "fixedstring" {
            return Some(format!("nvarchar({})", char_len(&f, 4000)));
        }
    }
    Some(match base {
        "bool" | "boolean" => "bit".into(),
        "bit" if f.args.is_empty() || f.args.first().map(String::as_str) == Some("1") => "bit".into(),
        "bit" | "varbit" | "bit varying" => "varbinary(max)".into(),
        "tinyint" | "int1" => {
            if source.is_mysql_family() && f.args.first().map(String::as_str) == Some("1") {
                "bit".into()
            } else if f.unsigned || matches!(source, DbKind::DuckDb) && base == "utinyint" {
                "tinyint".into()
            } else {
                "smallint".into()
            }
        }
        "utinyint" => "tinyint".into(),
        "smallint" | "int2" | "smallserial" | "serial2" => if f.unsigned { "int" } else { "smallint" }.into(),
        "usmallint" => "int".into(),
        "mediumint" => "int".into(),
        "int" | "integer" | "int4" | "serial" | "serial4" => if f.unsigned { "bigint" } else { "int" }.into(),
        "uinteger" => "bigint".into(),
        "bigint" | "int8" | "bigserial" | "serial8" => if f.unsigned { "decimal(20,0)" } else { "bigint" }.into(),
        "ubigint" => "decimal(20,0)".into(),
        "hugeint" | "uhugeint" => "decimal(38,0)".into(),
        "real" | "float4" => "real".into(),
        "float" => match source {
            DbKind::MySql | DbKind::MariaDb => "real".into(),
            _ => "float".into(),
        },
        "double" | "double precision" | "float8" => "float".into(),
        "numeric" | "decimal" | "dec" | "number" | "fixed" => match (arg(&f, 0), arg(&f, 1)) {
            (Some(p), s) => {
                let p = p.clamp(1, 38);
                format!("decimal({p},{})", s.unwrap_or(0).min(p))
            }
            (None, _) => "decimal(38,10)".into(),
        },
        "money" => "money".into(),
        "char" | "character" | "nchar" | "bpchar" => format!("nchar({})", char_len(&f, 4000)).replace("nchar(max)", "nvarchar(max)"),
        "varchar" | "character varying" | "nvarchar" | "varchar2" | "nvarchar2" => {
            format!("nvarchar({})", char_len(&f, 4000))
        }
        "text" | "tinytext" | "mediumtext" | "longtext" | "clob" | "citext" | "string" | "ntext" => "nvarchar(max)".into(),
        "name" => "sysname".into(),
        "uuid" => "uniqueidentifier".into(),
        "date" => "date".into(),
        "time" | "time without time zone" | "timetz" | "time with time zone" => {
            format!("time({})", arg(&f, 0).unwrap_or(7).min(7))
        }
        "timestamp" | "timestamp without time zone" | "datetime" => {
            let p = arg(&f, 0).unwrap_or(if base == "datetime" { 0 } else { 6 }).min(7);
            if base == "datetime" && f.args.is_empty() && source.is_mysql_family() { "datetime2(0)".into() } else { format!("datetime2({p})") }
        }
        "timestamptz" | "timestamp with time zone" => format!("datetimeoffset({})", arg(&f, 0).unwrap_or(6).min(7)),
        "interval" => "nvarchar(100)".into(),
        "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "raw" | "bytes" => "varbinary(max)".into(),
        "binary" => format!("binary({})", char_len(&f, 8000)).replace("binary(max)", "varbinary(max)"),
        "varbinary" => format!("varbinary({})", char_len(&f, 8000)),
        "json" | "jsonb" | "struct" | "map" | "union" => "nvarchar(max)".into(),
        "xml" => "xml".into(),
        "inet" | "cidr" | "macaddr" | "macaddr8" => "nvarchar(50)".into(),
        "enum" | "set" => "nvarchar(255)".into(),
        "year" => "smallint".into(),
        "geometry" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring" | "multipolygon" | "geometrycollection" | "box" | "circle" | "path" | "line"
        | "lseg" => "geometry".into(),
        "geography" => "geography".into(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TableDef {
        let mut t = TableDef::new(Some("dbo".into()), "orders");
        t.columns.push(ColumnDef::new("id", "int").not_null().auto_increment());
        let mut name = ColumnDef::new("name", "nvarchar(100)").default_value("(N'x')");
        name.comment = Some("Order name".into());
        name.collation = Some("Latin1_General_CI_AS".into());
        t.columns.push(name);
        t.columns.push(ColumnDef::new("qty", "int").not_null().default_value("((0))"));
        t.columns.push(ColumnDef::new("price", "decimal(10,2)").not_null());
        let mut total = ColumnDef::new("total", "decimal(21,2)");
        total.generated = Some("([qty]*[price]) PERSISTED".into());
        t.columns.push(total);
        t.columns.push(ColumnDef::new("customer_id", "int"));
        t.primary_key = Some(KeyDef { name: Some("PK_orders".into()), columns: vec!["id".into()] });
        t.unique_keys.push(KeyDef { name: Some("UQ_orders_name".into()), columns: vec!["name".into()] });
        t.checks.push(CheckDef { name: Some("CK_orders_qty".into()), expression: "([qty]>=(0))".into() });
        t.foreign_keys.push(ForeignKeyDef {
            name: Some("FK_orders_customers".into()),
            columns: vec!["customer_id".into()],
            ref_schema: Some("dbo".into()),
            ref_table: "customers".into(),
            ref_columns: vec!["id".into()],
            on_delete: FkAction::Cascade,
            on_update: FkAction::Restrict,
            ..Default::default()
        });
        t.indexes.push(IndexDef {
            name: "PK_orders".into(),
            primary: true,
            unique: true,
            method: Some("CLUSTERED".into()),
            columns: vec![IndexColumn::column("id")],
            ..Default::default()
        });
        t.indexes.push(IndexDef {
            name: "IX_orders_customer".into(),
            columns: vec![IndexColumn::column("customer_id"), IndexColumn { name: "qty".into(), is_expression: false, descending: true }],
            include: vec!["price".into()],
            predicate: Some("([customer_id] IS NOT NULL)".into()),
            method: Some("NONCLUSTERED".into()),
            ..Default::default()
        });
        t.options.insert(default_constraint_key("qty"), "DF_orders_qty_custom".into());
        t.options.insert(identity_key("id"), "100,5".into());
        t.comment = Some("All orders".into());
        t
    }

    #[test]
    fn create_table() {
        let d = MsSqlDdl::new();
        let sql = d.create_table(&sample(), true);
        let s = &sql[0];
        assert!(s.starts_with("CREATE TABLE dbo.orders ("), "{s}");
        assert!(s.contains("id int IDENTITY(100,5) NOT NULL"), "{s}");
        assert!(s.contains("name nvarchar(100) COLLATE Latin1_General_CI_AS NULL CONSTRAINT [DF_orders_name] DEFAULT (N'x')"), "{s}");
        assert!(s.contains("qty int NOT NULL CONSTRAINT [DF_orders_qty_custom] DEFAULT (0)"), "{s}");
        assert!(s.contains("total AS ([qty]*[price]) PERSISTED"), "{s}");
        assert!(s.contains("CONSTRAINT [PK_orders] PRIMARY KEY CLUSTERED (id)"), "{s}");
        assert!(s.contains("CONSTRAINT [UQ_orders_name] UNIQUE (name)"), "{s}");
        assert!(s.contains("CONSTRAINT [CK_orders_qty] CHECK ([qty]>=(0))"), "{s}");
        assert!(s.contains("CONSTRAINT [FK_orders_customers] FOREIGN KEY (customer_id) REFERENCES dbo.customers (id) ON DELETE CASCADE"), "{s}");
        assert!(!s.contains("ON UPDATE"), "{s}");
        assert_eq!(sql[1], "CREATE NONCLUSTERED INDEX [IX_orders_customer] ON dbo.orders (customer_id, qty DESC) INCLUDE (price) WHERE [customer_id] IS NOT NULL");
        assert!(sql[2].starts_with("EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'All orders', @level0type = N'SCHEMA', @level0name = N'dbo', @level1type = N'TABLE', @level1name = N'orders'"), "{}", sql[2]);
        assert!(sql[3].ends_with("@level2type = N'COLUMN', @level2name = N'name'"), "{}", sql[3]);
        assert_eq!(sql.len(), 4);
        let no_fk = d.create_table(&sample(), false);
        assert!(!no_fk[0].contains("FOREIGN KEY"));
    }

    #[test]
    fn referencing_foreign_keys_around_a_key_change() {
        let d = MsSqlDdl::new();
        let mut from = TableDef::new(Some("dbo".into()), "customers");
        from.columns = vec![ColumnDef::new("id", "int").not_null(), ColumnDef::new("name", "nvarchar(50)")];
        from.primary_key = Some(KeyDef { name: Some("PK_customers".into()), columns: vec!["id".into()] });
        from.referenced_by.push(ForeignKeyDef {
            name: Some("FK_orders_customers".into()),
            schema: Some("dbo".into()),
            table: "orders".into(),
            columns: vec!["customer_id".into()],
            ref_schema: Some("dbo".into()),
            ref_table: "customers".into(),
            ref_columns: vec!["id".into()],
            ..Default::default()
        });
        from.columns[0] = ColumnDef::new("id", "nvarchar(10)").not_null();
        let mut to = from.clone();
        to.columns[0].collation = Some("Latin1_General_BIN2".into());
        let sql = d.alter_table(&from, &to, &[]);
        let pos = |p: &str| sql.iter().position(|s| s.contains(p)).unwrap_or_else(|| panic!("{p} in {sql:#?}"));
        assert!(!sql[0].starts_with("THROW"), "{sql:#?}");
        assert!(pos("orders DROP CONSTRAINT [FK_orders_customers]") < pos("DROP CONSTRAINT [PK_customers]"), "{sql:#?}");
        assert!(pos("ADD CONSTRAINT [PK_customers]") < pos("orders ADD CONSTRAINT [FK_orders_customers]"), "{sql:#?}");
        // The referencing column keeps its type: the foreign key could not be added back.
        let mut retyped = from.clone();
        retyped.columns[0].data_type = "nvarchar(20)".into();
        assert!(d.alter_table(&from, &retyped, &[])[0].starts_with("THROW 50000"));
        let mut no_key = from.clone();
        no_key.primary_key = None;
        assert!(d.alter_table(&from, &no_key, &[])[0].contains("FK_orders_customers (orders)"));
    }

    #[test]
    fn alter_table_ordering() {
        let d = MsSqlDdl::new();
        let from = sample();
        let mut to = from.clone();
        // change type of qty (used by index, check and default)
        to.columns[2].data_type = "bigint".into();
        // rename name -> title, change its comment
        to.columns[1].name = "title".into();
        to.columns[1].comment = Some("Title".into());
        to.unique_keys[0].columns = vec!["title".into()];
        // add a column, drop customer_id (and its FK / index)
        to.columns.retain(|c| c.name != "customer_id");
        to.foreign_keys.clear();
        to.indexes.retain(|i| i.name != "IX_orders_customer");
        to.columns.push(ColumnDef::new("note", "nvarchar(max)"));
        to.comment = None;
        let stmts = d.alter_table(&from, &to, &[("name".into(), "title".into())]);
        let pos = |needle: &str| stmts.iter().position(|s| s.contains(needle)).unwrap_or_else(|| panic!("{needle} not in {stmts:#?}"));
        let drop_fk = pos("DROP CONSTRAINT [FK_orders_customers]");
        let drop_idx = pos("DROP INDEX [IX_orders_customer]");
        let drop_check = pos("DROP CONSTRAINT [CK_orders_qty]");
        let drop_df_qty = pos("N'qty'\nIF");
        let drop_col = pos("DROP COLUMN customer_id");
        let rename = pos("EXEC sp_rename N'[dbo].[orders].[name]', N'title', N'COLUMN'");
        let alter = pos("ALTER COLUMN qty bigint NOT NULL");
        let add_df = pos("ADD CONSTRAINT [DF_orders_qty_custom] DEFAULT (0) FOR qty");
        let add_col = pos("ADD note nvarchar(max) NULL");
        let add_check = pos("ADD CONSTRAINT [CK_orders_qty] CHECK");
        let drop_comment = pos("sp_dropextendedproperty");
        let upd_comment = pos("sp_updateextendedproperty");
        assert!(drop_fk < drop_col && drop_idx < drop_col);
        assert!(drop_check < alter && drop_df_qty < alter);
        assert!(rename < alter && alter < add_df && add_df < add_col && add_col < add_check);
        assert!(add_check < drop_comment && upd_comment > rename);
        // the unique key changed because of the rename: dropped and re-added
        assert!(pos("DROP CONSTRAINT [UQ_orders_name]") < pos("ADD CONSTRAINT [UQ_orders_name] UNIQUE (title)"));
        assert!(!stmts.iter().any(|s| s.contains("CREATE NONCLUSTERED INDEX")));
    }

    #[test]
    fn small_statements() {
        let d = MsSqlDdl::new();
        let t = sample();
        assert_eq!(d.rename_table(Some("dbo"), "a", "b"), "EXEC sp_rename N'[dbo].[a]', N'b'");
        assert!(d.drop_column(&t, "qty").starts_with("DECLARE @dbiji_df_"));
        assert_eq!(d.drop_column(&t, "price"), "ALTER TABLE dbo.orders DROP COLUMN price");
        assert_eq!(d.drop_index(&t, &t.indexes[1]), "DROP INDEX [IX_orders_customer] ON dbo.orders");
        assert!(d.drop_primary_key(&t, &KeyDef::default()).contains("type = 'PK'"));
        let cs = IndexDef { name: "CCI".into(), method: Some("CLUSTERED COLUMNSTORE".into()), ..Default::default() };
        assert_eq!(d.create_index(&t, &cs), "CREATE CLUSTERED COLUMNSTORE INDEX [CCI] ON dbo.orders");
        let seq = SequenceDef {
            schema: Some("dbo".into()),
            name: "s".into(),
            data_type: Some("bigint".into()),
            start: Some(1),
            increment: Some(1),
            cache: Some(0),
            ..Default::default()
        };
        assert_eq!(d.create_sequence(&seq), "CREATE SEQUENCE dbo.s AS bigint START WITH 1 INCREMENT BY 1 NO CYCLE NO CACHE");
        let mut seq2 = seq.clone();
        seq2.increment = Some(5);
        seq2.cycle = true;
        assert_eq!(d.alter_sequence(&seq, &seq2), vec!["ALTER SEQUENCE dbo.s INCREMENT BY 5 CYCLE"]);
        let v = ViewDef { schema: Some("dbo".into()), name: "v".into(), definition: "SELECT 1 AS a;".into(), ..Default::default() };
        assert_eq!(d.create_view(&v, true), vec!["CREATE OR ALTER VIEW dbo.v AS\nSELECT 1 AS a"]);
        assert_eq!(d.create_schema("sales"), "CREATE SCHEMA [sales]");
        let r = RoutineDef { schema: Some("dbo".into()), name: "p".into(), kind: Some(ObjectKind::Procedure), ..Default::default() };
        assert_eq!(d.drop_routine(&r), "DROP PROCEDURE dbo.p");
    }

    #[test]
    fn type_mapping() {
        let d = MsSqlDdl::new();
        let m = |src, t: &str| d.map_type(src, t, DataKind::guess_from_type_name(t));
        assert_eq!(m(DbKind::Postgres, "text"), "nvarchar(max)");
        assert_eq!(m(DbKind::Postgres, "character varying(50)"), "nvarchar(50)");
        assert_eq!(m(DbKind::Postgres, "varchar(10000)"), "nvarchar(max)");
        assert_eq!(m(DbKind::Postgres, "boolean"), "bit");
        assert_eq!(m(DbKind::Postgres, "timestamp with time zone"), "datetimeoffset(6)");
        assert_eq!(m(DbKind::Postgres, "timestamptz"), "datetimeoffset(6)");
        assert_eq!(m(DbKind::Postgres, "timestamp(3) without time zone"), "datetime2(3)");
        assert_eq!(m(DbKind::Postgres, "uuid"), "uniqueidentifier");
        assert_eq!(m(DbKind::Postgres, "bytea"), "varbinary(max)");
        assert_eq!(m(DbKind::Postgres, "jsonb"), "nvarchar(max)");
        assert_eq!(m(DbKind::Postgres, "serial"), "int");
        assert_eq!(m(DbKind::Postgres, "numeric(12,4)"), "decimal(12,4)");
        assert_eq!(m(DbKind::Postgres, "numeric"), "decimal(38,10)");
        assert_eq!(m(DbKind::Postgres, "integer[]"), "nvarchar(max)");
        assert_eq!(m(DbKind::MySql, "blob"), "varbinary(max)");
        assert_eq!(m(DbKind::MySql, "tinyint(1)"), "bit");
        assert_eq!(m(DbKind::MySql, "tinyint"), "smallint");
        assert_eq!(m(DbKind::MySql, "int unsigned"), "bigint");
        assert_eq!(m(DbKind::MySql, "datetime"), "datetime2(0)");
        assert_eq!(m(DbKind::MySql, "double"), "float");
        assert_eq!(m(DbKind::Sqlite, "INTEGER"), "int");
        assert_eq!(m(DbKind::ClickHouse, "Nullable(UInt64)"), "decimal(20,0)");
        assert_eq!(m(DbKind::ClickHouse, "DateTime64(3, 'UTC')"), "datetimeoffset(3)");
        assert_eq!(m(DbKind::ClickHouse, "LowCardinality(String)"), "nvarchar(max)");
        assert_eq!(m(DbKind::SqlServer, "nvarchar(42)"), "nvarchar(42)");
        assert_eq!(d.map_type(DbKind::Postgres, "mystery", DataKind::Json), "nvarchar(max)");
        assert_eq!(d.default_type_for(DataKind::DateTimeTz), "datetimeoffset");
        assert_eq!(d.default_type_for(DataKind::Bool), "bit");
    }
}
