//! SQL templates (FR-S5): snippets such as `sel` → `SELECT * FROM ${table};`
//! typed in the editor and expanded with Tab.
//!
//! Variables: `${name}` or `${name:default}`; `${cursor}` marks where the
//! caret goes after expansion; `$$` is a literal `$`. Built-in values when
//! not supplied by the caller: `${date}` (YYYY-MM-DD), `${time}` (HH:MM:SS),
//! `${datetime}`, `${year}`. Variables without a value are replaced by their
//! default or their name (shown as placeholders by the editor).
//!
//! User templates live in `config/templates.json`; a user template with the
//! same name and database kind replaces the built-in one.

use crate::paths::{AppPaths, backup_corrupt_file, write_json};
use dbiji_core::{DbKind, DbResult};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ops::Range;

/// A snippet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SqlTemplate {
    /// Abbreviation typed in the editor.
    pub name: String,
    /// Shown in the completion list.
    pub description: String,
    /// Template text with `${...}` variables.
    pub body: String,
    /// Restrict to one database kind (`None` = every database).
    pub kind: Option<DbKind>,
    /// Disabled templates are not offered.
    pub enabled: bool,
}

impl Default for SqlTemplate {
    fn default() -> Self {
        SqlTemplate {
            name: String::new(),
            description: String::new(),
            body: String::new(),
            kind: None,
            enabled: true,
        }
    }
}

impl SqlTemplate {
    /// A template available for every database.
    pub fn new(name: &str, description: &str, body: &str) -> SqlTemplate {
        SqlTemplate {
            name: name.into(),
            description: description.into(),
            body: body.into(),
            kind: None,
            enabled: true,
        }
    }

    /// Same template restricted to `kind`.
    pub fn for_kind(mut self, kind: DbKind) -> SqlTemplate {
        self.kind = Some(kind);
        self
    }

    /// Variable names used in the body (without `cursor`), in order of appearance.
    pub fn variables(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for seg in parse(&self.body) {
            if let Segment::Var { name, .. } = seg
                && name != "cursor"
                && !out.contains(&name)
            {
                out.push(name);
            }
        }
        out
    }
}

/// Built-in templates.
pub fn builtin_templates() -> Vec<SqlTemplate> {
    vec![
        SqlTemplate::new("sel", "Select all rows", "SELECT * FROM ${table}${cursor};"),
        SqlTemplate::new("sf", "Select columns", "SELECT ${cursor}\nFROM ${table};"),
        SqlTemplate::new("sw", "Select with condition", "SELECT *\nFROM ${table}\nWHERE ${cursor};"),
        SqlTemplate::new("scount", "Count rows", "SELECT COUNT(*) FROM ${table}${cursor};"),
        SqlTemplate::new("sdist", "Distinct values", "SELECT DISTINCT ${column}\nFROM ${table}${cursor};"),
        SqlTemplate::new(
            "sgb",
            "Group and count",
            "SELECT ${column}, COUNT(*)\nFROM ${table}\nGROUP BY ${column}\nORDER BY 2 DESC${cursor};",
        ),
        SqlTemplate::new("ins", "Insert a row", "INSERT INTO ${table} (${columns})\nVALUES (${cursor});"),
        SqlTemplate::new("upd", "Update rows", "UPDATE ${table}\nSET ${column} = ${cursor}\nWHERE ;"),
        SqlTemplate::new("del", "Delete rows", "DELETE FROM ${table}\nWHERE ${cursor};"),
        SqlTemplate::new("cte", "Common table expression", "WITH ${name:cte} AS (\n    ${cursor}\n)\nSELECT *\nFROM ${name:cte};"),
        SqlTemplate::new(
            "join",
            "Join two tables",
            "SELECT *\nFROM ${table} a\nJOIN ${table2} b ON b.${column} = a.${column}${cursor};",
        ),
        SqlTemplate::new("ljoin", "Left join", "LEFT JOIN ${table} ${alias:t} ON ${alias:t}.${cursor}"),
        SqlTemplate::new("case", "CASE expression", "CASE\n    WHEN ${cursor} THEN \n    ELSE \nEND"),
        SqlTemplate::new("exists", "EXISTS subquery", "EXISTS (SELECT 1 FROM ${table} WHERE ${cursor})"),
        SqlTemplate::new("crt", "Create table", "CREATE TABLE ${table} (\n    id INTEGER PRIMARY KEY,\n    ${cursor}\n);"),
        SqlTemplate::new(
            "crt",
            "Create table",
            "CREATE TABLE ${table} (\n    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,\n    ${cursor}\n);",
        )
        .for_kind(DbKind::Postgres),
        SqlTemplate::new("crt", "Create table", "CREATE TABLE ${table} (\n    id BIGINT AUTO_INCREMENT PRIMARY KEY,\n    ${cursor}\n);")
            .for_kind(DbKind::MySql),
        SqlTemplate::new("crt", "Create table", "CREATE TABLE ${table} (\n    id BIGINT AUTO_INCREMENT PRIMARY KEY,\n    ${cursor}\n);")
            .for_kind(DbKind::MariaDb),
        SqlTemplate::new(
            "crt",
            "Create table",
            "CREATE TABLE ${table} (\n    id BIGINT IDENTITY(1,1) PRIMARY KEY,\n    ${cursor}\n);",
        )
        .for_kind(DbKind::SqlServer),
        SqlTemplate::new("crt", "Create table", "CREATE TABLE ${table} (\n    id UInt64,\n    ${cursor}\n)\nENGINE = MergeTree\nORDER BY id;")
            .for_kind(DbKind::ClickHouse),
        SqlTemplate::new("idx", "Create index", "CREATE INDEX ${name:idx_name} ON ${table} (${column}${cursor});"),
        SqlTemplate::new("altadd", "Add a column", "ALTER TABLE ${table} ADD COLUMN ${column} ${cursor};"),
        SqlTemplate::new("lim", "Limit rows", "SELECT *\nFROM ${table}\nLIMIT ${limit:100}${cursor};"),
        SqlTemplate::new("lim", "Limit rows", "SELECT TOP ${limit:100} *\nFROM ${table}${cursor};").for_kind(DbKind::SqlServer),
        SqlTemplate::new("exp", "Explain", "EXPLAIN ${cursor}"),
        SqlTemplate::new("expa", "Explain analyze", "EXPLAIN ANALYZE ${cursor}").for_kind(DbKind::Postgres),
        SqlTemplate::new("now", "Current timestamp", "CURRENT_TIMESTAMP${cursor}"),
        SqlTemplate::new("today", "Today's date literal", "DATE '${date}'${cursor}"),
        // The shell languages have no SQL: their own snippets replace the SQL ones.
        SqlTemplate::new("find", "Find documents", "db.${table}.find({${cursor}}).limit(20)").for_kind(DbKind::MongoDb),
        SqlTemplate::new("find1", "Find one document", "db.${table}.findOne({${cursor}})").for_kind(DbKind::MongoDb),
        SqlTemplate::new(
            "aggr",
            "Aggregate pipeline",
            "db.${table}.aggregate([\n    {$match: {${cursor}}},\n    {$group: {_id: null, n: {$sum: 1}}}\n])",
        )
        .for_kind(DbKind::MongoDb),
        SqlTemplate::new("cnt", "Count documents", "db.${table}.countDocuments({${cursor}})").for_kind(DbKind::MongoDb),
        SqlTemplate::new("insdoc", "Insert a document", "db.${table}.insertOne({${cursor}})").for_kind(DbKind::MongoDb),
        SqlTemplate::new("upddoc", "Update documents", "db.${table}.updateMany({${cursor}}, {$set: {}})")
            .for_kind(DbKind::MongoDb),
        SqlTemplate::new("deldoc", "Delete documents", "db.${table}.deleteMany({${cursor}})").for_kind(DbKind::MongoDb),
        SqlTemplate::new("idx", "Create an index", "db.${table}.createIndex({${cursor}: 1})").for_kind(DbKind::MongoDb),
        SqlTemplate::new("colls", "List collections", "show collections${cursor}").for_kind(DbKind::MongoDb),
        SqlTemplate::new("dbs", "List databases", "show databases${cursor}").for_kind(DbKind::MongoDb),
        SqlTemplate::new("get", "Get a key", "GET ${cursor}").for_kind(DbKind::Redis),
        SqlTemplate::new("set", "Set a key", "SET ${cursor}").for_kind(DbKind::Redis),
        SqlTemplate::new("hgetall", "All fields of a hash", "HGETALL ${cursor}").for_kind(DbKind::Redis),
        SqlTemplate::new("lrange", "List range", "LRANGE ${cursor} 0 -1").for_kind(DbKind::Redis),
        SqlTemplate::new("smembers", "Set members", "SMEMBERS ${cursor}").for_kind(DbKind::Redis),
        SqlTemplate::new("zrange", "Sorted set range", "ZRANGE ${cursor} 0 -1 WITHSCORES").for_kind(DbKind::Redis),
        SqlTemplate::new("scan", "Scan keys", "SCAN 0 MATCH ${cursor}* COUNT 100").for_kind(DbKind::Redis),
        SqlTemplate::new("ttl", "Time to live", "TTL ${cursor}").for_kind(DbKind::Redis),
        SqlTemplate::new("info", "Server information", "INFO${cursor}").for_kind(DbKind::Redis),
    ]
}

/// Do the generic (SQL) templates apply to `kind`? Not to a shell language.
fn generic_applies(kind: Option<DbKind>) -> bool {
    !kind.is_some_and(|k| k.is_shell())
}

/// User templates plus built-ins.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TemplateLibrary {
    /// User defined templates (and overrides of built-ins).
    pub user: Vec<SqlTemplate>,
    /// Names of built-in templates the user disabled.
    pub disabled_builtins: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(default)]
struct TemplatesFile {
    templates: Vec<SqlTemplate>,
    disabled_builtins: Vec<String>,
}

impl TemplateLibrary {
    /// Loads `templates.json`; missing → no user templates, corrupt → backed up with a warning.
    pub fn load(paths: &AppPaths) -> (TemplateLibrary, Vec<String>) {
        let path = paths.templates_file();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (TemplateLibrary::default(), Vec::new());
            }
            Err(e) => {
                return (
                    TemplateLibrary::default(),
                    vec![format!("Cannot read {}: {e}", path.display())],
                );
            }
        };
        match serde_json::from_slice::<TemplatesFile>(&bytes) {
            Ok(f) => (
                TemplateLibrary {
                    user: f.templates,
                    disabled_builtins: f.disabled_builtins,
                },
                Vec::new(),
            ),
            Err(e) => {
                let mut w = format!(
                    "{} is corrupt ({e}); user templates were not loaded",
                    path.display()
                );
                if let Ok(b) = backup_corrupt_file(&path) {
                    w.push_str(&format!(" (saved as {})", b.display()));
                }
                (TemplateLibrary::default(), vec![w])
            }
        }
    }

    /// Writes `templates.json` atomically (mode 0600).
    pub fn save(&self, paths: &AppPaths) -> DbResult<()> {
        let file = TemplatesFile {
            templates: self.user.clone(),
            disabled_builtins: self.disabled_builtins.clone(),
        };
        write_json(&paths.templates_file(), &file)
    }

    /// Every template: built-ins (minus disabled / overridden ones) and user templates.
    pub fn all(&self) -> Vec<SqlTemplate> {
        let mut out: Vec<SqlTemplate> = builtin_templates()
            .into_iter()
            .filter(|b| {
                !self
                    .user
                    .iter()
                    .any(|u| u.name == b.name && u.kind == b.kind)
            })
            .map(|mut b| {
                if self.disabled_builtins.contains(&b.name) {
                    b.enabled = false;
                }
                b
            })
            .collect();
        out.extend(self.user.iter().cloned());
        out
    }

    /// The enabled template called `name` for `kind`: an exact kind match
    /// wins over a generic one; user templates win over built-ins.
    pub fn find(&self, name: &str, kind: Option<DbKind>) -> Option<SqlTemplate> {
        let all = self.all();
        let candidates = all
            .iter()
            .filter(|t| t.enabled && t.name.eq_ignore_ascii_case(name));
        let mut generic = None;
        for t in candidates {
            if t.kind.is_some() && t.kind == kind {
                return Some(t.clone());
            }
            if t.kind.is_none() && generic.is_none() && generic_applies(kind) {
                generic = Some(t.clone());
            }
        }
        generic
    }

    /// Enabled templates for `kind` whose name starts with `prefix` (completion).
    pub fn matching(&self, prefix: &str, kind: Option<DbKind>) -> Vec<SqlTemplate> {
        let mut names: Vec<String> = self
            .all()
            .into_iter()
            .filter(|t| {
                // The name is typed in any case: `HG` finds `hgetall`.
                t.enabled
                    && t.name
                        .to_ascii_lowercase()
                        .starts_with(&prefix.to_ascii_lowercase())
                    && (t.kind == kind || (t.kind.is_none() && generic_applies(kind)))
            })
            .map(|t| t.name)
            .collect();
        names.sort();
        names.dedup();
        names.iter().filter_map(|n| self.find(n, kind)).collect()
    }

    /// Adds or replaces a user template (same name and kind).
    pub fn upsert(&mut self, t: SqlTemplate) {
        match self
            .user
            .iter_mut()
            .find(|u| u.name == t.name && u.kind == t.kind)
        {
            Some(slot) => *slot = t,
            None => self.user.push(t),
        }
    }

    /// Removes a user template; returns whether it existed.
    pub fn remove(&mut self, name: &str, kind: Option<DbKind>) -> bool {
        let before = self.user.len();
        self.user.retain(|u| !(u.name == name && u.kind == kind));
        before != self.user.len()
    }
}

enum Segment {
    Text(String),
    Var {
        name: String,
        default: Option<String>,
    },
}

fn parse(body: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut rest = body;
    while let Some(i) = rest.find('$') {
        text.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(r) = after.strip_prefix('$') {
            text.push('$');
            rest = r;
        } else if let Some(inner) = after.strip_prefix('{') {
            match inner.find('}') {
                Some(end) => {
                    let spec = &inner[..end];
                    let (name, default) = match spec.split_once(':') {
                        Some((n, d)) => (n, Some(d.to_string())),
                        None => (spec, None),
                    };
                    if name.is_empty()
                        || !name
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
                    {
                        text.push_str("${");
                        rest = inner;
                        continue;
                    }
                    if !text.is_empty() {
                        out.push(Segment::Text(std::mem::take(&mut text)));
                    }
                    out.push(Segment::Var {
                        name: name.to_string(),
                        default,
                    });
                    rest = &inner[end + 1..];
                }
                None => {
                    text.push_str("${");
                    rest = inner;
                }
            }
        } else {
            text.push('$');
            rest = after;
        }
    }
    text.push_str(rest);
    if !text.is_empty() {
        out.push(Segment::Text(text));
    }
    out
}

/// Detailed result of an expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    /// Expanded text.
    pub text: String,
    /// Caret position in characters (from `${cursor}`), if present.
    pub cursor: Option<usize>,
    /// Caret position in bytes.
    pub cursor_byte: Option<usize>,
    /// Variables that had no value: (name, byte range of the inserted placeholder text).
    pub placeholders: Vec<(String, Range<usize>)>,
}

fn builtin_value(name: &str) -> Option<String> {
    let now = chrono::Local::now();
    Some(match name {
        "date" => now.format("%Y-%m-%d").to_string(),
        "time" => now.format("%H:%M:%S").to_string(),
        "datetime" => now.format("%Y-%m-%d %H:%M:%S").to_string(),
        "year" => now.format("%Y").to_string(),
        _ => return None,
    })
}

/// Expands a template body with full placeholder information.
pub fn expand_detailed(body: &str, vars: &BTreeMap<String, String>) -> Expansion {
    let mut text = String::with_capacity(body.len() + 16);
    let mut cursor_byte = None;
    let mut placeholders = Vec::new();
    for seg in parse(body) {
        match seg {
            Segment::Text(t) => text.push_str(&t),
            Segment::Var { name, default } => {
                if name == "cursor" {
                    if cursor_byte.is_none() {
                        cursor_byte = Some(text.len());
                    }
                    continue;
                }
                if let Some(v) = vars.get(&name) {
                    text.push_str(v);
                } else if let Some(v) = builtin_value(&name) {
                    text.push_str(&v);
                } else {
                    let start = text.len();
                    text.push_str(default.as_deref().unwrap_or(&name));
                    placeholders.push((name, start..text.len()));
                }
            }
        }
    }
    let cursor = cursor_byte.map(|b| text[..b].chars().count());
    Expansion {
        text,
        cursor,
        cursor_byte,
        placeholders,
    }
}

/// Expands `template` with `vars`; returns the text and the caret offset (in
/// characters) marked by `${cursor}`.
pub fn expand(template: &SqlTemplate, vars: &BTreeMap<String, String>) -> (String, Option<usize>) {
    let e = expand_detailed(&template.body, vars);
    (e.text, e.cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn expansion_and_cursor() {
        let lib = TemplateLibrary::default();
        let sel = lib.find("sel", None).unwrap();
        let (text, cursor) = expand(&sel, &vars(&[("table", "public.users")]));
        assert_eq!(text, "SELECT * FROM public.users;");
        assert_eq!(cursor, Some(text.len() - 1));

        let cte = lib.find("cte", None).unwrap();
        let e = expand_detailed(&cte.body, &BTreeMap::new());
        assert_eq!(e.text, "WITH cte AS (\n    \n)\nSELECT *\nFROM cte;");
        assert_eq!(e.placeholders.len(), 2);
        assert_eq!(&e.text[e.placeholders[0].1.clone()], "cte");
        assert_eq!(e.cursor, Some("WITH cte AS (\n    ".chars().count()));

        let e = expand_detailed(
            "SELECT '${greeting}' -- é ${cursor}",
            &vars(&[("greeting", "héllo")]),
        );
        assert_eq!(e.text, "SELECT 'héllo' -- é ");
        assert_eq!(e.cursor, Some(e.text.chars().count()));
        assert_eq!(e.cursor_byte, Some(e.text.len()));
        assert!(e.cursor.unwrap() < e.cursor_byte.unwrap());

        let e = expand_detailed(
            "cost $$5 ${unclosed and ${ bad } ${x:default value}",
            &BTreeMap::new(),
        );
        assert_eq!(e.text, "cost $5 ${unclosed and ${ bad } default value");
        assert_eq!(e.cursor, None);

        let today = expand_detailed("${date}", &BTreeMap::new()).text;
        assert_eq!(today.len(), 10);
        assert_eq!(
            expand_detailed("${date}", &vars(&[("date", "x")])).text,
            "x",
            "caller values win"
        );
    }

    #[test]
    fn dialect_specific_lookup() {
        let lib = TemplateLibrary::default();
        assert!(
            lib.find("crt", Some(DbKind::Postgres))
                .unwrap()
                .body
                .contains("IDENTITY PRIMARY KEY")
        );
        assert!(
            lib.find("crt", Some(DbKind::MySql))
                .unwrap()
                .body
                .contains("AUTO_INCREMENT")
        );
        assert!(
            lib.find("crt", Some(DbKind::Sqlite))
                .unwrap()
                .body
                .contains("INTEGER PRIMARY KEY")
        );
        assert!(lib.find("crt", None).unwrap().kind.is_none());
        assert!(
            lib.find("lim", Some(DbKind::SqlServer))
                .unwrap()
                .body
                .contains("TOP")
        );
        assert!(lib.find("expa", Some(DbKind::MySql)).is_none());
        assert!(lib.find("nope", None).is_none());
        let names: Vec<String> = lib
            .matching("s", Some(DbKind::Postgres))
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["scount", "sdist", "sel", "sf", "sgb", "sw"]);
        for t in builtin_templates() {
            assert!(!t.name.is_empty() && !t.body.is_empty());
        }
        let ins = lib.find("ins", None).unwrap();
        assert_eq!(
            ins.variables(),
            vec!["table".to_string(), "columns".to_string()]
        );
    }

    #[test]
    fn user_templates_override_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::with_root(dir.path());
        let (mut lib, w) = TemplateLibrary::load(&paths);
        assert!(w.is_empty());
        lib.upsert(SqlTemplate::new(
            "sel",
            "Mine",
            "SELECT TOP 10 * FROM ${table}",
        ));
        lib.upsert(SqlTemplate::new(
            "pk",
            "Primary key lookup",
            "SELECT * FROM ${table} WHERE id = ${cursor}",
        ));
        lib.disabled_builtins.push("del".into());
        lib.save(&paths).unwrap();
        let (back, w) = TemplateLibrary::load(&paths);
        assert!(w.is_empty());
        assert_eq!(back, lib);
        assert_eq!(back.find("sel", None).unwrap().description, "Mine");
        assert!(back.find("pk", Some(DbKind::Postgres)).is_some());
        assert!(back.find("del", None).is_none(), "disabled built-in");
        assert!(back.all().iter().any(|t| t.name == "del" && !t.enabled));
        let mut back = back;
        assert!(back.remove("sel", None));
        assert!(!back.remove("sel", None));
        assert_eq!(
            back.find("sel", None).unwrap().description,
            "Select all rows"
        );

        std::fs::write(paths.templates_file(), "[oops").unwrap();
        let (fallback, w) = TemplateLibrary::load(&paths);
        assert!(fallback.user.is_empty());
        assert_eq!(w.len(), 1);
    }
}
