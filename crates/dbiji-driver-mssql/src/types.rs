//! SQL Server column types, value conversion (`ColumnData` → [`Value`]) and
//! parameter binding ([`Value`] → RPC parameters of `sp_executesql`).

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use dbiji_core::value::{ColumnMeta, DataKind, Value, is_decimal_text};
use dbiji_core::{DbError, DbResult};
use std::borrow::Cow;
use tiberius::numeric::Numeric;
use tiberius::{ColumnData, ColumnType, Command};

/// Type name and logical kind of a result column type.
pub fn column_type_info(ct: ColumnType) -> (&'static str, DataKind) {
    match ct {
        ColumnType::Null => ("null", DataKind::Other),
        ColumnType::Bit | ColumnType::Bitn => ("bit", DataKind::Bool),
        ColumnType::Int1 => ("tinyint", DataKind::Int),
        ColumnType::Int2 => ("smallint", DataKind::Int),
        ColumnType::Int4 | ColumnType::Intn => ("int", DataKind::Int),
        ColumnType::Int8 => ("bigint", DataKind::Int),
        ColumnType::Float4 => ("real", DataKind::Float),
        ColumnType::Float8 | ColumnType::Floatn => ("float", DataKind::Float),
        ColumnType::Money => ("money", DataKind::Decimal),
        ColumnType::Money4 => ("smallmoney", DataKind::Decimal),
        ColumnType::Decimaln => ("decimal", DataKind::Decimal),
        ColumnType::Numericn => ("numeric", DataKind::Decimal),
        ColumnType::Datetime4 => ("smalldatetime", DataKind::DateTime),
        ColumnType::Datetime | ColumnType::Datetimen => ("datetime", DataKind::DateTime),
        ColumnType::Daten => ("date", DataKind::Date),
        ColumnType::Timen => ("time", DataKind::Time),
        ColumnType::Datetime2 => ("datetime2", DataKind::DateTime),
        ColumnType::DatetimeOffsetn => ("datetimeoffset", DataKind::DateTimeTz),
        ColumnType::Guid => ("uniqueidentifier", DataKind::Uuid),
        ColumnType::BigVarBin => ("varbinary", DataKind::Binary),
        ColumnType::BigBinary => ("binary", DataKind::Binary),
        ColumnType::Image => ("image", DataKind::Binary),
        ColumnType::BigVarChar => ("varchar", DataKind::Text),
        ColumnType::BigChar => ("char", DataKind::Text),
        ColumnType::NVarchar => ("nvarchar", DataKind::Text),
        ColumnType::NChar => ("nchar", DataKind::Text),
        ColumnType::Text => ("text", DataKind::Text),
        ColumnType::NText => ("ntext", DataKind::Text),
        ColumnType::Xml => ("xml", DataKind::Xml),
        // CLR types (geography, geometry, hierarchyid): raw serialized bytes
        ColumnType::Udt => ("udt", DataKind::Binary),
        ColumnType::SSVariant => ("sql_variant", DataKind::Text),
    }
}

/// Result column description from tiberius metadata.
pub fn column_meta(col: &tiberius::Column) -> ColumnMeta {
    let (name, kind) = column_type_info(col.column_type());
    let mut m = ColumnMeta::new(col.name(), name, kind);
    match col.column_type() {
        ColumnType::Bit | ColumnType::Int1 | ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 => {
            m.nullable = Some(false)
        }
        ColumnType::Money => {
            m.precision = Some(19);
            m.scale = Some(4);
        }
        ColumnType::Money4 => {
            m.precision = Some(10);
            m.scale = Some(4);
        }
        ColumnType::Guid => m.max_length = Some(36),
        _ => {}
    }
    m
}

/// Logical kind of a SQL Server type name (`nvarchar(50)`, `decimal(10,2)` ...).
pub fn kind_of_type_name(type_name: &str) -> DataKind {
    let lower = type_name.trim().to_ascii_lowercase();
    let base = lower.split(['(', ' ']).next().unwrap_or("").trim_matches(|c| c == '[' || c == ']');
    match base {
        "bit" => DataKind::Bool,
        "tinyint" | "smallint" | "int" | "bigint" => DataKind::Int,
        "real" | "float" => DataKind::Float,
        "decimal" | "numeric" | "money" | "smallmoney" => DataKind::Decimal,
        "char" | "varchar" | "nchar" | "nvarchar" | "text" | "ntext" | "sysname" | "sql_variant" => DataKind::Text,
        "binary" | "varbinary" | "image" | "timestamp" | "rowversion" => DataKind::Binary,
        "date" => DataKind::Date,
        "time" => DataKind::Time,
        "datetime" | "datetime2" | "smalldatetime" => DataKind::DateTime,
        "datetimeoffset" => DataKind::DateTimeTz,
        "uniqueidentifier" => DataKind::Uuid,
        "xml" => DataKind::Xml,
        "json" => DataKind::Json,
        "geography" | "geometry" => DataKind::Geometry,
        _ => DataKind::guess_from_type_name(type_name),
    }
}

// ------------------------------------------------------------ values

fn base_1900() -> NaiveDate {
    NaiveDate::from_ymd_opt(1900, 1, 1).unwrap_or_default()
}

fn base_0001() -> NaiveDate {
    NaiveDate::from_ymd_opt(1, 1, 1).unwrap_or_default()
}

fn add_days(base: NaiveDate, days: i64) -> Option<NaiveDate> {
    base.checked_add_signed(Duration::days(days))
}

/// `time(n)` increments → time of day.
fn time_from_increments(increments: u64, scale: u8) -> Option<NaiveTime> {
    let scale = u32::from(scale.min(9));
    let nanos = (increments as u128).checked_mul(10u128.pow(9 - scale))?;
    let secs = u32::try_from(nanos / 1_000_000_000).ok()?;
    let frac = u32::try_from(nanos % 1_000_000_000).ok()?;
    NaiveTime::from_num_seconds_from_midnight_opt(secs, frac)
}

/// `datetime`: days since 1900-01-01 and 1/300 s ticks (rounded to the
/// millisecond values SQL Server displays: .000, .003, .007).
pub fn datetime_value(days: i32, fragments: u32) -> Option<NaiveDateTime> {
    let date = add_days(base_1900(), i64::from(days))?;
    let ms = (u64::from(fragments) * 10 + 1) / 3; // round(f * 1000 / 300)
    let secs = u32::try_from(ms / 1000).ok()?;
    let millis = u32::try_from(ms % 1000).ok()?;
    let time = NaiveTime::from_num_seconds_from_midnight_opt(secs.min(86_399), millis * 1_000_000)?;
    Some(NaiveDateTime::new(date, time))
}

fn numeric_text(n: Numeric) -> String {
    let value = n.value();
    let scale = n.scale() as usize;
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let mut s = String::with_capacity(digits.len() + 3);
    if negative {
        s.push('-');
    }
    if scale == 0 {
        s.push_str(&digits);
    } else if digits.len() <= scale {
        s.push_str("0.");
        for _ in 0..(scale - digits.len()) {
            s.push('0');
        }
        s.push_str(&digits);
    } else {
        let (int_part, frac) = digits.split_at(digits.len() - scale);
        s.push_str(int_part);
        s.push('.');
        s.push_str(frac);
    }
    s
}

/// Convert one cell. `ct` is the declared column type (used for `money`,
/// which tiberius decodes as `f64`, and for `sql_variant`).
pub fn convert(data: ColumnData<'static>, ct: ColumnType) -> Value {
    let v = convert_raw(data, ct);
    if ct == ColumnType::SSVariant {
        return match v {
            Value::Null | Value::Text(_) => v,
            other => Value::text(other.to_text().into_owned()),
        };
    }
    v
}

fn convert_raw(data: ColumnData<'static>, ct: ColumnType) -> Value {
    match data {
        ColumnData::U8(v) => v.map(|x| Value::Int(i64::from(x))).unwrap_or(Value::Null),
        ColumnData::I16(v) => v.map(|x| Value::Int(i64::from(x))).unwrap_or(Value::Null),
        ColumnData::I32(v) => v.map(|x| Value::Int(i64::from(x))).unwrap_or(Value::Null),
        ColumnData::I64(v) => v.map(Value::Int).unwrap_or(Value::Null),
        ColumnData::F32(v) => v.map(|x| Value::Float(f64::from(x))).unwrap_or(Value::Null),
        ColumnData::F64(v) => match v {
            None => Value::Null,
            // tiberius decodes money as f64: render it with its fixed scale
            Some(x) if matches!(ct, ColumnType::Money | ColumnType::Money4) => Value::decimal(format!("{x:.4}")),
            Some(x) => Value::Float(x),
        },
        ColumnData::Bit(v) => v.map(Value::Bool).unwrap_or(Value::Null),
        ColumnData::String(v) => v.map(|s| Value::Text(s.into_owned().into_boxed_str())).unwrap_or(Value::Null),
        ColumnData::Guid(v) => v.map(Value::Uuid).unwrap_or(Value::Null),
        ColumnData::Binary(v) => v.map(|b| Value::Bytes(b.into_owned().into_boxed_slice())).unwrap_or(Value::Null),
        ColumnData::Numeric(v) => v.map(|n| Value::decimal(numeric_text(n))).unwrap_or(Value::Null),
        ColumnData::Xml(v) => {
            v.map(|x| Value::Text(x.into_owned().into_string().into_boxed_str())).unwrap_or(Value::Null)
        }
        ColumnData::DateTime(v) => v
            .and_then(|d| datetime_value(d.days(), d.seconds_fragments()))
            .map(Value::DateTime)
            .unwrap_or(Value::Null),
        ColumnData::SmallDateTime(v) => v
            .and_then(|d| {
                let date = add_days(base_1900(), i64::from(d.days()))?;
                let time = NaiveTime::from_num_seconds_from_midnight_opt(u32::from(d.seconds_fragments()) * 60, 0)?;
                Some(NaiveDateTime::new(date, time))
            })
            .map(Value::DateTime)
            .unwrap_or(Value::Null),
        ColumnData::Time(v) => v
            .and_then(|t| time_from_increments(t.increments(), t.scale()))
            .map(Value::Time)
            .unwrap_or(Value::Null),
        ColumnData::Date(v) => v
            .and_then(|d| add_days(base_0001(), i64::from(d.days())))
            .map(Value::Date)
            .unwrap_or(Value::Null),
        ColumnData::DateTime2(v) => v
            .and_then(|dt| {
                let date = add_days(base_0001(), i64::from(dt.date().days()))?;
                let time = time_from_increments(dt.time().increments(), dt.time().scale())?;
                Some(NaiveDateTime::new(date, time))
            })
            .map(Value::DateTime)
            .unwrap_or(Value::Null),
        ColumnData::DateTimeOffset(v) => v
            .and_then(|dto| {
                // the date/time part is UTC; the offset gives the local zone
                let dt2 = dto.datetime2();
                let date = add_days(base_0001(), i64::from(dt2.date().days()))?;
                let time = time_from_increments(dt2.time().increments(), dt2.time().scale())?;
                let offset = FixedOffset::east_opt(i32::from(dto.offset()) * 60)?;
                Some(DateTime::<FixedOffset>::from_naive_utc_and_offset(NaiveDateTime::new(date, time), offset))
            })
            .map(Value::DateTimeTz)
            .unwrap_or(Value::Null),
    }
}

// ------------------------------------------------------------ parameters

/// Parse decimal text into a tiberius `Numeric` (at most 38 digits).
pub fn parse_numeric(text: &str) -> Option<Numeric> {
    let t = text.trim();
    if !is_decimal_text(t) || t.contains(['e', 'E']) || t.eq_ignore_ascii_case("nan") || t.to_ascii_lowercase().contains("inf") {
        return None;
    }
    let (negative, digits) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (int_part, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let int_part = int_part.trim_start_matches('0');
    if int_part.len() + frac.len() > 38 || frac.len() > usize::from(Numeric::MAX_NUMERIC_SCALE) {
        return None;
    }
    let all = format!("{int_part}{frac}");
    let mut value: i128 = if all.is_empty() { 0 } else { all.parse().ok()? };
    if negative {
        value = -value;
    }
    let scale = u8::try_from(frac.len()).ok()?;
    Some(Numeric::new_with_scale(value, scale))
}

/// Declared type of a numeric parameter (same rule tiberius uses).
fn numeric_decl(n: Numeric) -> String {
    format!("numeric({},{})", n.precision(), n.scale())
}

fn string_decl(s: &str) -> &'static str {
    if s.len() <= 4000 { "nvarchar(4000)" } else { "nvarchar(max)" }
}

/// Declared SQL type of a parameter value (mirrors the RPC encoding tiberius
/// uses for the bound value).
pub fn param_decl(index: usize, v: &Value) -> DbResult<String> {
    Ok(match v {
        Value::Null => "nvarchar(4000)".into(),
        Value::Bool(_) => "bit".into(),
        Value::Int(_) => "bigint".into(),
        Value::UInt(u) => match i64::try_from(*u) {
            Ok(_) => "bigint".into(),
            Err(_) => numeric_decl(Numeric::new_with_scale(i128::from(*u), 0)),
        },
        Value::Float(f) => {
            if !f.is_finite() {
                return Err(DbError::config(format!("Parameter @P{index}: SQL Server float cannot store {f}")));
            }
            "float(53)".into()
        }
        Value::Decimal(s) => match parse_numeric(s) {
            Some(n) => numeric_decl(n),
            None => string_decl(s).into(),
        },
        Value::Text(s) | Value::Json(s) => string_decl(s).into(),
        Value::Bytes(b) => if b.len() <= 8000 { "varbinary(8000)" } else { "varbinary(max)" }.into(),
        Value::Date(_) => "date".into(),
        Value::Time(_) => "time".into(),
        Value::DateTime(_) => "datetime2".into(),
        Value::DateTimeTz(_) => "datetimeoffset".into(),
        Value::Uuid(_) => "uniqueidentifier".into(),
        Value::Array(_) => string_decl(&v.to_text()).into(),
    })
}

/// `@P1 bigint, @P2 nvarchar(4000), ...` for `values`.
pub fn param_declarations(values: &[Value]) -> DbResult<String> {
    let mut decls = Vec::with_capacity(values.len());
    for (i, v) in values.iter().enumerate() {
        decls.push(format!("@P{} {}", i + 1, param_decl(i + 1, v)?));
    }
    Ok(decls.join(", "))
}

fn bind_value(cmd: &mut Command<'static>, name: String, v: &Value) {
    match v {
        Value::Null => cmd.bind_param(name, Option::<String>::None),
        Value::Bool(b) => cmd.bind_param(name, *b),
        Value::Int(n) => cmd.bind_param(name, *n),
        Value::UInt(u) => match i64::try_from(*u) {
            Ok(n) => cmd.bind_param(name, n),
            Err(_) => cmd.bind_param(name, Numeric::new_with_scale(i128::from(*u), 0)),
        },
        Value::Float(f) => cmd.bind_param(name, *f),
        Value::Decimal(s) => match parse_numeric(s) {
            Some(n) => cmd.bind_param(name, n),
            None => cmd.bind_param(name, s.to_string()),
        },
        Value::Text(s) | Value::Json(s) => cmd.bind_param(name, s.to_string()),
        Value::Bytes(b) => cmd.bind_param(name, b.to_vec()),
        Value::Date(d) => cmd.bind_param(name, *d),
        Value::Time(t) => cmd.bind_param(name, *t),
        Value::DateTime(dt) => cmd.bind_param(name, *dt),
        Value::DateTimeTz(dt) => cmd.bind_param(name, *dt),
        Value::Uuid(u) => cmd.bind_param(name, *u),
        Value::Array(_) => cmd.bind_param(name, v.to_text().into_owned()),
    }
}

/// Build the `sp_executesql` RPC for `sql` with positional `@P1..@Pn`
/// parameters.
pub fn executesql_command(sql: &str, values: &[Value]) -> DbResult<Command<'static>> {
    let decls = param_declarations(values)?;
    let mut cmd = Command::new("sp_executesql");
    cmd.bind_param("@stmt", sql.to_string());
    if !values.is_empty() {
        cmd.bind_param("@params", decls);
        for (i, v) in values.iter().enumerate() {
            bind_value(&mut cmd, format!("@P{}", i + 1), v);
        }
    }
    Ok(cmd)
}

/// `Cow` helper for statement text passed to tiberius.
pub fn owned_sql(sql: &str) -> Cow<'static, str> {
    Cow::Owned(sql.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiberius::time::{Date, DateTime2, DateTimeOffset, Time};

    #[test]
    fn numeric_rendering() {
        assert_eq!(numeric_text(Numeric::new_with_scale(12345, 2)), "123.45");
        assert_eq!(numeric_text(Numeric::new_with_scale(-5, 3)), "-0.005");
        assert_eq!(numeric_text(Numeric::new_with_scale(42, 0)), "42");
        assert_eq!(numeric_text(Numeric::new_with_scale(0, 2)), "0.00");
        assert_eq!(
            numeric_text(Numeric::new_with_scale(99_999_999_999_999_999_999_999_999_999_999_999_999, 0)),
            "99999999999999999999999999999999999999"
        );
    }

    #[test]
    fn parse_numeric_text() {
        let n = parse_numeric("-123.450").unwrap();
        assert_eq!((n.value(), n.scale()), (-123_450, 3));
        assert_eq!(parse_numeric("0.5").unwrap().precision(), 2);
        assert!(parse_numeric("1e5").is_none());
        assert!(parse_numeric("abc").is_none());
        assert!(parse_numeric(&"9".repeat(39)).is_none());
        assert_eq!(parse_numeric("+7").unwrap().value(), 7);
    }

    #[test]
    fn temporal_conversions() {
        // datetime: 1900-01-01 + 45000 days, 1 tick = 3.33 ms -> .003
        let v = convert(ColumnData::DateTime(Some(tiberius::time::DateTime::new(45_000, 1))), ColumnType::Datetimen);
        assert_eq!(v.to_text(), "2023-03-17 00:00:00.003");
        let v = convert(ColumnData::DateTime(Some(tiberius::time::DateTime::new(0, 2))), ColumnType::Datetime);
        assert_eq!(v.to_text(), "1900-01-01 00:00:00.007");
        // pre-1900 datetime
        let v = convert(ColumnData::DateTime(Some(tiberius::time::DateTime::new(-53_690, 0))), ColumnType::Datetime);
        assert_eq!(v.to_text(), "1753-01-01 00:00:00");
        let v = convert(ColumnData::SmallDateTime(Some(tiberius::time::SmallDateTime::new(1, 61))), ColumnType::Datetime4);
        assert_eq!(v.to_text(), "1900-01-02 01:01:00");
        let v = convert(ColumnData::Date(Some(Date::new(738_886))), ColumnType::Daten);
        assert_eq!(v.to_text(), "2024-01-02");
        let v = convert(ColumnData::Time(Some(Time::new(123_456_789, 7))), ColumnType::Timen);
        assert_eq!(v.to_text(), "00:00:12.3456789");
        let dt2 = DateTime2::new(Date::new(738_886), Time::new(36_000_000_000, 7)); // 01:00 UTC
        let v = convert(ColumnData::DateTime2(Some(dt2)), ColumnType::Datetime2);
        assert_eq!(v.to_text(), "2024-01-02 01:00:00");
        let v = convert(ColumnData::DateTimeOffset(Some(DateTimeOffset::new(dt2, 420))), ColumnType::DatetimeOffsetn);
        assert_eq!(v.to_text(), "2024-01-02 08:00:00+07");
        assert_eq!(convert(ColumnData::Time(None), ColumnType::Timen), Value::Null);
    }

    #[test]
    fn scalar_conversions() {
        assert_eq!(convert(ColumnData::U8(Some(255)), ColumnType::Int1), Value::Int(255));
        assert_eq!(convert(ColumnData::F64(Some(12.5)), ColumnType::Money), Value::decimal("12.5000"));
        assert_eq!(convert(ColumnData::F64(Some(12.5)), ColumnType::Float8), Value::Float(12.5));
        assert_eq!(convert(ColumnData::Bit(Some(true)), ColumnType::Bitn), Value::Bool(true));
        assert_eq!(convert(ColumnData::I32(Some(5)), ColumnType::SSVariant), Value::text("5"));
        assert_eq!(
            convert(ColumnData::Numeric(Some(Numeric::new_with_scale(1050, 2))), ColumnType::Decimaln),
            Value::decimal("10.50")
        );
        assert_eq!(
            convert(ColumnData::Binary(Some(Cow::Owned(vec![1, 2]))), ColumnType::BigVarBin),
            Value::bytes(vec![1u8, 2])
        );
        assert_eq!(convert(ColumnData::String(None), ColumnType::NVarchar), Value::Null);
    }

    #[test]
    fn type_info() {
        assert_eq!(column_type_info(ColumnType::NVarchar), ("nvarchar", DataKind::Text));
        assert_eq!(column_type_info(ColumnType::DatetimeOffsetn).1, DataKind::DateTimeTz);
        assert_eq!(column_type_info(ColumnType::Xml).1, DataKind::Xml);
        assert_eq!(kind_of_type_name("nvarchar(max)"), DataKind::Text);
        assert_eq!(kind_of_type_name("decimal(10,2)"), DataKind::Decimal);
        assert_eq!(kind_of_type_name("datetime2(7)"), DataKind::DateTime);
        assert_eq!(kind_of_type_name("[geography]"), DataKind::Geometry);
        assert_eq!(kind_of_type_name("rowversion"), DataKind::Binary);
    }

    #[test]
    fn binding_declarations() {
        let decl = param_declarations(&[
            Value::Int(1),
            Value::text("x"),
            Value::Null,
            Value::decimal("12.50"),
            Value::Bool(true),
            Value::bytes(vec![0u8; 10]),
            Value::UInt(u64::MAX),
            Value::text("y".repeat(5000)),
        ])
        .unwrap();
        assert_eq!(
            decl,
            "@P1 bigint, @P2 nvarchar(4000), @P3 nvarchar(4000), @P4 numeric(4,2), @P5 bit, @P6 varbinary(8000), @P7 numeric(20,0), @P8 nvarchar(max)"
        );
        assert!(param_declarations(&[Value::Float(f64::NAN)]).is_err());
        assert!(executesql_command("SELECT @P1", &[Value::Int(1)]).is_ok());
    }
}
