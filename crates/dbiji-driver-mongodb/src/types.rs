//! Conversion between BSON and the DBiji value model, and the shaping of
//! documents into result rows.
//!
//! A result set is one column per top-level field of the documents; the
//! columns are the union of the fields seen, in the order they first appear.
//! Nested documents and arrays are shown as JSON text (the grid's JSON view
//! expands them), so a row keeps a stable shape whatever the documents hold.

use dbiji_core::value::{DataKind, is_decimal_text};
use dbiji_core::{ColumnMeta, Value};
use mongodb::bson::{Bson, Document, spec::ElementType};

/// Longest text a cell keeps; longer values are cut (the grid would show a
/// tooltip of it anyway and the row store is bounded).
const MAX_TEXT: usize = 1 << 20;

/// BSON type name as the shell reports it (`ObjectId`, `ISODate` …).
pub fn type_name(b: &Bson) -> &'static str {
    match b.element_type() {
        ElementType::Double => "double",
        ElementType::String => "string",
        ElementType::Array => "array",
        ElementType::Binary => "binData",
        ElementType::Undefined => "undefined",
        ElementType::ObjectId => "objectId",
        ElementType::Boolean => "bool",
        ElementType::DateTime => "date",
        ElementType::Null => "null",
        ElementType::RegularExpression => "regex",
        ElementType::DbPointer => "dbPointer",
        ElementType::JavaScriptCode => "javascript",
        ElementType::Symbol => "symbol",
        ElementType::JavaScriptCodeWithScope => "javascriptWithScope",
        ElementType::Int32 => "int",
        ElementType::Timestamp => "timestamp",
        ElementType::Int64 => "long",
        ElementType::Decimal128 => "decimal",
        ElementType::MinKey => "minKey",
        ElementType::MaxKey => "maxKey",
        ElementType::EmbeddedDocument => "object",
    }
}

/// Logical kind of a BSON value.
pub fn data_kind(b: &Bson) -> DataKind {
    match b {
        Bson::Double(_) => DataKind::Float,
        Bson::String(_) => DataKind::Text,
        Bson::Array(_) | Bson::Document(_) => DataKind::Json,
        Bson::Binary(_) => DataKind::Binary,
        Bson::ObjectId(_) => DataKind::Text,
        Bson::Boolean(_) => DataKind::Bool,
        Bson::DateTime(_) => DataKind::DateTimeTz,
        Bson::Null | Bson::Undefined | Bson::MinKey | Bson::MaxKey => DataKind::Other,
        Bson::RegularExpression(_) | Bson::JavaScriptCode(_) | Bson::Symbol(_) | Bson::DbPointer(_) => DataKind::Text,
        Bson::Int32(_) | Bson::Int64(_) => DataKind::Int,
        Bson::Timestamp(_) => DataKind::DateTimeTz,
        Bson::Decimal128(_) => DataKind::Decimal,
        Bson::JavaScriptCodeWithScope(_) => DataKind::Json,
    }
}

/// Column metadata for a field, typed from the first non-null value seen.
pub fn column(name: &str, sample: Option<&Bson>) -> ColumnMeta {
    match sample {
        Some(b) => ColumnMeta::new(name, type_name(b), data_kind(b)),
        None => ColumnMeta::new(name, "null", DataKind::Other),
    }
}

/// Render one BSON value as a cell.
pub fn to_value(b: &Bson) -> Value {
    match b {
        Bson::Double(f) => {
            if f.is_finite() {
                Value::Float(*f)
            } else {
                Value::Text(format!("{f}").into())
            }
        }
        Bson::String(s) => Value::Text(s.as_str().into()),
        Bson::Array(_) | Bson::Document(_) => Value::Json(json_text(b).into()),
        Bson::Binary(bin) => match bin.subtype {
            mongodb::bson::spec::BinarySubtype::Uuid if bin.bytes.len() == 16 => match uuid_from_bytes(&bin.bytes) {
                Some(u) => Value::Uuid(u),
                None => Value::Bytes(bin.bytes.as_slice().into()),
            },
            _ => Value::Bytes(bin.bytes.as_slice().into()),
        },
        Bson::ObjectId(oid) => Value::Text(oid.to_hex().into()),
        Bson::Boolean(v) => Value::Bool(*v),
        Bson::DateTime(dt) => {
            let secs = dt.timestamp_millis().div_euclid(1000);
            let nanos = (dt.timestamp_millis().rem_euclid(1000) * 1_000_000) as u32;
            chrono::DateTime::from_timestamp(secs, nanos).map(|d| Value::DateTimeTz(d.fixed_offset())).unwrap_or(Value::Null)
        }
        Bson::Null | Bson::Undefined | Bson::MinKey | Bson::MaxKey => Value::Null,
        Bson::RegularExpression(r) => Value::Text(format!("/{}/{}", r.pattern, r.options).into()),
        Bson::JavaScriptCode(c) | Bson::Symbol(c) => Value::Text(c.as_str().into()),
        Bson::JavaScriptCodeWithScope(c) => Value::Json(c.code.as_str().into()),
        Bson::DbPointer(_) => Value::Text(b.to_string().into()),
        Bson::Int32(i) => Value::Int(*i as i64),
        Bson::Int64(i) => Value::Int(*i),
        Bson::Timestamp(t) => {
            let secs = t.time as i64;
            chrono::DateTime::from_timestamp(secs, 0).map(|d| Value::DateTimeTz(d.fixed_offset())).unwrap_or(Value::Null)
        }
        Bson::Decimal128(d) => {
            let text = d.to_string();
            if is_decimal_text(&text) { Value::Decimal(text.into()) } else { Value::Text(text.into()) }
        }
    }
}

fn uuid_from_bytes(b: &[u8]) -> Option<uuid::Uuid> {
    let arr: [u8; 16] = b.try_into().ok()?;
    Some(uuid::Uuid::from_bytes(arr))
}

/// JSON text of a nested value (`{"a": 1}`, `[1, 2]`); other values keep the
/// shell's own representation.
pub fn json_text(b: &Bson) -> String {
    let text = match b {
        Bson::Document(d) => serde_json::to_string(&document_to_json(d)).unwrap_or_default(),
        Bson::Array(a) => serde_json::to_string(&serde_json::Value::Array(a.iter().map(bson_to_json).collect())).unwrap_or_default(),
        other => serde_json::to_string(&bson_to_json(other)).unwrap_or_else(|_| other.to_string()),
    };
    truncate(text)
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_TEXT {
        let mut cut = MAX_TEXT;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
    s
}

fn document_to_json(d: &Document) -> serde_json::Value {
    let mut map = serde_json::Map::with_capacity(d.len());
    for (k, v) in d {
        map.insert(k.clone(), bson_to_json(v));
    }
    serde_json::Value::Object(map)
}

fn bson_to_json(b: &Bson) -> serde_json::Value {
    use serde_json::Value as J;
    match b {
        Bson::Double(f) => serde_json::Number::from_f64(*f).map(J::Number).unwrap_or(J::Null),
        Bson::String(s) => J::String(s.clone()),
        Bson::Array(a) => J::Array(a.iter().map(bson_to_json).collect()),
        Bson::Document(d) => document_to_json(d),
        Bson::Boolean(v) => J::Bool(*v),
        Bson::Null | Bson::Undefined | Bson::MinKey | Bson::MaxKey => J::Null,
        Bson::Int32(i) => J::Number((*i).into()),
        Bson::Int64(i) => J::Number((*i).into()),
        Bson::ObjectId(oid) => J::String(oid.to_hex()),
        Bson::DateTime(dt) => J::String(iso(dt.timestamp_millis())),
        Bson::Timestamp(t) => J::String(iso(t.time as i64 * 1000)),
        Bson::Binary(bin) => J::String(hex(&bin.bytes)),
        Bson::RegularExpression(r) => J::String(format!("/{}/{}", r.pattern, r.options)),
        Bson::Decimal128(d) => {
            let text = d.to_string();
            text.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(J::Number).unwrap_or(J::String(text))
        }
        Bson::JavaScriptCode(c) | Bson::Symbol(c) => J::String(c.clone()),
        Bson::JavaScriptCodeWithScope(c) => J::String(c.code.clone()),
        Bson::DbPointer(_) => J::String(b.to_string()),
    }
}

/// RFC 3339 text of a millisecond timestamp.
pub fn iso(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let nanos = (millis.rem_euclid(1000) * 1_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nanos).map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)).unwrap_or_default()
}

/// Lowercase hex of bytes.
pub fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// Column set of a run of documents: the union of the top-level fields, in
/// the order they first appear, typed from the first non-null value.
#[derive(Debug, Default)]
pub struct Columns {
    names: Vec<String>,
    samples: Vec<Option<Bson>>,
}

impl Columns {
    pub fn new() -> Self {
        Columns::default()
    }

    /// Add a document's fields.
    pub fn observe(&mut self, doc: &Document) {
        for (k, v) in doc {
            match self.names.iter().position(|n| n == k) {
                Some(i) => {
                    if self.samples[i].is_none() && !matches!(v, Bson::Null) {
                        self.samples[i] = Some(v.clone());
                    }
                }
                None => {
                    self.names.push(k.clone());
                    self.samples.push(if matches!(v, Bson::Null) { None } else { Some(v.clone()) });
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Column metadata; `fallback` types the fields that are always null.
    pub fn metas(&self, fallback: &str) -> Vec<ColumnMeta> {
        self.names
            .iter()
            .zip(&self.samples)
            .map(|(n, s)| match s {
                Some(b) => column(n, Some(b)),
                None => ColumnMeta::new(n.clone(), fallback, DataKind::Other),
            })
            .collect()
    }

    /// One row per document, in the column order (missing fields are NULL).
    pub fn row(&self, doc: &Document) -> Vec<Value> {
        self.names.iter().map(|n| doc.get(n).map(to_value).unwrap_or(Value::Null)).collect()
    }
}

/// A single-column result holding one value per row.
pub fn single_column(name: &str, kind: DataKind, values: impl IntoIterator<Item = Value>) -> (Vec<ColumnMeta>, Vec<Vec<Value>>) {
    (vec![ColumnMeta::new(name, type_name_for_kind(kind), kind)], values.into_iter().map(|v| vec![v]).collect())
}

fn type_name_for_kind(kind: DataKind) -> &'static str {
    match kind {
        DataKind::Bool => "bool",
        DataKind::Int => "int",
        DataKind::Float => "double",
        DataKind::Decimal => "decimal",
        DataKind::Text => "string",
        DataKind::Binary => "binData",
        DataKind::DateTime | DataKind::DateTimeTz => "date",
        DataKind::Json => "object",
        _ => "string",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::doc;

    #[test]
    fn values_map_to_the_grid() {
        assert_eq!(to_value(&Bson::Int32(3)), Value::Int(3));
        assert_eq!(to_value(&Bson::String("x".into())), Value::Text("x".into()));
        assert!(matches!(to_value(&Bson::Double(1.5)), Value::Float(f) if f == 1.5));
        assert_eq!(to_value(&Bson::Null), Value::Null);
        assert!(matches!(to_value(&Bson::DateTime(mongodb::bson::DateTime::from_millis(0))), Value::DateTimeTz(_)));
        assert!(matches!(to_value(&Bson::Document(doc! {"a": 1})), Value::Json(j) if j.contains("\"a\"")));
    }

    #[test]
    fn columns_are_the_union_of_the_fields() {
        let mut c = Columns::new();
        c.observe(&doc! {"_id": 1, "name": "a"});
        c.observe(&doc! {"_id": 2, "age": 30});
        assert_eq!(c.names(), ["_id", "name", "age"]);
        let row = c.row(&doc! {"_id": 3});
        assert_eq!(row.len(), 3);
        assert_eq!(row[1], Value::Null);
        let metas = c.metas("string");
        assert_eq!(metas[0].type_name, "int");
        assert_eq!(metas[2].kind, DataKind::Int);
    }

    #[test]
    fn null_only_fields_keep_the_fallback_type() {
        let mut c = Columns::new();
        c.observe(&doc! {"a": Bson::Null});
        let metas = c.metas("string");
        assert_eq!(metas[0].type_name, "string");
    }
}
