//! Shaping of Redis replies into result tables.
//!
//! Redis has no result sets, so the driver invents a table per reply:
//! * a scalar reply (`GET`, `INCR`, `PING`) is one row, one column `value`;
//! * a list of scalars (`KEYS`, `LRANGE`, `SMEMBERS`) is one column `value`;
//! * a map (`HGETALL` on RESP3, `CONFIG GET`) is `field` / `value` rows;
//! * a scan reply (`SCAN`, `HSCAN` …) is `cursor` plus the element column;
//! * a list of lists (`XRANGE`, `ZRANGE … WITHSCORES`) has one column per
//!   position (`1`, `2`, …), named after the command where it is known.

use dbiji_core::value::DataKind;
use dbiji_core::{ColumnMeta, Value};
use redis::Value as Rv;

/// One shaped reply.
pub struct Table {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Value>>,
}

impl Table {
    fn one(name: &str, kind: DataKind, type_name: &str, value: Value) -> Table {
        Table { columns: vec![ColumnMeta::new(name, type_name, kind)], rows: vec![vec![value]] }
    }
}

/// Shapes `reply` of the command `args`.
pub fn shape(args: &[String], reply: Rv) -> Table {
    let name = args.first().map(|a| a.to_ascii_uppercase()).unwrap_or_default();
    // SCAN family: [cursor, [elements…]].
    if is_scan(&name)
        && let Rv::Array(items) = &reply
        && items.len() == 2
        && let Rv::Array(elements) = &items[1]
    {
        let cursor = scalar_text(&items[0]);
        let (element_name, element_kind) = element_column(&name);
        let columns = vec![ColumnMeta::new("cursor", "integer", DataKind::Int), ColumnMeta::new(element_name, "string", element_kind)];
        let rows = elements.iter().map(|e| vec![Value::Text(cursor.clone().into()), to_value(e)]).collect();
        return Table { columns, rows };
    }
    // HGETALL: flat pairs on RESP2, a map on RESP3.
    if name == "HGETALL" {
        return pairs(reply, "field", "value");
    }
    // ZRANGE … WITHSCORES, ZPOPMIN …: member / score pairs.
    let with_scores = name.starts_with('Z') && args.iter().any(|a| a.eq_ignore_ascii_case("WITHSCORES"));
    if with_scores {
        return pairs(reply, "member", "score");
    }
    match reply {
        Rv::Nil => Table::one("value", DataKind::Other, "nil", Value::Null),
        Rv::Array(items) | Rv::Set(items) => {
            if items.is_empty() {
                return Table { columns: vec![ColumnMeta::new("value", "string", DataKind::Text)], rows: Vec::new() };
            }
            if items.iter().all(|i| !matches!(i, Rv::Array(_) | Rv::Map(_) | Rv::Set(_))) {
                let kind = kind_of(&items[0]);
                let rows = items.iter().map(|i| vec![to_value(i)]).collect();
                return Table { columns: vec![ColumnMeta::new("value", type_name(kind), kind)], rows };
            }
            // Nested lists: one column per position.
            let width = items
                .iter()
                .map(|i| match i {
                    Rv::Array(a) | Rv::Set(a) => a.len(),
                    Rv::Map(m) => m.len() * 2,
                    _ => 1,
                })
                .max()
                .unwrap_or(1);
            let columns = (1..=width).map(|i| ColumnMeta::new(i.to_string(), "string", DataKind::Text)).collect();
            let rows = items
                .iter()
                .map(|i| match i {
                    Rv::Array(a) | Rv::Set(a) => a.iter().map(to_value).collect(),
                    Rv::Map(m) => m.iter().flat_map(|(k, v)| [to_value(k), to_value(v)]).collect(),
                    other => vec![to_value(other)],
                })
                .collect();
            Table { columns, rows }
        }
        Rv::Map(entries) => {
            let rows = entries.iter().map(|(k, v)| vec![to_value(k), to_value(v)]).collect();
            Table { columns: vec![ColumnMeta::new("field", "string", DataKind::Text), ColumnMeta::new("value", "string", DataKind::Text)], rows }
        }
        other => {
            let kind = kind_of(&other);
            Table::one("value", kind, type_name(kind), to_value(&other))
        }
    }
}

fn is_scan(name: &str) -> bool {
    matches!(name, "SCAN" | "HSCAN" | "SSCAN" | "ZSCAN")
}

fn element_column(name: &str) -> (&'static str, DataKind) {
    match name {
        "HSCAN" => ("field", DataKind::Text),
        "ZSCAN" => ("member", DataKind::Text),
        _ => ("key", DataKind::Text),
    }
}

/// Flat `[a, b, a, b …]` or a map into two columns.
fn pairs(reply: Rv, first: &str, second: &str) -> Table {
    let mut rows = Vec::new();
    match reply {
        Rv::Map(entries) => {
            for (k, v) in entries {
                rows.push(vec![to_value(&k), to_value(&v)]);
            }
        }
        Rv::Array(items) => {
            let mut it = items.chunks(2);
            for pair in it.by_ref() {
                let mut row = vec![to_value(&pair[0])];
                row.push(pair.get(1).map(to_value).unwrap_or(Value::Null));
                rows.push(row);
            }
        }
        Rv::Nil => {
            return Table { columns: Vec::new(), rows: Vec::new() };
        }
        other => rows.push(vec![to_value(&other), Value::Null]),
    }
    Table { columns: vec![ColumnMeta::new(first, "string", DataKind::Text), ColumnMeta::new(second, "string", DataKind::Text)], rows }
}

/// The grid value of a reply element.
pub fn to_value(v: &Rv) -> Value {
    match v {
        Rv::Nil => Value::Null,
        Rv::Int(i) => Value::Int(*i),
        Rv::Double(f) => Value::Float(*f),
        Rv::Boolean(b) => Value::Bool(*b),
        Rv::BulkString(bytes) => match std::str::from_utf8(bytes) {
            Ok(s) => Value::Text(s.into()),
            Err(_) => Value::Bytes(bytes.as_slice().into()),
        },
        Rv::SimpleString(s) | Rv::VerbatimString { text: s, .. } => Value::Text(s.as_str().into()),
        Rv::Okay => Value::Text("OK".into()),
        Rv::Array(items) | Rv::Set(items) => Value::Json(array_json(items).into()),
        Rv::Map(entries) => {
            let mut doc = serde_json::Map::new();
            for (k, v) in entries {
                doc.insert(scalar_text(k), json_of(v));
            }
            Value::Json(serde_json::Value::Object(doc).to_string().into())
        }
        Rv::Attribute { data, .. } => to_value(data),
        Rv::Push { data, .. } => Value::Json(array_json(data).into()),
        Rv::ServerError(e) => Value::Text(e.to_string().into()),
        other => Value::Text(format!("{other:?}").into()),
    }
}

fn array_json(items: &[Rv]) -> String {
    serde_json::Value::Array(items.iter().map(json_of).collect()).to_string()
}

fn json_of(v: &Rv) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Rv::Nil => J::Null,
        Rv::Int(i) => J::Number((*i).into()),
        Rv::Double(f) => serde_json::Number::from_f64(*f).map(J::Number).unwrap_or(J::Null),
        Rv::Boolean(b) => J::Bool(*b),
        Rv::BulkString(bytes) => J::String(String::from_utf8_lossy(bytes).into_owned()),
        Rv::SimpleString(s) | Rv::VerbatimString { text: s, .. } => J::String(s.clone()),
        Rv::Okay => J::String("OK".into()),
        Rv::Array(items) | Rv::Set(items) => J::Array(items.iter().map(json_of).collect()),
        Rv::Map(entries) => {
            let mut doc = serde_json::Map::new();
            for (k, v) in entries {
                doc.insert(scalar_text(k), json_of(v));
            }
            J::Object(doc)
        }
        Rv::Attribute { data, .. } => json_of(data),
        Rv::Push { data, .. } => J::Array(data.iter().map(json_of).collect()),
        Rv::ServerError(e) => J::String(e.to_string()),
        other => J::String(format!("{other:?}")),
    }
}

/// Text of a scalar reply (cursor, field name).
pub fn scalar_text(v: &Rv) -> String {
    match v {
        Rv::Int(i) => i.to_string(),
        Rv::BulkString(b) => String::from_utf8_lossy(b).into_owned(),
        Rv::SimpleString(s) => s.clone(),
        other => to_value(other).to_text().into_owned(),
    }
}

fn kind_of(v: &Rv) -> DataKind {
    match v {
        Rv::Int(_) => DataKind::Int,
        Rv::Double(_) => DataKind::Float,
        Rv::Boolean(_) => DataKind::Bool,
        Rv::Nil => DataKind::Other,
        Rv::Array(_) | Rv::Map(_) | Rv::Set(_) => DataKind::Json,
        _ => DataKind::Text,
    }
}

fn type_name(kind: DataKind) -> &'static str {
    match kind {
        DataKind::Int => "integer",
        DataKind::Float => "double",
        DataKind::Bool => "boolean",
        DataKind::Json => "array",
        DataKind::Other => "nil",
        _ => "string",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        crate::command::parse_command(line).unwrap()
    }

    #[test]
    fn scalars_are_one_row() {
        let t = shape(&args("GET a"), Rv::BulkString(b"hello".to_vec()));
        assert_eq!(t.columns.len(), 1);
        assert_eq!(t.rows, vec![vec![Value::Text("hello".into())]]);
        let t = shape(&args("INCR n"), Rv::Int(4));
        assert_eq!(t.rows[0][0], Value::Int(4));
        assert_eq!(t.columns[0].kind, DataKind::Int);
        let t = shape(&args("GET a"), Rv::Nil);
        assert_eq!(t.rows[0][0], Value::Null);
    }

    #[test]
    fn lists_and_maps() {
        let t = shape(&args("KEYS *"), Rv::Array(vec![Rv::BulkString(b"a".to_vec()), Rv::BulkString(b"b".to_vec())]));
        assert_eq!(t.columns[0].name, "value");
        assert_eq!(t.rows.len(), 2);

        let t = shape(&args("HGETALL h"), Rv::Array(vec![Rv::BulkString(b"f".to_vec()), Rv::BulkString(b"v".to_vec())]));
        assert_eq!(t.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["field", "value"]);
        assert_eq!(t.rows, vec![vec![Value::Text("f".into()), Value::Text("v".into())]]);
    }

    #[test]
    fn scans_show_the_cursor() {
        let t = shape(&args("SCAN 0"), Rv::Array(vec![Rv::BulkString(b"17".to_vec()), Rv::Array(vec![Rv::BulkString(b"k1".to_vec())])]));
        assert_eq!(t.columns[0].name, "cursor");
        assert_eq!(t.rows, vec![vec![Value::Text("17".into()), Value::Text("k1".into())]]);
    }

    #[test]
    fn sorted_sets_with_scores_are_pairs() {
        let reply = Rv::Array(vec![Rv::BulkString(b"a".to_vec()), Rv::BulkString(b"1".to_vec()), Rv::BulkString(b"b".to_vec()), Rv::BulkString(b"2".to_vec())]);
        let t = shape(&args("ZRANGE z 0 -1 WITHSCORES"), reply);
        assert_eq!(t.columns[0].name, "member");
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[1][0], Value::Text("b".into()));
    }

    #[test]
    fn nested_lists_get_positional_columns() {
        let t = shape(&args("XRANGE s - +"), Rv::Array(vec![Rv::Array(vec![Rv::BulkString(b"1-1".to_vec()), Rv::Array(vec![Rv::BulkString(b"f".to_vec())])])]));
        assert_eq!(t.columns.len(), 2);
        assert_eq!(t.rows[0].len(), 2);
    }

    #[test]
    fn non_utf8_values_stay_bytes() {
        let t = shape(&args("GET a"), Rv::BulkString(vec![0xff, 0x00]));
        assert!(matches!(t.rows[0][0], Value::Bytes(_)));
    }
}
