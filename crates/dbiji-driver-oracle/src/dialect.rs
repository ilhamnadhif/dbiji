//! Oracle SQL dialect: upper-case identifiers, `FETCH FIRST n ROWS ONLY`,
//! `:1` parameters, PL/SQL blocks ended by a `/` line.

use crate::ddl::OracleDdl;
use dbiji_core::ddl::DdlGenerator;
use dbiji_core::dialect::{Dialect, IdentCase, SqlSyntax, ansi};
use dbiji_core::meta::ObjectRef;
use dbiji_core::{DbKind, Value};

/// Reserved words (`V$RESERVED_WORDS` with `RESERVED = 'Y'`): always quoted.
pub(crate) const RESERVED: &[&str] = &[
    "ACCESS",
    "ADD",
    "ALL",
    "ALTER",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "AUDIT",
    "BETWEEN",
    "BY",
    "CHAR",
    "CHECK",
    "CLUSTER",
    "COLUMN",
    "COMMENT",
    "COMPRESS",
    "CONNECT",
    "CREATE",
    "CURRENT",
    "DATE",
    "DECIMAL",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "EXCLUSIVE",
    "EXISTS",
    "FILE",
    "FLOAT",
    "FOR",
    "FROM",
    "GRANT",
    "GROUP",
    "HAVING",
    "IDENTIFIED",
    "IMMEDIATE",
    "IN",
    "INCREMENT",
    "INDEX",
    "INITIAL",
    "INSERT",
    "INTEGER",
    "INTERSECT",
    "INTO",
    "IS",
    "LEVEL",
    "LIKE",
    "LOCK",
    "LONG",
    "MAXEXTENTS",
    "MINUS",
    "MLSLABEL",
    "MODE",
    "MODIFY",
    "NOAUDIT",
    "NOCOMPRESS",
    "NOT",
    "NOWAIT",
    "NULL",
    "NUMBER",
    "OF",
    "OFFLINE",
    "ON",
    "ONLINE",
    "OPTION",
    "OR",
    "ORDER",
    "PCTFREE",
    "PRIOR",
    "PUBLIC",
    "RAW",
    "RENAME",
    "RESOURCE",
    "REVOKE",
    "ROW",
    "ROWID",
    "ROWNUM",
    "ROWS",
    "SELECT",
    "SESSION",
    "SET",
    "SHARE",
    "SIZE",
    "SMALLINT",
    "START",
    "SUCCESSFUL",
    "SYNONYM",
    "SYSDATE",
    "TABLE",
    "THEN",
    "TO",
    "TRIGGER",
    "UID",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USER",
    "VALIDATE",
    "VALUES",
    "VARCHAR",
    "VARCHAR2",
    "VIEW",
    "WHENEVER",
    "WHERE",
    "WITH",
];

const KEYWORDS: &[&str] = &[
    "ACCESS",
    "ADD",
    "ALL",
    "ALTER",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "AUDIT",
    "AUTHID",
    "BEGIN",
    "BETWEEN",
    "BODY",
    "BULK",
    "BY",
    "CASCADE",
    "CASE",
    "CHECK",
    "CLOSE",
    "CLUSTER",
    "COLLECT",
    "COLUMN",
    "COMMENT",
    "COMMIT",
    "COMPRESS",
    "CONNECT",
    "CONSTANT",
    "CONSTRAINT",
    "CONSTRAINTS",
    "CONTINUE",
    "CREATE",
    "CROSS",
    "CURRENT",
    "CURSOR",
    "DECLARE",
    "DEFAULT",
    "DEFINER",
    "DELETE",
    "DESC",
    "DETERMINISTIC",
    "DISABLE",
    "DISTINCT",
    "DROP",
    "EACH",
    "EDITIONABLE",
    "ELSE",
    "ELSIF",
    "ENABLE",
    "END",
    "EXCEPTION",
    "EXCEPTIONS",
    "EXCLUSIVE",
    "EXECUTE",
    "EXISTS",
    "EXIT",
    "EXPLAIN",
    "FETCH",
    "FIRST",
    "FOLLOWING",
    "FOR",
    "FORALL",
    "FOREIGN",
    "FROM",
    "FULL",
    "FUNCTION",
    "GENERATED",
    "GOTO",
    "GRANT",
    "GROUP",
    "HAVING",
    "IDENTIFIED",
    "IDENTITY",
    "IF",
    "IMMEDIATE",
    "IN",
    "INCREMENT",
    "INDEX",
    "INNER",
    "INSERT",
    "INSTEAD",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LAST",
    "LEFT",
    "LEVEL",
    "LIKE",
    "LIMIT",
    "LOCK",
    "LOOP",
    "MATCHED",
    "MATERIALIZED",
    "MERGE",
    "MINUS",
    "MODE",
    "MODIFY",
    "NATURAL",
    "NEXT",
    "NOCOPY",
    "NOCYCLE",
    "NOORDER",
    "NOT",
    "NOWAIT",
    "NULL",
    "NULLS",
    "OF",
    "OFFSET",
    "ON",
    "ONLY",
    "OPEN",
    "OPTION",
    "OR",
    "ORDER",
    "OTHERS",
    "OUT",
    "OUTER",
    "OVER",
    "PACKAGE",
    "PARTITION",
    "PIPELINED",
    "PIVOT",
    "PRAGMA",
    "PRECEDING",
    "PRIMARY",
    "PRIOR",
    "PROCEDURE",
    "PUBLIC",
    "PURGE",
    "RAISE",
    "RANGE",
    "RECORD",
    "REFERENCES",
    "REFRESH",
    "RENAME",
    "REPLACE",
    "RESULT_CACHE",
    "RETURN",
    "RETURNING",
    "REVERSE",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "ROW",
    "ROWS",
    "ROWTYPE",
    "SAVEPOINT",
    "SELECT",
    "SEQUENCE",
    "SESSION",
    "SET",
    "SHARE",
    "SIBLINGS",
    "START",
    "SUBTYPE",
    "SYNONYM",
    "SYSTEM",
    "TABLE",
    "TEMPORARY",
    "THEN",
    "TIES",
    "TO",
    "TRIGGER",
    "TRUNCATE",
    "TYPE",
    "UNBOUNDED",
    "UNION",
    "UNIQUE",
    "UNPIVOT",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WHILE",
    "WITH",
    "WITHIN",
];

const FUNCTIONS: &[&str] = &[
    "ABS",
    "ADD_MONTHS",
    "ASCII",
    "AVG",
    "CAST",
    "CEIL",
    "CHR",
    "COALESCE",
    "CONCAT",
    "COUNT",
    "CUME_DIST",
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "DECODE",
    "DENSE_RANK",
    "DUMP",
    "EMPTY_BLOB",
    "EMPTY_CLOB",
    "EXTRACT",
    "FIRST_VALUE",
    "FLOOR",
    "GREATEST",
    "HEXTORAW",
    "INITCAP",
    "INSTR",
    "JSON_ARRAY",
    "JSON_ARRAYAGG",
    "JSON_OBJECT",
    "JSON_QUERY",
    "JSON_TABLE",
    "JSON_VALUE",
    "LAG",
    "LAST_DAY",
    "LAST_VALUE",
    "LEAD",
    "LEAST",
    "LENGTH",
    "LISTAGG",
    "LNNVL",
    "LOWER",
    "LPAD",
    "LTRIM",
    "MAX",
    "MEDIAN",
    "MIN",
    "MOD",
    "MONTHS_BETWEEN",
    "NEXT_DAY",
    "NTILE",
    "NULLIF",
    "NUMTODSINTERVAL",
    "NUMTOYMINTERVAL",
    "NVL",
    "NVL2",
    "ORA_HASH",
    "POWER",
    "RANK",
    "RATIO_TO_REPORT",
    "RAWTOHEX",
    "REGEXP_COUNT",
    "REGEXP_INSTR",
    "REGEXP_LIKE",
    "REGEXP_REPLACE",
    "REGEXP_SUBSTR",
    "REPLACE",
    "ROUND",
    "ROW_NUMBER",
    "RPAD",
    "RTRIM",
    "SIGN",
    "SQRT",
    "STANDARD_HASH",
    "STDDEV",
    "SUBSTR",
    "SUM",
    "SYS_CONTEXT",
    "SYS_GUID",
    "SYSDATE",
    "SYSTIMESTAMP",
    "TO_CHAR",
    "TO_CLOB",
    "TO_DATE",
    "TO_DSINTERVAL",
    "TO_NUMBER",
    "TO_TIMESTAMP",
    "TO_TIMESTAMP_TZ",
    "TO_YMINTERVAL",
    "TRANSLATE",
    "TRIM",
    "TRUNC",
    "UPPER",
    "USERENV",
    "VARIANCE",
    "XMLAGG",
    "XMLELEMENT",
];

const TYPES: &[&str] = &[
    "BFILE",
    "BINARY_DOUBLE",
    "BINARY_FLOAT",
    "BINARY_INTEGER",
    "BLOB",
    "BOOLEAN",
    "CHAR",
    "CLOB",
    "DATE",
    "DECIMAL",
    "FLOAT",
    "INT",
    "INTEGER",
    "INTERVAL",
    "JSON",
    "LONG",
    "NCHAR",
    "NCLOB",
    "NUMBER",
    "NVARCHAR2",
    "PLS_INTEGER",
    "RAW",
    "REAL",
    "ROWID",
    "SMALLINT",
    "TIMESTAMP",
    "UROWID",
    "VARCHAR",
    "VARCHAR2",
    "VECTOR",
    "XMLTYPE",
];

pub const SYNTAX: SqlSyntax = SqlSyntax {
    keywords: KEYWORDS,
    functions: FUNCTIONS,
    data_types: TYPES,
    identifier_quotes: &[('"', '"')],
    double_quoted_strings: false,
    backslash_escapes: false,
    e_strings: false,
    n_strings: true,
    x_strings: false,
    dollar_quoting: false,
    hash_comments: false,
    nested_block_comments: false,
    dash_comment_needs_space: false,
    question_params: false,
    dollar_params: false,
    at_variables: false,
    batch_separator: Some("/"),
    delimiter_command: false,
    compound_blocks: true,
    plsql_blocks: true,
    extra_ident_chars: "$#",
    ident_case: IdentCase::Upper,
    case_sensitive_names: false,
};

/// An unquoted name is folded to upper case: anything else must be quoted.
pub(crate) fn needs_quoting(ident: &str) -> bool {
    let mut chars = ident.chars();
    let Some(first) = chars.next() else {
        return true;
    };
    if !first.is_ascii_uppercase() {
        return true;
    }
    if !chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '_' | '$' | '#'))
    {
        return true;
    }
    RESERVED.contains(&ident)
}

pub(crate) fn quote(ident: &str) -> String {
    if needs_quoting(ident) {
        format!("\"{}\"", ident.replace('"', "\"\""))
    } else {
        ident.to_string()
    }
}

pub(crate) fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) if !s.is_empty() => format!("{}.{}", quote(s), quote(name)),
        _ => quote(name),
    }
}

pub struct OracleDialect {
    ddl: OracleDdl,
}

impl OracleDialect {
    pub fn new() -> Self {
        OracleDialect { ddl: OracleDdl }
    }
}

impl Default for OracleDialect {
    fn default() -> Self {
        Self::new()
    }
}

impl Dialect for OracleDialect {
    fn kind(&self) -> DbKind {
        DbKind::Oracle
    }

    fn syntax(&self) -> &SqlSyntax {
        &SYNTAX
    }

    fn ddl(&self) -> &dyn DdlGenerator {
        &self.ddl
    }

    fn needs_quoting(&self, ident: &str) -> bool {
        needs_quoting(ident)
    }

    fn bytes_literal(&self, b: &[u8]) -> String {
        format!("HEXTORAW('{}')", hex::encode_upper(b))
    }

    /// `1` / `0`: there is no BOOLEAN before 23ai, and 23ai converts them.
    fn bool_literal(&self, b: bool) -> String {
        if b { "1".into() } else { "0".into() }
    }

    fn literal(&self, value: &Value, type_name: Option<&str>) -> String {
        let _ = type_name;
        match value {
            Value::Date(d) => format!("DATE '{}'", d.format("%Y-%m-%d")),
            Value::DateTime(t) => format!("TIMESTAMP '{}'", t.format("%Y-%m-%d %H:%M:%S%.f")),
            Value::DateTimeTz(t) => format!("TIMESTAMP '{}'", t.format("%Y-%m-%d %H:%M:%S%.f %:z")),
            _ => ansi::literal(self, value),
        }
    }

    fn placeholder(&self, index: usize) -> String {
        format!(":{index}")
    }

    fn limit_query(&self, sql: &str, limit: u64, offset: u64) -> String {
        let inner = ansi::strip_trailing_semicolon(sql);
        if offset > 0 {
            format!(
                "SELECT * FROM ({inner}) dbiji_q OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"
            )
        } else {
            format!("SELECT * FROM ({inner}) dbiji_q FETCH FIRST {limit} ROWS ONLY")
        }
    }

    fn select_table(
        &self,
        table: &ObjectRef,
        columns: Option<&[String]>,
        where_clause: Option<&str>,
        order_by: Option<&str>,
        limit: Option<u64>,
    ) -> String {
        let cols = match columns {
            Some(c) if !c.is_empty() => c.iter().map(|c| quote(c)).collect::<Vec<_>>().join(", "),
            _ => "*".to_string(),
        };
        let mut sql = format!(
            "SELECT {cols} FROM {}",
            qualified(table.schema.as_deref(), &table.name)
        );
        if let Some(w) = where_clause.filter(|w| !w.trim().is_empty()) {
            sql.push_str(" WHERE ");
            sql.push_str(w);
        }
        if let Some(o) = order_by.filter(|o| !o.trim().is_empty()) {
            sql.push_str(" ORDER BY ");
            sql.push_str(o);
        }
        if let Some(l) = limit {
            sql.push_str(&format!(" FETCH FIRST {l} ROWS ONLY"));
        }
        sql
    }

    /// `MERGE INTO t USING (SELECT v AS c ... FROM dual) s ON (...)`.
    fn upsert(
        &self,
        table: &str,
        columns: &[String],
        key_columns: &[String],
        values: &[String],
    ) -> Option<String> {
        if key_columns.is_empty() || columns.len() != values.len() {
            return None;
        }
        let source = columns
            .iter()
            .zip(values)
            .map(|(c, v)| format!("{v} {c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let on = key_columns
            .iter()
            .map(|k| format!("t.{k} = s.{k}"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let updates: Vec<String> = columns
            .iter()
            .filter(|c| !key_columns.contains(c))
            .map(|c| format!("t.{c} = s.{c}"))
            .collect();
        let mut sql = format!("MERGE INTO {table} t USING (SELECT {source} FROM dual) s ON ({on})");
        if !updates.is_empty() {
            sql.push_str(&format!(
                " WHEN MATCHED THEN UPDATE SET {}",
                updates.join(", ")
            ));
        }
        sql.push_str(&format!(
            " WHEN NOT MATCHED THEN INSERT ({}) VALUES ({})",
            columns.join(", "),
            columns
                .iter()
                .map(|c| format!("s.{c}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        Some(sql)
    }

    fn change_password_sql(&self, user: Option<&str>, new_password: &str) -> Option<String> {
        let user = user?;
        Some(format!(
            "ALTER USER {} IDENTIFIED BY \"{}\"",
            quote(user),
            new_password.replace('"', "")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::meta::ObjectKind;

    #[test]
    fn identifiers_are_quoted_unless_upper_case() {
        let d = OracleDialect::new();
        assert_eq!(d.quote_ident("EMPLOYEES"), "EMPLOYEES");
        assert_eq!(d.quote_ident("employees"), "\"employees\"");
        assert_eq!(d.quote_ident("Order Items"), "\"Order Items\"");
        assert_eq!(d.quote_ident("LEVEL"), "\"LEVEL\"");
        assert_eq!(d.quote_ident("SYS$X#1"), "SYS$X#1");
        assert_eq!(d.qualified(Some("HR"), "emp"), "HR.\"emp\"");
    }

    #[test]
    fn paging_and_literals() {
        let d = OracleDialect::new();
        assert_eq!(
            d.limit_query("SELECT * FROM t;", 10, 0),
            "SELECT * FROM (SELECT * FROM t) dbiji_q FETCH FIRST 10 ROWS ONLY"
        );
        assert_eq!(
            d.limit_query("SELECT * FROM t", 10, 20),
            "SELECT * FROM (SELECT * FROM t) dbiji_q OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY"
        );
        let t = ObjectRef::new(ObjectKind::Table, Some("HR".into()), "EMP");
        assert_eq!(
            d.select_table(&t, None, Some("ID > 1"), Some("ID"), Some(200)),
            "SELECT * FROM HR.EMP WHERE ID > 1 ORDER BY ID FETCH FIRST 200 ROWS ONLY"
        );
        assert_eq!(d.placeholder(2), ":2");
        let day = chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
        assert_eq!(d.literal(&Value::Date(day), None), "DATE '2024-01-02'");
        assert_eq!(
            d.literal(&Value::DateTime(day.and_hms_opt(3, 4, 5).unwrap()), None),
            "TIMESTAMP '2024-01-02 03:04:05'"
        );
        assert_eq!(
            d.literal(&Value::Bytes(vec![0xab, 0xcd].into()), None),
            "HEXTORAW('ABCD')"
        );
        assert_eq!(d.literal(&Value::Bool(true), None), "1");
        assert_eq!(d.literal(&Value::Text("it's".into()), None), "'it''s'");
    }

    #[test]
    fn upsert_is_a_merge() {
        let d = OracleDialect::new();
        let sql = d
            .upsert(
                "HR.EMP",
                &["ID".into(), "NAME".into()],
                &["ID".into()],
                &[":1".into(), ":2".into()],
            )
            .unwrap();
        assert_eq!(
            sql,
            "MERGE INTO HR.EMP t USING (SELECT :1 ID, :2 NAME FROM dual) s ON (t.ID = s.ID) WHEN MATCHED THEN UPDATE SET t.NAME = s.NAME WHEN NOT MATCHED THEN INSERT (ID, NAME) VALUES (s.ID, s.NAME)"
        );
    }
}
