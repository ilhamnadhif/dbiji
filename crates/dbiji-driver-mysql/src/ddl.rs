//! DDL generation for MySQL / MariaDB ([`MySqlDdl`]).
//!
//! MySQL specific column details that the core model has no field for are
//! carried in [`TableDef::options`] (see [`option_keys`]):
//! * `on_update:<column>` — `ON UPDATE` expression (e.g. `CURRENT_TIMESTAMP(3)`)
//! * `generated:<column>` — `VIRTUAL` or `STORED` for generated columns
//!   (generated columns without this key are created `STORED`)
//! * `invisible:<column>` — `1` for invisible columns (MySQL 8.0.23+, MariaDB 10.3+)
//!
//! Index prefix lengths are encoded in the index column name as `col(10)`
//! (only when the table has no column literally named like that).

use crate::flavor::ServerProfile;
use dbiji_core::DbKind;
use dbiji_core::ddl::*;
use dbiji_core::dialect::ansi;
use dbiji_core::meta::ObjectKind;
use dbiji_core::value::DataKind;

/// Keys of [`TableDef::options`] understood by the generator.
pub mod option_keys {
    /// Storage engine (`InnoDB`).
    pub const ENGINE: &str = "engine";
    /// Default character set of the table.
    pub const CHARSET: &str = "charset";
    /// Default collation of the table.
    pub const COLLATION: &str = "collation";
    /// Next AUTO_INCREMENT value.
    pub const AUTO_INCREMENT: &str = "auto_increment";
    /// Explicitly specified `ROW_FORMAT`.
    pub const ROW_FORMAT: &str = "row_format";
    /// Prefix of `on_update:<column>` keys.
    pub const ON_UPDATE_PREFIX: &str = "on_update:";
    /// Prefix of `generated:<column>` keys (`VIRTUAL` / `STORED`).
    pub const GENERATED_PREFIX: &str = "generated:";
    /// Prefix of `invisible:<column>` keys (`1` = invisible column).
    pub const INVISIBLE_PREFIX: &str = "invisible:";
}

/// Table options rendered as `NAME=value` (lower case option key → SQL keyword).
const TABLE_OPTIONS: &[(&str, &str)] = &[
    ("engine", "ENGINE"),
    ("charset", "DEFAULT CHARSET"),
    ("collation", "COLLATE"),
    ("auto_increment", "AUTO_INCREMENT"),
    ("row_format", "ROW_FORMAT"),
    ("key_block_size", "KEY_BLOCK_SIZE"),
    ("avg_row_length", "AVG_ROW_LENGTH"),
    ("max_rows", "MAX_ROWS"),
    ("min_rows", "MIN_ROWS"),
    ("pack_keys", "PACK_KEYS"),
    ("checksum", "CHECKSUM"),
    ("delay_key_write", "DELAY_KEY_WRITE"),
    ("stats_persistent", "STATS_PERSISTENT"),
    ("stats_auto_recalc", "STATS_AUTO_RECALC"),
    ("stats_sample_pages", "STATS_SAMPLE_PAGES"),
    ("compression", "COMPRESSION"),
    ("encryption", "ENCRYPTION"),
    ("page_compressed", "PAGE_COMPRESSED"),
    ("page_compression_level", "PAGE_COMPRESSION_LEVEL"),
    ("transactional", "TRANSACTIONAL"),
    ("insert_method", "INSERT_METHOD"),
];

/// Options whose values are string literals.
const QUOTED_OPTIONS: &[&str] = &["compression", "encryption"];

/// Backtick-quote an identifier.
pub fn quote(ident: &str) -> String {
    format!("`{}`", ident.replace('`', "``"))
}

/// `` `schema`.`name` `` (schema omitted when empty).
pub fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(s) => format!("{}.{}", quote(s), quote(name)),
        None => quote(name),
    }
}

/// MySQL string literal (backslash escapes).
pub fn string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            '\0' => out.push_str("\\0"),
            '\u{1a}' => out.push_str("\\Z"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Default expression is a literal that MySQL accepts without parentheses.
pub fn is_literal_default(d: &str) -> bool {
    let t = d.trim();
    if t.is_empty() {
        return false;
    }
    let u = t.to_ascii_uppercase();
    if matches!(u.as_str(), "NULL" | "TRUE" | "FALSE") {
        return true;
    }
    let is_quoted = |s: &str, q: char| s.len() >= 2 && s.starts_with(q) && s.ends_with(q);
    if is_quoted(t, '\'') || is_quoted(t, '"') {
        return true;
    }
    if (u.starts_with("B'") || u.starts_with("X'")) && t.ends_with('\'') {
        return true;
    }
    if u.starts_with("0X") && t[2..].chars().all(|c| c.is_ascii_hexdigit()) && t.len() > 2 {
        return true;
    }
    if u.starts_with('_') && t.contains('\'') && t.ends_with('\'') {
        // charset introducer: _utf8mb4'abc'
        return true;
    }
    let num = t.strip_prefix(['-', '+']).unwrap_or(t);
    !num.is_empty()
        && num
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '-' || c == '+')
        && num
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || c == '.')
}

/// `CURRENT_TIMESTAMP`-like defaults allowed without parentheses for
/// DATETIME / TIMESTAMP columns.
pub fn is_now_default(d: &str) -> bool {
    let u = d.trim().to_ascii_uppercase();
    let base = u.split('(').next().unwrap_or("").trim();
    let args_ok = match u.find('(') {
        Some(i) => {
            u.ends_with(')')
                && u[i + 1..u.len() - 1]
                    .trim()
                    .chars()
                    .all(|c| c.is_ascii_digit())
        }
        None => true,
    };
    args_ok
        && matches!(
            base,
            "CURRENT_TIMESTAMP" | "NOW" | "LOCALTIME" | "LOCALTIMESTAMP"
        )
}

/// DEFAULT clause text: literals and `CURRENT_TIMESTAMP` as is, every other
/// expression in parentheses (MySQL 8.0.13+ expression defaults).
pub fn default_clause(d: &str) -> String {
    let t = d.trim();
    if is_literal_default(t)
        || is_now_default(t)
        || (t.starts_with('(') && crate::sqltext::strip_outer_parens(t) != t)
    {
        t.to_string()
    } else {
        format!("({t})")
    }
}

/// Generator of MySQL / MariaDB DDL statements.
#[derive(Debug, Clone)]
pub struct MySqlDdl {
    profile: ServerProfile,
}

impl MySqlDdl {
    /// Generator for a server profile (flavor + version; unknown versions
    /// produce the most portable syntax).
    pub fn new(profile: ServerProfile) -> Self {
        MySqlDdl { profile }
    }

    /// The server profile used for version dependent syntax.
    pub fn profile(&self) -> &ServerProfile {
        &self.profile
    }

    fn tname(&self, t: &TableDef) -> String {
        qualified(t.schema.as_deref(), &t.name)
    }

    fn on_update<'a>(&self, t: &'a TableDef, col: &str) -> Option<&'a str> {
        t.options
            .get(&format!("{}{col}", option_keys::ON_UPDATE_PREFIX))
            .map(String::as_str)
            .filter(|s| !s.is_empty())
    }

    fn generated_storage(&self, t: &TableDef, col: &str) -> &'static str {
        match t
            .options
            .get(&format!("{}{col}", option_keys::GENERATED_PREFIX))
            .map(|s| s.to_ascii_uppercase())
        {
            Some(s) if s == "VIRTUAL" => "VIRTUAL",
            _ => "STORED",
        }
    }

    fn invisible(&self, t: &TableDef, col: &str) -> bool {
        t.options
            .get(&format!("{}{col}", option_keys::INVISIBLE_PREFIX))
            .is_some_and(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
    }

    /// Full column definition (as used by CREATE TABLE, ADD / MODIFY COLUMN).
    pub fn column_definition(&self, t: &TableDef, c: &ColumnDef) -> String {
        let mut s = format!("{} {}", quote(&c.name), c.data_type.trim());
        let table_coll = t.options.get(option_keys::COLLATION).map(String::as_str);
        if let Some(coll) = c.collation.as_deref().filter(|x| !x.is_empty())
            && Some(coll) != table_coll
            && is_character_type(&c.data_type)
        {
            s.push_str(" COLLATE ");
            s.push_str(coll);
        }
        if let Some(g) = c.generated.as_deref().filter(|g| !g.trim().is_empty()) {
            s.push_str(&format!(
                " GENERATED ALWAYS AS ({}) {}",
                g.trim(),
                self.generated_storage(t, &c.name)
            ));
            if !c.nullable {
                s.push_str(" NOT NULL");
            }
        } else {
            s.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
            if let Some(d) = c.default.as_deref().filter(|d| !d.trim().is_empty()) {
                s.push_str(" DEFAULT ");
                s.push_str(&default_clause(d));
            }
            if let Some(u) = self.on_update(t, &c.name) {
                s.push_str(" ON UPDATE ");
                s.push_str(u);
            }
            if c.auto_increment && !c.data_type.to_ascii_lowercase().contains("auto_increment") {
                s.push_str(" AUTO_INCREMENT");
            }
        }
        if self.invisible(t, &c.name) {
            s.push_str(" INVISIBLE");
        }
        if let Some(cm) = c.comment.as_deref().filter(|x| !x.is_empty()) {
            s.push_str(" COMMENT ");
            s.push_str(&string_literal(cm));
        }
        s
    }

    /// Key part list `(a, b(10) DESC, (lower(c)))`.
    fn key_parts(&self, t: &TableDef, cols: &[IndexColumn]) -> String {
        cols.iter()
            .map(|c| {
                let mut s = if c.is_expression {
                    format!("({})", c.name)
                } else {
                    match split_prefix(t, &c.name) {
                        Some((base, n)) => format!("{}({n})", quote(base)),
                        None => quote(&c.name),
                    }
                };
                if c.descending {
                    s.push_str(" DESC");
                }
                s
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn plain_key_parts(cols: &[String]) -> String {
        cols.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", ")
    }

    fn find_index<'a>(
        t: &'a TableDef,
        name: Option<&str>,
        cols: &[String],
    ) -> Option<&'a IndexDef> {
        t.indexes
            .iter()
            .find(|i| name.is_some_and(|n| n == i.name))
            .or_else(|| {
                t.indexes.iter().find(|i| {
                    i.columns.len() == cols.len()
                        && i.columns
                            .iter()
                            .zip(cols)
                            .all(|(a, b)| base_name(t, &a.name) == b.as_str())
                })
            })
    }

    fn index_suffix(idx: &IndexDef) -> String {
        let mut s = String::new();
        if let Some(m) = idx.method.as_deref()
            && m.eq_ignore_ascii_case("HASH")
        {
            s.push_str(" USING HASH");
        }
        if let Some(c) = idx.comment.as_deref().filter(|c| !c.is_empty()) {
            s.push_str(" COMMENT ");
            s.push_str(&string_literal(c));
        }
        s
    }

    fn index_kind(idx: &IndexDef) -> &'static str {
        match idx
            .method
            .as_deref()
            .map(str::to_ascii_uppercase)
            .as_deref()
        {
            Some("FULLTEXT") => "FULLTEXT ",
            Some("SPATIAL") | Some("RTREE") => "SPATIAL ",
            _ => {
                if idx.unique {
                    "UNIQUE "
                } else {
                    ""
                }
            }
        }
    }

    fn pk_clause(&self, t: &TableDef, pk: &KeyDef) -> String {
        let parts = match t.indexes.iter().find(|i| i.primary) {
            Some(idx) if same_columns(t, idx, &pk.columns) => self.key_parts(t, &idx.columns),
            _ => Self::plain_key_parts(&pk.columns),
        };
        format!("PRIMARY KEY ({parts})")
    }

    fn unique_clause(&self, t: &TableDef, k: &KeyDef) -> String {
        let idx = Self::find_index(t, k.name.as_deref(), &k.columns);
        let parts = match idx {
            Some(i) if same_columns(t, i, &k.columns) => self.key_parts(t, &i.columns),
            _ => Self::plain_key_parts(&k.columns),
        };
        let mut s = match &k.name {
            Some(n) => format!("UNIQUE KEY {} ({parts})", quote(n)),
            None => format!("UNIQUE KEY ({parts})"),
        };
        if let Some(i) = idx {
            s.push_str(&Self::index_suffix(i));
        }
        s
    }

    fn index_clause(&self, t: &TableDef, idx: &IndexDef) -> String {
        format!(
            "{}KEY {} ({}){}",
            Self::index_kind(idx),
            quote(&idx.name),
            self.key_parts(t, &idx.columns),
            Self::index_suffix(idx)
        )
    }

    fn check_clause(c: &CheckDef) -> String {
        match &c.name {
            Some(n) => format!("CONSTRAINT {} CHECK ({})", quote(n), c.expression.trim()),
            None => format!("CHECK ({})", c.expression.trim()),
        }
    }

    fn fk_clause(&self, t: &TableDef, fk: &ForeignKeyDef) -> String {
        let mut s = String::new();
        if let Some(n) = &fk.name {
            s.push_str(&format!("CONSTRAINT {} ", quote(n)));
        }
        let ref_schema = fk
            .ref_schema
            .as_deref()
            .filter(|rs| Some(*rs) != t.schema.as_deref());
        s.push_str(&format!(
            "FOREIGN KEY ({}) REFERENCES {} ({})",
            Self::plain_key_parts(&fk.columns),
            qualified(ref_schema, &fk.ref_table),
            Self::plain_key_parts(&fk.ref_columns)
        ));
        if fk.on_delete != FkAction::NoAction {
            s.push_str(&format!(" ON DELETE {}", fk.on_delete.sql()));
        }
        if fk.on_update != FkAction::NoAction {
            s.push_str(&format!(" ON UPDATE {}", fk.on_update.sql()));
        }
        s
    }

    /// `ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 ... COMMENT='...'`.
    pub fn table_options(&self, t: &TableDef) -> String {
        let mut parts = Vec::new();
        for (key, kw) in TABLE_OPTIONS {
            if let Some(v) = t
                .options
                .get(*key)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
            {
                if QUOTED_OPTIONS.contains(key) {
                    parts.push(format!("{kw}={}", string_literal(v)));
                } else if v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    parts.push(format!("{kw}={v}"));
                }
            }
        }
        if let Some(c) = t.comment.as_deref().filter(|c| !c.is_empty()) {
            parts.push(format!("COMMENT={}", string_literal(c)));
        }
        parts.join(" ")
    }

    /// `PARTITION BY ...` clause, if the table is partitioned.
    pub fn partition_clause(&self, t: &TableDef) -> Option<String> {
        let p = t.partitioning.as_ref()?;
        let strategy = p.strategy.trim().to_ascii_uppercase();
        if strategy.is_empty() {
            return None;
        }
        let key = p.key.trim();
        let mut s = format!("PARTITION BY {strategy} ({key})");
        let ranged = strategy.starts_with("RANGE") || strategy.starts_with("LIST");
        if !t.partitions.is_empty() {
            let defs: Vec<String> = t
                .partitions
                .iter()
                .map(|pd| {
                    let bound = pd.bound.trim();
                    if ranged && !bound.is_empty() {
                        format!("PARTITION {} {bound}", quote(&pd.name))
                    } else {
                        format!("PARTITION {}", quote(&pd.name))
                    }
                })
                .collect();
            s.push_str(&format!("\n(\n    {}\n)", defs.join(",\n    ")));
        }
        Some(s)
    }

    /// `CREATE TABLE` statement body lines.
    fn create_lines(&self, t: &TableDef, include_fks: bool) -> Vec<String> {
        let mut lines: Vec<String> = t
            .columns
            .iter()
            .map(|c| self.column_definition(t, c))
            .collect();
        if let Some(pk) = t.primary_key.as_ref().filter(|pk| !pk.columns.is_empty()) {
            lines.push(self.pk_clause(t, pk));
        }
        for u in &t.unique_keys {
            lines.push(self.unique_clause(t, u));
        }
        for idx in ansi::secondary_indexes(t) {
            lines.push(self.index_clause(t, idx));
        }
        for c in &t.checks {
            lines.push(Self::check_clause(c));
        }
        if include_fks {
            for fk in &t.foreign_keys {
                lines.push(self.fk_clause(t, fk));
            }
        }
        lines
    }

    fn column_changed(
        &self,
        from_t: &TableDef,
        f: &ColumnDef,
        to_t: &TableDef,
        t: &ColumnDef,
    ) -> bool {
        norm_type(&f.data_type) != norm_type(&t.data_type)
            || f.nullable != t.nullable
            || f.auto_increment != t.auto_increment
            || norm_opt(&f.generated) != norm_opt(&t.generated)
            || norm_opt(&f.comment) != norm_opt(&t.comment)
            || (is_character_type(&t.data_type)
                && t.collation.is_some()
                && f.collation != t.collation)
            || self.on_update(from_t, &f.name) != self.on_update(to_t, &t.name)
            || (t.generated.is_some()
                && self.generated_storage(from_t, &f.name) != self.generated_storage(to_t, &t.name))
            || self.invisible(from_t, &f.name) != self.invisible(to_t, &t.name)
    }

    /// `DROP EVENT IF EXISTS` statement (events are not part of [`DdlGenerator`]).
    pub fn drop_event(&self, schema: Option<&str>, name: &str) -> String {
        format!("DROP EVENT IF EXISTS {}", qualified(schema, name))
    }
}

/// Copy of `t` whose key, index and foreign key column lists use the new
/// names of renamed columns.
fn apply_renames(t: &TableDef, pairs: &[(&ColumnDef, &ColumnDef)]) -> TableDef {
    let map = |name: &str| -> String {
        pairs
            .iter()
            .find(|(f, n)| f.name == name && f.name != n.name)
            .map(|(_, n)| n.name.clone())
            .unwrap_or_else(|| name.to_string())
    };
    let mut out = t.clone();
    if let Some(pk) = out.primary_key.as_mut() {
        pk.columns = pk.columns.iter().map(|c| map(c)).collect();
    }
    for k in out.unique_keys.iter_mut() {
        k.columns = k.columns.iter().map(|c| map(c)).collect();
    }
    for fk in out.foreign_keys.iter_mut() {
        fk.columns = fk.columns.iter().map(|c| map(c)).collect();
    }
    for idx in out.indexes.iter_mut() {
        for c in idx.columns.iter_mut() {
            if c.is_expression {
                continue;
            }
            c.name = match split_prefix(t, &c.name) {
                Some((base, n)) => format!("{}({n})", map(base)),
                None => map(&c.name),
            };
        }
    }
    // column-keyed options follow the rename too
    let keys: Vec<String> = out.options.keys().cloned().collect();
    for k in keys {
        for prefix in [
            option_keys::ON_UPDATE_PREFIX,
            option_keys::GENERATED_PREFIX,
            option_keys::INVISIBLE_PREFIX,
        ] {
            if let Some(col) = k.strip_prefix(prefix) {
                let new_name = map(col);
                if new_name != col
                    && let Some(v) = out.options.remove(&k)
                {
                    out.options.insert(format!("{prefix}{new_name}"), v);
                }
            }
        }
    }
    out
}

/// The table has a column with exactly this name.
fn has_column(t: &TableDef, name: &str) -> bool {
    t.columns.iter().any(|c| c.name == name)
}

/// `col(10)` → (`col`, 10) unless the table really has a column named `col(10)`.
fn split_prefix<'a>(t: &TableDef, name: &'a str) -> Option<(&'a str, u32)> {
    if has_column(t, name) || !name.ends_with(')') {
        return None;
    }
    let open = name.rfind('(')?;
    let n: u32 = name[open + 1..name.len() - 1].parse().ok()?;
    let base = &name[..open];
    if base.is_empty() {
        None
    } else {
        Some((base, n))
    }
}

fn base_name<'a>(t: &TableDef, name: &'a str) -> &'a str {
    split_prefix(t, name).map(|(b, _)| b).unwrap_or(name)
}

fn same_columns(t: &TableDef, idx: &IndexDef, cols: &[String]) -> bool {
    idx.columns.len() == cols.len()
        && idx
            .columns
            .iter()
            .zip(cols)
            .all(|(a, b)| !a.is_expression && base_name(t, &a.name) == b.as_str())
}

fn is_character_type(data_type: &str) -> bool {
    let t = data_type.trim().to_ascii_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or("");
    matches!(
        base,
        "char"
            | "varchar"
            | "tinytext"
            | "text"
            | "mediumtext"
            | "longtext"
            | "enum"
            | "set"
            | "nchar"
            | "nvarchar"
            | "national"
            | "character"
            | "long"
    )
}

/// Integer display widths are cosmetic (`int(11)` == `int`).
fn norm_type(t: &str) -> String {
    let lower = t.trim().to_ascii_lowercase();
    for base in [
        "tinyint",
        "smallint",
        "mediumint",
        "int",
        "integer",
        "bigint",
    ] {
        if let Some(rest) = lower.strip_prefix(base)
            && let Some(after) = rest.strip_prefix('(')
            && let Some(close) = after.find(')')
        {
            let width = &after[..close];
            if !(base == "tinyint" && width == "1") && width.chars().all(|c| c.is_ascii_digit()) {
                return format!("{base}{}", &after[close + 1..]);
            }
        }
    }
    lower
}

fn norm_opt(s: &Option<String>) -> Option<String> {
    s.as_deref()
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
}

/// Same foreign key. A reference without schema (or to the table's own
/// schema) is schema relative, so tables of two schemas compare equal.
fn fk_same(a: &ForeignKeyDef, b: &ForeignKeyDef, from: &TableDef, to: &TableDef) -> bool {
    let rs = |fk: &ForeignKeyDef, t: &TableDef| {
        fk.ref_schema
            .clone()
            .filter(|s| Some(s) != t.schema.as_ref())
    };
    a.name == b.name
        && a.columns == b.columns
        && a.ref_table == b.ref_table
        && a.ref_columns == b.ref_columns
        && rs(a, from) == rs(b, to)
        && a.on_delete == b.on_delete
        && a.on_update == b.on_update
}

/// Same trigger: compared by its parts when both have them (definitions are
/// qualified with the schema), else by definition text.
fn trigger_same(a: &TriggerDef, b: &TriggerDef) -> bool {
    if a.name != b.name {
        return false;
    }
    match (&a.body, &b.body) {
        (Some(x), Some(y)) => {
            x.trim() == y.trim() && a.timing.eq_ignore_ascii_case(&b.timing) && a.events == b.events
        }
        _ => a.definition.trim() == b.definition.trim(),
    }
}

fn index_same(a: &IndexDef, b: &IndexDef) -> bool {
    ansi::same_index(a, b)
        && a.method.as_deref().map(str::to_ascii_uppercase)
            == b.method.as_deref().map(str::to_ascii_uppercase)
        && norm_opt(&a.comment) == norm_opt(&b.comment)
}

fn table_option_set(t: &TableDef) -> Vec<(&'static str, Option<String>)> {
    TABLE_OPTIONS
        .iter()
        .map(|(k, _)| {
            (
                *k,
                t.options
                    .get(*k)
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty()),
            )
        })
        .collect()
}

impl DdlGenerator for MySqlDdl {
    fn create_table(&self, t: &TableDef, include_foreign_keys: bool) -> Vec<String> {
        let lines = self.create_lines(t, include_foreign_keys);
        let temp = if t.table_kind == TableKind::Temporary {
            "TEMPORARY "
        } else {
            ""
        };
        let mut sql = format!(
            "CREATE {temp}TABLE {} (\n    {}\n)",
            self.tname(t),
            lines.join(",\n    ")
        );
        let opts = self.table_options(t);
        if !opts.is_empty() {
            sql.push(' ');
            sql.push_str(&opts);
        }
        if let Some(p) = self.partition_clause(t) {
            sql.push('\n');
            sql.push_str(&p);
        }
        let mut out = vec![sql];
        for tr in &t.triggers {
            out.extend(self.create_trigger(t, tr));
        }
        out
    }

    fn drop_table(&self, schema: Option<&str>, name: &str, _cascade: bool) -> String {
        format!("DROP TABLE {}", qualified(schema, name))
    }

    fn rename_table(&self, schema: Option<&str>, old_name: &str, new_name: &str) -> String {
        format!(
            "RENAME TABLE {} TO {}",
            qualified(schema, old_name),
            qualified(schema, new_name)
        )
    }

    fn alter_table(
        &self,
        from: &TableDef,
        to: &TableDef,
        renamed_columns: &[(String, String)],
    ) -> Vec<String> {
        let mut out = Vec::new();
        if from.name != to.name {
            // Statements address the table in `to`'s schema (for schema
            // compare the two definitions usually live in different schemas).
            let old = qualified(to.schema.as_deref().or(from.schema.as_deref()), &from.name);
            out.push(format!("RENAME TABLE {old} TO {}", self.tname(to)));
        }
        let t = self.tname(to);
        let (pairs, dropped, added) = ansi::match_columns(from, to, renamed_columns);
        // Keys and indexes of `from` with renamed columns applied: a renamed
        // column keeps its indexes, so they must not be dropped and re-created.
        let rf = apply_renames(from, &pairs);
        let rf = &rf;

        // 1. foreign keys that change or disappear (before columns go away)
        for fk in &rf.foreign_keys {
            if !to.foreign_keys.iter().any(|x| fk_same(fk, x, rf, to)) {
                out.push(self.drop_foreign_key(to, fk));
            }
        }
        // 2. checks
        for c in &rf.checks {
            if !to.checks.contains(c) && c.name.is_some() {
                out.push(self.drop_check(to, c));
            }
        }
        // 3. secondary indexes, unique keys, primary key
        let from_idx: Vec<&IndexDef> = ansi::secondary_indexes(rf).collect();
        let to_idx: Vec<&IndexDef> = ansi::secondary_indexes(to).collect();
        for i in &from_idx {
            if !to_idx.iter().any(|x| index_same(x, i)) {
                out.push(self.drop_index(to, i));
            }
        }
        for u in &rf.unique_keys {
            if !to.unique_keys.contains(u) && u.name.is_some() {
                out.push(self.drop_unique_key(to, u));
            }
        }
        let pk_changed = rf.primary_key.as_ref().filter(|k| !k.columns.is_empty())
            != to.primary_key.as_ref().filter(|k| !k.columns.is_empty());
        let mut pk_pending_add = false;
        // A primary key change that needs the column steps first: a key on a column
        // added below, or dropping the key of a column that loses AUTO_INCREMENT below.
        let mut pk_after_columns: Option<String> = None;
        if pk_changed {
            let keeps_columns = |k: &KeyDef| {
                !k.columns
                    .iter()
                    .any(|c| dropped.iter().any(|d| &d.name == c))
            };
            match (
                rf.primary_key.as_ref().filter(|k| !k.columns.is_empty()),
                to.primary_key.as_ref().filter(|k| !k.columns.is_empty()),
            ) {
                (Some(old_pk), Some(new_pk)) => {
                    let sql = format!(
                        "ALTER TABLE {t} DROP PRIMARY KEY, ADD {}",
                        self.pk_clause(to, new_pk)
                    );
                    let on_added = new_pk
                        .columns
                        .iter()
                        .any(|c| added.iter().any(|a| &a.name == c));
                    if on_added && keeps_columns(old_pk) {
                        pk_after_columns = Some(sql);
                    } else if on_added {
                        // The old key goes with its dropped columns (or is dropped now when some
                        // stay); the new one waits for its added column.
                        let all_dropped = old_pk
                            .columns
                            .iter()
                            .all(|c| dropped.iter().any(|d| &d.name == c));
                        if !all_dropped {
                            out.push(self.drop_primary_key(to, old_pk));
                        }
                        pk_after_columns = Some(format!(
                            "ALTER TABLE {t} ADD {}",
                            self.pk_clause(to, new_pk)
                        ));
                    } else {
                        out.push(sql);
                    }
                }
                (Some(old_pk), None) => {
                    let loses_auto = pairs.iter().any(|(fc, tc)| {
                        fc.auto_increment && !tc.auto_increment && old_pk.columns.contains(&tc.name)
                    });
                    if loses_auto && keeps_columns(old_pk) {
                        pk_after_columns = Some(self.drop_primary_key(to, old_pk));
                    } else {
                        out.push(self.drop_primary_key(to, old_pk));
                    }
                }
                (None, Some(_)) => pk_pending_add = true,
                (None, None) => {}
            }
        }
        // 4. dropped columns
        for c in &dropped {
            out.push(self.drop_column(to, &c.name));
        }
        // 5. renamed / modified columns
        for (fc, tc) in &pairs {
            let changed = self.column_changed(from, fc, to, tc);
            let default_changed = norm_opt(&fc.default) != norm_opt(&tc.default);
            if fc.name != tc.name {
                if changed || default_changed || !self.profile.has_rename_column() {
                    out.push(format!(
                        "ALTER TABLE {t} CHANGE COLUMN {} {}",
                        quote(&fc.name),
                        self.column_definition(to, tc)
                    ));
                } else {
                    out.push(format!(
                        "ALTER TABLE {t} RENAME COLUMN {} TO {}",
                        quote(&fc.name),
                        quote(&tc.name)
                    ));
                }
                continue;
            }
            if changed {
                out.push(format!(
                    "ALTER TABLE {t} MODIFY COLUMN {}",
                    self.column_definition(to, tc)
                ));
            } else if default_changed {
                match tc.default.as_deref().filter(|d| !d.trim().is_empty()) {
                    Some(d) if is_literal_default(d) => out.push(format!(
                        "ALTER TABLE {t} ALTER COLUMN {} SET DEFAULT {}",
                        quote(&tc.name),
                        d.trim()
                    )),
                    Some(_) => out.push(format!(
                        "ALTER TABLE {t} MODIFY COLUMN {}",
                        self.column_definition(to, tc)
                    )),
                    None => out.push(format!(
                        "ALTER TABLE {t} ALTER COLUMN {} DROP DEFAULT",
                        quote(&tc.name)
                    )),
                }
            }
        }
        // 6. added columns at their position
        for c in &added {
            out.extend(self.add_column(to, c));
        }
        out.extend(pk_after_columns);
        // 7. reorder remaining columns
        let mut current: Vec<String> = Vec::new();
        for fc in &from.columns {
            if let Some((_, tc)) = pairs.iter().find(|(f, _)| std::ptr::eq(*f, fc)) {
                current.push(tc.name.clone());
            }
        }
        // simulate `ADD COLUMN ... FIRST | AFTER <previous column of `to`>`
        for (i, tc) in to.columns.iter().enumerate() {
            if added.iter().any(|a| std::ptr::eq(*a, tc)) {
                let at = if i == 0 {
                    0
                } else {
                    current
                        .iter()
                        .position(|n| n == &to.columns[i - 1].name)
                        .map(|p| p + 1)
                        .unwrap_or(current.len())
                };
                current.insert(at, tc.name.clone());
            }
        }
        for (i, tc) in to.columns.iter().enumerate() {
            if current.get(i).map(String::as_str) == Some(tc.name.as_str()) {
                continue;
            }
            if let Some(j) = current.iter().position(|n| n == &tc.name) {
                let name = current.remove(j);
                current.insert(i, name);
                let pos = if i == 0 {
                    " FIRST".to_string()
                } else {
                    format!(" AFTER {}", quote(&to.columns[i - 1].name))
                };
                out.push(format!(
                    "ALTER TABLE {t} MODIFY COLUMN {}{pos}",
                    self.column_definition(to, tc)
                ));
            }
        }
        // 8. new keys, indexes, checks
        if pk_pending_add && let Some(pk) = &to.primary_key {
            out.push(self.add_primary_key(to, pk));
        }
        for u in &to.unique_keys {
            if !rf.unique_keys.contains(u) {
                out.push(self.add_unique_key(to, u));
            }
        }
        for i in &to_idx {
            if !from_idx.iter().any(|x| index_same(x, i)) {
                out.push(self.create_index(to, i));
            }
        }
        for c in &to.checks {
            if !rf.checks.contains(c) {
                out.push(self.add_check(to, c));
            }
        }
        // 9. table options and comment
        let mut opt_changes = Vec::new();
        let (fo, to_o) = (table_option_set(rf), table_option_set(to));
        for ((key, fv), (_, tv)) in fo.iter().zip(to_o.iter()) {
            if fv != tv
                && let Some(v) = tv
            {
                let kw = TABLE_OPTIONS
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, kw)| *kw)
                    .unwrap_or("");
                if QUOTED_OPTIONS.contains(key) {
                    opt_changes.push(format!("{kw}={}", string_literal(v)));
                } else if v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    opt_changes.push(format!("{kw}={v}"));
                }
            }
        }
        if norm_opt(&rf.comment) != norm_opt(&to.comment) {
            opt_changes.push(format!(
                "COMMENT={}",
                string_literal(to.comment.as_deref().unwrap_or(""))
            ));
        }
        if !opt_changes.is_empty() {
            out.push(format!("ALTER TABLE {t} {}", opt_changes.join(" ")));
        }
        // 10. partitioning
        if rf.partitioning != to.partitioning
            || rf
                .partitions
                .iter()
                .map(|p| (&p.name, &p.bound))
                .ne(to.partitions.iter().map(|p| (&p.name, &p.bound)))
        {
            match self.partition_clause(to) {
                Some(p) => out.push(format!("ALTER TABLE {t} {p}")),
                None => {
                    if rf.partitioning.is_some() {
                        out.push(format!("ALTER TABLE {t} REMOVE PARTITIONING"));
                    }
                }
            }
        }
        // 11. foreign keys last
        for fk in &to.foreign_keys {
            if !rf.foreign_keys.iter().any(|x| fk_same(x, fk, rf, to)) {
                out.push(self.add_foreign_key(to, fk));
            }
        }
        // 12. triggers
        for tr in &rf.triggers {
            if !to.triggers.iter().any(|x| trigger_same(x, tr)) {
                out.push(self.drop_trigger(to, tr));
            }
        }
        for tr in &to.triggers {
            if !rf.triggers.iter().any(|x| trigger_same(x, tr)) {
                out.extend(self.create_trigger(to, tr));
            }
        }
        out
    }

    fn add_column(&self, table: &TableDef, column: &ColumnDef) -> Vec<String> {
        let pos = match table.columns.iter().position(|c| c.name == column.name) {
            Some(0) => " FIRST".to_string(),
            Some(i) => format!(" AFTER {}", quote(&table.columns[i - 1].name)),
            None => String::new(),
        };
        vec![format!(
            "ALTER TABLE {} ADD COLUMN {}{pos}",
            self.tname(table),
            self.column_definition(table, column)
        )]
    }

    fn drop_column(&self, table: &TableDef, column: &str) -> String {
        format!(
            "ALTER TABLE {} DROP COLUMN {}",
            self.tname(table),
            quote(column)
        )
    }

    fn create_index(&self, table: &TableDef, index: &IndexDef) -> String {
        format!(
            "CREATE {}INDEX {} ON {} ({}){}",
            Self::index_kind(index),
            quote(&index.name),
            self.tname(table),
            self.key_parts(table, &index.columns),
            Self::index_suffix(index)
        )
    }

    fn drop_index(&self, table: &TableDef, index: &IndexDef) -> String {
        format!("DROP INDEX {} ON {}", quote(&index.name), self.tname(table))
    }

    fn add_primary_key(&self, table: &TableDef, key: &KeyDef) -> String {
        format!(
            "ALTER TABLE {} ADD {}",
            self.tname(table),
            self.pk_clause(table, key)
        )
    }

    fn drop_primary_key(&self, table: &TableDef, _key: &KeyDef) -> String {
        format!("ALTER TABLE {} DROP PRIMARY KEY", self.tname(table))
    }

    fn add_unique_key(&self, table: &TableDef, key: &KeyDef) -> String {
        format!(
            "ALTER TABLE {} ADD {}",
            self.tname(table),
            self.unique_clause(table, key)
        )
    }

    fn drop_unique_key(&self, table: &TableDef, key: &KeyDef) -> String {
        format!(
            "ALTER TABLE {} DROP INDEX {}",
            self.tname(table),
            quote(key.name.as_deref().unwrap_or_default())
        )
    }

    fn add_check(&self, table: &TableDef, check: &CheckDef) -> String {
        format!(
            "ALTER TABLE {} ADD {}",
            self.tname(table),
            Self::check_clause(check)
        )
    }

    fn drop_check(&self, table: &TableDef, check: &CheckDef) -> String {
        format!(
            "ALTER TABLE {} DROP {} {}",
            self.tname(table),
            self.profile.drop_check_keyword(),
            quote(check.name.as_deref().unwrap_or_default())
        )
    }

    fn add_foreign_key(&self, table: &TableDef, fk: &ForeignKeyDef) -> String {
        format!(
            "ALTER TABLE {} ADD {}",
            self.tname(table),
            self.fk_clause(table, fk)
        )
    }

    fn drop_foreign_key(&self, table: &TableDef, fk: &ForeignKeyDef) -> String {
        format!(
            "ALTER TABLE {} DROP FOREIGN KEY {}",
            self.tname(table),
            quote(fk.name.as_deref().unwrap_or_default())
        )
    }

    /// Built from `timing`, `events` and `body` (qualified with the table's
    /// schema, so definitions can be re-created in another schema) when those
    /// are present; otherwise `definition` is used as is.
    fn create_trigger(&self, table: &TableDef, trigger: &TriggerDef) -> Vec<String> {
        let def = trigger.definition.trim();
        let complete_parts = trigger
            .body
            .as_deref()
            .is_some_and(|b| !b.trim().is_empty())
            && !trigger.timing.trim().is_empty()
            && !trigger.events.is_empty();
        if !complete_parts && !def.is_empty() {
            return vec![ansi::strip_trailing_semicolon(def).to_string()];
        }
        let body = trigger.body.as_deref().unwrap_or("BEGIN END").trim();
        let events = if trigger.events.is_empty() {
            "INSERT".to_string()
        } else {
            trigger.events.join(" OR ")
        };
        vec![format!(
            "CREATE TRIGGER {} {} {} ON {} FOR EACH ROW {}",
            qualified(table.schema.as_deref(), &trigger.name),
            if trigger.timing.is_empty() {
                "BEFORE"
            } else {
                trigger.timing.as_str()
            },
            events,
            self.tname(table),
            ansi::strip_trailing_semicolon(body)
        )]
    }

    fn drop_trigger(&self, table: &TableDef, trigger: &TriggerDef) -> String {
        format!(
            "DROP TRIGGER IF EXISTS {}",
            qualified(table.schema.as_deref(), &trigger.name)
        )
    }

    fn create_view(&self, view: &ViewDef, or_replace: bool) -> Vec<String> {
        let mut s = String::from("CREATE ");
        if or_replace {
            s.push_str("OR REPLACE ");
        }
        if let Some(a) = view
            .options
            .get("algorithm")
            .filter(|a| !a.is_empty() && a.as_str() != "UNDEFINED")
            && a.chars().all(|c| c.is_ascii_alphabetic())
        {
            s.push_str(&format!("ALGORITHM={a} "));
        }
        if let Some(sec) = view.options.get("security").map(|v| v.to_ascii_uppercase())
            && (sec == "INVOKER" || sec == "DEFINER")
        {
            s.push_str(&format!("SQL SECURITY {sec} "));
        }
        s.push_str(&format!(
            "VIEW {} AS\n{}",
            qualified(view.schema.as_deref(), &view.name),
            ansi::strip_trailing_semicolon(view.definition.trim())
        ));
        if let Some(co) = view
            .options
            .get("check_option")
            .map(|v| v.to_ascii_uppercase())
            && (co == "CASCADED" || co == "LOCAL")
        {
            s.push_str(&format!("\nWITH {co} CHECK OPTION"));
        }
        vec![s]
    }

    fn drop_view(&self, view: &ViewDef) -> String {
        format!(
            "DROP VIEW IF EXISTS {}",
            qualified(view.schema.as_deref(), &view.name)
        )
    }

    fn create_sequence(&self, seq: &SequenceDef) -> String {
        let mut s = format!(
            "CREATE SEQUENCE {}",
            qualified(seq.schema.as_deref(), &seq.name)
        );
        if let Some(dt) = seq
            .data_type
            .as_deref()
            .filter(|d| !d.is_empty() && self.profile.at_least(11, 5, 0))
        {
            s.push_str(&format!(" AS {dt}"));
        }
        if let Some(v) = seq.start {
            s.push_str(&format!(" START WITH {v}"));
        }
        if let Some(v) = seq.min_value {
            s.push_str(&format!(" MINVALUE {v}"));
        }
        if let Some(v) = seq.max_value {
            s.push_str(&format!(" MAXVALUE {v}"));
        }
        if let Some(v) = seq.increment {
            s.push_str(&format!(" INCREMENT BY {v}"));
        }
        match seq.cache {
            Some(0) => s.push_str(" NOCACHE"),
            Some(v) => s.push_str(&format!(" CACHE {v}")),
            None => {}
        }
        s.push_str(if seq.cycle { " CYCLE" } else { " NOCYCLE" });
        s
    }

    fn alter_sequence(&self, from: &SequenceDef, to: &SequenceDef) -> Vec<String> {
        let mut parts = Vec::new();
        if from.increment != to.increment
            && let Some(v) = to.increment
        {
            parts.push(format!("INCREMENT BY {v}"));
        }
        if from.min_value != to.min_value {
            parts.push(
                to.min_value
                    .map(|v| format!("MINVALUE {v}"))
                    .unwrap_or_else(|| "NO MINVALUE".into()),
            );
        }
        if from.max_value != to.max_value {
            parts.push(
                to.max_value
                    .map(|v| format!("MAXVALUE {v}"))
                    .unwrap_or_else(|| "NO MAXVALUE".into()),
            );
        }
        if from.start != to.start
            && let Some(v) = to.start
        {
            parts.push(format!("START WITH {v}"));
        }
        if from.cache != to.cache {
            match to.cache {
                Some(0) | None => parts.push("NOCACHE".into()),
                Some(v) => parts.push(format!("CACHE {v}")),
            }
        }
        if from.cycle != to.cycle {
            parts.push(if to.cycle {
                "CYCLE".into()
            } else {
                "NOCYCLE".into()
            });
        }
        if parts.is_empty() {
            Vec::new()
        } else {
            vec![format!(
                "ALTER SEQUENCE {} {}",
                qualified(to.schema.as_deref(), &to.name),
                parts.join(" ")
            )]
        }
    }

    fn drop_sequence(&self, seq: &SequenceDef) -> String {
        format!(
            "DROP SEQUENCE IF EXISTS {}",
            qualified(seq.schema.as_deref(), &seq.name)
        )
    }

    fn create_routine(&self, routine: &RoutineDef) -> Vec<String> {
        let def = ansi::strip_trailing_semicolon(routine.definition.trim()).to_string();
        if def.is_empty() {
            return Vec::new();
        }
        if self.profile.is_mariadb() && self.profile.has_create_or_replace_routines() {
            if let Some(rest) = strip_keyword(&def, "CREATE")
                && strip_keyword(rest, "OR").is_none()
            {
                return vec![format!("CREATE OR REPLACE {}", rest.trim_start())];
            }
            return vec![def];
        }
        vec![self.drop_routine(routine), def]
    }

    fn drop_routine(&self, routine: &RoutineDef) -> String {
        let kind = match routine.kind {
            Some(ObjectKind::Procedure) => "PROCEDURE",
            _ => "FUNCTION",
        };
        format!(
            "DROP {kind} IF EXISTS {}",
            qualified(routine.schema.as_deref(), &routine.name)
        )
    }

    fn create_schema(&self, name: &str) -> String {
        format!("CREATE DATABASE {}", quote(name))
    }

    fn drop_schema(&self, name: &str, _cascade: bool) -> String {
        format!("DROP DATABASE {}", quote(name))
    }

    fn map_type(&self, source: DbKind, data_type: &str, kind: DataKind) -> String {
        map_type(&self.profile, source, data_type, kind)
    }

    fn default_type_for(&self, kind: DataKind) -> String {
        default_type_for(kind).to_string()
    }
}

/// `strip_keyword("CREATE x", "CREATE")` → `Some(" x")` (case-insensitive, word boundary).
fn strip_keyword<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let t = s.trim_start();
    if t.len() >= kw.len() && t[..kw.len()].eq_ignore_ascii_case(kw) {
        let rest = &t[kw.len()..];
        if rest
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_')
        {
            return Some(rest);
        }
    }
    None
}

/// Type used for a logical kind when nothing better is known.
pub fn default_type_for(kind: DataKind) -> &'static str {
    match kind {
        DataKind::Bool => "tinyint(1)",
        DataKind::Int => "bigint",
        DataKind::Float => "double",
        DataKind::Decimal => "decimal(65,30)",
        DataKind::Text | DataKind::Xml | DataKind::Other => "longtext",
        DataKind::Binary => "longblob",
        DataKind::Date => "date",
        DataKind::Time | DataKind::TimeTz => "time(6)",
        DataKind::DateTime | DataKind::DateTimeTz => "datetime(6)",
        DataKind::Interval => "varchar(64)",
        DataKind::Uuid => "char(36)",
        DataKind::Json | DataKind::Array => "json",
        DataKind::Geometry => "geometry",
        DataKind::Enum | DataKind::Range => "varchar(255)",
        DataKind::Network => "varchar(45)",
        DataKind::Bit => "varchar(255)",
    }
}

/// Parse `name(a, b)` into (`name`, ["a", "b"]); modifiers after `)` are returned separately.
fn split_type(t: &str) -> (String, Vec<String>, String) {
    let t = t.trim();
    match t.find('(') {
        Some(open) => {
            let close = t.rfind(')').unwrap_or(t.len());
            let args = if close > open {
                &t[open + 1..close]
            } else {
                ""
            };
            let rest = if close < t.len() {
                t[close + 1..].trim().to_string()
            } else {
                String::new()
            };
            (
                t[..open].trim().to_ascii_lowercase(),
                args.split(',')
                    .map(|a| a.trim().to_string())
                    .filter(|a| !a.is_empty())
                    .collect(),
                rest,
            )
        }
        None => (t.to_ascii_lowercase(), Vec::new(), String::new()),
    }
}

fn num_arg(args: &[String], i: usize) -> Option<u64> {
    args.get(i).and_then(|a| a.parse::<u64>().ok())
}

fn varchar(n: Option<u64>) -> String {
    match n {
        Some(n) if n > 0 && n <= 16_383 => format!("varchar({n})"),
        Some(n) if n > 16_383 && n <= 16_777_215 => "mediumtext".into(),
        _ => "longtext".into(),
    }
}

fn decimal(args: &[String]) -> String {
    match (num_arg(args, 0), num_arg(args, 1)) {
        (Some(p), Some(s)) => format!(
            "decimal({},{})",
            p.clamp(1, 65),
            s.min(30).min(p.clamp(1, 65))
        ),
        (Some(p), None) => format!("decimal({},0)", p.clamp(1, 65)),
        _ => "decimal(65,30)".into(),
    }
}

fn fsp(args: &[String], default: u64) -> u64 {
    num_arg(args, 0).unwrap_or(default).min(6)
}

/// Is `base` a type name MySQL / MariaDB understand natively?
fn is_native(base: &str) -> bool {
    crate::keywords::DATA_TYPES
        .iter()
        .any(|t| t.eq_ignore_ascii_case(base))
}

/// Convert a column type of another product into a MySQL / MariaDB type.
pub fn map_type(
    profile: &ServerProfile,
    source: DbKind,
    data_type: &str,
    kind: DataKind,
) -> String {
    let raw = data_type.trim();
    let lower = raw.to_ascii_lowercase();
    if lower.ends_with("[]") || lower.starts_with('_') && source.is_postgres_family() {
        return "json".into();
    }
    let (base, args, rest) = split_type(&lower);
    let base = base.trim().to_string();

    // ClickHouse wrappers
    if source == DbKind::ClickHouse
        && let Some(inner) = lower
            .strip_prefix("nullable(")
            .or_else(|| lower.strip_prefix("lowcardinality("))
    {
        let inner = inner.strip_suffix(')').unwrap_or(inner);
        return map_type(profile, source, inner, kind);
    }

    match source {
        DbKind::MySql | DbKind::MariaDb => {
            return match base.as_str() {
                "uuid" if !profile.is_mariadb() => "char(36)".into(),
                "inet6" if !profile.is_mariadb() => "varchar(45)".into(),
                "inet4" if !profile.is_mariadb() => "varchar(15)".into(),
                _ => raw.to_string(),
            };
        }
        DbKind::Postgres | DbKind::CockroachDb => {
            let mapped: Option<String> = match base.as_str() {
                "boolean" | "bool" => Some("tinyint(1)".into()),
                "smallint" | "int2" => Some("smallint".into()),
                "integer" | "int" | "int4" => Some("int".into()),
                "bigint" | "int8" | "int64" => Some("bigint".into()),
                "smallserial" | "serial2" => Some("smallint auto_increment".into()),
                "serial" | "serial4" => Some("int auto_increment".into()),
                "bigserial" | "serial8" => Some("bigint auto_increment".into()),
                "real" | "float4" => Some("float".into()),
                "double precision" | "float8" | "double" => Some("double".into()),
                "float" => Some(if num_arg(&args, 0).is_some_and(|p| p <= 24) {
                    "float".into()
                } else {
                    "double".into()
                }),
                "numeric" | "decimal" => Some(decimal(&args)),
                "money" => Some("decimal(19,4)".into()),
                "text" | "citext" | "tsvector" | "tsquery" | "xml" => Some("longtext".into()),
                "character varying" | "varchar" => Some(varchar(num_arg(&args, 0))),
                "character" | "char" | "bpchar" => match num_arg(&args, 0) {
                    Some(n) if n <= 255 => Some(format!("char({n})")),
                    Some(n) => Some(varchar(Some(n))),
                    None => Some("char(1)".into()),
                },
                "\"char\"" => Some("char(1)".into()),
                "name" => Some("varchar(64)".into()),
                "bytea" => Some("longblob".into()),
                "uuid" => Some("char(36)".into()),
                "json" | "jsonb" => Some("json".into()),
                "date" => Some("date".into()),
                "time" | "time without time zone" | "timetz" | "time with time zone" => {
                    Some(format!("time({})", fsp(&args, 6)))
                }
                "timestamp"
                | "timestamp without time zone"
                | "timestamptz"
                | "timestamp with time zone" => Some(format!("datetime({})", fsp(&args, 6))),
                "interval" => Some("varchar(64)".into()),
                "inet" | "cidr" => Some("varchar(45)".into()),
                "macaddr" => Some("varchar(17)".into()),
                "macaddr8" => Some("varchar(23)".into()),
                "bit" => Some(format!(
                    "bit({})",
                    num_arg(&args, 0).unwrap_or(1).clamp(1, 64)
                )),
                "bit varying" | "varbit" => Some(varchar(num_arg(&args, 0).or(Some(255)))),
                "oid" | "regclass" | "regproc" | "regtype" | "xid" | "cid" => {
                    Some("int unsigned".into())
                }
                "point" => Some("point".into()),
                "polygon" => Some("polygon".into()),
                "line" | "lseg" | "path" => Some("linestring".into()),
                "box" | "circle" | "geometry" | "geography" => Some("geometry".into()),
                b if b.ends_with("range") || b.ends_with("multirange") => {
                    Some("varchar(255)".into())
                }
                _ => None,
            };
            if let Some(m) = mapped {
                return m;
            }
            // `timestamp(3) with time zone` and friends
            if lower.starts_with("timestamp") {
                return format!("datetime({})", fsp(&args, 6));
            }
            if lower.starts_with("time") {
                return format!("time({})", fsp(&args, 6));
            }
            if lower.starts_with("character varying") {
                return varchar(num_arg(&args, 0));
            }
            if lower.starts_with("double precision") {
                return "double".into();
            }
        }
        DbKind::Sqlite => {
            let mapped: Option<String> = match base.as_str() {
                "integer" => Some("bigint".into()),
                "int" | "mediumint" | "smallint" | "tinyint" | "bigint" => Some(base.clone()),
                "real" | "double" | "double precision" | "float" => Some("double".into()),
                "numeric" | "decimal" => Some(decimal(&args)),
                "text" | "clob" => Some("longtext".into()),
                "blob" => Some("longblob".into()),
                "boolean" | "bool" => Some("tinyint(1)".into()),
                "varchar" | "nvarchar" | "character varying" | "varying character"
                | "native character" => Some(varchar(num_arg(&args, 0))),
                "char" | "nchar" | "character" => match num_arg(&args, 0) {
                    Some(n) if n <= 255 => Some(format!("char({n})")),
                    n => Some(varchar(n)),
                },
                "date" => Some("date".into()),
                "datetime" | "timestamp" => Some("datetime(6)".into()),
                "time" => Some("time(6)".into()),
                "json" => Some("json".into()),
                "uuid" => Some("char(36)".into()),
                "" => Some(default_type_for(kind).into()),
                _ => None,
            };
            if let Some(m) = mapped {
                return m;
            }
        }
        DbKind::SqlServer => {
            let max = args.first().is_some_and(|a| a == "max");
            let mapped: Option<String> = match base.as_str() {
                "bit" => Some("tinyint(1)".into()),
                "tinyint" => Some("tinyint unsigned".into()),
                "smallint" | "int" | "bigint" => Some(base.clone()),
                "decimal" | "numeric" => Some(decimal(&args)),
                "money" => Some("decimal(19,4)".into()),
                "smallmoney" => Some("decimal(10,4)".into()),
                "float" => Some(if num_arg(&args, 0).is_some_and(|p| p <= 24) {
                    "float".into()
                } else {
                    "double".into()
                }),
                "real" => Some("float".into()),
                "char" | "nchar" => match num_arg(&args, 0) {
                    Some(n) if n <= 255 => Some(format!("char({n})")),
                    n => Some(varchar(n)),
                },
                "varchar" | "nvarchar" => Some(if max {
                    "longtext".into()
                } else {
                    varchar(num_arg(&args, 0).or(Some(1)))
                }),
                "text" | "ntext" | "xml" | "sql_variant" => Some("longtext".into()),
                "binary" => Some(format!(
                    "binary({})",
                    num_arg(&args, 0).unwrap_or(1).min(255)
                )),
                "varbinary" => Some(if max {
                    "longblob".into()
                } else {
                    format!("varbinary({})", num_arg(&args, 0).unwrap_or(1).min(65_532))
                }),
                "image" => Some("longblob".into()),
                "uniqueidentifier" => Some("char(36)".into()),
                "date" => Some("date".into()),
                "time" => Some(format!("time({})", fsp(&args, 6))),
                "datetime" => Some("datetime(3)".into()),
                "datetime2" => Some(format!("datetime({})", fsp(&args, 6))),
                "smalldatetime" => Some("datetime".into()),
                "datetimeoffset" => Some(format!("datetime({})", fsp(&args, 6))),
                "timestamp" | "rowversion" => Some("binary(8)".into()),
                "hierarchyid" => Some("varbinary(892)".into()),
                "geometry" | "geography" => Some("geometry".into()),
                "sysname" => Some("varchar(128)".into()),
                _ => None,
            };
            if let Some(m) = mapped {
                return m;
            }
        }
        DbKind::ClickHouse => {
            let mapped: Option<String> = match base.as_str() {
                "uint8" => Some("tinyint unsigned".into()),
                "uint16" => Some("smallint unsigned".into()),
                "uint32" => Some("int unsigned".into()),
                "uint64" => Some("bigint unsigned".into()),
                "uint128" | "int128" => Some("decimal(39,0)".into()),
                "uint256" | "int256" => Some("varchar(78)".into()),
                "int8" => Some("tinyint".into()),
                "int16" => Some("smallint".into()),
                "int32" => Some("int".into()),
                "int64" => Some("bigint".into()),
                "float32" => Some("float".into()),
                "float64" => Some("double".into()),
                "decimal" => Some(decimal(&args)),
                "decimal32" => Some(format!(
                    "decimal(9,{})",
                    num_arg(&args, 0).unwrap_or(0).min(9)
                )),
                "decimal64" => Some(format!(
                    "decimal(18,{})",
                    num_arg(&args, 0).unwrap_or(0).min(18)
                )),
                "decimal128" => Some(format!(
                    "decimal(38,{})",
                    num_arg(&args, 0).unwrap_or(0).min(30)
                )),
                "decimal256" => Some(format!(
                    "decimal(65,{})",
                    num_arg(&args, 0).unwrap_or(0).min(30)
                )),
                "string" => Some("longtext".into()),
                "fixedstring" => Some(format!(
                    "binary({})",
                    num_arg(&args, 0).unwrap_or(1).min(255)
                )),
                "uuid" => Some("char(36)".into()),
                "date" | "date32" => Some("date".into()),
                "datetime" => Some("datetime".into()),
                "datetime64" => Some(format!("datetime({})", fsp(&args, 3))),
                "bool" | "boolean" => Some("tinyint(1)".into()),
                "ipv4" => Some("varchar(15)".into()),
                "ipv6" => Some("varchar(45)".into()),
                "enum8" | "enum16" | "enum" => {
                    let labels: Vec<String> = raw
                        .split('\'')
                        .enumerate()
                        .filter(|(i, _)| i % 2 == 1)
                        .map(|(_, l)| string_literal(l))
                        .collect();
                    Some(if labels.is_empty() {
                        "varchar(255)".into()
                    } else {
                        format!("enum({})", labels.join(","))
                    })
                }
                "array" | "map" | "tuple" | "nested" | "json" | "object" => Some("json".into()),
                "point" => Some("point".into()),
                "ring" | "polygon" => Some("polygon".into()),
                "multipolygon" => Some("multipolygon".into()),
                _ => None,
            };
            if let Some(m) = mapped {
                return m;
            }
        }
        DbKind::Oracle => {
            // `VARCHAR2(100 BYTE)`: the length is the first word.
            let size = args
                .first()
                .and_then(|a| a.split_whitespace().next())
                .and_then(|a| a.parse::<u64>().ok());
            let mapped: Option<String> = match base.as_str() {
                "number" | "numeric" | "decimal" | "dec" => {
                    Some(match (num_arg(&args, 0), num_arg(&args, 1)) {
                        (Some(p), None | Some(0)) if p <= 9 => "int".into(),
                        (Some(p), None | Some(0)) if p <= 18 => "bigint".into(),
                        (Some(_), _) => decimal(&args),
                        _ => "decimal(65,30)".into(),
                    })
                }
                "integer" | "int" | "smallint" => Some("decimal(38,0)".into()),
                "float" | "binary_double" | "double precision" => Some("double".into()),
                "binary_float" | "real" => Some("float".into()),
                "varchar2" | "nvarchar2" | "varchar" | "char" | "nchar" => Some(varchar(size)),
                "clob" | "nclob" | "long" | "xmltype" => Some("longtext".into()),
                "blob" | "raw" | "long raw" | "bfile" => Some("longblob".into()),
                "date" => Some("datetime".into()),
                b if b.starts_with("timestamp") => Some("datetime(6)".into()),
                b if b.starts_with("interval") => Some("varchar(64)".into()),
                "json" => Some("json".into()),
                "boolean" => Some("tinyint(1)".into()),
                "rowid" | "urowid" | "vector" => Some("varchar(4000)".into()),
                _ => None,
            };
            if let Some(m) = mapped {
                return m;
            }
        }
        DbKind::Cassandra => {
            return match base.as_str() {
                "ascii" | "text" | "varchar" => "longtext".into(),
                "bigint" | "counter" => "bigint".into(),
                "int" => "int".into(),
                "smallint" => "smallint".into(),
                "tinyint" => "tinyint".into(),
                "varint" => "decimal(65,0)".into(),
                "decimal" => "decimal(65,30)".into(),
                "double" => "double".into(),
                "float" => "float".into(),
                "boolean" => "tinyint(1)".into(),
                "blob" => "longblob".into(),
                "date" => "date".into(),
                "time" => "time(6)".into(),
                "timestamp" => "datetime(6)".into(),
                "uuid" | "timeuuid" => "char(36)".into(),
                "inet" => "varchar(45)".into(),
                "duration" => "varchar(64)".into(),
                _ => "json".into(),
            };
        }
        DbKind::Generic
        | DbKind::Redis
        | DbKind::MongoDb
        | DbKind::Elasticsearch
        | DbKind::Neo4j => {}
    }
    let _ = rest;
    if is_native(&base) {
        raw.to_string()
    } else {
        default_type_for(kind).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flavor::Flavor;

    fn ddl() -> MySqlDdl {
        MySqlDdl::new(ServerProfile::new(Flavor::MySql, (8, 0, 36)))
    }

    fn sample() -> TableDef {
        let mut t = TableDef::new(Some("shop".into()), "orders");
        let mut id = ColumnDef::new("id", "int unsigned").not_null();
        id.auto_increment = true;
        let mut name = ColumnDef::new("name", "varchar(100)")
            .not_null()
            .default_value("'it''s'");
        name.comment = Some("Customer's name".into());
        let created = ColumnDef::new("created", "timestamp")
            .not_null()
            .default_value("CURRENT_TIMESTAMP");
        let updated =
            ColumnDef::new("updated", "datetime(3)").default_value("CURRENT_TIMESTAMP(3)");
        let mut total = ColumnDef::new("total", "decimal(10,2)").default_value("(rand() * 10)");
        total.nullable = true;
        let mut twice = ColumnDef::new("twice", "decimal(12,2)");
        twice.generated = Some("`total` * 2".into());
        t.columns = vec![id, name, created, updated, total, twice];
        t.options.insert("engine".into(), "InnoDB".into());
        t.options.insert("charset".into(), "utf8mb4".into());
        t.options
            .insert("collation".into(), "utf8mb4_0900_ai_ci".into());
        t.options
            .insert("on_update:updated".into(), "CURRENT_TIMESTAMP(3)".into());
        t.options.insert("generated:twice".into(), "VIRTUAL".into());
        t.comment = Some("All orders".into());
        t.primary_key = Some(KeyDef {
            name: Some("PRIMARY".into()),
            columns: vec!["id".into()],
        });
        t.unique_keys = vec![KeyDef {
            name: Some("uk_name".into()),
            columns: vec!["name".into()],
        }];
        t.indexes = vec![
            IndexDef {
                name: "PRIMARY".into(),
                columns: vec![IndexColumn::column("id")],
                unique: true,
                primary: true,
                method: Some("BTREE".into()),
                ..Default::default()
            },
            IndexDef {
                name: "uk_name".into(),
                columns: vec![IndexColumn::column("name(20)")],
                unique: true,
                method: Some("BTREE".into()),
                ..Default::default()
            },
            IndexDef {
                name: "idx_created".into(),
                columns: vec![
                    IndexColumn {
                        name: "created".into(),
                        is_expression: false,
                        descending: true,
                    },
                    IndexColumn {
                        name: "lower(`name`)".into(),
                        is_expression: true,
                        descending: false,
                    },
                ],
                method: Some("BTREE".into()),
                comment: Some("by date".into()),
                ..Default::default()
            },
        ];
        t.checks = vec![CheckDef {
            name: Some("chk_total".into()),
            expression: "`total` >= 0".into(),
        }];
        t.foreign_keys = vec![ForeignKeyDef {
            name: Some("fk_cust".into()),
            columns: vec!["id".into()],
            ref_schema: Some("shop".into()),
            ref_table: "customers".into(),
            ref_columns: vec!["id".into()],
            on_delete: FkAction::Cascade,
            ..Default::default()
        }];
        t
    }

    #[test]
    fn create_table_full() {
        let sql = ddl().create_table(&sample(), true);
        assert_eq!(sql.len(), 1);
        let s = &sql[0];
        assert!(
            s.starts_with(
                "CREATE TABLE `shop`.`orders` (\n    `id` int unsigned NOT NULL AUTO_INCREMENT,"
            ),
            "{s}"
        );
        assert!(
            s.contains("`name` varchar(100) NOT NULL DEFAULT 'it''s' COMMENT 'Customer''s name'"),
            "{s}"
        );
        assert!(
            s.contains("`created` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,"),
            "{s}"
        );
        assert!(s.contains("`updated` datetime(3) NULL DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3)"), "{s}");
        assert!(
            s.contains("`total` decimal(10,2) NULL DEFAULT (rand() * 10)"),
            "{s}"
        );
        assert!(
            s.contains("`twice` decimal(12,2) GENERATED ALWAYS AS (`total` * 2) VIRTUAL"),
            "{s}"
        );
        assert!(s.contains("PRIMARY KEY (`id`)"), "{s}");
        assert!(s.contains("UNIQUE KEY `uk_name` (`name`(20))"), "{s}");
        assert!(
            s.contains("KEY `idx_created` (`created` DESC, (lower(`name`))) COMMENT 'by date'"),
            "{s}"
        );
        assert!(
            s.contains("CONSTRAINT `chk_total` CHECK (`total` >= 0)"),
            "{s}"
        );
        assert!(s.contains("CONSTRAINT `fk_cust` FOREIGN KEY (`id`) REFERENCES `customers` (`id`) ON DELETE CASCADE"), "{s}");
        assert!(s.ends_with(") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='All orders'"), "{s}");
        let no_fk = ddl().create_table(&sample(), false);
        assert!(!no_fk[0].contains("FOREIGN KEY"));
    }

    #[test]
    fn partitions() {
        let mut t = TableDef::new(None, "logs");
        t.columns = vec![
            ColumnDef::new("id", "int").not_null(),
            ColumnDef::new("y", "int").not_null(),
        ];
        t.partitioning = Some(PartitioningDef {
            strategy: "RANGE".into(),
            key: "`y`".into(),
        });
        t.partitions = vec![
            PartitionDef {
                name: "p2023".into(),
                bound: "VALUES LESS THAN (2024)".into(),
                row_estimate: None,
            },
            PartitionDef {
                name: "pmax".into(),
                bound: "VALUES LESS THAN MAXVALUE".into(),
                row_estimate: None,
            },
        ];
        let s = &ddl().create_table(&t, true)[0];
        assert!(s.ends_with("PARTITION BY RANGE (`y`)\n(\n    PARTITION `p2023` VALUES LESS THAN (2024),\n    PARTITION `pmax` VALUES LESS THAN MAXVALUE\n)"), "{s}");
        t.partitioning = Some(PartitioningDef {
            strategy: "HASH".into(),
            key: "`id`".into(),
        });
        t.partitions = vec![
            PartitionDef {
                name: "p0".into(),
                ..Default::default()
            },
            PartitionDef {
                name: "p1".into(),
                ..Default::default()
            },
        ];
        let s = &ddl().create_table(&t, true)[0];
        assert!(
            s.ends_with("PARTITION BY HASH (`id`)\n(\n    PARTITION `p0`,\n    PARTITION `p1`\n)"),
            "{s}"
        );
    }

    #[test]
    fn alter_ordering() {
        let from = sample();
        let mut to = sample();
        // drop fk + column it uses? change FK action
        to.foreign_keys[0].on_delete = FkAction::Restrict;
        // rename name -> full_name
        to.columns[1].name = "full_name".into();
        to.unique_keys[0].columns = vec!["full_name".into()];
        to.indexes[1].columns = vec![IndexColumn::column("full_name(20)")];
        // modify total type
        to.columns[4].data_type = "decimal(12,2)".into();
        // add column at position 2
        to.columns
            .insert(2, ColumnDef::new("email", "varchar(200)"));
        // drop 'twice'
        to.columns.retain(|c| c.name != "twice");
        to.comment = Some("Orders".into());
        let stmts = ddl().alter_table(&from, &to, &[("name".into(), "full_name".into())]);
        let pos = |needle: &str| {
            stmts
                .iter()
                .position(|s| s.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle}: {stmts:#?}"))
        };
        assert!(pos("DROP FOREIGN KEY `fk_cust`") < pos("DROP COLUMN `twice`"));
        assert!(pos("RENAME COLUMN `name` TO `full_name`") > pos("DROP COLUMN"));
        // the unique key follows the renamed column; it is not rebuilt
        assert!(!stmts.iter().any(|s| s.contains("uk_name")), "{stmts:#?}");
        assert!(pos("MODIFY COLUMN `total` decimal(12,2)") > 0);
        assert!(
            stmts
                .iter()
                .any(|s| s.contains("ADD COLUMN `email` varchar(200) NULL AFTER `full_name`")),
            "{stmts:#?}"
        );
        assert!(pos("COMMENT='Orders'") < pos("ADD CONSTRAINT `fk_cust`"));
        assert_eq!(
            stmts.last().map(|s| s.contains("FOREIGN KEY")),
            Some(true),
            "{stmts:#?}"
        );
    }

    #[test]
    fn alter_simple_rename_and_defaults() {
        let mut from = TableDef::new(None, "t");
        from.columns = vec![
            ColumnDef::new("a", "int"),
            ColumnDef::new("b", "int").default_value("1"),
        ];
        let mut to = from.clone();
        to.columns[0].name = "a2".into();
        to.columns[1].default = Some("2".into());
        let stmts = ddl().alter_table(&from, &to, &[("a".into(), "a2".into())]);
        assert_eq!(
            stmts,
            vec![
                "ALTER TABLE `t` RENAME COLUMN `a` TO `a2`",
                "ALTER TABLE `t` ALTER COLUMN `b` SET DEFAULT 2"
            ]
        );
        let old = MySqlDdl::new(ServerProfile::new(Flavor::MySql, (5, 7, 44)));
        let stmts = old.alter_table(&from, &to, &[("a".into(), "a2".into())]);
        assert_eq!(stmts[0], "ALTER TABLE `t` CHANGE COLUMN `a` `a2` int NULL");
        to.columns[1].default = None;
        let stmts = ddl().alter_table(&from, &to, &[("a".into(), "a2".into())]);
        assert_eq!(stmts[1], "ALTER TABLE `t` ALTER COLUMN `b` DROP DEFAULT");
        // int(11) vs int is not a change
        let mut to2 = from.clone();
        to2.columns[0].data_type = "int(11)".into();
        assert!(ddl().alter_table(&from, &to2, &[]).is_empty());
        // reorder
        let mut to3 = from.clone();
        to3.columns.swap(0, 1);
        let stmts = ddl().alter_table(&from, &to3, &[]);
        assert_eq!(
            stmts,
            vec!["ALTER TABLE `t` MODIFY COLUMN `b` int NULL DEFAULT 1 FIRST"]
        );
    }

    #[test]
    fn pk_change_is_atomic() {
        let mut from = TableDef::new(None, "t");
        from.columns = vec![
            ColumnDef::new("a", "int").not_null(),
            ColumnDef::new("b", "int").not_null(),
        ];
        from.primary_key = Some(KeyDef {
            name: None,
            columns: vec!["a".into()],
        });
        let mut to = from.clone();
        to.primary_key = Some(KeyDef {
            name: None,
            columns: vec!["a".into(), "b".into()],
        });
        assert_eq!(
            ddl().alter_table(&from, &to, &[]),
            vec!["ALTER TABLE `t` DROP PRIMARY KEY, ADD PRIMARY KEY (`a`, `b`)"]
        );
    }

    #[test]
    fn pk_moved_to_an_added_column_replacing_the_old_one() {
        let mut from = TableDef::new(None, "t");
        from.columns = vec![
            ColumnDef::new("a", "int").not_null(),
            ColumnDef::new("v", "int"),
        ];
        from.primary_key = Some(KeyDef {
            name: None,
            columns: vec!["a".into()],
        });
        let mut to = from.clone();
        to.columns = vec![
            ColumnDef::new("v", "int"),
            ColumnDef::new("id", "bigint").not_null(),
        ];
        to.primary_key = Some(KeyDef {
            name: None,
            columns: vec!["id".into()],
        });
        let sql = ddl().alter_table(&from, &to, &[]);
        let pos = |p: &str| {
            sql.iter()
                .position(|s| s.contains(p))
                .unwrap_or_else(|| panic!("{p} in {sql:#?}"))
        };
        assert!(
            pos("ADD COLUMN `id`") < pos("ADD PRIMARY KEY (`id`)"),
            "{sql:#?}"
        );
        assert!(
            !sql.iter().any(|s| s.contains("DROP PRIMARY KEY")),
            "{sql:#?}"
        );
    }

    #[test]
    fn pk_on_added_column_comes_after_it() {
        let mut from = TableDef::new(None, "t");
        from.columns = vec![ColumnDef::new("a", "int").not_null()];
        from.primary_key = Some(KeyDef {
            name: None,
            columns: vec!["a".into()],
        });
        let mut to = from.clone();
        to.columns.push(ColumnDef::new("b", "int").not_null());
        to.primary_key = Some(KeyDef {
            name: None,
            columns: vec!["a".into(), "b".into()],
        });
        let sql = ddl().alter_table(&from, &to, &[]);
        let add = sql
            .iter()
            .position(|s| s.contains("ADD COLUMN `b`"))
            .expect("add column");
        let pk = sql
            .iter()
            .position(|s| s.contains("DROP PRIMARY KEY"))
            .expect("pk change");
        assert!(add < pk, "{sql:#?}");
    }

    #[test]
    fn invisible_columns() {
        let mut t = TableDef::new(None, "t");
        t.columns = vec![
            ColumnDef::new("a", "int"),
            ColumnDef::new("h", "int").default_value("7"),
        ];
        t.options.insert("invisible:h".into(), "1".into());
        let sql = &ddl().create_table(&t, true)[0];
        assert!(sql.contains("`h` int NULL DEFAULT 7 INVISIBLE"), "{sql}");
        let mut visible = t.clone();
        visible.options.remove("invisible:h");
        assert_eq!(
            ddl().alter_table(&t, &visible, &[]),
            vec!["ALTER TABLE `t` MODIFY COLUMN `h` int NULL DEFAULT 7"]
        );
    }

    #[test]
    fn objects() {
        let d = ddl();
        let t = sample();
        assert_eq!(
            d.drop_index(&t, &t.indexes[2]),
            "DROP INDEX `idx_created` ON `shop`.`orders`"
        );
        assert_eq!(
            d.drop_check(&t, &t.checks[0]),
            "ALTER TABLE `shop`.`orders` DROP CHECK `chk_total`"
        );
        let maria = MySqlDdl::new(ServerProfile::new(Flavor::MariaDb, (11, 4, 0)));
        assert_eq!(
            maria.drop_check(&t, &t.checks[0]),
            "ALTER TABLE `shop`.`orders` DROP CONSTRAINT `chk_total`"
        );
        let v = ViewDef {
            schema: Some("s".into()),
            name: "v".into(),
            definition: "SELECT 1;".into(),
            ..Default::default()
        };
        assert_eq!(
            d.create_view(&v, true),
            vec!["CREATE OR REPLACE VIEW `s`.`v` AS\nSELECT 1"]
        );
        assert_eq!(d.drop_view(&v), "DROP VIEW IF EXISTS `s`.`v`");
        let r = RoutineDef {
            schema: Some("s".into()),
            name: "p".into(),
            kind: Some(ObjectKind::Procedure),
            definition: "CREATE PROCEDURE `p`() BEGIN SELECT 1; END".into(),
            ..Default::default()
        };
        assert_eq!(
            d.create_routine(&r),
            vec![
                "DROP PROCEDURE IF EXISTS `s`.`p`".to_string(),
                r.definition.clone()
            ]
        );
        assert_eq!(
            maria.create_routine(&r),
            vec!["CREATE OR REPLACE PROCEDURE `p`() BEGIN SELECT 1; END".to_string()]
        );
        let seq = SequenceDef {
            schema: Some("s".into()),
            name: "q".into(),
            start: Some(10),
            increment: Some(5),
            max_value: Some(1000),
            cache: Some(20),
            ..Default::default()
        };
        assert_eq!(
            maria.create_sequence(&seq),
            "CREATE SEQUENCE `s`.`q` START WITH 10 MAXVALUE 1000 INCREMENT BY 5 CACHE 20 NOCYCLE"
        );
        assert_eq!(d.create_schema("new db"), "CREATE DATABASE `new db`");
        assert_eq!(
            d.rename_table(Some("s"), "a", "b"),
            "RENAME TABLE `s`.`a` TO `s`.`b`"
        );
        let tr = TriggerDef {
            name: "trg".into(),
            timing: "BEFORE".into(),
            events: vec!["UPDATE".into()],
            body: Some("SET NEW.a = 1".into()),
            ..Default::default()
        };
        assert_eq!(
            d.create_trigger(&t, &tr),
            vec![
                "CREATE TRIGGER `shop`.`trg` BEFORE UPDATE ON `shop`.`orders` FOR EACH ROW SET NEW.a = 1"
            ]
        );
    }

    #[test]
    fn type_mapping() {
        let p = ServerProfile::new(Flavor::MySql, (8, 0, 36));
        let m = |src, t: &str| map_type(&p, src, t, DataKind::guess_from_type_name(t));
        assert_eq!(m(DbKind::Postgres, "boolean"), "tinyint(1)");
        assert_eq!(m(DbKind::Postgres, "text"), "longtext");
        assert_eq!(m(DbKind::Postgres, "bytea"), "longblob");
        assert_eq!(m(DbKind::Postgres, "uuid"), "char(36)");
        assert_eq!(m(DbKind::Postgres, "jsonb"), "json");
        assert_eq!(m(DbKind::Postgres, "timestamptz"), "datetime(6)");
        assert_eq!(
            m(DbKind::Postgres, "timestamp(3) with time zone"),
            "datetime(3)"
        );
        assert_eq!(m(DbKind::Postgres, "serial"), "int auto_increment");
        assert_eq!(m(DbKind::Postgres, "double precision"), "double");
        assert_eq!(m(DbKind::Postgres, "integer[]"), "json");
        assert_eq!(m(DbKind::Postgres, "_int4"), "json");
        assert_eq!(m(DbKind::Postgres, "interval"), "varchar(64)");
        assert_eq!(m(DbKind::Postgres, "inet"), "varchar(45)");
        assert_eq!(m(DbKind::Postgres, "money"), "decimal(19,4)");
        assert_eq!(m(DbKind::Postgres, "numeric(12,3)"), "decimal(12,3)");
        assert_eq!(m(DbKind::Postgres, "numeric"), "decimal(65,30)");
        assert_eq!(m(DbKind::Postgres, "character varying(50)"), "varchar(50)");
        assert_eq!(m(DbKind::Postgres, "varchar"), "longtext");
        assert_eq!(m(DbKind::SqlServer, "nvarchar(max)"), "longtext");
        assert_eq!(m(DbKind::SqlServer, "nvarchar(40)"), "varchar(40)");
        assert_eq!(m(DbKind::SqlServer, "bit"), "tinyint(1)");
        assert_eq!(m(DbKind::SqlServer, "uniqueidentifier"), "char(36)");
        assert_eq!(m(DbKind::SqlServer, "datetime2(7)"), "datetime(6)");
        assert_eq!(m(DbKind::SqlServer, "money"), "decimal(19,4)");
        assert_eq!(m(DbKind::SqlServer, "varbinary(max)"), "longblob");
        assert_eq!(m(DbKind::Sqlite, "INTEGER"), "bigint");
        assert_eq!(m(DbKind::Sqlite, "TEXT"), "longtext");
        assert_eq!(m(DbKind::ClickHouse, "UInt32"), "int unsigned");
        assert_eq!(m(DbKind::ClickHouse, "Nullable(String)"), "longtext");
        assert_eq!(
            m(DbKind::ClickHouse, "LowCardinality(Nullable(String))"),
            "longtext"
        );
        assert_eq!(m(DbKind::ClickHouse, "DateTime64(3)"), "datetime(3)");
        assert_eq!(
            m(DbKind::ClickHouse, "Enum8('a' = 1, 'b' = 2)"),
            "enum('a','b')"
        );
        assert_eq!(m(DbKind::ClickHouse, "Array(UInt8)"), "json");
        assert_eq!(m(DbKind::Oracle, "NUMBER(10,2)"), "decimal(10,2)");
        assert_eq!(m(DbKind::Oracle, "NUMBER(9)"), "int");
        assert_eq!(m(DbKind::Oracle, "VARCHAR2(100 BYTE)"), "varchar(100)");
        assert_eq!(
            m(DbKind::Oracle, "TIMESTAMP(6) WITH TIME ZONE"),
            "datetime(6)"
        );
        assert_eq!(m(DbKind::Oracle, "CLOB"), "longtext");
        assert_eq!(m(DbKind::Cassandra, "map<text, int>"), "json");
        assert_eq!(m(DbKind::Cassandra, "timeuuid"), "char(36)");
        assert_eq!(m(DbKind::MySql, "int unsigned"), "int unsigned");
        assert_eq!(m(DbKind::MariaDb, "uuid"), "char(36)");
        let maria = ServerProfile::new(Flavor::MariaDb, (11, 4, 0));
        assert_eq!(
            map_type(&maria, DbKind::MariaDb, "uuid", DataKind::Uuid),
            "uuid"
        );
        assert_eq!(m(DbKind::Postgres, "some_custom_enum"), "longtext");
        assert_eq!(default_type_for(DataKind::Bool), "tinyint(1)");
        assert_eq!(default_type_for(DataKind::Uuid), "char(36)");
    }

    #[test]
    fn default_rendering() {
        assert_eq!(default_clause("0"), "0");
        assert_eq!(default_clause("-1.5"), "-1.5");
        assert_eq!(default_clause("'x'"), "'x'");
        assert_eq!(default_clause("NULL"), "NULL");
        assert_eq!(
            default_clause("CURRENT_TIMESTAMP(3)"),
            "CURRENT_TIMESTAMP(3)"
        );
        assert_eq!(default_clause("current_timestamp()"), "current_timestamp()");
        assert_eq!(default_clause("b'101'"), "b'101'");
        assert_eq!(default_clause("0x6162"), "0x6162");
        assert_eq!(default_clause("x'6162'"), "x'6162'");
        assert_eq!(default_clause("rand()"), "(rand())");
        assert_eq!(default_clause("(rand())"), "(rand())");
        assert_eq!(default_clause("uuid()"), "(uuid())");
        assert_eq!(string_literal("a'b\\c"), "'a''b\\\\c'");
    }
}
