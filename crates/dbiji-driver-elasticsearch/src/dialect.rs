//! Elasticsearch / OpenSearch SQL: indices are tables (quoted when they
//! hold `-` or `.`), no subqueries (limits and counts are rewritten), and
//! REST requests (`GET /index/_search` + a JSON body) beside SQL.

use dbiji_core::DbKind;
use dbiji_core::ddl::*;
use dbiji_core::dialect::{Dialect, IdentCase, SqlSyntax, ansi};
use dbiji_core::keywords;
use dbiji_core::meta::ObjectRef;
use dbiji_core::value::DataKind;

pub const SYNTAX: SqlSyntax = SqlSyntax {
    keywords: keywords::ANSI_KEYWORDS,
    functions: keywords::ANSI_FUNCTIONS,
    data_types: &[
        "keyword",
        "text",
        "long",
        "integer",
        "short",
        "byte",
        "double",
        "float",
        "boolean",
        "date",
        "datetime",
        "ip",
        "object",
        "nested",
        "geo_point",
    ],
    identifier_quotes: &[('"', '"')],
    double_quoted_strings: false,
    backslash_escapes: false,
    e_strings: false,
    n_strings: false,
    x_strings: false,
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
    plsql_blocks: false,
    newline_ends_statement: false,
    extra_ident_chars: "",
    ident_case: IdentCase::Preserve,
    case_sensitive_names: true,
};

pub(crate) fn quote(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !keywords::is_reserved(name);
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// A REST request: `METHOD /path` (SQL starts with SELECT, SHOW, DESCRIBE).
pub(crate) fn is_rest(text: &str) -> bool {
    let mut words = text.split_whitespace();
    let method = words.next().unwrap_or("").to_ascii_uppercase();
    let next = words.next().unwrap_or("");
    matches!(method.as_str(), "GET" | "POST" | "PUT" | "DELETE" | "HEAD")
        && !next.is_empty()
        && !next.eq_ignore_ascii_case("FROM")
}

/// Byte offset of the top-level `FROM` (outside quotes and parentheses).
fn top_level_from(sql: &str) -> Option<usize> {
    let b = sql.as_bytes();
    let (mut i, mut depth) = (0usize, 0i32);
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0 && sql.len() >= i + 4 && sql[i..i + 4].eq_ignore_ascii_case("from") => {
                let before = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
                let after = b
                    .get(i + 4)
                    .is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_'));
                if before && after {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn strip_limit(sql: &str) -> &str {
    let upper = sql.to_ascii_uppercase();
    match upper.rfind(" LIMIT ") {
        Some(i) if upper[i + 7..].trim().chars().all(|c| c.is_ascii_digit()) => sql[..i].trim_end(),
        _ => sql,
    }
}

pub struct SearchDialect {
    ddl: SearchDdl,
}

impl SearchDialect {
    pub fn new() -> Self {
        SearchDialect { ddl: SearchDdl }
    }
}

impl Default for SearchDialect {
    fn default() -> Self {
        Self::new()
    }
}

impl Dialect for SearchDialect {
    fn kind(&self) -> DbKind {
        DbKind::Elasticsearch
    }
    fn syntax(&self) -> &SqlSyntax {
        &SYNTAX
    }
    fn ddl(&self) -> &dyn DdlGenerator {
        &self.ddl
    }
    fn needs_quoting(&self, ident: &str) -> bool {
        quote(ident) != ident
    }
    /// Indices have no schema (the navigator's schema is the cluster).
    fn qualified(&self, _schema: Option<&str>, name: &str) -> String {
        quote(name)
    }
    fn qualified_ref(&self, obj: &ObjectRef) -> String {
        quote(&obj.name)
    }
    fn limit_query(&self, sql: &str, limit: u64, offset: u64) -> String {
        if is_rest(sql) {
            return sql.to_string();
        }
        format!(
            "{} LIMIT {}",
            strip_limit(ansi::strip_trailing_semicolon(sql).trim()),
            limit.saturating_add(offset)
        )
    }
    fn count_query(&self, sql: &str) -> String {
        self.aggregate_query(sql, "COUNT(*)")
    }
    fn aggregate_query(&self, sql: &str, select_list: &str) -> String {
        let s = strip_limit(ansi::strip_trailing_semicolon(sql).trim());
        match top_level_from(s) {
            Some(i) => format!("SELECT {select_list} {}", &s[i..]),
            None => format!("SELECT {select_list} FROM ({s})"),
        }
    }
    fn truncate_table(&self, table: &ObjectRef) -> String {
        format!(
            "POST /{}/_delete_by_query\n{{\"query\": {{\"match_all\": {{}}}}}}",
            table.name
        )
    }
}

pub struct SearchDdl;

const NOT_SEARCH: &str = "-- Not available for Elasticsearch indices";

impl DdlGenerator for SearchDdl {
    fn create_table(&self, t: &TableDef, _include_foreign_keys: bool) -> Vec<String> {
        let props: serde_json::Map<String, serde_json::Value> = t
            .columns
            .iter()
            .map(|c| (c.name.clone(), serde_json::json!({"type": c.data_type})))
            .collect();
        vec![format!(
            "PUT /{}\n{}",
            t.name,
            serde_json::to_string_pretty(&serde_json::json!({"mappings": {"properties": props}}))
                .unwrap_or_default()
        )]
    }
    fn drop_table(&self, _schema: Option<&str>, name: &str, _cascade: bool) -> String {
        format!("DELETE /{name}")
    }
    fn rename_table(&self, _schema: Option<&str>, old_name: &str, new_name: &str) -> String {
        format!(
            "POST /_reindex\n{{\"source\": {{\"index\": \"{old_name}\"}}, \"dest\": {{\"index\": \"{new_name}\"}}}}"
        )
    }
    fn alter_table(
        &self,
        _from: &TableDef,
        to: &TableDef,
        _renamed: &[(String, String)],
    ) -> Vec<String> {
        let _ = to;
        Vec::new()
    }
    fn add_column(&self, table: &TableDef, column: &ColumnDef) -> Vec<String> {
        vec![format!(
            "PUT /{}/_mapping\n{{\"properties\": {{\"{}\": {{\"type\": \"{}\"}}}}}}",
            table.name, column.name, column.data_type
        )]
    }
    fn drop_column(&self, _t: &TableDef, _c: &str) -> String {
        NOT_SEARCH.into()
    }
    fn create_index(&self, _t: &TableDef, _i: &IndexDef) -> String {
        NOT_SEARCH.into()
    }
    fn drop_index(&self, _t: &TableDef, _i: &IndexDef) -> String {
        NOT_SEARCH.into()
    }
    fn add_primary_key(&self, _t: &TableDef, _k: &KeyDef) -> String {
        NOT_SEARCH.into()
    }
    fn drop_primary_key(&self, _t: &TableDef, _k: &KeyDef) -> String {
        NOT_SEARCH.into()
    }
    fn add_unique_key(&self, _t: &TableDef, _k: &KeyDef) -> String {
        NOT_SEARCH.into()
    }
    fn drop_unique_key(&self, _t: &TableDef, _k: &KeyDef) -> String {
        NOT_SEARCH.into()
    }
    fn add_check(&self, _t: &TableDef, _c: &CheckDef) -> String {
        NOT_SEARCH.into()
    }
    fn drop_check(&self, _t: &TableDef, _c: &CheckDef) -> String {
        NOT_SEARCH.into()
    }
    fn add_foreign_key(&self, _t: &TableDef, _f: &ForeignKeyDef) -> String {
        NOT_SEARCH.into()
    }
    fn drop_foreign_key(&self, _t: &TableDef, _f: &ForeignKeyDef) -> String {
        NOT_SEARCH.into()
    }
    fn create_trigger(&self, _t: &TableDef, _tr: &TriggerDef) -> Vec<String> {
        Vec::new()
    }
    fn drop_trigger(&self, _t: &TableDef, _tr: &TriggerDef) -> String {
        NOT_SEARCH.into()
    }
    fn create_view(&self, view: &ViewDef, _or_replace: bool) -> Vec<String> {
        vec![view.definition.clone()]
    }
    fn drop_view(&self, view: &ViewDef) -> String {
        format!("DELETE /_all/_alias/{}", view.name)
    }
    fn create_sequence(&self, _s: &SequenceDef) -> String {
        NOT_SEARCH.into()
    }
    fn alter_sequence(&self, _f: &SequenceDef, _t: &SequenceDef) -> Vec<String> {
        Vec::new()
    }
    fn drop_sequence(&self, _s: &SequenceDef) -> String {
        NOT_SEARCH.into()
    }
    fn create_routine(&self, _r: &RoutineDef) -> Vec<String> {
        Vec::new()
    }
    fn drop_routine(&self, _r: &RoutineDef) -> String {
        NOT_SEARCH.into()
    }
    fn create_schema(&self, _name: &str) -> String {
        NOT_SEARCH.into()
    }
    fn drop_schema(&self, _name: &str, _cascade: bool) -> String {
        NOT_SEARCH.into()
    }
    fn map_type(&self, _source: DbKind, _data_type: &str, kind: DataKind) -> String {
        self.default_type_for(kind)
    }
    fn default_type_for(&self, kind: DataKind) -> String {
        match kind {
            DataKind::Bool => "boolean",
            DataKind::Int => "long",
            DataKind::Float | DataKind::Decimal => "double",
            DataKind::Date | DataKind::DateTime | DataKind::DateTimeTz => "date",
            DataKind::Json | DataKind::Array => "object",
            DataKind::Network => "ip",
            DataKind::Binary => "binary",
            _ => "keyword",
        }
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::meta::ObjectKind;

    #[test]
    fn sql_without_subqueries_and_rest() {
        let d = SearchDialect::new();
        let t = ObjectRef::new(
            ObjectKind::Table,
            Some("docker-cluster".into()),
            "logs-2024.01",
        );
        assert_eq!(
            d.select_table(&t, None, None, None, Some(200)),
            "SELECT * FROM \"logs-2024.01\" LIMIT 200"
        );
        assert_eq!(
            d.count_query("SELECT * FROM \"logs-2024.01\" WHERE a = 1 LIMIT 200"),
            "SELECT COUNT(*) FROM \"logs-2024.01\" WHERE a = 1"
        );
        assert!(is_rest("GET /logs/_search\n{\"query\": {}}"));
        assert!(is_rest("post _bulk"));
        assert!(!is_rest("SELECT 1"));
        assert_eq!(d.ddl().drop_table(None, "logs", false), "DELETE /logs");
    }
}
