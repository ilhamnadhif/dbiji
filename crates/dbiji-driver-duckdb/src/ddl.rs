//! DDL generation for DuckDB.
//!
//! DuckDB follows PostgreSQL syntax with some restrictions: no triggers, only
//! `VIRTUAL` generated columns, foreign keys without cascading actions, no
//! partial indexes, and `ALTER TABLE` cannot add or drop UNIQUE / CHECK /
//! FOREIGN KEY constraints. [`DuckDbDdl::alter_table`] uses native
//! `ALTER TABLE` statements for column changes and rebuilds the table
//! (`CREATE` + `INSERT ... SELECT` + `DROP` + `RENAME`, in one transaction)
//! for constraint changes and column reordering. Auto-increment columns are
//! backed by a sequence (`DEFAULT nextval('<table>_<column>_seq')`).

use crate::dialect::{qualified, quote_ident, string_literal};
use dbiji_core::config::DbKind;
use dbiji_core::ddl::*;
use dbiji_core::dialect::ansi;
use dbiji_core::value::DataKind;

/// Prefix of the temporary table used by table rebuilds.
const REBUILD_PREFIX: &str = "__dbiji_new_";

fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn q_list(cols: &[String]) -> String {
    cols.iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ")
}

fn tname(t: &TableDef) -> String {
    qualified(t.schema.as_deref(), &t.name)
}

/// Sequence backing an auto-increment column.
fn sequence_name(table: &str, column: &str) -> String {
    format!("{table}_{column}_seq")
}

fn sequence_default(t: &TableDef, c: &ColumnDef) -> String {
    let seq = qualified(t.schema.as_deref(), &sequence_name(&t.name, &c.name));
    format!("nextval({})", string_literal(&seq))
}

fn column_sql(t: &TableDef, c: &ColumnDef) -> String {
    let mut s = quote_ident(&c.name);
    let ty = c.data_type.trim();
    if !ty.is_empty() {
        s.push(' ');
        s.push_str(ty);
    }
    if let Some(g) = c.generated.as_deref().filter(|g| !g.trim().is_empty()) {
        s.push_str(&format!(" GENERATED ALWAYS AS ({}) VIRTUAL", g.trim()));
    } else if let Some(d) = c.default.as_deref().filter(|d| !d.trim().is_empty()) {
        s.push_str(&format!(" DEFAULT {}", d.trim()));
    } else if c.auto_increment {
        s.push_str(&format!(" DEFAULT {}", sequence_default(t, c)));
    }
    if !c.nullable {
        s.push_str(" NOT NULL");
    }
    if let Some(coll) = c.collation.as_deref().filter(|c| !c.trim().is_empty()) {
        s.push_str(&format!(" COLLATE {}", quote_ident(coll.trim())));
    }
    s
}

fn key_sql(kind: &str, k: &KeyDef) -> String {
    match k.name.as_deref().filter(|n| !n.is_empty()) {
        Some(n) => format!(
            "CONSTRAINT {} {kind} ({})",
            quote_ident(n),
            q_list(&k.columns)
        ),
        None => format!("{kind} ({})", q_list(&k.columns)),
    }
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

/// FOREIGN KEY clause. DuckDB only supports NO ACTION / RESTRICT, so
/// cascading actions are not emitted.
fn fk_sql(fk: &ForeignKeyDef) -> String {
    let mut s = String::new();
    if let Some(n) = fk.name.as_deref().filter(|n| !n.is_empty()) {
        s.push_str(&format!("CONSTRAINT {} ", quote_ident(n)));
    }
    s.push_str(&format!(
        "FOREIGN KEY ({}) REFERENCES {}",
        q_list(&fk.columns),
        qualified(fk.ref_schema.as_deref(), &fk.ref_table)
    ));
    if !fk.ref_columns.is_empty() {
        s.push_str(&format!(" ({})", q_list(&fk.ref_columns)));
    }
    if fk.on_delete == FkAction::Restrict {
        s.push_str(" ON DELETE RESTRICT");
    }
    if fk.on_update == FkAction::Restrict {
        s.push_str(" ON UPDATE RESTRICT");
    }
    s
}

fn create_table_sql(t: &TableDef, include_fks: bool) -> String {
    let mut lines: Vec<String> = t.columns.iter().map(|c| column_sql(t, c)).collect();
    if let Some(pk) = t.primary_key.as_ref().filter(|k| !k.columns.is_empty()) {
        lines.push(key_sql("PRIMARY KEY", pk));
    }
    for u in &t.unique_keys {
        lines.push(key_sql("UNIQUE", u));
    }
    for c in &t.checks {
        lines.push(check_sql(c));
    }
    if include_fks {
        for fk in &t.foreign_keys {
            lines.push(fk_sql(fk));
        }
    }
    let temp = if t.table_kind == TableKind::Temporary {
        "TEMP "
    } else {
        ""
    };
    format!(
        "CREATE {temp}TABLE {} (\n    {}\n)",
        tname(t),
        lines.join(",\n    ")
    )
}

/// Indexes not implied by the primary key or a UNIQUE constraint.
fn secondary_indexes(t: &TableDef) -> impl Iterator<Item = &IndexDef> {
    t.indexes.iter().filter(move |i| {
        if i.primary {
            return false;
        }
        let cols: Vec<String> = i.columns.iter().map(|c| c.name.clone()).collect();
        !(i.unique
            && t.unique_keys.iter().any(|u| {
                u.name.as_deref().is_some_and(|n| eq_ci(n, &i.name))
                    || (u.columns.len() == cols.len()
                        && u.columns.iter().zip(&cols).all(|(a, b)| eq_ci(a, b)))
            }))
    })
}

fn index_sql(table: &TableDef, idx: &IndexDef) -> String {
    if idx
        .predicate
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty())
        && idx.unique
    {
        return format!(
            "-- skipped unique index {}: DuckDB does not support partial indexes (WHERE {})",
            idx.name,
            idx.predicate.as_deref().unwrap_or("")
        );
    }
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
        format!("idx_{}_{}", table.name, cols.join("_"))
    } else {
        idx.name.clone()
    };
    let cols = idx
        .columns
        .iter()
        .map(|c| {
            let mut s = if c.is_expression {
                format!("({})", c.name.trim())
            } else {
                quote_ident(&c.name)
            };
            if c.descending {
                s.push_str(" DESC");
            }
            s
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE {}INDEX {} ON {} ({cols})",
        if idx.unique { "UNIQUE " } else { "" },
        quote_ident(&name),
        tname(table)
    )
}

fn comment_statements(t: &TableDef) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(c) = t.comment.as_deref().filter(|c| !c.is_empty()) {
        out.push(format!(
            "COMMENT ON TABLE {} IS {}",
            tname(t),
            string_literal(c)
        ));
    }
    for col in &t.columns {
        if let Some(c) = col.comment.as_deref().filter(|c| !c.is_empty()) {
            out.push(format!(
                "COMMENT ON COLUMN {}.{} IS {}",
                tname(t),
                quote_ident(&col.name),
                string_literal(c)
            ));
        }
    }
    out
}

fn sequence_statements(t: &TableDef) -> Vec<String> {
    t.columns
        .iter()
        .filter(|c| c.auto_increment && c.default.is_none() && c.generated.is_none())
        .map(|c| {
            format!(
                "CREATE SEQUENCE IF NOT EXISTS {}",
                qualified(t.schema.as_deref(), &sequence_name(&t.name, &c.name))
            )
        })
        .collect()
}

fn same_key(a: &KeyDef, b: &KeyDef, map: &dyn Fn(&str) -> String) -> bool {
    a.name.as_deref().unwrap_or("") == b.name.as_deref().unwrap_or("")
        && a.columns.len() == b.columns.len()
        && a.columns
            .iter()
            .zip(&b.columns)
            .all(|(x, y)| eq_ci(&map(x), y))
}

fn same_fk(a: &ForeignKeyDef, b: &ForeignKeyDef, map: &dyn Fn(&str) -> String) -> bool {
    a.name.as_deref().unwrap_or("") == b.name.as_deref().unwrap_or("")
        && a.columns.len() == b.columns.len()
        && a.columns
            .iter()
            .zip(&b.columns)
            .all(|(x, y)| eq_ci(&map(x), y))
        && eq_ci(&a.ref_table, &b.ref_table)
        && a.ref_columns.len() == b.ref_columns.len()
        && a.ref_columns
            .iter()
            .zip(&b.ref_columns)
            .all(|(x, y)| eq_ci(x, y))
}

fn same_index(a: &IndexDef, b: &IndexDef, map: &dyn Fn(&str) -> String) -> bool {
    eq_ci(&a.name, &b.name)
        && a.unique == b.unique
        && a.columns.len() == b.columns.len()
        && a.columns.iter().zip(&b.columns).all(|(x, y)| {
            x.is_expression == y.is_expression
                && x.descending == y.descending
                && if x.is_expression {
                    squash(&x.name) == squash(&y.name)
                } else {
                    eq_ci(&map(&x.name), &y.name)
                }
        })
}

/// DuckDB DDL generator.
#[derive(Debug, Default, Clone, Copy)]
pub struct DuckDbDdl;

impl DuckDbDdl {
    /// Create the generator.
    pub fn new() -> Self {
        DuckDbDdl
    }

    /// Rebuild `from` as `to`: create the new table, copy the data, drop the
    /// old one, rename, recreate indexes and comments.
    fn rebuild(
        &self,
        from: &TableDef,
        to: &TableDef,
        pairs: &[(&ColumnDef, &ColumnDef)],
    ) -> Vec<String> {
        let mut tmp = to.clone();
        tmp.schema = from.schema.clone();
        tmp.name = format!("{REBUILD_PREFIX}{}", from.name);
        let mut out = vec!["BEGIN TRANSACTION".to_string()];
        out.extend(sequence_statements(to));
        // auto-increment defaults must keep pointing at the final table's sequence
        let mut create = tmp.clone();
        for c in &mut create.columns {
            if c.auto_increment && c.default.is_none() && c.generated.is_none() {
                c.default = Some(sequence_default(to, c));
            }
        }
        out.push(create_table_sql(&create, true));
        let targets: Vec<String> = pairs
            .iter()
            .filter(|(_, t)| t.generated.as_deref().is_none_or(|g| g.trim().is_empty()))
            .map(|(_, t)| quote_ident(&t.name))
            .collect();
        let sources: Vec<String> = pairs
            .iter()
            .filter(|(_, t)| t.generated.as_deref().is_none_or(|g| g.trim().is_empty()))
            .map(|(f, _)| quote_ident(&f.name))
            .collect();
        if !targets.is_empty() {
            out.push(format!(
                "INSERT INTO {} ({}) SELECT {} FROM {}",
                tname(&tmp),
                targets.join(", "),
                sources.join(", "),
                tname(from)
            ));
        }
        out.push(format!("DROP TABLE {}", tname(from)));
        out.push(format!(
            "ALTER TABLE {} RENAME TO {}",
            tname(&tmp),
            quote_ident(&to.name)
        ));
        let mut final_t = to.clone();
        final_t.schema = from.schema.clone();
        for idx in secondary_indexes(&final_t) {
            out.push(index_sql(&final_t, idx));
        }
        out.extend(comment_statements(&final_t));
        out.push("COMMIT".to_string());
        out
    }
}

impl DdlGenerator for DuckDbDdl {
    fn create_table(&self, table: &TableDef, include_foreign_keys: bool) -> Vec<String> {
        let mut out = sequence_statements(table);
        out.push(create_table_sql(table, include_foreign_keys));
        for idx in secondary_indexes(table) {
            out.push(index_sql(table, idx));
        }
        out.extend(comment_statements(table));
        out
    }

    fn drop_table(&self, schema: Option<&str>, name: &str, cascade: bool) -> String {
        format!(
            "DROP TABLE {}{}",
            qualified(schema, name),
            if cascade { " CASCADE" } else { "" }
        )
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
        let (pairs, dropped, added) = ansi::match_columns(from, to, renamed_columns);
        let renames: Vec<(String, String)> = pairs
            .iter()
            .filter(|(f, t)| f.name != t.name)
            .map(|(f, t)| (f.name.clone(), t.name.clone()))
            .collect();
        let map = |old: &str| -> String {
            renames
                .iter()
                .find(|(o, _)| eq_ci(o, old))
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| old.to_string())
        };
        // which changes need a rebuild?
        let order: Vec<usize> = pairs
            .iter()
            .filter_map(|(_, t)| to.columns.iter().position(|c| std::ptr::eq(c, *t)))
            .collect();
        let reordered = order.windows(2).any(|w| w[0] > w[1])
            || added.iter().any(|a| {
                let pos = to
                    .columns
                    .iter()
                    .position(|c| std::ptr::eq(c, *a))
                    .unwrap_or(usize::MAX);
                order.iter().any(|&p| p > pos)
            });
        let pk_from = from.primary_key.as_ref().filter(|k| !k.columns.is_empty());
        let pk_to = to.primary_key.as_ref().filter(|k| !k.columns.is_empty());
        let pk_added_only = pk_from.is_none() && pk_to.is_some();
        let pk_changed = match (pk_from, pk_to) {
            (None, None) => false,
            (Some(a), Some(b)) => !same_key(a, b, &map),
            (None, Some(_)) => false,
            (Some(_), None) => true,
        };
        let uniques_changed = from.unique_keys.len() != to.unique_keys.len()
            || from
                .unique_keys
                .iter()
                .any(|f| !to.unique_keys.iter().any(|t| same_key(f, t, &map)));
        let checks_changed = from.checks.len() != to.checks.len()
            || from.checks.iter().any(|f| {
                !to.checks.iter().any(|t| {
                    f.name.as_deref().unwrap_or("") == t.name.as_deref().unwrap_or("")
                        && squash(&f.expression) == squash(&t.expression)
                })
            });
        let fks_changed = from.foreign_keys.len() != to.foreign_keys.len()
            || from
                .foreign_keys
                .iter()
                .any(|f| !to.foreign_keys.iter().any(|t| same_fk(f, t, &map)));
        let generated_changed = pairs
            .iter()
            .any(|(f, t)| f.generated.as_deref().map(squash) != t.generated.as_deref().map(squash))
            || added.iter().any(|a| a.generated.is_some());
        let dropped_in_constraint = dropped.iter().any(|c| {
            from.primary_key
                .as_ref()
                .is_some_and(|k| k.columns.iter().any(|x| eq_ci(x, &c.name)))
                || from
                    .unique_keys
                    .iter()
                    .any(|k| k.columns.iter().any(|x| eq_ci(x, &c.name)))
                || from
                    .foreign_keys
                    .iter()
                    .any(|k| k.columns.iter().any(|x| eq_ci(x, &c.name)))
        });
        if reordered
            || pk_changed
            || uniques_changed
            || checks_changed
            || fks_changed
            || generated_changed
            || dropped_in_constraint
        {
            let mut out = Vec::new();
            // renames happen inside the rebuild (the copy maps old → new names)
            out.extend(self.rebuild(from, to, &pairs));
            return out;
        }

        let s = from.schema.as_deref();
        let old_t = qualified(s, &from.name);
        let mut out = Vec::new();
        // indexes that change or disappear are dropped first (they may use dropped columns)
        let from_idx: Vec<&IndexDef> = secondary_indexes(from).collect();
        let to_idx: Vec<&IndexDef> = secondary_indexes(to).collect();
        for fi in &from_idx {
            if !to_idx.iter().any(|ti| same_index(fi, ti, &map)) {
                out.push(format!("DROP INDEX {}", qualified(s, &fi.name)));
            }
        }
        for c in &dropped {
            out.push(format!(
                "ALTER TABLE {old_t} DROP COLUMN {}",
                quote_ident(&c.name)
            ));
        }
        for (f, t) in &pairs {
            if f.name != t.name {
                out.push(format!(
                    "ALTER TABLE {old_t} RENAME COLUMN {} TO {}",
                    quote_ident(&f.name),
                    quote_ident(&t.name)
                ));
            }
        }
        if from.name != to.name {
            out.push(format!(
                "ALTER TABLE {old_t} RENAME TO {}",
                quote_ident(&to.name)
            ));
        }
        let new_t = qualified(s, &to.name);
        for (f, t) in &pairs {
            let col = quote_ident(&t.name);
            if !squash(&f.data_type).eq_ignore_ascii_case(&squash(&t.data_type)) {
                out.push(format!(
                    "ALTER TABLE {new_t} ALTER COLUMN {col} SET DATA TYPE {}",
                    t.data_type.trim()
                ));
            }
            if f.default.as_deref().map(squash) != t.default.as_deref().map(squash) {
                match t.default.as_deref() {
                    Some(d) => out.push(format!(
                        "ALTER TABLE {new_t} ALTER COLUMN {col} SET DEFAULT {}",
                        d.trim()
                    )),
                    None => out.push(format!(
                        "ALTER TABLE {new_t} ALTER COLUMN {col} DROP DEFAULT"
                    )),
                }
            }
            if f.nullable != t.nullable {
                let action = if t.nullable { "DROP" } else { "SET" };
                out.push(format!(
                    "ALTER TABLE {new_t} ALTER COLUMN {col} {action} NOT NULL"
                ));
            }
        }
        let mut to_in_schema = to.clone();
        to_in_schema.schema = from.schema.clone();
        for c in &added {
            out.extend(sequence_statements(&TableDef {
                columns: vec![(*c).clone()],
                ..to_in_schema.clone()
            }));
            out.push(format!(
                "ALTER TABLE {new_t} ADD COLUMN {}",
                column_sql(&to_in_schema, c)
            ));
        }
        if pk_added_only && let Some(pk) = pk_to {
            out.push(format!(
                "ALTER TABLE {new_t} ADD PRIMARY KEY ({})",
                q_list(&pk.columns)
            ));
        }
        for ti in &to_idx {
            if !from_idx.iter().any(|fi| same_index(fi, ti, &map)) {
                out.push(index_sql(&to_in_schema, ti));
            }
        }
        if from.comment != to.comment {
            out.push(format!(
                "COMMENT ON TABLE {new_t} IS {}",
                to.comment
                    .as_deref()
                    .map(string_literal)
                    .unwrap_or_else(|| "NULL".into())
            ));
        }
        for (f, t) in &pairs {
            if f.comment != t.comment {
                out.push(format!(
                    "COMMENT ON COLUMN {new_t}.{} IS {}",
                    quote_ident(&t.name),
                    t.comment
                        .as_deref()
                        .map(string_literal)
                        .unwrap_or_else(|| "NULL".into())
                ));
            }
        }
        for c in &added {
            if let Some(comment) = c.comment.as_deref().filter(|c| !c.is_empty()) {
                out.push(format!(
                    "COMMENT ON COLUMN {new_t}.{} IS {}",
                    quote_ident(&c.name),
                    string_literal(comment)
                ));
            }
        }
        if out.len() > 1 {
            out.insert(0, "BEGIN TRANSACTION".into());
            out.push("COMMIT".into());
        }
        out
    }

    fn add_column(&self, table: &TableDef, column: &ColumnDef) -> Vec<String> {
        let mut out = sequence_statements(&TableDef {
            columns: vec![column.clone()],
            ..table.clone()
        });
        out.push(format!(
            "ALTER TABLE {} ADD COLUMN {}",
            tname(table),
            column_sql(table, column)
        ));
        out
    }

    fn drop_column(&self, table: &TableDef, column: &str) -> String {
        format!(
            "ALTER TABLE {} DROP COLUMN {}",
            tname(table),
            quote_ident(column)
        )
    }

    fn create_index(&self, table: &TableDef, index: &IndexDef) -> String {
        index_sql(table, index)
    }

    fn drop_index(&self, table: &TableDef, index: &IndexDef) -> String {
        format!(
            "DROP INDEX {}",
            qualified(table.schema.as_deref(), &index.name)
        )
    }

    fn add_primary_key(&self, table: &TableDef, key: &KeyDef) -> String {
        format!(
            "ALTER TABLE {} ADD PRIMARY KEY ({})",
            tname(table),
            q_list(&key.columns)
        )
    }

    fn drop_primary_key(&self, table: &TableDef, _key: &KeyDef) -> String {
        let mut to = table.clone();
        to.primary_key = None;
        self.alter_table(table, &to, &[]).join(";\n")
    }

    fn add_unique_key(&self, table: &TableDef, key: &KeyDef) -> String {
        let name = key
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("uq_{}_{}", table.name, key.columns.join("_")));
        format!(
            "CREATE UNIQUE INDEX {} ON {} ({})",
            quote_ident(&name),
            tname(table),
            q_list(&key.columns)
        )
    }

    fn drop_unique_key(&self, table: &TableDef, key: &KeyDef) -> String {
        let mut to = table.clone();
        to.unique_keys.retain(|u| u != key);
        self.alter_table(table, &to, &[]).join(";\n")
    }

    fn add_check(&self, table: &TableDef, check: &CheckDef) -> String {
        let mut to = table.clone();
        to.checks.push(check.clone());
        self.alter_table(table, &to, &[]).join(";\n")
    }

    fn drop_check(&self, table: &TableDef, check: &CheckDef) -> String {
        let mut to = table.clone();
        to.checks.retain(|c| c != check);
        self.alter_table(table, &to, &[]).join(";\n")
    }

    fn add_foreign_key(&self, table: &TableDef, fk: &ForeignKeyDef) -> String {
        let mut to = table.clone();
        if !to.foreign_keys.contains(fk) {
            to.foreign_keys.push(fk.clone());
        }
        self.alter_table(table, &to, &[]).join(";\n")
    }

    fn drop_foreign_key(&self, table: &TableDef, fk: &ForeignKeyDef) -> String {
        let mut to = table.clone();
        to.foreign_keys.retain(|f| f != fk);
        self.alter_table(table, &to, &[]).join(";\n")
    }

    fn create_trigger(&self, _table: &TableDef, _trigger: &TriggerDef) -> Vec<String> {
        Vec::new()
    }

    fn drop_trigger(&self, _table: &TableDef, trigger: &TriggerDef) -> String {
        format!("-- DuckDB has no triggers ({})", trigger.name)
    }

    fn create_view(&self, view: &ViewDef, or_replace: bool) -> Vec<String> {
        vec![format!(
            "CREATE {}VIEW {} AS\n{}",
            if or_replace { "OR REPLACE " } else { "" },
            qualified(view.schema.as_deref(), &view.name),
            ansi::strip_trailing_semicolon(view.definition.trim())
        )]
    }

    fn drop_view(&self, view: &ViewDef) -> String {
        format!(
            "DROP VIEW {}",
            qualified(view.schema.as_deref(), &view.name)
        )
    }

    fn create_sequence(&self, seq: &SequenceDef) -> String {
        let mut s = format!(
            "CREATE SEQUENCE {}",
            qualified(seq.schema.as_deref(), &seq.name)
        );
        if let Some(v) = seq.increment {
            s.push_str(&format!(" INCREMENT BY {v}"));
        }
        if let Some(v) = seq.min_value {
            s.push_str(&format!(" MINVALUE {v}"));
        }
        if let Some(v) = seq.max_value {
            s.push_str(&format!(" MAXVALUE {v}"));
        }
        if let Some(v) = seq.start {
            s.push_str(&format!(" START WITH {v}"));
        }
        if seq.cycle {
            s.push_str(" CYCLE");
        }
        s
    }

    fn alter_sequence(&self, from: &SequenceDef, to: &SequenceDef) -> Vec<String> {
        if from == to {
            return Vec::new();
        }
        // DuckDB cannot ALTER a sequence: recreate it
        vec![self.drop_sequence(from), self.create_sequence(to)]
    }

    fn drop_sequence(&self, seq: &SequenceDef) -> String {
        format!(
            "DROP SEQUENCE {}",
            qualified(seq.schema.as_deref(), &seq.name)
        )
    }

    fn create_routine(&self, routine: &RoutineDef) -> Vec<String> {
        if routine.definition.trim().is_empty() {
            Vec::new()
        } else {
            vec![routine.definition.trim().to_string()]
        }
    }

    fn drop_routine(&self, routine: &RoutineDef) -> String {
        let table = routine
            .returns
            .as_deref()
            .is_some_and(|r| eq_ci(r, "TABLE"));
        format!(
            "DROP MACRO {}{}",
            if table { "TABLE " } else { "" },
            qualified(routine.schema.as_deref(), &routine.name)
        )
    }

    fn create_type(&self, ty: &TypeDef) -> Vec<String> {
        if !ty.definition.trim().is_empty() {
            return vec![ty.definition.trim().to_string()];
        }
        if ty.category.eq_ignore_ascii_case("enum") {
            let labels: Vec<String> = ty.elements.iter().map(|l| string_literal(l)).collect();
            return vec![format!(
                "CREATE TYPE {} AS ENUM ({})",
                qualified(ty.schema.as_deref(), &ty.name),
                labels.join(", ")
            )];
        }
        Vec::new()
    }

    fn drop_type(&self, ty: &TypeDef) -> Option<String> {
        Some(format!(
            "DROP TYPE {}",
            qualified(ty.schema.as_deref(), &ty.name)
        ))
    }

    fn create_schema(&self, name: &str) -> String {
        format!("CREATE SCHEMA {}", quote_ident(name))
    }

    fn drop_schema(&self, name: &str, cascade: bool) -> String {
        format!(
            "DROP SCHEMA {}{}",
            quote_ident(name),
            if cascade { " CASCADE" } else { "" }
        )
    }

    fn map_type(&self, source: DbKind, data_type: &str, kind: DataKind) -> String {
        let t = data_type.trim();
        if source == DbKind::DuckDb {
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
        if let Some(inner) = lower.strip_suffix("[]") {
            let elem = self.map_type(source, &t[..inner.len()], DataKind::Other);
            return format!("{elem}[]");
        }
        if lower.starts_with("array(") && lower.ends_with(')') {
            let elem = self.map_type(source, &t[6..t.len() - 1], DataKind::Other);
            return format!("{elem}[]");
        }
        if lower.starts_with('_') && source.is_postgres_family() {
            let elem = self.map_type(source, &t[1..], DataKind::Other);
            return format!("{elem}[]");
        }
        let base = lower.split(['(', ' ']).next().unwrap_or("");
        let params = t.find('(').map(|i| &t[i..]).unwrap_or("");
        let unsigned = lower.contains("unsigned");
        let mapped: Option<String> = match base {
            "bool" | "boolean" => Some("BOOLEAN".into()),
            "bit" if lower == "bit" || lower == "bit(1)" => {
                Some(if source == DbKind::SqlServer || source.is_mysql_family() {
                    "BOOLEAN".into()
                } else {
                    "BIT".into()
                })
            }
            "tinyint" if lower.starts_with("tinyint(1)") && source.is_mysql_family() => {
                Some("BOOLEAN".into())
            }
            "tinyint" | "int1" => Some(if unsigned { "UTINYINT" } else { "TINYINT" }.into()),
            "smallint" | "int2" | "smallserial" | "serial2" => {
                Some(if unsigned { "USMALLINT" } else { "SMALLINT" }.into())
            }
            "mediumint" | "int" | "integer" | "int4" | "serial" | "serial4" => {
                if source == DbKind::Sqlite && base == "integer" {
                    Some("BIGINT".into())
                } else {
                    Some(if unsigned { "UINTEGER" } else { "INTEGER" }.into())
                }
            }
            "bigint" | "int8" | "bigserial" | "serial8" | "long" => {
                Some(if unsigned { "UBIGINT" } else { "BIGINT" }.into())
            }
            "int16" if source == DbKind::ClickHouse => Some("SMALLINT".into()),
            "int32" => Some("INTEGER".into()),
            "int64" => Some("BIGINT".into()),
            "int128" | "int256" => Some("HUGEINT".into()),
            "uint8" => Some("UTINYINT".into()),
            "uint16" => Some("USMALLINT".into()),
            "uint32" => Some("UINTEGER".into()),
            "uint64" => Some("UBIGINT".into()),
            "uint128" | "uint256" => Some("UHUGEINT".into()),
            "real" if source == DbKind::Sqlite => Some("DOUBLE".into()),
            "real" | "float4" | "float32" => Some("FLOAT".into()),
            "float" if source.is_mysql_family() => Some("FLOAT".into()),
            "float" | "double" | "float8" | "float64" => Some("DOUBLE".into()),
            "numeric" | "decimal" | "dec" | "number" => Some(if params.is_empty() {
                "DECIMAL(38,10)".into()
            } else {
                // DuckDB allows at most 38 digits
                let nums: Vec<u32> = params
                    .trim_matches(|c| c == '(' || c == ')')
                    .split(',')
                    .filter_map(|p| p.trim().parse().ok())
                    .collect();
                match nums.as_slice() {
                    [p] => format!("DECIMAL({},0)", (*p).min(38)),
                    [p, s] => format!("DECIMAL({},{})", (*p).min(38), (*s).min((*p).min(38))),
                    _ => "DECIMAL(38,10)".into(),
                }
            }),
            "money" | "smallmoney" => Some("DECIMAL(19,4)".into()),
            "char" | "character" | "nchar" | "varchar" | "nvarchar" | "text" | "tinytext"
            | "mediumtext" | "longtext" | "ntext" | "string" | "fixedstring" | "citext"
            | "name" | "clob" | "bpchar" | "sysname" => Some("VARCHAR".into()),
            "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary"
            | "image" | "raw" | "bytes" => Some("BLOB".into()),
            "json" | "jsonb" => Some("JSON".into()),
            "uuid" | "uniqueidentifier" => Some("UUID".into()),
            "date" | "date32" => Some("DATE".into()),
            "time" if lower.contains("with time zone") => Some("TIMETZ".into()),
            "timetz" => Some("TIMETZ".into()),
            "time" => Some("TIME".into()),
            "timestamp" if lower.contains("with time zone") => Some("TIMESTAMPTZ".into()),
            "timestamptz" | "datetimeoffset" => Some("TIMESTAMPTZ".into()),
            "timestamp" | "datetime" | "datetime2" | "smalldatetime" | "datetime64" => {
                Some("TIMESTAMP".into())
            }
            "interval" => Some("INTERVAL".into()),
            "year" => Some("INTEGER".into()),
            "varbit" => Some("BIT".into()),
            "hstore" => Some("MAP(VARCHAR, VARCHAR)".into()),
            "enum" | "enum8" | "enum16" | "set" | "xml" | "inet" | "cidr" | "macaddr"
            | "macaddr8" | "ipv4" | "ipv6" | "tsvector" | "tsquery" | "geometry" | "geography"
            | "point" | "polygon" | "linestring" => Some("VARCHAR".into()),
            "hugeint" | "uhugeint" | "utinyint" | "usmallint" | "uinteger" | "ubigint" => {
                Some(base.to_ascii_uppercase())
            }
            _ => None,
        };
        mapped.unwrap_or_else(|| self.default_type_for(kind))
    }

    fn default_type_for(&self, kind: DataKind) -> String {
        match kind {
            DataKind::Bool => "BOOLEAN",
            DataKind::Int => "BIGINT",
            DataKind::Float => "DOUBLE",
            DataKind::Decimal => "DECIMAL(38,10)",
            DataKind::Binary => "BLOB",
            DataKind::Date => "DATE",
            DataKind::Time => "TIME",
            DataKind::TimeTz => "TIMETZ",
            DataKind::DateTime => "TIMESTAMP",
            DataKind::DateTimeTz => "TIMESTAMPTZ",
            DataKind::Interval => "INTERVAL",
            DataKind::Uuid => "UUID",
            DataKind::Json => "JSON",
            DataKind::Array => "VARCHAR[]",
            DataKind::Bit => "BIT",
            _ => "VARCHAR",
        }
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> TableDef {
        let mut t = TableDef::new(Some("main".into()), "items");
        t.columns
            .push(ColumnDef::new("id", "BIGINT").auto_increment().not_null());
        t.columns.push(ColumnDef::new("name", "VARCHAR").not_null());
        t.columns
            .push(ColumnDef::new("price", "DECIMAL(10,2)").default_value("0"));
        t.primary_key = Some(KeyDef {
            name: None,
            columns: vec!["id".into()],
        });
        t.indexes.push(IndexDef {
            name: "ix_name".into(),
            columns: vec![IndexColumn::column("name")],
            ..Default::default()
        });
        t.comment = Some("catalog".into());
        t
    }

    #[test]
    fn create() {
        let sql = DuckDbDdl.create_table(&items(), true);
        assert_eq!(sql[0], "CREATE SEQUENCE IF NOT EXISTS main.items_id_seq");
        assert!(
            sql[1].contains("id BIGINT DEFAULT nextval('main.items_id_seq') NOT NULL"),
            "{}",
            sql[1]
        );
        assert!(sql[1].contains("PRIMARY KEY (id)"));
        assert_eq!(sql[2], "CREATE INDEX ix_name ON main.items (name)");
        assert_eq!(sql[3], "COMMENT ON TABLE main.items IS 'catalog'");
    }

    #[test]
    fn native_and_rebuild_alters() {
        let from = items();
        let mut to = from.clone();
        to.columns[1].name = "title".into();
        to.columns[2].data_type = "DECIMAL(12,2)".into();
        to.columns[2].nullable = false;
        to.columns
            .push(ColumnDef::new("stock", "INTEGER").default_value("0"));
        to.indexes[0].columns[0].name = "title".into();
        let sql = DuckDbDdl.alter_table(&from, &to, &[("name".into(), "title".into())]);
        assert_eq!(
            sql,
            vec![
                "BEGIN TRANSACTION",
                "ALTER TABLE main.items RENAME COLUMN name TO title",
                "ALTER TABLE main.items ALTER COLUMN price SET DATA TYPE DECIMAL(12,2)",
                "ALTER TABLE main.items ALTER COLUMN price SET NOT NULL",
                "ALTER TABLE main.items ADD COLUMN stock INTEGER DEFAULT 0",
                "COMMIT",
            ]
        );
        let mut to = from.clone();
        to.checks.push(CheckDef {
            name: None,
            expression: "price >= 0".into(),
        });
        let sql = DuckDbDdl.alter_table(&from, &to, &[]);
        assert_eq!(sql[0], "BEGIN TRANSACTION");
        assert!(
            sql.iter()
                .any(|s| s.starts_with("CREATE TABLE main.__dbiji_new_items")),
            "{sql:#?}"
        );
        assert!(sql.contains(&"INSERT INTO main.__dbiji_new_items (id, name, price) SELECT id, name, price FROM main.items".to_string()));
        assert!(sql.contains(&"ALTER TABLE main.__dbiji_new_items RENAME TO items".to_string()));
        assert!(sql.contains(&"CREATE INDEX ix_name ON main.items (name)".to_string()));
    }

    #[test]
    fn type_mapping() {
        let d = DuckDbDdl;
        assert_eq!(
            d.map_type(DbKind::Postgres, "jsonb", DataKind::Json),
            "JSON"
        );
        assert_eq!(
            d.map_type(DbKind::Postgres, "int4[]", DataKind::Array),
            "INTEGER[]"
        );
        assert_eq!(
            d.map_type(DbKind::Postgres, "_text", DataKind::Array),
            "VARCHAR[]"
        );
        assert_eq!(
            d.map_type(DbKind::Postgres, "numeric(50,4)", DataKind::Decimal),
            "DECIMAL(38,4)"
        );
        assert_eq!(
            d.map_type(DbKind::Postgres, "numeric", DataKind::Decimal),
            "DECIMAL(38,10)"
        );
        assert_eq!(
            d.map_type(
                DbKind::Postgres,
                "timestamp with time zone",
                DataKind::DateTimeTz
            ),
            "TIMESTAMPTZ"
        );
        assert_eq!(
            d.map_type(DbKind::MySql, "tinyint(1)", DataKind::Bool),
            "BOOLEAN"
        );
        assert_eq!(
            d.map_type(DbKind::MySql, "int unsigned", DataKind::Int),
            "UINTEGER"
        );
        assert_eq!(
            d.map_type(DbKind::Sqlite, "INTEGER", DataKind::Int),
            "BIGINT"
        );
        assert_eq!(
            d.map_type(DbKind::Sqlite, "REAL", DataKind::Float),
            "DOUBLE"
        );
        assert_eq!(
            d.map_type(
                DbKind::ClickHouse,
                "Array(Nullable(UInt32))",
                DataKind::Array
            ),
            "UINTEGER[]"
        );
        assert_eq!(
            d.map_type(DbKind::SqlServer, "bit", DataKind::Bool),
            "BOOLEAN"
        );
        assert_eq!(
            d.map_type(DbKind::Postgres, "mood", DataKind::Enum),
            "VARCHAR"
        );
    }
}
