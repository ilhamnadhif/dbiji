//! Catalog folders: server-level and auxiliary objects (roles, tablespaces,
//! settings, foreign data wrappers, users, status variables, policies …)
//! described declaratively by each dialect as catalog queries, so the
//! navigator and the object editor can show them without a dedicated object
//! kind and editor for every one of them.
//!
//! A folder's `list_sql` returns one row per object: the first column is the
//! object name, an optional column named `detail` is shown next to it, all
//! columns are its properties. `ddl_sql` statements return the object's DDL
//! (the first column of every row, joined); `children` are sub-folders of an
//! object (e.g. the members of a role).
//!
//! Placeholders, substituted before execution (never with user input other
//! than names read from the catalog itself):
//! - `{name}` the object name as a string literal, `{name_ident}` as a quoted
//!   identifier, `{name_raw}` verbatim (for names the catalog query already
//!   quoted, e.g. MySQL `'user'@'host'`);
//! - `{parent}` / `{parent_ident}` the parent object of a sub-folder;
//! - `{schema}` / `{table}` (string literals) and `{schema_ident}` /
//!   `{table_ident}` for table scoped folders;
//! - `{database}` / `{database_ident}` the database of database scoped folders.

use crate::dialect::Dialect;
use crate::driver::{Session, query_all};
use crate::error::DbResult;
use crate::value::Value;

/// Where a folder appears in the navigator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CatalogScope {
    /// Under the connection (one per server).
    Server,
    /// Under each database (or the connection when it has one database).
    Database,
    /// Under each table.
    Table,
    /// Under an object of another folder (`CatalogFolder::children`).
    Child,
}

/// Navigator group of server scoped folders, like DBeaver's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CatalogGroup {
    Security,
    Storage,
    Administer,
    SystemInfo,
    /// Shown directly (database, table and child scopes).
    None,
}

impl CatalogGroup {
    pub fn label(self) -> &'static str {
        match self {
            CatalogGroup::Security => "Security",
            CatalogGroup::Storage => "Storage",
            CatalogGroup::Administer => "Administer",
            CatalogGroup::SystemInfo => "System Info",
            CatalogGroup::None => "",
        }
    }
}

/// Icon hint (mapped to an icon by the UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CatalogIcon {
    User,
    Users,
    Storage,
    Settings,
    Plug,
    Server,
    Trigger,
    Language,
    Text,
    Shield,
    Rule,
    Gauge,
    Engine,
    Key,
    Puzzle,
}

/// A folder of catalog objects.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogFolder {
    /// Stable id, e.g. `pg.roles`.
    pub id: &'static str,
    /// Plural label, e.g. "Roles".
    pub label: &'static str,
    pub group: CatalogGroup,
    pub scope: CatalogScope,
    pub icon: CatalogIcon,
    pub list_sql: String,
    pub ddl_sql: Vec<String>,
    /// Ids of sub-folders shown under each object.
    pub children: Vec<&'static str>,
    /// The folder is a table of values (status, variables, settings): it
    /// opens as a table and its rows are not navigable objects.
    pub table_only: bool,
}

impl CatalogFolder {
    pub fn new(id: &'static str, label: &'static str, group: CatalogGroup, scope: CatalogScope, icon: CatalogIcon, list_sql: impl Into<String>) -> Self {
        CatalogFolder { id, label, group, scope, icon, list_sql: list_sql.into(), ddl_sql: Vec::new(), children: Vec::new(), table_only: false }
    }

    pub fn ddl(mut self, sql: impl Into<String>) -> Self {
        self.ddl_sql.push(sql.into());
        self
    }

    pub fn children(mut self, ids: &[&'static str]) -> Self {
        self.children.extend_from_slice(ids);
        self
    }

    pub fn table_only(mut self) -> Self {
        self.table_only = true;
        self
    }
}

/// Values for the placeholders of a catalog query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CatalogContext {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub parent: Option<String>,
    pub name: Option<String>,
}

/// Substitute the placeholders of `sql`.
pub fn fill(sql: &str, ctx: &CatalogContext, dialect: &dyn Dialect) -> String {
    let lit = |v: &Option<String>| v.as_deref().map(|s| dialect.string_literal(s)).unwrap_or_else(|| "NULL".into());
    let ident = |v: &Option<String>| v.as_deref().map(|s| dialect.quote_ident(s)).unwrap_or_default();
    // Longest placeholders first: `{name}` is a prefix of `{name_ident}`' text.
    let pairs: [(&str, String); 11] = [
        ("{name_ident}", ident(&ctx.name)),
        ("{name_raw}", ctx.name.clone().unwrap_or_default()),
        ("{name}", lit(&ctx.name)),
        ("{parent_ident}", ident(&ctx.parent)),
        ("{parent}", lit(&ctx.parent)),
        ("{schema_ident}", ident(&ctx.schema)),
        ("{schema}", lit(&ctx.schema)),
        ("{table_ident}", ident(&ctx.table)),
        ("{table}", lit(&ctx.table)),
        ("{database_ident}", ident(&ctx.database)),
        ("{database}", lit(&ctx.database)),
    ];
    let mut out = String::with_capacity(sql.len() + 32);
    let mut rest = sql;
    'outer: while let Some(pos) = rest.find('{') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        for (ph, value) in &pairs {
            if let Some(after) = tail.strip_prefix(ph) {
                out.push_str(value);
                rest = after;
                continue 'outer;
            }
        }
        out.push('{');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

/// One object of a folder.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogRow {
    pub name: String,
    pub detail: Option<String>,
    /// All columns: (name, value).
    pub values: Vec<(String, Value)>,
}

/// Run a folder's list query.
pub async fn list(session: &mut dyn Session, folder: &CatalogFolder, ctx: &CatalogContext) -> DbResult<Vec<CatalogRow>> {
    let dialect = session.dialect();
    let sql = fill(&folder.list_sql, ctx, dialect.as_ref());
    let (cols, rows) = query_all(session, &sql, &[]).await?;
    // A `detail` column, or the value of a two-column name / value list
    // (`SHOW STATUS`, `SHOW VARIABLES`).
    let detail = cols.iter().position(|c| c.name.eq_ignore_ascii_case("detail")).or((cols.len() == 2).then_some(1));
    Ok(rows
        .into_iter()
        .map(|r| {
            let name = r.first().map(|v| v.to_text().into_owned()).unwrap_or_default();
            let detail = detail.and_then(|i| r.get(i)).filter(|v| !v.is_null()).map(|v| v.to_text().into_owned()).filter(|s| !s.is_empty());
            let values = cols.iter().zip(r).map(|(c, v)| (c.name.clone(), v)).collect();
            CatalogRow { name, detail, values }
        })
        .collect())
}

/// The DDL of one object (`None` when the folder has no DDL queries).
pub async fn ddl(session: &mut dyn Session, folder: &CatalogFolder, ctx: &CatalogContext) -> DbResult<Option<String>> {
    if folder.ddl_sql.is_empty() {
        return Ok(None);
    }
    let dialect = session.dialect();
    let mut parts = Vec::new();
    for sql in &folder.ddl_sql {
        let sql = fill(sql, ctx, dialect.as_ref());
        let (_, rows) = query_all(session, &sql, &[]).await?;
        for r in rows {
            // MySQL `SHOW CREATE …` puts the statement in the last column.
            let text = r.iter().rev().find(|v| v.to_text().trim_start().len() > 6 && !v.is_null()).or(r.first()).map(|v| v.to_text().trim().to_string()).unwrap_or_default();
            if !text.is_empty() {
                parts.push(if text.ends_with(';') { text } else { format!("{text};") });
            }
        }
    }
    Ok(Some(parts.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct D;
    impl Dialect for D {
        fn kind(&self) -> crate::DbKind {
            crate::DbKind::Postgres
        }
        fn syntax(&self) -> &crate::SqlSyntax {
            unimplemented!()
        }
        fn ddl(&self) -> &dyn crate::ddl::DdlGenerator {
            unimplemented!()
        }
        fn string_literal(&self, s: &str) -> String {
            format!("'{}'", s.replace('\'', "''"))
        }
        fn quote_ident(&self, s: &str) -> String {
            format!("\"{}\"", s.replace('"', "\"\""))
        }
    }

    #[test]
    fn placeholders() {
        let ctx = CatalogContext { name: Some("o'neil".into()), schema: Some("app".into()), table: Some("My T".into()), ..Default::default() };
        let d = D;
        assert_eq!(
            fill("SELECT * FROM t WHERE n = {name} AND id = {name_ident} AND s = {schema} AND x = {table_ident} AND r = {name_raw} AND y = {parent} {", &ctx, &d),
            "SELECT * FROM t WHERE n = 'o''neil' AND id = \"o'neil\" AND s = 'app' AND x = \"My T\" AND r = o'neil AND y = NULL {"
        );
        assert_eq!(fill("{unknown} {{name}}", &ctx, &d), "{unknown} {'o''neil'}");
    }
}
