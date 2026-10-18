//! Result column metadata and value conversion between `mysql_async` and the
//! DBiji value model, for both the text and the binary protocol.

use crate::geometry;
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use dbiji_core::{ColumnMeta, ColumnSource, DataKind, DbError, DbResult, Value};
use encoding_rs::Encoding;
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::{Column, Value as MyValue};

/// Collation id of the `binary` pseudo character set.
pub const BINARY_COLLATION: u16 = 63;

/// `decimals` value meaning "not fixed" (computed expressions).
const NOT_FIXED_DEC: u8 = 31;

/// A character set as far as value decoding is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Charset {
    /// MySQL character set name.
    pub name: &'static str,
    /// Maximum bytes per character.
    pub mbmaxlen: u8,
    /// Encoding used to decode values; `None` = UTF-8 (or ASCII / binary).
    pub encoding: Option<&'static Encoding>,
    /// UTF-32 has no `encoding_rs` decoder and is handled separately.
    pub utf32: bool,
}

impl Charset {
    const fn utf8(name: &'static str, mbmaxlen: u8) -> Charset {
        Charset {
            name,
            mbmaxlen,
            encoding: None,
            utf32: false,
        }
    }
    const fn enc(name: &'static str, mbmaxlen: u8, e: &'static Encoding) -> Charset {
        Charset {
            name,
            mbmaxlen,
            encoding: Some(e),
            utf32: false,
        }
    }

    /// UTF-8 compatible (no transcoding needed).
    pub fn is_utf8(&self) -> bool {
        self.encoding.is_none() && !self.utf32
    }
}

/// `utf8mb4`.
pub const UTF8MB4: Charset = Charset::utf8("utf8mb4", 4);

/// Character set of a collation id (as found in column definitions).
/// Unknown ids (e.g. MariaDB UCA 14.0 collations above 255) are assumed to be
/// UTF-8 based.
pub fn charset_for_collation(id: u16) -> Charset {
    use encoding_rs::*;
    match id {
        63 => Charset::utf8("binary", 1),
        33 | 76 | 83 | 192..=215 | 223 => Charset::utf8("utf8mb3", 3),
        45 | 46 | 224..=247 | 255..=323 => UTF8MB4,
        11 | 65 => Charset::utf8("ascii", 1),
        5 | 8 | 15 | 31 | 47 | 48 | 49 | 94 => Charset::enc("latin1", 1, WINDOWS_1252),
        2 | 9 | 21 | 27 | 77 => Charset::enc("latin2", 1, ISO_8859_2),
        7 | 74 => Charset::enc("koi8r", 1, KOI8_R),
        22 | 75 => Charset::enc("koi8u", 1, KOI8_U),
        14 | 23 | 50 | 51 | 52 => Charset::enc("cp1251", 1, WINDOWS_1251),
        26 | 34 | 44 | 66 | 99 => Charset::enc("cp1250", 1, WINDOWS_1250),
        57 | 67 => Charset::enc("cp1256", 1, WINDOWS_1256),
        29 | 58 | 59 => Charset::enc("cp1257", 1, WINDOWS_1257),
        25 | 70 => Charset::enc("greek", 1, ISO_8859_7),
        16 | 71 => Charset::enc("hebrew", 1, ISO_8859_8),
        18 | 89 => Charset::enc("tis620", 1, WINDOWS_874),
        30 | 78 => Charset::enc("latin5", 1, WINDOWS_1254),
        20 | 41 | 42 | 79 => Charset::enc("latin7", 1, ISO_8859_13),
        36 | 68 => Charset::enc("cp866", 1, IBM866),
        39 | 53 => Charset::enc("macroman", 1, MACINTOSH),
        1 | 84 => Charset::enc("big5", 2, BIG5),
        13 | 88 => Charset::enc("sjis", 2, SHIFT_JIS),
        95 | 96 => Charset::enc("cp932", 2, SHIFT_JIS),
        12 | 91 => Charset::enc("ujis", 3, EUC_JP),
        97 | 98 => Charset::enc("eucjpms", 3, EUC_JP),
        19 | 85 => Charset::enc("euckr", 2, EUC_KR),
        24 | 86 => Charset::enc("gb2312", 2, GBK),
        28 | 87 => Charset::enc("gbk", 2, GBK),
        248..=250 => Charset::enc("gb18030", 4, GB18030),
        35 | 90 | 128..=151 | 159 => Charset::enc("ucs2", 2, UTF_16BE),
        54 | 55 | 101..=124 => Charset::enc("utf16", 4, UTF_16BE),
        56 | 62 => Charset::enc("utf16le", 4, UTF_16LE),
        60 | 61 | 160..=183 => Charset {
            name: "utf32",
            mbmaxlen: 4,
            encoding: None,
            utf32: true,
        },
        // dec8, cp850, cp852, hp8, swe7, keybcs2, armscii8, geostd8, macce: no
        // decoder available; Windows-1252 keeps ASCII intact.
        3 | 4 | 6 | 10 | 32 | 37 | 38 | 40 | 43 | 64 | 69 | 72 | 73 | 80 | 81 | 82 | 92 | 93 => {
            Charset::enc("other", 1, WINDOWS_1252)
        }
        _ => UTF8MB4,
    }
}

/// Character set by MySQL name (`SET NAMES` argument), for encoding SQL text
/// and parameters sent to the server.
pub fn charset_by_name(name: &str) -> Option<Charset> {
    use encoding_rs::*;
    let n = name
        .trim()
        .trim_matches(|c| c == '\'' || c == '"' || c == '`')
        .to_ascii_lowercase();
    Some(match n.as_str() {
        "utf8mb4" => UTF8MB4,
        "utf8" | "utf8mb3" => Charset::utf8("utf8mb3", 3),
        "ascii" => Charset::utf8("ascii", 1),
        "binary" => Charset::utf8("binary", 1),
        "latin1" => Charset::enc("latin1", 1, WINDOWS_1252),
        "latin2" => Charset::enc("latin2", 1, ISO_8859_2),
        "cp1250" => Charset::enc("cp1250", 1, WINDOWS_1250),
        "cp1251" => Charset::enc("cp1251", 1, WINDOWS_1251),
        "cp1256" => Charset::enc("cp1256", 1, WINDOWS_1256),
        "cp1257" => Charset::enc("cp1257", 1, WINDOWS_1257),
        "koi8r" => Charset::enc("koi8r", 1, KOI8_R),
        "koi8u" => Charset::enc("koi8u", 1, KOI8_U),
        "greek" => Charset::enc("greek", 1, ISO_8859_7),
        "hebrew" => Charset::enc("hebrew", 1, ISO_8859_8),
        "tis620" => Charset::enc("tis620", 1, WINDOWS_874),
        "latin5" => Charset::enc("latin5", 1, WINDOWS_1254),
        "latin7" => Charset::enc("latin7", 1, ISO_8859_13),
        "cp866" => Charset::enc("cp866", 1, IBM866),
        "macroman" => Charset::enc("macroman", 1, MACINTOSH),
        "big5" => Charset::enc("big5", 2, BIG5),
        "sjis" | "cp932" => Charset::enc("sjis", 2, SHIFT_JIS),
        "ujis" | "eucjpms" => Charset::enc("ujis", 3, EUC_JP),
        "euckr" => Charset::enc("euckr", 2, EUC_KR),
        "gb2312" | "gbk" => Charset::enc("gbk", 2, GBK),
        "gb18030" => Charset::enc("gb18030", 4, GB18030),
        _ => return None,
    })
}

/// Encode client text (SQL statements, string parameters) into the
/// connection character set.
pub fn encode_text(cs: &Charset, s: &str) -> DbResult<Vec<u8>> {
    match cs.encoding {
        None => Ok(s.as_bytes().to_vec()),
        Some(enc) => {
            let (bytes, _, had_errors) = enc.encode(s);
            if had_errors {
                Err(DbError::query(format!(
                    "The text contains characters that cannot be represented in the connection character set '{}'",
                    cs.name
                )))
            } else {
                Ok(bytes.into_owned())
            }
        }
    }
}

/// Decode a text value; `None` when the bytes are not valid in `cs`.
pub fn decode_text(cs: &Charset, bytes: Vec<u8>) -> Result<String, Vec<u8>> {
    if cs.utf32 {
        if !bytes.len().is_multiple_of(4) {
            return Err(bytes);
        }
        let mut s = String::with_capacity(bytes.len() / 4);
        for ch in bytes.as_chunks::<4>().0 {
            match char::from_u32(u32::from_be_bytes([ch[0], ch[1], ch[2], ch[3]])) {
                Some(c) => s.push(c),
                None => return Err(bytes),
            }
        }
        return Ok(s);
    }
    match cs.encoding {
        None => String::from_utf8(bytes).map_err(|e| e.into_bytes()),
        Some(enc) => match enc.decode_without_bom_handling_and_without_replacement(&bytes) {
            Some(s) => Ok(s.into_owned()),
            None => Err(bytes),
        },
    }
}

/// How the values of one result column are converted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decode {
    /// `MYSQL_TYPE_NULL`.
    Null,
    /// Integer types (signed, or unsigned that fits `i64`).
    Int,
    /// `BIGINT UNSIGNED`.
    UInt,
    /// `YEAR`.
    Year,
    /// `DECIMAL`.
    Decimal,
    /// `FLOAT` (single precision).
    Float,
    /// `DOUBLE`.
    Double,
    /// `DATE`.
    Date,
    /// `DATETIME` / `TIMESTAMP`.
    DateTime,
    /// `TIME` with the number of fractional digits.
    Time(u8),
    /// `BIT(1)`.
    Bool,
    /// `BIT(n)`, rendered as a string of `n` binary digits.
    Bits(u32),
    /// `JSON`.
    Json,
    /// Character data in the given character set.
    Text(Charset),
    /// Binary data.
    Bytes,
    /// Geometry values (SRID + WKB) rendered as EWKT.
    Geometry,
    /// MySQL 9 `VECTOR` (packed little-endian f32).
    Vector,
}

fn is_binary(col: &Column) -> bool {
    col.character_set() == BINARY_COLLATION
}

fn flags(col: &Column) -> ColumnFlags {
    col.flags()
}

/// Map a column definition to its decoder and DBiji metadata.
pub fn column_meta(col: &Column) -> (ColumnMeta, Decode) {
    let ty = col.column_type();
    let fl = flags(col);
    let unsigned = fl.contains(ColumnFlags::UNSIGNED_FLAG);
    let len = col.column_length();
    let decimals = col.decimals();
    let cs = charset_for_collation(col.character_set());
    let binary = is_binary(col);

    let (kind, decode) = match ty {
        ColumnType::MYSQL_TYPE_NULL => (DataKind::Text, Decode::Null),
        ColumnType::MYSQL_TYPE_TINY
        | ColumnType::MYSQL_TYPE_SHORT
        | ColumnType::MYSQL_TYPE_INT24
        | ColumnType::MYSQL_TYPE_LONG => (DataKind::Int, Decode::Int),
        ColumnType::MYSQL_TYPE_LONGLONG => (
            DataKind::Int,
            if unsigned { Decode::UInt } else { Decode::Int },
        ),
        ColumnType::MYSQL_TYPE_YEAR => (DataKind::Int, Decode::Year),
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => {
            (DataKind::Decimal, Decode::Decimal)
        }
        ColumnType::MYSQL_TYPE_FLOAT => (DataKind::Float, Decode::Float),
        ColumnType::MYSQL_TYPE_DOUBLE => (DataKind::Float, Decode::Double),
        ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => {
            (DataKind::Date, Decode::Date)
        }
        ColumnType::MYSQL_TYPE_DATETIME
        | ColumnType::MYSQL_TYPE_DATETIME2
        | ColumnType::MYSQL_TYPE_TIMESTAMP
        | ColumnType::MYSQL_TYPE_TIMESTAMP2 => (DataKind::DateTime, Decode::DateTime),
        ColumnType::MYSQL_TYPE_TIME | ColumnType::MYSQL_TYPE_TIME2 => (
            DataKind::Time,
            Decode::Time(if decimals <= 6 { decimals } else { 6 }),
        ),
        ColumnType::MYSQL_TYPE_BIT => {
            if len <= 1 {
                (DataKind::Bool, Decode::Bool)
            } else {
                (DataKind::Bit, Decode::Bits(len.min(64)))
            }
        }
        ColumnType::MYSQL_TYPE_JSON => (DataKind::Json, Decode::Json),
        ColumnType::MYSQL_TYPE_GEOMETRY => (DataKind::Geometry, Decode::Geometry),
        ColumnType::MYSQL_TYPE_VECTOR => (DataKind::Array, Decode::Vector),
        ColumnType::MYSQL_TYPE_ENUM | ColumnType::MYSQL_TYPE_SET => {
            (DataKind::Enum, Decode::Text(cs))
        }
        ColumnType::MYSQL_TYPE_STRING
        | ColumnType::MYSQL_TYPE_VAR_STRING
        | ColumnType::MYSQL_TYPE_VARCHAR
        | ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB => {
            if fl.contains(ColumnFlags::ENUM_FLAG) || fl.contains(ColumnFlags::SET_FLAG) {
                (DataKind::Enum, Decode::Text(cs))
            } else if binary {
                (DataKind::Binary, Decode::Bytes)
            } else {
                (DataKind::Text, Decode::Text(cs))
            }
        }
        _ => {
            if binary {
                (DataKind::Binary, Decode::Bytes)
            } else {
                (DataKind::Other, Decode::Text(cs))
            }
        }
    };

    let mut meta = ColumnMeta::new(col.name_str().into_owned(), type_name(col), kind);
    meta.nullable = Some(!fl.contains(ColumnFlags::NOT_NULL_FLAG));
    meta.auto_increment = Some(fl.contains(ColumnFlags::AUTO_INCREMENT_FLAG));
    match ty {
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => {
            let scale = if decimals == NOT_FIXED_DEC {
                0
            } else {
                decimals as u32
            };
            meta.precision = Some(decimal_precision(len, scale, unsigned));
            meta.scale = Some(scale);
        }
        ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE => {
            if decimals != NOT_FIXED_DEC && decimals > 0 {
                meta.scale = Some(decimals as u32);
            }
        }
        ColumnType::MYSQL_TYPE_TINY
        | ColumnType::MYSQL_TYPE_SHORT
        | ColumnType::MYSQL_TYPE_INT24
        | ColumnType::MYSQL_TYPE_LONG
        | ColumnType::MYSQL_TYPE_LONGLONG => {
            meta.precision = Some(len);
        }
        ColumnType::MYSQL_TYPE_DATETIME
        | ColumnType::MYSQL_TYPE_TIMESTAMP
        | ColumnType::MYSQL_TYPE_TIME
        | ColumnType::MYSQL_TYPE_DATETIME2
        | ColumnType::MYSQL_TYPE_TIMESTAMP2
        | ColumnType::MYSQL_TYPE_TIME2
            if decimals > 0 && decimals <= 6 =>
        {
            meta.scale = Some(decimals as u32);
        }
        _ => {}
    }
    if matches!(decode, Decode::Text(_) | Decode::Bytes) && kind != DataKind::Enum {
        let chars = if binary {
            len
        } else {
            len / (cs.mbmaxlen.max(1) as u32)
        };
        meta.max_length = Some(chars);
    }
    if matches!(decode, Decode::Bits(_)) {
        meta.precision = Some(len);
    }
    let org_table = col.org_table_str();
    if !org_table.is_empty() && !org_table.starts_with("#sql") {
        let schema = col.schema_str();
        let org_name = col.org_name_str();
        meta.source = Some(ColumnSource {
            database: None,
            schema: if schema.is_empty() {
                None
            } else {
                Some(schema.into_owned())
            },
            table: org_table.into_owned(),
            column: if org_name.is_empty() {
                col.name_str().into_owned()
            } else {
                org_name.into_owned()
            },
        });
    }
    (meta, decode)
}

/// Precision of a DECIMAL from its display length.
fn decimal_precision(len: u32, scale: u32, unsigned: bool) -> u32 {
    let mut p = len;
    if scale > 0 {
        p = p.saturating_sub(1);
    }
    if !unsigned {
        p = p.saturating_sub(1);
    }
    p
}

/// Readable type name of a result column, e.g. `int unsigned`,
/// `varchar(255)`, `decimal(10,2)`, `tinyint(1)`, `datetime(6)`.
pub fn type_name(col: &Column) -> String {
    let ty = col.column_type();
    let fl = flags(col);
    let unsigned = fl.contains(ColumnFlags::UNSIGNED_FLAG);
    let len = col.column_length();
    let decimals = col.decimals();
    let binary = is_binary(col);
    let cs = charset_for_collation(col.character_set());
    let chars = if binary {
        len
    } else {
        len / cs.mbmaxlen.max(1) as u32
    };
    let fsp = |base: &str| {
        if decimals > 0 && decimals <= 6 {
            format!("{base}({decimals})")
        } else {
            base.to_string()
        }
    };
    let int = |base: &str| {
        let mut s = base.to_string();
        if base == "tinyint" && len == 1 {
            s.push_str("(1)");
        }
        if unsigned {
            s.push_str(" unsigned");
        }
        if fl.contains(ColumnFlags::ZEROFILL_FLAG) {
            s.push_str(" zerofill");
        }
        s
    };
    match ty {
        ColumnType::MYSQL_TYPE_NULL => "null".into(),
        ColumnType::MYSQL_TYPE_TINY => int("tinyint"),
        ColumnType::MYSQL_TYPE_SHORT => int("smallint"),
        ColumnType::MYSQL_TYPE_INT24 => int("mediumint"),
        ColumnType::MYSQL_TYPE_LONG => int("int"),
        ColumnType::MYSQL_TYPE_LONGLONG => int("bigint"),
        ColumnType::MYSQL_TYPE_YEAR => "year".into(),
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => {
            let scale = if decimals == NOT_FIXED_DEC {
                0
            } else {
                decimals as u32
            };
            let mut s = format!(
                "decimal({},{})",
                decimal_precision(len, scale, unsigned),
                scale
            );
            if unsigned {
                s.push_str(" unsigned");
            }
            s
        }
        ColumnType::MYSQL_TYPE_FLOAT => {
            if unsigned {
                "float unsigned".into()
            } else {
                "float".into()
            }
        }
        ColumnType::MYSQL_TYPE_DOUBLE => {
            if unsigned {
                "double unsigned".into()
            } else {
                "double".into()
            }
        }
        ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => "date".into(),
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_DATETIME2 => fsp("datetime"),
        ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2 => fsp("timestamp"),
        ColumnType::MYSQL_TYPE_TIME | ColumnType::MYSQL_TYPE_TIME2 => fsp("time"),
        ColumnType::MYSQL_TYPE_BIT => format!("bit({})", len.max(1)),
        ColumnType::MYSQL_TYPE_JSON => "json".into(),
        ColumnType::MYSQL_TYPE_GEOMETRY => "geometry".into(),
        ColumnType::MYSQL_TYPE_VECTOR => format!("vector({})", len / 4),
        ColumnType::MYSQL_TYPE_ENUM => "enum".into(),
        ColumnType::MYSQL_TYPE_SET => "set".into(),
        ColumnType::MYSQL_TYPE_STRING => {
            if fl.contains(ColumnFlags::ENUM_FLAG) {
                "enum".into()
            } else if fl.contains(ColumnFlags::SET_FLAG) {
                "set".into()
            } else if binary {
                format!("binary({chars})")
            } else {
                format!("char({chars})")
            }
        }
        ColumnType::MYSQL_TYPE_VAR_STRING | ColumnType::MYSQL_TYPE_VARCHAR => {
            if binary {
                format!("varbinary({chars})")
            } else {
                format!("varchar({chars})")
            }
        }
        ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB => {
            let size = match chars {
                0..=255 => "tiny",
                256..=65_535 => "",
                65_536..=16_777_215 => "medium",
                _ => "long",
            };
            if binary {
                format!("{size}blob")
            } else {
                format!("{size}text")
            }
        }
        other => format!("{other:?}")
            .trim_start_matches("MYSQL_TYPE_")
            .to_ascii_lowercase(),
    }
}

/// Convert one value of a result row.
pub fn convert_value(decode: &Decode, v: MyValue) -> Value {
    match v {
        MyValue::NULL => Value::Null,
        MyValue::Bytes(b) => from_bytes(decode, b),
        MyValue::Int(i) => match decode {
            Decode::Bool => Value::Bool(i != 0),
            Decode::Decimal => Value::decimal(i.to_string()),
            Decode::Float | Decode::Double => Value::Float(i as f64),
            Decode::Text(_) | Decode::Json => Value::text(i.to_string()),
            _ => Value::Int(i),
        },
        MyValue::UInt(u) => match i64::try_from(u) {
            Ok(i) if !matches!(decode, Decode::UInt) => Value::Int(i),
            _ => Value::UInt(u),
        },
        MyValue::Float(f) => Value::Float(f32_to_f64(f)),
        MyValue::Double(d) => Value::Float(d),
        MyValue::Date(y, m, d, h, mi, s, us) => date_value(decode, y, m, d, h, mi, s, us),
        MyValue::Time(neg, days, h, m, s, us) => time_value(decode, neg, days, h, m, s, us),
    }
}

/// Convert a whole row (raw values as produced by `Row::unwrap_raw`).
pub fn convert_row(decoders: &[Decode], values: Vec<Option<MyValue>>) -> Vec<Value> {
    let mut out = Vec::with_capacity(values.len());
    for (i, v) in values.into_iter().enumerate() {
        let d = decoders.get(i).unwrap_or(&Decode::Bytes);
        out.push(match v {
            Some(v) => convert_value(d, v),
            None => Value::Null,
        });
    }
    out
}

/// `f32` → `f64` through the shortest decimal representation, so `0.1f32`
/// becomes `0.1` instead of `0.10000000149011612`.
fn f32_to_f64(f: f32) -> f64 {
    if !f.is_finite() {
        return f as f64;
    }
    format!("{f}").parse::<f64>().unwrap_or(f as f64)
}

fn bytes_to_text(cs: &Charset, b: Vec<u8>) -> Value {
    match decode_text(cs, b) {
        Ok(s) => Value::Text(s.into_boxed_str()),
        Err(b) => Value::Bytes(b.into_boxed_slice()),
    }
}

fn ascii(b: &[u8]) -> Option<&str> {
    std::str::from_utf8(b).ok()
}

fn from_bytes(decode: &Decode, b: Vec<u8>) -> Value {
    match decode {
        Decode::Null => Value::Null,
        Decode::Int => match ascii(&b).and_then(|s| s.parse::<i64>().ok()) {
            Some(i) => Value::Int(i),
            None => match ascii(&b).and_then(|s| s.parse::<u64>().ok()) {
                Some(u) => Value::UInt(u),
                None => bytes_to_text(&UTF8MB4, b),
            },
        },
        Decode::UInt => match ascii(&b).and_then(|s| s.parse::<u64>().ok()) {
            Some(u) => match i64::try_from(u) {
                Ok(i) => Value::Int(i),
                Err(_) => Value::UInt(u),
            },
            None => bytes_to_text(&UTF8MB4, b),
        },
        Decode::Year => match ascii(&b).and_then(|s| s.parse::<i64>().ok()) {
            Some(i) => Value::Int(i),
            None => bytes_to_text(&UTF8MB4, b),
        },
        Decode::Decimal => match String::from_utf8(b) {
            Ok(s) => Value::Decimal(s.into_boxed_str()),
            Err(e) => Value::Bytes(e.into_bytes().into_boxed_slice()),
        },
        Decode::Float | Decode::Double => match ascii(&b).and_then(|s| s.parse::<f64>().ok()) {
            Some(f) => Value::Float(f),
            None => bytes_to_text(&UTF8MB4, b),
        },
        Decode::Date => match parse_date(&b) {
            Some(d) => Value::Date(d),
            None => bytes_to_text(&UTF8MB4, b),
        },
        Decode::DateTime => match parse_datetime(&b) {
            Some(dt) => Value::DateTime(dt),
            None => bytes_to_text(&UTF8MB4, b),
        },
        Decode::Time(_) => match parse_time(&b) {
            Some(t) => Value::Time(t),
            None => bytes_to_text(&UTF8MB4, b),
        },
        Decode::Bool => Value::Bool(b.iter().any(|x| *x != 0)),
        Decode::Bits(n) => Value::text(bits_text(&b, *n)),
        Decode::Json => match String::from_utf8(b) {
            Ok(s) => Value::Json(s.into_boxed_str()),
            Err(e) => Value::Bytes(e.into_bytes().into_boxed_slice()),
        },
        Decode::Text(cs) => bytes_to_text(cs, b),
        Decode::Bytes => Value::Bytes(b.into_boxed_slice()),
        Decode::Geometry => match geometry::mysql_geometry_to_ewkt(&b) {
            Some(s) => Value::text(s),
            None => Value::Bytes(b.into_boxed_slice()),
        },
        Decode::Vector => {
            if b.len().is_multiple_of(4) {
                let items: Vec<Value> = b
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| Value::Float(f32_to_f64(f32::from_le_bytes([c[0], c[1], c[2], c[3]]))))
                    .collect();
                Value::Array(items.into_boxed_slice())
            } else {
                Value::Bytes(b.into_boxed_slice())
            }
        }
    }
}

fn bits_text(b: &[u8], n: u32) -> String {
    let total = b.len() * 8;
    let n = (n as usize).min(total.max(1));
    let mut s = String::with_capacity(n);
    for i in (total - n.min(total))..total {
        let byte = b[i / 8];
        let bit = (byte >> (7 - (i % 8))) & 1;
        s.push(if bit == 1 { '1' } else { '0' });
    }
    // fewer bytes than bits (should not happen): left pad
    while s.len() < n {
        s.insert(0, '0');
    }
    s
}

fn digits(b: &[u8]) -> Option<u32> {
    if b.is_empty() {
        return None;
    }
    let mut v: u32 = 0;
    for c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u32)?;
    }
    Some(v)
}

/// Fraction digits (`.123`) to nanoseconds.
fn fraction_nanos(b: &[u8]) -> Option<u32> {
    if b.is_empty() || b.len() > 9 {
        return None;
    }
    let v = digits(b)?;
    Some(v * 10u32.pow(9 - b.len() as u32))
}

/// Parse `YYYY-MM-DD`; `None` for zero / invalid dates.
pub fn parse_date(b: &[u8]) -> Option<NaiveDate> {
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y = digits(&b[0..4])?;
    let m = digits(&b[5..7])?;
    let d = digits(&b[8..10])?;
    if y == 0 {
        return None;
    }
    NaiveDate::from_ymd_opt(y as i32, m, d)
}

fn parse_hms(b: &[u8]) -> Option<(u32, u32, u32)> {
    if b.len() != 8 || b[2] != b':' || b[5] != b':' {
        return None;
    }
    Some((digits(&b[0..2])?, digits(&b[3..5])?, digits(&b[6..8])?))
}

/// Parse `YYYY-MM-DD HH:MM:SS[.ffffff]`.
pub fn parse_datetime(b: &[u8]) -> Option<NaiveDateTime> {
    if b.len() == 10 {
        return parse_date(b).and_then(|d| d.and_hms_opt(0, 0, 0));
    }
    if b.len() < 19 || (b[10] != b' ' && b[10] != b'T') {
        return None;
    }
    let date = parse_date(&b[..10])?;
    let (h, m, s) = parse_hms(&b[11..19])?;
    let nanos = if b.len() > 19 {
        if b[19] != b'.' {
            return None;
        }
        fraction_nanos(&b[20..])?
    } else {
        0
    };
    Some(NaiveDateTime::new(
        date,
        NaiveTime::from_hms_nano_opt(h, m, s, nanos)?,
    ))
}

/// Parse `HH:MM:SS[.ffffff]`; `None` for negative values or hours >= 24
/// (those are returned as text, see [`DataKind::Time`] note in the crate docs).
pub fn parse_time(b: &[u8]) -> Option<NaiveTime> {
    if b.first() == Some(&b'-') {
        return None;
    }
    let colon = b.iter().position(|c| *c == b':')?;
    let h = digits(&b[..colon])?;
    if h >= 24 {
        return None;
    }
    let rest = &b[colon + 1..];
    if rest.len() < 5 || rest[2] != b':' {
        return None;
    }
    let m = digits(&rest[0..2])?;
    let s = digits(&rest[3..5])?;
    let nanos = if rest.len() > 5 {
        if rest[5] != b'.' {
            return None;
        }
        fraction_nanos(&rest[6..])?
    } else {
        0
    };
    NaiveTime::from_hms_nano_opt(h, m, s, nanos)
}

#[allow(clippy::too_many_arguments)]
fn date_value(decode: &Decode, y: u16, m: u8, d: u8, h: u8, mi: u8, s: u8, us: u32) -> Value {
    let date = if y == 0 {
        None
    } else {
        NaiveDate::from_ymd_opt(y as i32, m as u32, d as u32)
    };
    let text = || {
        if matches!(decode, Decode::Date) {
            format!("{y:04}-{m:02}-{d:02}")
        } else if us > 0 {
            format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}")
        } else {
            format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
        }
    };
    match (decode, date) {
        (Decode::Date, Some(dt)) => Value::Date(dt),
        (_, Some(dt)) => match NaiveTime::from_hms_micro_opt(h as u32, mi as u32, s as u32, us) {
            Some(t) => {
                if matches!(decode, Decode::DateTime) || h != 0 || mi != 0 || s != 0 || us != 0 {
                    Value::DateTime(NaiveDateTime::new(dt, t))
                } else {
                    Value::Date(dt)
                }
            }
            None => Value::text(text()),
        },
        (_, None) => Value::text(text()),
    }
}

fn time_value(decode: &Decode, neg: bool, days: u32, h: u8, m: u8, s: u8, us: u32) -> Value {
    if !neg
        && days == 0
        && let Some(t) = NaiveTime::from_hms_micro_opt(h as u32, m as u32, s as u32, us)
    {
        return Value::Time(t);
    }
    let hours = days as u64 * 24 + h as u64;
    let fsp = match decode {
        Decode::Time(f) => *f as usize,
        _ => 6,
    };
    let mut out = format!("{}{hours:02}:{m:02}:{s:02}", if neg { "-" } else { "" });
    if fsp > 0 {
        let frac = format!("{us:06}");
        out.push('.');
        out.push_str(&frac[..fsp.min(6)]);
    } else if us > 0 {
        out.push_str(&format!(".{us:06}"));
    }
    Value::text(out)
}

/// Convert a DBiji value into a statement parameter.
pub fn to_param(v: &Value, cs: &Charset) -> DbResult<MyValue> {
    Ok(match v {
        Value::Null => MyValue::NULL,
        Value::Bool(b) => MyValue::Int(*b as i64),
        Value::Int(i) => MyValue::Int(*i),
        Value::UInt(u) => MyValue::UInt(*u),
        Value::Float(f) => MyValue::Double(*f),
        Value::Decimal(s) | Value::Text(s) | Value::Json(s) => MyValue::Bytes(encode_text(cs, s)?),
        Value::Bytes(b) => MyValue::Bytes(b.to_vec()),
        Value::Date(d) => {
            let y = checked_year(d.year())?;
            MyValue::Date(y, d.month() as u8, d.day() as u8, 0, 0, 0, 0)
        }
        Value::Time(t) => MyValue::Time(
            false,
            0,
            t.hour() as u8,
            t.minute() as u8,
            t.second() as u8,
            (t.nanosecond() / 1000).min(999_999),
        ),
        Value::DateTime(dt) => datetime_param(dt)?,
        Value::DateTimeTz(dt) => datetime_param(&dt.naive_utc())?,
        Value::Uuid(u) => MyValue::Bytes(u.hyphenated().to_string().into_bytes()),
        Value::Array(_) => MyValue::Bytes(encode_text(cs, &v.to_json().to_string())?),
    })
}

fn checked_year(y: i32) -> DbResult<u16> {
    if (0..=9999).contains(&y) {
        Ok(y as u16)
    } else {
        Err(DbError::query(format!(
            "Year {y} is outside the range supported by MySQL (0-9999)"
        )))
    }
}

fn datetime_param(dt: &NaiveDateTime) -> DbResult<MyValue> {
    let y = checked_year(dt.year())?;
    Ok(MyValue::Date(
        y,
        dt.month() as u8,
        dt.day() as u8,
        dt.hour() as u8,
        dt.minute() as u8,
        dt.second() as u8,
        (dt.nanosecond() / 1000).min(999_999),
    ))
}

/// Read helpers for internal (catalog) queries.
pub mod read {
    use mysql_async::{Row, Value as MyValue};

    /// Column `i` as text (numbers are formatted).
    pub fn text(row: &Row, i: usize) -> Option<String> {
        match row.as_ref(i)? {
            MyValue::NULL => None,
            MyValue::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            MyValue::Int(v) => Some(v.to_string()),
            MyValue::UInt(v) => Some(v.to_string()),
            MyValue::Float(v) => Some(v.to_string()),
            MyValue::Double(v) => Some(v.to_string()),
            other @ (MyValue::Date(..) | MyValue::Time(..)) => {
                let s = other.as_sql(true);
                Some(s.trim_matches('\'').to_string())
            }
        }
    }

    /// Column `i` as text, empty string for NULL.
    pub fn text_or_empty(row: &Row, i: usize) -> String {
        text(row, i).unwrap_or_default()
    }

    /// Column `i` as a non-empty string.
    pub fn non_empty(row: &Row, i: usize) -> Option<String> {
        text(row, i).filter(|s| !s.is_empty())
    }

    /// Column `i` as integer.
    pub fn int(row: &Row, i: usize) -> Option<i64> {
        match row.as_ref(i)? {
            MyValue::Int(v) => Some(*v),
            MyValue::UInt(v) => i64::try_from(*v).ok(),
            MyValue::Float(v) => Some(*v as i64),
            MyValue::Double(v) => Some(*v as i64),
            MyValue::Bytes(b) => {
                let s = std::str::from_utf8(b).ok()?.trim();
                s.parse::<i64>()
                    .ok()
                    .or_else(|| s.parse::<f64>().ok().map(|f| f as i64))
            }
            _ => None,
        }
    }

    /// Column `i` as float.
    pub fn float(row: &Row, i: usize) -> Option<f64> {
        match row.as_ref(i)? {
            MyValue::Int(v) => Some(*v as f64),
            MyValue::UInt(v) => Some(*v as f64),
            MyValue::Float(v) => Some(*v as f64),
            MyValue::Double(v) => Some(*v),
            MyValue::Bytes(b) => std::str::from_utf8(b).ok()?.trim().parse().ok(),
            _ => None,
        }
    }

    /// Column `i` as boolean (`YES`/`NO`, `1`/`0`).
    pub fn flag(row: &Row, i: usize) -> bool {
        match text(row, i) {
            Some(s) => matches!(
                s.to_ascii_uppercase().as_str(),
                "YES" | "1" | "Y" | "TRUE" | "ON" | "ENABLED"
            ),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(ty: ColumnType) -> Column {
        Column::new(ty).with_name(b"c").with_character_set(255)
    }

    #[test]
    fn integer_columns() {
        let c = col(ColumnType::MYSQL_TYPE_LONG)
            .with_flags(ColumnFlags::UNSIGNED_FLAG | ColumnFlags::NOT_NULL_FLAG)
            .with_column_length(10);
        let (m, d) = column_meta(&c);
        assert_eq!(m.type_name, "int unsigned");
        assert_eq!(m.kind, DataKind::Int);
        assert_eq!(m.nullable, Some(false));
        assert_eq!(
            convert_value(&d, MyValue::Bytes(b"42".to_vec())),
            Value::Int(42)
        );
        assert_eq!(convert_value(&d, MyValue::Int(7)), Value::Int(7));

        let c = col(ColumnType::MYSQL_TYPE_LONGLONG)
            .with_flags(ColumnFlags::UNSIGNED_FLAG)
            .with_column_length(20);
        let (m, d) = column_meta(&c);
        assert_eq!(m.type_name, "bigint unsigned");
        assert_eq!(
            convert_value(&d, MyValue::Bytes(b"18446744073709551615".to_vec())),
            Value::UInt(u64::MAX)
        );
        assert_eq!(
            convert_value(&d, MyValue::Bytes(b"5".to_vec())),
            Value::Int(5)
        );
        assert_eq!(
            convert_value(&d, MyValue::UInt(u64::MAX)),
            Value::UInt(u64::MAX)
        );

        let c = col(ColumnType::MYSQL_TYPE_TINY)
            .with_column_length(1)
            .with_flags(ColumnFlags::AUTO_INCREMENT_FLAG);
        let (m, _) = column_meta(&c);
        assert_eq!(m.type_name, "tinyint(1)");
        assert_eq!(m.auto_increment, Some(true));

        let c = col(ColumnType::MYSQL_TYPE_YEAR).with_column_length(4);
        let (m, d) = column_meta(&c);
        assert_eq!(m.type_name, "year");
        assert_eq!(
            convert_value(&d, MyValue::Bytes(b"2024".to_vec())),
            Value::Int(2024)
        );
    }

    #[test]
    fn decimal_and_float() {
        let c = col(ColumnType::MYSQL_TYPE_NEWDECIMAL)
            .with_column_length(12)
            .with_decimals(2);
        let (m, d) = column_meta(&c);
        assert_eq!(m.type_name, "decimal(10,2)");
        assert_eq!((m.precision, m.scale), (Some(10), Some(2)));
        assert_eq!(
            convert_value(&d, MyValue::Bytes(b"-12.50".to_vec())),
            Value::decimal("-12.50")
        );
        let c = col(ColumnType::MYSQL_TYPE_FLOAT).with_decimals(31);
        let (_, d) = column_meta(&c);
        assert_eq!(convert_value(&d, MyValue::Float(0.1)), Value::Float(0.1));
        assert_eq!(
            convert_value(&d, MyValue::Bytes(b"1.5".to_vec())),
            Value::Float(1.5)
        );
    }

    #[test]
    fn strings_and_binary() {
        let c = col(ColumnType::MYSQL_TYPE_VAR_STRING).with_column_length(1020);
        let (m, d) = column_meta(&c);
        assert_eq!(m.type_name, "varchar(255)");
        assert_eq!(m.max_length, Some(255));
        assert_eq!(
            convert_value(&d, MyValue::Bytes("héllo".as_bytes().to_vec())),
            Value::text("héllo")
        );

        let c = col(ColumnType::MYSQL_TYPE_VAR_STRING)
            .with_column_length(16)
            .with_character_set(63);
        let (m, d) = column_meta(&c);
        assert_eq!(m.type_name, "varbinary(16)");
        assert_eq!(m.kind, DataKind::Binary);
        assert_eq!(
            convert_value(&d, MyValue::Bytes(vec![0, 255])),
            Value::bytes(vec![0u8, 255])
        );

        let c = col(ColumnType::MYSQL_TYPE_BLOB)
            .with_column_length(4_294_967_295)
            .with_character_set(255);
        assert_eq!(type_name(&c), "longtext");
        let c = col(ColumnType::MYSQL_TYPE_BLOB)
            .with_column_length(65_535)
            .with_character_set(63);
        assert_eq!(type_name(&c), "blob");
        let c = col(ColumnType::MYSQL_TYPE_BLOB)
            .with_column_length(262_140)
            .with_character_set(255);
        assert_eq!(type_name(&c), "text");

        let c = col(ColumnType::MYSQL_TYPE_STRING)
            .with_flags(ColumnFlags::ENUM_FLAG)
            .with_column_length(4);
        let (m, d) = column_meta(&c);
        assert_eq!(m.type_name, "enum");
        assert_eq!(m.kind, DataKind::Enum);
        assert_eq!(
            convert_value(&d, MyValue::Bytes(b"a".to_vec())),
            Value::text("a")
        );

        // latin1 result character set
        let c = col(ColumnType::MYSQL_TYPE_VAR_STRING)
            .with_column_length(10)
            .with_character_set(8);
        let (_, d) = column_meta(&c);
        assert_eq!(
            convert_value(&d, MyValue::Bytes(vec![b'c', 0xE9])),
            Value::text("cé")
        );
        // invalid UTF-8 in a utf8mb4 column stays binary
        let c = col(ColumnType::MYSQL_TYPE_VAR_STRING).with_column_length(10);
        let (_, d) = column_meta(&c);
        assert_eq!(
            convert_value(&d, MyValue::Bytes(vec![0xFF])),
            Value::bytes(vec![0xFFu8])
        );
    }

    #[test]
    fn temporal_text() {
        let c = col(ColumnType::MYSQL_TYPE_DATETIME).with_decimals(6);
        let (m, d) = column_meta(&c);
        assert_eq!(m.type_name, "datetime(6)");
        let v = convert_value(&d, MyValue::Bytes(b"2024-02-29 13:14:15.123456".to_vec()));
        assert_eq!(v.to_text(), "2024-02-29 13:14:15.123456");
        assert_eq!(
            convert_value(&d, MyValue::Bytes(b"0000-00-00 00:00:00".to_vec())),
            Value::text("0000-00-00 00:00:00")
        );
        let c = col(ColumnType::MYSQL_TYPE_DATE);
        let (_, d) = column_meta(&c);
        assert_eq!(
            convert_value(&d, MyValue::Bytes(b"2024-01-02".to_vec())),
            Value::Date(NaiveDate::from_ymd_opt(2024, 1, 2).unwrap())
        );
        assert_eq!(
            convert_value(&d, MyValue::Bytes(b"2024-00-10".to_vec())),
            Value::text("2024-00-10")
        );
        let c = col(ColumnType::MYSQL_TYPE_TIME);
        let (m, d) = column_meta(&c);
        assert_eq!(m.kind, DataKind::Time);
        assert_eq!(
            convert_value(&d, MyValue::Bytes(b"12:30:01.5".to_vec())),
            Value::Time(NaiveTime::from_hms_milli_opt(12, 30, 1, 500).unwrap())
        );
        assert_eq!(
            convert_value(&d, MyValue::Bytes(b"-01:00:00".to_vec())),
            Value::text("-01:00:00")
        );
        assert_eq!(
            convert_value(&d, MyValue::Bytes(b"838:59:59".to_vec())),
            Value::text("838:59:59")
        );
    }

    #[test]
    fn temporal_binary() {
        let (_, d) = column_meta(&col(ColumnType::MYSQL_TYPE_DATETIME));
        assert_eq!(
            convert_value(&d, MyValue::Date(2024, 1, 2, 3, 4, 5, 6)).to_text(),
            "2024-01-02 03:04:05.000006"
        );
        assert_eq!(
            convert_value(&d, MyValue::Date(0, 0, 0, 0, 0, 0, 0)),
            Value::text("0000-00-00 00:00:00")
        );
        let (_, d) = column_meta(&col(ColumnType::MYSQL_TYPE_DATE));
        assert_eq!(
            convert_value(&d, MyValue::Date(2024, 1, 2, 0, 0, 0, 0)).to_text(),
            "2024-01-02"
        );
        let (_, d) = column_meta(&col(ColumnType::MYSQL_TYPE_TIME).with_decimals(0));
        assert_eq!(
            convert_value(&d, MyValue::Time(false, 0, 1, 2, 3, 0)).to_text(),
            "01:02:03"
        );
        assert_eq!(
            convert_value(&d, MyValue::Time(true, 1, 1, 0, 0, 0)),
            Value::text("-25:00:00")
        );
        assert_eq!(
            convert_value(&d, MyValue::Time(false, 34, 22, 59, 59, 0)),
            Value::text("838:59:59")
        );
        let (_, d) = column_meta(&col(ColumnType::MYSQL_TYPE_TIME).with_decimals(3));
        assert_eq!(
            convert_value(&d, MyValue::Time(true, 0, 1, 0, 0, 500_000)),
            Value::text("-01:00:00.500")
        );
    }

    #[test]
    fn bits_json_geometry() {
        let (m, d) = column_meta(
            &col(ColumnType::MYSQL_TYPE_BIT)
                .with_column_length(1)
                .with_character_set(63),
        );
        assert_eq!(m.kind, DataKind::Bool);
        assert_eq!(
            convert_value(&d, MyValue::Bytes(vec![1])),
            Value::Bool(true)
        );
        assert_eq!(
            convert_value(&d, MyValue::Bytes(vec![0])),
            Value::Bool(false)
        );
        let (m, d) = column_meta(
            &col(ColumnType::MYSQL_TYPE_BIT)
                .with_column_length(10)
                .with_character_set(63),
        );
        assert_eq!(m.type_name, "bit(10)");
        assert_eq!(m.kind, DataKind::Bit);
        assert_eq!(
            convert_value(&d, MyValue::Bytes(vec![0b10, 0b0000_0101])),
            Value::text("1000000101")
        );
        let (m, d) = column_meta(&col(ColumnType::MYSQL_TYPE_JSON));
        assert_eq!(m.kind, DataKind::Json);
        assert_eq!(
            convert_value(&d, MyValue::Bytes(br#"{"a": 1}"#.to_vec())),
            Value::json(r#"{"a": 1}"#)
        );
        let (m, d) = column_meta(&col(ColumnType::MYSQL_TYPE_GEOMETRY).with_character_set(63));
        assert_eq!(m.kind, DataKind::Geometry);
        let mut g = 4326u32.to_le_bytes().to_vec();
        g.push(1);
        g.extend_from_slice(&1u32.to_le_bytes());
        g.extend_from_slice(&1.0f64.to_le_bytes());
        g.extend_from_slice(&2.0f64.to_le_bytes());
        assert_eq!(
            convert_value(&d, MyValue::Bytes(g)),
            Value::text("SRID=4326;POINT(1 2)")
        );
    }

    #[test]
    fn sources() {
        let c = Column::new(ColumnType::MYSQL_TYPE_LONG)
            .with_schema(b"shop")
            .with_table(b"o")
            .with_org_table(b"orders")
            .with_name(b"order_id")
            .with_org_name(b"id");
        let (m, _) = column_meta(&c);
        let s = m.source.unwrap();
        assert_eq!(
            (s.schema.as_deref(), s.table.as_str(), s.column.as_str()),
            (Some("shop"), "orders", "id")
        );
        assert_eq!(m.name, "order_id");
        let (m, _) = column_meta(&Column::new(ColumnType::MYSQL_TYPE_LONG).with_name(b"x"));
        assert!(m.source.is_none());
    }

    #[test]
    fn params() {
        let cs = UTF8MB4;
        assert_eq!(to_param(&Value::Bool(true), &cs).unwrap(), MyValue::Int(1));
        assert_eq!(
            to_param(&Value::text("x"), &cs).unwrap(),
            MyValue::Bytes(b"x".to_vec())
        );
        let dt = NaiveDate::from_ymd_opt(2024, 1, 2)
            .unwrap()
            .and_hms_micro_opt(3, 4, 5, 6)
            .unwrap();
        assert_eq!(
            to_param(&Value::DateTime(dt), &cs).unwrap(),
            MyValue::Date(2024, 1, 2, 3, 4, 5, 6)
        );
        let latin1 = charset_by_name("latin1").unwrap();
        assert_eq!(
            to_param(&Value::text("é"), &latin1).unwrap(),
            MyValue::Bytes(vec![0xE9])
        );
        assert!(to_param(&Value::text("日本"), &latin1).is_err());
        let arr = Value::Array(vec![Value::Int(1), Value::text("a")].into_boxed_slice());
        assert_eq!(
            to_param(&arr, &cs).unwrap(),
            MyValue::Bytes(br#"[1,"a"]"#.to_vec())
        );
    }

    #[test]
    fn charsets() {
        assert_eq!(charset_for_collation(255).name, "utf8mb4");
        assert_eq!(charset_for_collation(33).mbmaxlen, 3);
        assert_eq!(charset_for_collation(8).name, "latin1");
        assert_eq!(charset_for_collation(2304).name, "utf8mb4");
        let utf32 = charset_for_collation(60);
        assert_eq!(
            decode_text(&utf32, vec![0, 0, 0, b'A', 0, 0, 0x30, 0x42]).unwrap(),
            "A\u{3042}"
        );
        assert!(charset_by_name("utf8mb4").unwrap().is_utf8());
        assert!(charset_by_name("klingon").is_none());
    }
}
