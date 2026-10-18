//! SQL dialect interface: lexical rules, identifier quoting, literals and
//! query shaping. Each driver crate provides an implementation; this module
//! also contains [`GenericDialect`] (ANSI SQL) and reusable [`ansi`] helpers.

use crate::admin::MaintenanceOp;
use crate::config::DbKind;
use crate::ddl::*;
use crate::keywords;
use crate::meta::ObjectRef;
use crate::value::{DataKind, Value};
use std::collections::BTreeMap;

/// How a database folds unquoted identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentCase {
    /// PostgreSQL: unquoted identifiers become lower case.
    Lower,
    /// Oracle / Firebird / Db2: upper case.
    Upper,
    /// MySQL, SQLite, SQL Server: case preserved.
    Preserve,
}

/// Lexical description of a dialect, consumed by the tokenizer, highlighter,
/// statement splitter and autocomplete.
#[derive(Debug, Clone)]
pub struct SqlSyntax {
    /// Keywords for highlighting and completion (upper case).
    pub keywords: &'static [&'static str],
    /// Built-in functions (lower or upper case, matched case-insensitively).
    pub functions: &'static [&'static str],
    /// Built-in data type names.
    pub data_types: &'static [&'static str],
    /// Identifier quote pairs; the first one is used when quoting.
    pub identifier_quotes: &'static [(char, char)],
    /// `"..."` is a string literal (MySQL without ANSI_QUOTES).
    pub double_quoted_strings: bool,
    /// Backslash escapes inside '...' strings (MySQL default).
    pub backslash_escapes: bool,
    /// PostgreSQL `E'...'` strings with backslash escapes.
    pub e_strings: bool,
    /// `N'...'` national strings.
    pub n_strings: bool,
    /// `X'...'` / `x'...'` hex literals.
    pub x_strings: bool,
    /// PostgreSQL `$tag$ ... $tag$` quoting.
    pub dollar_quoting: bool,
    /// `#` starts a line comment (MySQL).
    pub hash_comments: bool,
    /// `/* /* */ */` nests (PostgreSQL).
    pub nested_block_comments: bool,
    /// `--` must be followed by whitespace to start a comment (MySQL).
    pub dash_comment_needs_space: bool,
    /// `?` positional parameters.
    pub question_params: bool,
    /// `$1` numbered parameters.
    pub dollar_params: bool,
    /// `@name` variables are part of the language (MySQL user variables, T-SQL).
    pub at_variables: bool,
    /// Batch separator on its own line (T-SQL `GO`).
    pub batch_separator: Option<&'static str>,
    /// Supports the client `DELIMITER xx` command (MySQL scripts).
    pub delimiter_command: bool,
    /// `BEGIN ... END` compound statements appear in DDL bodies without
    /// dollar quoting (MySQL / SQLite triggers / T-SQL), so `;` inside them
    /// must not split statements.
    pub compound_blocks: bool,
    /// Characters allowed inside unquoted identifiers besides `[A-Za-z0-9_]`.
    pub extra_ident_chars: &'static str,
    pub ident_case: IdentCase,
}

impl SqlSyntax {
    pub const fn ansi() -> SqlSyntax {
        SqlSyntax {
            keywords: keywords::ANSI_KEYWORDS,
            functions: keywords::ANSI_FUNCTIONS,
            data_types: keywords::ANSI_TYPES,
            identifier_quotes: &[('"', '"')],
            double_quoted_strings: false,
            backslash_escapes: false,
            e_strings: false,
            n_strings: true,
            x_strings: true,
            dollar_quoting: false,
            hash_comments: false,
            nested_block_comments: false,
            dash_comment_needs_space: false,
            question_params: true,
            dollar_params: false,
            at_variables: false,
            batch_separator: None,
            delimiter_command: false,
            compound_blocks: false,
            extra_ident_chars: "",
            ident_case: IdentCase::Upper,
        }
    }

    pub fn is_keyword(&self, word: &str) -> bool {
        self.keywords.iter().any(|k| k.eq_ignore_ascii_case(word))
    }

    pub fn is_function(&self, word: &str) -> bool {
        self.functions.iter().any(|k| k.eq_ignore_ascii_case(word))
    }

    pub fn is_data_type(&self, word: &str) -> bool {
        self.data_types.iter().any(|k| k.eq_ignore_ascii_case(word))
    }

    pub fn is_ident_char(&self, c: char, first: bool) -> bool {
        if c == '_' || c.is_alphabetic() {
            return true;
        }
        if !first && c.is_ascii_digit() {
            return true;
        }
        if !first && self.extra_ident_chars.contains(c) {
            return true;
        }
        // T-SQL allows # and @ as first characters (temp tables, variables).
        first && self.extra_ident_chars.contains(c) && c != '$'
    }
}

/// A SQL dialect. All methods have ANSI defaults so dialects only override what differs.
pub trait Dialect: Send + Sync + 'static {
    fn kind(&self) -> DbKind;
    fn syntax(&self) -> &SqlSyntax;
    fn ddl(&self) -> &dyn DdlGenerator;

    /// Quote `ident` only when required (reserved word, special characters, case).
    fn quote_ident(&self, ident: &str) -> String {
        if self.needs_quoting(ident) { self.force_quote_ident(ident) } else { ident.to_string() }
    }

    /// Always quote `ident`.
    fn force_quote_ident(&self, ident: &str) -> String {
        let (open, close) = self.syntax().identifier_quotes.first().copied().unwrap_or(('"', '"'));
        let mut s = String::with_capacity(ident.len() + 2);
        s.push(open);
        for c in ident.chars() {
            if c == close {
                s.push(close);
            }
            s.push(c);
        }
        s.push(close);
        s
    }

    fn needs_quoting(&self, ident: &str) -> bool {
        ansi::needs_quoting(self.syntax(), ident)
    }

    /// `schema.name` with quoting as needed.
    fn qualified(&self, schema: Option<&str>, name: &str) -> String {
        match schema {
            Some(s) if !s.is_empty() => format!("{}.{}", self.quote_ident(s), self.quote_ident(name)),
            _ => self.quote_ident(name),
        }
    }

    /// Qualified name of an object (table children are qualified by their table's schema only).
    fn qualified_ref(&self, obj: &ObjectRef) -> String {
        self.qualified(obj.schema.as_deref(), &obj.name)
    }

    fn string_literal(&self, s: &str) -> String {
        ansi::string_literal(s, self.syntax().backslash_escapes)
    }

    fn bytes_literal(&self, b: &[u8]) -> String {
        format!("X'{}'", hex::encode_upper(b))
    }

    fn bool_literal(&self, b: bool) -> String {
        if b { "TRUE".into() } else { "FALSE".into() }
    }

    /// SQL literal for `value`. `type_name` is the target column type when
    /// known, letting dialects add casts (PostgreSQL `'..'::jsonb`).
    fn literal(&self, value: &Value, type_name: Option<&str>) -> String {
        let _ = type_name;
        ansi::literal(self, value)
    }

    /// Positional parameter placeholder, 1-based (`$1` for PostgreSQL, `?` elsewhere).
    fn placeholder(&self, index: usize) -> String {
        let _ = index;
        "?".into()
    }

    /// Wrap `sql` so that at most `limit` rows starting at `offset` are returned.
    fn limit_query(&self, sql: &str, limit: u64, offset: u64) -> String {
        let inner = ansi::strip_trailing_semicolon(sql);
        if offset > 0 { format!("SELECT * FROM ({inner}) dbiji_q LIMIT {limit} OFFSET {offset}") } else { format!("SELECT * FROM ({inner}) dbiji_q LIMIT {limit}") }
    }

    /// Query returning the row count of `sql`.
    fn count_query(&self, sql: &str) -> String {
        format!("SELECT COUNT(*) FROM ({}) dbiji_cnt", ansi::strip_trailing_semicolon(sql))
    }

    /// `SELECT` for browsing a table in the data editor.
    fn select_table(&self, table: &ObjectRef, columns: Option<&[String]>, where_clause: Option<&str>, order_by: Option<&str>, limit: Option<u64>) -> String {
        let cols = match columns {
            Some(c) if !c.is_empty() => c.iter().map(|c| self.quote_ident(c)).collect::<Vec<_>>().join(", "),
            _ => "*".to_string(),
        };
        let mut sql = format!("SELECT {cols} FROM {}", self.qualified_ref(table));
        if let Some(w) = where_clause.filter(|w| !w.trim().is_empty()) {
            sql.push_str(" WHERE ");
            sql.push_str(w);
        }
        if let Some(o) = order_by.filter(|o| !o.trim().is_empty()) {
            sql.push_str(" ORDER BY ");
            sql.push_str(o);
        }
        if let Some(l) = limit {
            sql.push_str(&format!(" LIMIT {l}"));
        }
        sql
    }

    /// Insert-or-update statement. `table` is already qualified/quoted;
    /// `columns`/`key_columns` are already quoted; `values` are literals or placeholders.
    fn upsert(&self, table: &str, columns: &[String], key_columns: &[String], values: &[String]) -> Option<String> {
        let _ = (table, columns, key_columns, values);
        None
    }

    fn supports_returning(&self) -> bool {
        false
    }

    fn truncate_table(&self, table: &ObjectRef) -> String {
        format!("TRUNCATE TABLE {}", self.qualified_ref(table))
    }

    /// Maintenance operations offered for objects of this dialect.
    fn maintenance_ops(&self) -> Vec<MaintenanceOp> {
        Vec::new()
    }

    /// SQL for a maintenance operation (`op` is a [`MaintenanceOp::id`]).
    fn maintenance_sql(&self, op: &str, targets: &[ObjectRef], options: &BTreeMap<String, bool>) -> Vec<String> {
        let _ = (op, targets, options);
        Vec::new()
    }

    /// Statement changing a user's password (`None` = current user).
    fn change_password_sql(&self, user: Option<&str>, new_password: &str) -> Option<String> {
        let _ = (user, new_password);
        None
    }

    /// Logical kind of a type name.
    fn type_kind(&self, type_name: &str) -> DataKind {
        DataKind::guess_from_type_name(type_name)
    }
}

/// ANSI SQL dialect used when no database is attached.
pub struct GenericDialect {
    syntax: SqlSyntax,
    ddl: ansi::AnsiDdl,
}

impl GenericDialect {
    pub fn new() -> Self {
        GenericDialect { syntax: SqlSyntax::ansi(), ddl: ansi::AnsiDdl::new(SqlSyntax::ansi()) }
    }
}

impl Default for GenericDialect {
    fn default() -> Self {
        Self::new()
    }
}

impl Dialect for GenericDialect {
    fn kind(&self) -> DbKind {
        DbKind::Generic
    }
    fn syntax(&self) -> &SqlSyntax {
        &self.syntax
    }
    fn ddl(&self) -> &dyn DdlGenerator {
        &self.ddl
    }
}

/// Reusable ANSI building blocks for dialect implementations.
pub mod ansi {
    use super::*;

    pub fn strip_trailing_semicolon(sql: &str) -> &str {
        let mut s = sql.trim_end();
        while let Some(rest) = s.strip_suffix(';') {
            s = rest.trim_end();
        }
        s
    }

    pub fn string_literal(s: &str, backslash_escapes: bool) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('\'');
        for c in s.chars() {
            match c {
                '\'' => out.push_str("''"),
                '\\' if backslash_escapes => out.push_str("\\\\"),
                '\0' if backslash_escapes => out.push_str("\\0"),
                c => out.push(c),
            }
        }
        out.push('\'');
        out
    }

    pub fn needs_quoting(syntax: &SqlSyntax, ident: &str) -> bool {
        let mut chars = ident.chars();
        let Some(first) = chars.next() else { return true };
        if !(first == '_' || first.is_ascii_alphabetic()) {
            return true;
        }
        for c in ident.chars() {
            let ok = c == '_' || c.is_ascii_alphanumeric() || syntax.extra_ident_chars.contains(c);
            if !ok {
                return true;
            }
            match syntax.ident_case {
                IdentCase::Lower if c.is_ascii_uppercase() => return true,
                IdentCase::Upper if c.is_ascii_lowercase() => {}
                _ => {}
            }
        }
        keywords::is_reserved(ident)
    }

    /// Default literal rendering shared by dialects.
    pub fn literal<D: Dialect + ?Sized>(d: &D, value: &Value) -> String {
        match value {
            Value::Null => "NULL".into(),
            Value::Bool(b) => d.bool_literal(*b),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) if f.is_finite() => {
                let s = format!("{f}");
                s
            }
            Value::Float(f) => d.string_literal(&value_text_float(*f)),
            Value::Decimal(s) => {
                if crate::value::is_decimal_text(s) && s.chars().all(|c| c.is_ascii_digit() || "+-.eE".contains(c)) {
                    s.to_string()
                } else {
                    d.string_literal(s)
                }
            }
            Value::Bytes(b) => d.bytes_literal(b),
            other => d.string_literal(&other.to_text()),
        }
    }

    fn value_text_float(f: f64) -> String {
        if f.is_nan() {
            "NaN".into()
        } else if f > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        }
    }

    /// Quote helper bound to a syntax.
    pub struct Quoter<'a> {
        pub syntax: &'a SqlSyntax,
    }

    impl Quoter<'_> {
        pub fn ident(&self, s: &str) -> String {
            if needs_quoting(self.syntax, s) { self.force(s) } else { s.to_string() }
        }
        pub fn force(&self, s: &str) -> String {
            let (open, close) = self.syntax.identifier_quotes.first().copied().unwrap_or(('"', '"'));
            let mut out = String::new();
            out.push(open);
            for c in s.chars() {
                if c == close {
                    out.push(close);
                }
                out.push(c);
            }
            out.push(close);
            out
        }
        pub fn qualified(&self, schema: Option<&str>, name: &str) -> String {
            match schema {
                Some(s) if !s.is_empty() => format!("{}.{}", self.ident(s), self.ident(name)),
                _ => self.ident(name),
            }
        }
        pub fn list(&self, cols: &[String]) -> String {
            cols.iter().map(|c| self.ident(c)).collect::<Vec<_>>().join(", ")
        }
    }

    /// `name type [NOT NULL] [DEFAULT x]` in ANSI form.
    pub fn column_definition(q: &Quoter<'_>, c: &ColumnDef) -> String {
        let mut s = format!("{} {}", q.ident(&c.name), c.data_type);
        if let Some(g) = &c.generated {
            s.push_str(&format!(" GENERATED ALWAYS AS ({g}) STORED"));
        } else if c.auto_increment {
            s.push_str(" GENERATED BY DEFAULT AS IDENTITY");
        } else if let Some(d) = &c.default {
            s.push_str(" DEFAULT ");
            s.push_str(d);
        }
        if !c.nullable {
            s.push_str(" NOT NULL");
        }
        s
    }

    pub fn fk_clause(q: &Quoter<'_>, fk: &ForeignKeyDef) -> String {
        let mut s = String::new();
        if let Some(n) = &fk.name {
            s.push_str(&format!("CONSTRAINT {} ", q.ident(n)));
        }
        s.push_str(&format!("FOREIGN KEY ({}) REFERENCES {} ({})", q.list(&fk.columns), q.qualified(fk.ref_schema.as_deref(), &fk.ref_table), q.list(&fk.ref_columns)));
        if fk.on_update != FkAction::NoAction {
            s.push_str(&format!(" ON UPDATE {}", fk.on_update.sql()));
        }
        if fk.on_delete != FkAction::NoAction {
            s.push_str(&format!(" ON DELETE {}", fk.on_delete.sql()));
        }
        if fk.deferrable {
            s.push_str(" DEFERRABLE");
        }
        s
    }

    pub fn key_clause(q: &Quoter<'_>, kind: &str, k: &KeyDef) -> String {
        match &k.name {
            Some(n) => format!("CONSTRAINT {} {kind} ({})", q.ident(n), q.list(&k.columns)),
            None => format!("{kind} ({})", q.list(&k.columns)),
        }
    }

    pub fn check_clause(q: &Quoter<'_>, c: &CheckDef) -> String {
        match &c.name {
            Some(n) => format!("CONSTRAINT {} CHECK ({})", q.ident(n), c.expression),
            None => format!("CHECK ({})", c.expression),
        }
    }

    pub fn index_columns(q: &Quoter<'_>, idx: &IndexDef) -> String {
        idx.columns
            .iter()
            .map(|c| {
                let mut s = if c.is_expression { format!("({})", c.name) } else { q.ident(&c.name) };
                if c.descending {
                    s.push_str(" DESC");
                }
                s
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Body lines of `CREATE TABLE (...)`: columns, PK, uniques, checks, optionally FKs.
    pub fn create_table_body(q: &Quoter<'_>, t: &TableDef, include_fks: bool) -> Vec<String> {
        let mut lines: Vec<String> = t.columns.iter().map(|c| column_definition(q, c)).collect();
        if let Some(pk) = &t.primary_key
            && !pk.columns.is_empty()
        {
            lines.push(key_clause(q, "PRIMARY KEY", pk));
        }
        for u in &t.unique_keys {
            lines.push(key_clause(q, "UNIQUE", u));
        }
        for c in &t.checks {
            lines.push(check_clause(q, c));
        }
        if include_fks {
            for fk in &t.foreign_keys {
                lines.push(fk_clause(q, fk));
            }
        }
        lines
    }

    /// Index definitions that are not implied by PK / unique constraints.
    pub fn secondary_indexes(t: &TableDef) -> impl Iterator<Item = &IndexDef> {
        t.indexes.iter().filter(move |i| {
            if i.primary {
                return false;
            }
            let cols: Vec<&str> = i.columns.iter().map(|c| c.name.as_str()).collect();
            let backs_unique =
                i.unique && t.unique_keys.iter().any(|u| u.name.as_deref() == Some(i.name.as_str()) || u.columns.iter().map(String::as_str).collect::<Vec<_>>() == cols);
            !backs_unique
        })
    }

    /// Match columns of two table versions: returns (from, to) pairs, dropped and added.
    pub fn match_columns<'a>(from: &'a TableDef, to: &'a TableDef, renamed: &[(String, String)]) -> (Vec<(&'a ColumnDef, &'a ColumnDef)>, Vec<&'a ColumnDef>, Vec<&'a ColumnDef>) {
        let mut pairs = Vec::new();
        let mut dropped = Vec::new();
        let mut added = Vec::new();
        for fc in &from.columns {
            let target_name = renamed.iter().find(|(old, _)| old == &fc.name).map(|(_, new)| new.as_str()).unwrap_or(fc.name.as_str());
            match to.columns.iter().find(|c| c.name == target_name) {
                Some(tc) => pairs.push((fc, tc)),
                None => dropped.push(fc),
            }
        }
        for tc in &to.columns {
            if !pairs.iter().any(|(_, t)| std::ptr::eq(*t, tc)) {
                added.push(tc);
            }
        }
        (pairs, dropped, added)
    }

    /// ANSI DDL generator; dialect generators can wrap it and override pieces.
    pub struct AnsiDdl {
        pub syntax: SqlSyntax,
    }

    impl AnsiDdl {
        pub fn new(syntax: SqlSyntax) -> Self {
            AnsiDdl { syntax }
        }
        pub fn q(&self) -> Quoter<'_> {
            Quoter { syntax: &self.syntax }
        }
        fn tname(&self, t: &TableDef) -> String {
            self.q().qualified(t.schema.as_deref(), &t.name)
        }
    }

    impl DdlGenerator for AnsiDdl {
        fn create_table(&self, t: &TableDef, include_foreign_keys: bool) -> Vec<String> {
            let q = self.q();
            let body = create_table_body(&q, t, include_foreign_keys);
            let mut out = vec![format!("CREATE TABLE {} (\n    {}\n)", self.tname(t), body.join(",\n    "))];
            for idx in secondary_indexes(t) {
                out.push(self.create_index(t, idx));
            }
            out
        }
        fn drop_table(&self, schema: Option<&str>, name: &str, cascade: bool) -> String {
            format!("DROP TABLE {}{}", self.q().qualified(schema, name), if cascade { " CASCADE" } else { "" })
        }
        fn rename_table(&self, schema: Option<&str>, old_name: &str, new_name: &str) -> String {
            format!("ALTER TABLE {} RENAME TO {}", self.q().qualified(schema, old_name), self.q().ident(new_name))
        }
        fn alter_table(&self, from: &TableDef, to: &TableDef, renamed: &[(String, String)]) -> Vec<String> {
            let q = self.q();
            let t = self.tname(to);
            let mut out = Vec::new();
            if from.name != to.name {
                out.push(self.rename_table(from.schema.as_deref(), &from.name, &to.name));
            }
            let (pairs, dropped, added) = match_columns(from, to, renamed);
            for fk in &from.foreign_keys {
                if !to.foreign_keys.contains(fk) {
                    out.push(self.drop_foreign_key(to, fk));
                }
            }
            for c in dropped {
                out.push(self.drop_column(to, &c.name));
            }
            for (fc, tc) in &pairs {
                if fc.name != tc.name {
                    out.push(format!("ALTER TABLE {t} RENAME COLUMN {} TO {}", q.ident(&fc.name), q.ident(&tc.name)));
                }
                if fc.data_type != tc.data_type {
                    out.push(format!("ALTER TABLE {t} ALTER COLUMN {} SET DATA TYPE {}", q.ident(&tc.name), tc.data_type));
                }
                if fc.default != tc.default {
                    match &tc.default {
                        Some(d) => out.push(format!("ALTER TABLE {t} ALTER COLUMN {} SET DEFAULT {d}", q.ident(&tc.name))),
                        None => out.push(format!("ALTER TABLE {t} ALTER COLUMN {} DROP DEFAULT", q.ident(&tc.name))),
                    }
                }
                if fc.nullable != tc.nullable {
                    out.push(format!("ALTER TABLE {t} ALTER COLUMN {} {} NOT NULL", q.ident(&tc.name), if tc.nullable { "DROP" } else { "SET" }));
                }
            }
            for c in added {
                out.extend(self.add_column(to, c));
            }
            if from.primary_key != to.primary_key {
                if let Some(pk) = &from.primary_key {
                    out.push(self.drop_primary_key(to, pk));
                }
                if let Some(pk) = &to.primary_key {
                    out.push(self.add_primary_key(to, pk));
                }
            }
            for u in &from.unique_keys {
                if !to.unique_keys.contains(u) {
                    out.push(self.drop_unique_key(to, u));
                }
            }
            for u in &to.unique_keys {
                if !from.unique_keys.contains(u) {
                    out.push(self.add_unique_key(to, u));
                }
            }
            for c in &from.checks {
                if !to.checks.contains(c) {
                    out.push(self.drop_check(to, c));
                }
            }
            for c in &to.checks {
                if !from.checks.contains(c) {
                    out.push(self.add_check(to, c));
                }
            }
            let from_idx: Vec<&IndexDef> = secondary_indexes(from).collect();
            let to_idx: Vec<&IndexDef> = secondary_indexes(to).collect();
            for i in &from_idx {
                if !to_idx.iter().any(|x| same_index(x, i)) {
                    out.push(self.drop_index(to, i));
                }
            }
            for i in &to_idx {
                if !from_idx.iter().any(|x| same_index(x, i)) {
                    out.push(self.create_index(to, i));
                }
            }
            for fk in &to.foreign_keys {
                if !from.foreign_keys.contains(fk) {
                    out.push(self.add_foreign_key(to, fk));
                }
            }
            out
        }
        fn add_column(&self, table: &TableDef, column: &ColumnDef) -> Vec<String> {
            vec![format!("ALTER TABLE {} ADD COLUMN {}", self.tname(table), column_definition(&self.q(), column))]
        }
        fn drop_column(&self, table: &TableDef, column: &str) -> String {
            format!("ALTER TABLE {} DROP COLUMN {}", self.tname(table), self.q().ident(column))
        }
        fn create_index(&self, table: &TableDef, index: &IndexDef) -> String {
            let q = self.q();
            let mut s = format!("CREATE {}INDEX {} ON {} ({})", if index.unique { "UNIQUE " } else { "" }, q.ident(&index.name), self.tname(table), index_columns(&q, index));
            if let Some(p) = &index.predicate {
                s.push_str(" WHERE ");
                s.push_str(p);
            }
            s
        }
        fn drop_index(&self, table: &TableDef, index: &IndexDef) -> String {
            format!("DROP INDEX {}", self.q().qualified(table.schema.as_deref(), &index.name))
        }
        fn add_primary_key(&self, table: &TableDef, key: &KeyDef) -> String {
            format!("ALTER TABLE {} ADD {}", self.tname(table), key_clause(&self.q(), "PRIMARY KEY", key))
        }
        fn drop_primary_key(&self, table: &TableDef, key: &KeyDef) -> String {
            match &key.name {
                Some(n) => format!("ALTER TABLE {} DROP CONSTRAINT {}", self.tname(table), self.q().ident(n)),
                None => format!("ALTER TABLE {} DROP PRIMARY KEY", self.tname(table)),
            }
        }
        fn add_unique_key(&self, table: &TableDef, key: &KeyDef) -> String {
            format!("ALTER TABLE {} ADD {}", self.tname(table), key_clause(&self.q(), "UNIQUE", key))
        }
        fn drop_unique_key(&self, table: &TableDef, key: &KeyDef) -> String {
            format!("ALTER TABLE {} DROP CONSTRAINT {}", self.tname(table), self.q().ident(key.name.as_deref().unwrap_or("?")))
        }
        fn add_check(&self, table: &TableDef, check: &CheckDef) -> String {
            format!("ALTER TABLE {} ADD {}", self.tname(table), check_clause(&self.q(), check))
        }
        fn drop_check(&self, table: &TableDef, check: &CheckDef) -> String {
            format!("ALTER TABLE {} DROP CONSTRAINT {}", self.tname(table), self.q().ident(check.name.as_deref().unwrap_or("?")))
        }
        fn add_foreign_key(&self, table: &TableDef, fk: &ForeignKeyDef) -> String {
            format!("ALTER TABLE {} ADD {}", self.tname(table), fk_clause(&self.q(), fk))
        }
        fn drop_foreign_key(&self, table: &TableDef, fk: &ForeignKeyDef) -> String {
            format!("ALTER TABLE {} DROP CONSTRAINT {}", self.tname(table), self.q().ident(fk.name.as_deref().unwrap_or("?")))
        }
        fn create_trigger(&self, _table: &TableDef, trigger: &TriggerDef) -> Vec<String> {
            vec![trigger.definition.clone()]
        }
        fn drop_trigger(&self, _table: &TableDef, trigger: &TriggerDef) -> String {
            format!("DROP TRIGGER {}", self.q().ident(&trigger.name))
        }
        fn create_view(&self, view: &ViewDef, or_replace: bool) -> Vec<String> {
            vec![format!(
                "CREATE {}VIEW {} AS\n{}",
                if or_replace { "OR REPLACE " } else { "" },
                self.q().qualified(view.schema.as_deref(), &view.name),
                strip_trailing_semicolon(&view.definition)
            )]
        }
        fn drop_view(&self, view: &ViewDef) -> String {
            format!("DROP VIEW {}", self.q().qualified(view.schema.as_deref(), &view.name))
        }
        fn create_sequence(&self, seq: &SequenceDef) -> String {
            let mut s = format!("CREATE SEQUENCE {}", self.q().qualified(seq.schema.as_deref(), &seq.name));
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
            if let Some(v) = seq.cache {
                s.push_str(&format!(" CACHE {v}"));
            }
            if seq.cycle {
                s.push_str(" CYCLE");
            }
            s
        }
        fn alter_sequence(&self, from: &SequenceDef, to: &SequenceDef) -> Vec<String> {
            let name = self.q().qualified(to.schema.as_deref(), &to.name);
            let mut parts = Vec::new();
            if from.increment != to.increment
                && let Some(v) = to.increment
            {
                parts.push(format!("INCREMENT BY {v}"));
            }
            if from.min_value != to.min_value {
                parts.push(to.min_value.map(|v| format!("MINVALUE {v}")).unwrap_or("NO MINVALUE".into()));
            }
            if from.max_value != to.max_value {
                parts.push(to.max_value.map(|v| format!("MAXVALUE {v}")).unwrap_or("NO MAXVALUE".into()));
            }
            if from.cycle != to.cycle {
                parts.push(if to.cycle { "CYCLE".into() } else { "NO CYCLE".into() });
            }
            if parts.is_empty() { Vec::new() } else { vec![format!("ALTER SEQUENCE {name} {}", parts.join(" "))] }
        }
        fn drop_sequence(&self, seq: &SequenceDef) -> String {
            format!("DROP SEQUENCE {}", self.q().qualified(seq.schema.as_deref(), &seq.name))
        }
        fn create_routine(&self, routine: &RoutineDef) -> Vec<String> {
            vec![routine.definition.clone()]
        }
        fn drop_routine(&self, routine: &RoutineDef) -> String {
            let kind = match routine.kind {
                Some(crate::meta::ObjectKind::Procedure) => "PROCEDURE",
                _ => "FUNCTION",
            };
            format!("DROP {kind} {}", self.q().qualified(routine.schema.as_deref(), &routine.name))
        }
        fn create_schema(&self, name: &str) -> String {
            format!("CREATE SCHEMA {}", self.q().ident(name))
        }
        fn drop_schema(&self, name: &str, cascade: bool) -> String {
            format!("DROP SCHEMA {}{}", self.q().ident(name), if cascade { " CASCADE" } else { "" })
        }
        fn map_type(&self, _source: DbKind, data_type: &str, kind: DataKind) -> String {
            if self.syntax.is_data_type(data_type.split('(').next().unwrap_or("")) { data_type.to_string() } else { self.default_type_for(kind) }
        }
        fn default_type_for(&self, kind: DataKind) -> String {
            match kind {
                DataKind::Bool => "BOOLEAN",
                DataKind::Int => "BIGINT",
                DataKind::Float => "DOUBLE PRECISION",
                DataKind::Decimal => "NUMERIC",
                DataKind::Binary => "BLOB",
                DataKind::Date => "DATE",
                DataKind::Time | DataKind::TimeTz => "TIME",
                DataKind::DateTime => "TIMESTAMP",
                DataKind::DateTimeTz => "TIMESTAMP WITH TIME ZONE",
                _ => "VARCHAR(4000)",
            }
            .to_string()
        }
    }

    pub fn same_index(a: &IndexDef, b: &IndexDef) -> bool {
        a.name == b.name && a.columns == b.columns && a.unique == b.unique && a.predicate == b.predicate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        let d = GenericDialect::new();
        assert_eq!(d.quote_ident("users"), "users");
        assert_eq!(d.quote_ident("select"), "\"select\"");
        assert_eq!(d.quote_ident("my table"), "\"my table\"");
        assert_eq!(d.force_quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(d.qualified(Some("public"), "Order"), "public.\"Order\"");
        assert_eq!(d.string_literal("it's"), "'it''s'");
    }

    #[test]
    fn literals() {
        let d = GenericDialect::new();
        assert_eq!(d.literal(&Value::Null, None), "NULL");
        assert_eq!(d.literal(&Value::Int(5), None), "5");
        assert_eq!(d.literal(&Value::text("x'y"), None), "'x''y'");
        assert_eq!(d.literal(&Value::bytes(vec![1u8, 171]), None), "X'01AB'");
        assert_eq!(d.literal(&Value::decimal("1.50"), None), "1.50");
    }

    #[test]
    fn create_and_alter() {
        let d = GenericDialect::new();
        let mut t = TableDef::new(Some("app".into()), "users");
        t.columns.push(ColumnDef::new("id", "INTEGER").not_null());
        t.columns.push(ColumnDef::new("name", "VARCHAR(100)"));
        t.primary_key = Some(KeyDef { name: None, columns: vec!["id".into()] });
        let sql = d.ddl().create_table(&t, true);
        assert!(sql[0].starts_with("CREATE TABLE app.users ("), "{}", sql[0]);
        assert!(sql[0].contains("PRIMARY KEY (id)"));

        let mut t2 = t.clone();
        t2.columns[1].nullable = false;
        t2.columns.push(ColumnDef::new("email", "VARCHAR(200)"));
        let alter = d.ddl().alter_table(&t, &t2, &[]);
        assert_eq!(alter.len(), 2, "{alter:?}");
        assert!(alter[0].contains("SET NOT NULL"));
        assert!(alter[1].contains("ADD COLUMN email"));
    }

    #[test]
    fn limit_wrap() {
        let d = GenericDialect::new();
        assert_eq!(d.limit_query("select 1;", 10, 0), "SELECT * FROM (select 1) dbiji_q LIMIT 10");
        assert_eq!(d.count_query("select 1"), "SELECT COUNT(*) FROM (select 1) dbiji_cnt");
    }
}
