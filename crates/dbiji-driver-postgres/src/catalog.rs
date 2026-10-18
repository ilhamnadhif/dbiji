//! Helpers for internal catalog queries: typed parameters, row accessors and
//! small SQL utilities shared by the metadata, admin and explain modules.
//!
//! Internal queries use the extended protocol with an unnamed statement and
//! explicit parameter types (`query_typed`), i.e. one round trip and no
//! server side prepared statements (PgBouncer friendly).

use crate::decode::binary_value;
use crate::error::pg;
use crate::types::Conv;
use dbiji_core::admin::TabularData;
use dbiji_core::{DbError, DbResult, Value};
use futures::TryStreamExt;
use postgres_types::{ToSql, Type};
use tokio_postgres::{Client, Row};

/// A typed query parameter.
#[derive(Debug, Clone, Copy)]
pub(crate) enum P<'a> {
    /// `name` (catalog identifiers; keeps catalog indexes usable).
    Name(&'a str),
    Text(&'a str),
    Oid(u32),
    Oids(&'a [u32]),
    Int2s(&'a [i16]),
    Int4(i32),
    Int8(i64),
    Names(&'a [String]),
}

impl<'a> P<'a> {
    fn typed(&self) -> (&(dyn ToSql + Sync), Type) {
        match self {
            P::Name(s) => (s, Type::NAME),
            P::Text(s) => (s, Type::TEXT),
            P::Oid(o) => (o, Type::OID),
            P::Oids(o) => (o, Type::OID_ARRAY),
            P::Int2s(v) => (v, Type::INT2_ARRAY),
            P::Int4(i) => (i, Type::INT4),
            P::Int8(i) => (i, Type::INT8),
            P::Names(n) => (n, Type::NAME_ARRAY),
        }
    }
}

/// Run a catalog query and collect all rows (catalog results are small).
pub(crate) async fn query(client: &Client, sql: &str, params: &[P<'_>]) -> DbResult<Vec<Row>> {
    let typed: Vec<(&(dyn ToSql + Sync), Type)> = params.iter().map(P::typed).collect();
    let stream = client.query_typed_raw(sql, typed).await.map_err(pg)?;
    stream.try_collect().await.map_err(pg)
}

/// Run a query and return the first row, if any.
pub(crate) async fn query_opt(client: &Client, sql: &str, params: &[P<'_>]) -> DbResult<Option<Row>> {
    Ok(query(client, sql, params).await?.into_iter().next())
}

/// Run a query and convert every column generically into [`Value`]s.
pub(crate) async fn query_tabular(client: &Client, sql: &str, params: &[P<'_>]) -> DbResult<TabularData> {
    let rows = query(client, sql, params).await?;
    Ok(rows_to_tabular(&rows))
}

/// Generic conversion of binary rows.
pub(crate) fn rows_to_tabular(rows: &[Row]) -> TabularData {
    let Some(first) = rows.first() else { return TabularData::default() };
    let columns: Vec<String> = first.columns().iter().map(|c| c.name().to_string()).collect();
    let convs: Vec<Conv> = first.columns().iter().map(|c| Conv::for_type(c.type_())).collect();
    let data = rows
        .iter()
        .map(|r| {
            convs
                .iter()
                .enumerate()
                .map(|(i, conv)| match r.try_get::<_, crate::cursor::Raw<'_>>(i) {
                    Ok(raw) => binary_value(conv, raw.0),
                    Err(_) => Value::Null,
                })
                .collect()
        })
        .collect();
    TabularData { columns, rows: data }
}

/// Typed accessors with proper error mapping.
pub(crate) trait RowExt {
    /// Text column; NULL becomes an empty string.
    fn s(&self, i: usize) -> DbResult<String>;
    fn os(&self, i: usize) -> DbResult<Option<String>>;
    /// int8 column; NULL becomes 0.
    fn i(&self, i: usize) -> DbResult<i64>;
    fn oi(&self, i: usize) -> DbResult<Option<i64>>;
    /// bool column; NULL becomes false.
    fn b(&self, i: usize) -> DbResult<bool>;
    /// text[] column; NULL becomes empty, NULL elements become empty strings.
    fn ss(&self, i: usize) -> DbResult<Vec<String>>;
    /// bool[] column.
    fn bs(&self, i: usize) -> DbResult<Vec<bool>>;
}

fn col_err(i: usize, e: tokio_postgres::Error) -> DbError {
    use std::error::Error as _;
    match e.source() {
        Some(src) => DbError::internal(format!("catalog query column {i}: {e}: {src}")),
        None => DbError::internal(format!("catalog query column {i}: {e}")),
    }
}

impl RowExt for Row {
    fn s(&self, i: usize) -> DbResult<String> {
        Ok(self.try_get::<_, Option<String>>(i).map_err(|e| col_err(i, e))?.unwrap_or_default())
    }
    fn os(&self, i: usize) -> DbResult<Option<String>> {
        self.try_get::<_, Option<String>>(i).map_err(|e| col_err(i, e))
    }
    fn i(&self, i: usize) -> DbResult<i64> {
        Ok(self.oi(i)?.unwrap_or(0))
    }
    fn oi(&self, i: usize) -> DbResult<Option<i64>> {
        self.try_get::<_, Option<i64>>(i).map_err(|e| col_err(i, e))
    }
    fn b(&self, i: usize) -> DbResult<bool> {
        Ok(self.try_get::<_, Option<bool>>(i).map_err(|e| col_err(i, e))?.unwrap_or(false))
    }
    fn ss(&self, i: usize) -> DbResult<Vec<String>> {
        Ok(self.try_get::<_, Option<Vec<Option<String>>>>(i).map_err(|e| col_err(i, e))?.unwrap_or_default().into_iter().map(Option::unwrap_or_default).collect())
    }
    fn bs(&self, i: usize) -> DbResult<Vec<bool>> {
        Ok(self.try_get::<_, Option<Vec<Option<bool>>>>(i).map_err(|e| col_err(i, e))?.unwrap_or_default().into_iter().map(Option::unwrap_or_default).collect())
    }
}

/// `'text'` literal safe regardless of `standard_conforming_strings`.
pub(crate) fn quote_literal(s: &str) -> String {
    if s.contains('\\') {
        let mut out = String::with_capacity(s.len() + 4);
        out.push_str("E'");
        for c in s.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\'' => out.push_str("''"),
                c => out.push(c),
            }
        }
        out.push('\'');
        out
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

/// Always double-quote an identifier.
pub(crate) fn quote_ident_always(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals() {
        assert_eq!(quote_literal("it's"), "'it''s'");
        assert_eq!(quote_literal("a\\b'"), "E'a\\\\b'''");
        assert_eq!(quote_ident_always("My\"T"), "\"My\"\"T\"");
    }
}
