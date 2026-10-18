//! Conversion of PostgreSQL wire values into [`Value`]s.
//!
//! * [`text_value`] handles the text format used by the simple query
//!   protocol (the session forces `DateStyle=ISO`, `IntervalStyle=postgres`,
//!   `bytea_output=hex` and `extra_float_digits=3`, so the formats are fixed).
//! * [`binary_value`] handles the binary format returned by the extended
//!   protocol (statements executed with parameters). Values whose binary
//!   format is not understood are rendered as `(binary) <hex>`.
//!
//! Database specific types without a natural Rust representation (intervals,
//! ranges, arrays, network addresses ...) are carried as [`Value::Text`]
//! containing exactly the text PostgreSQL itself would print, so they can be
//! edited and sent back as text parameters.

use crate::ewkb;
use crate::types::Conv;
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, Timelike};
use dbiji_core::Value;
use postgres_protocol::types as pt;
use std::fmt::Write;
use tokio_postgres::fallible_iterator::FallibleIterator;
use uuid::Uuid;

// ------------------------------------------------------------------- text

/// Convert one value of the text protocol.
pub(crate) fn text_value(conv: &Conv, s: &str) -> Value {
    match conv {
        Conv::Bool => match s {
            "t" => Value::Bool(true),
            "f" => Value::Bool(false),
            _ => Value::text(s),
        },
        Conv::Int2 | Conv::Int4 | Conv::Int8 | Conv::Oid => {
            s.parse::<i64>().map(Value::Int).unwrap_or_else(|_| Value::text(s))
        }
        Conv::Xid8 => match s.parse::<i64>() {
            Ok(i) => Value::Int(i),
            Err(_) => s.parse::<u64>().map(Value::UInt).unwrap_or_else(|_| Value::text(s)),
        },
        Conv::Float4 | Conv::Float8 => s.parse::<f64>().map(Value::Float).unwrap_or_else(|_| Value::text(s)),
        Conv::Numeric => {
            if s.as_bytes().first().is_some_and(|b| b.is_ascii_digit() || *b == b'-' || *b == b'.')
                && !s.ends_with("Infinity")
            {
                Value::decimal(s)
            } else {
                Value::text(s)
            }
        }
        Conv::Date => parse_pg_date(s).map(Value::Date).unwrap_or_else(|| Value::text(s)),
        Conv::Time => parse_pg_time(s).map(Value::Time).unwrap_or_else(|| Value::text(s)),
        Conv::Timestamp => parse_pg_timestamp(s).map(Value::DateTime).unwrap_or_else(|| Value::text(s)),
        Conv::TimestampTz => parse_pg_timestamptz(s).map(Value::DateTimeTz).unwrap_or_else(|| Value::text(s)),
        Conv::Uuid => Uuid::parse_str(s).map(Value::Uuid).unwrap_or_else(|_| Value::text(s)),
        Conv::Json | Conv::Jsonb => Value::json(s),
        Conv::Bytea => parse_bytea_text(s).map(Value::bytes).unwrap_or_else(|| Value::text(s)),
        Conv::Geometry => match ewkb::hex_ewkb_to_ewkt(s) {
            Ok(t) => Value::text(t),
            Err(_) => Value::text(s),
        },
        _ => Value::text(s),
    }
}

/// `2024-01-02`, `0044-03-15 BC`.
pub(crate) fn parse_pg_date(s: &str) -> Option<NaiveDate> {
    let (s, bc) = match s.strip_suffix(" BC") {
        Some(r) => (r, true),
        None => (s, false),
    };
    let mut it = s.splitn(3, '-');
    let y: i32 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    let y = if bc { 1 - y } else { y };
    NaiveDate::from_ymd_opt(y, m, d)
}

/// `13:14:15`, `13:14:15.123456`. `24:00:00` is not representable and yields `None`.
pub(crate) fn parse_pg_time(s: &str) -> Option<NaiveTime> {
    let (hms, frac) = match s.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    let mut it = hms.splitn(3, ':');
    let h: u32 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let sec: u32 = it.next().unwrap_or("0").parse().ok()?;
    let nanos = match frac {
        Some(f) => {
            if f.is_empty() || f.len() > 9 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let v: u32 = f.parse().ok()?;
            v * 10u32.pow(9 - f.len() as u32)
        }
        None => 0,
    };
    NaiveTime::from_hms_nano_opt(h, m, sec, nanos)
}

fn split_bc(s: &str) -> (&str, bool) {
    match s.strip_suffix(" BC") {
        Some(r) => (r, true),
        None => (s, false),
    }
}

/// `2024-01-02 13:14:15.5`.
pub(crate) fn parse_pg_timestamp(s: &str) -> Option<NaiveDateTime> {
    let (s, bc) = split_bc(s);
    let (d, t) = s.split_once(' ')?;
    let mut date = parse_pg_date(d)?;
    if bc {
        date = NaiveDate::from_ymd_opt(1 - date.year(), date.month(), date.day())?;
    }
    Some(NaiveDateTime::new(date, parse_pg_time(t)?))
}

/// `2024-01-02 13:14:15.5+07`, `...+05:30`, `...-03:00:15` (historic offsets).
pub(crate) fn parse_pg_timestamptz(s: &str) -> Option<DateTime<FixedOffset>> {
    let (s, bc) = split_bc(s);
    let (d, rest) = s.split_once(' ')?;
    let pos = rest.rfind(['+', '-'])?;
    let (t, off) = rest.split_at(pos);
    let mut date = parse_pg_date(d)?;
    if bc {
        date = NaiveDate::from_ymd_opt(1 - date.year(), date.month(), date.day())?;
    }
    let offset = FixedOffset::east_opt(parse_offset(off)?)?;
    let local = NaiveDateTime::new(date, parse_pg_time(t)?);
    local.and_local_timezone(offset).single()
}

/// `+07`, `-05:30`, `+00:53:28` → seconds east of UTC.
pub(crate) fn parse_offset(s: &str) -> Option<i32> {
    let sign = match s.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let mut it = s[1..].split(':');
    let h: i32 = it.next()?.parse().ok()?;
    let m: i32 = it.next().map(str::parse).transpose().ok()?.unwrap_or(0);
    let sec: i32 = it.next().map(str::parse).transpose().ok()?.unwrap_or(0);
    Some(sign * (h * 3600 + m * 60 + sec))
}

/// `\x0102ff` (hex output) or the legacy escape format.
pub(crate) fn parse_bytea_text(s: &str) -> Option<Vec<u8>> {
    if let Some(h) = s.strip_prefix("\\x") {
        return hex::decode(h).ok();
    }
    // escape format: \\ and \ooo octal escapes
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            if b.get(i + 1) == Some(&b'\\') {
                out.push(b'\\');
                i += 2;
            } else if i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
                let v = (b[i + 1] - b'0') as u32 * 64 + (b[i + 2] - b'0') as u32 * 8 + (b[i + 3] - b'0') as u32;
                out.push(u8::try_from(v).ok()?);
                i += 4;
            } else {
                return None;
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

// ----------------------------------------------------------------- binary

fn be_i16(b: &[u8], at: usize) -> Option<i16> {
    Some(i16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn be_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn be_i32(b: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn be_i64(b: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn pg_epoch_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2000, 1, 1).unwrap_or_default()
}

fn pg_epoch() -> NaiveDateTime {
    pg_epoch_date().and_hms_opt(0, 0, 0).unwrap_or_default()
}

/// Render a binary value that has no dedicated [`Value`] variant.
fn hex_fallback(raw: &[u8]) -> Value {
    Value::text(format!("(binary) {}", hex::encode(raw)))
}

/// Convert one value of the binary protocol. `raw` is `None` for SQL NULL.
pub(crate) fn binary_value(conv: &Conv, raw: Option<&[u8]>) -> Value {
    let Some(b) = raw else { return Value::Null };
    binary_value_inner(conv, b).unwrap_or_else(|| hex_fallback(b))
}

fn binary_value_inner(conv: &Conv, b: &[u8]) -> Option<Value> {
    Some(match conv {
        Conv::Bool => Value::Bool(pt::bool_from_sql(b).ok()?),
        Conv::Int2 => Value::Int(pt::int2_from_sql(b).ok()? as i64),
        Conv::Int4 => Value::Int(pt::int4_from_sql(b).ok()? as i64),
        Conv::Int8 => Value::Int(pt::int8_from_sql(b).ok()?),
        Conv::Oid => Value::Int(pt::oid_from_sql(b).ok()? as i64),
        Conv::Xid8 => {
            let v = u64::from_be_bytes(b.try_into().ok()?);
            i64::try_from(v).map(Value::Int).unwrap_or(Value::UInt(v))
        }
        Conv::Float4 => {
            let f = pt::float4_from_sql(b).ok()?;
            // keep the shortest decimal form of the f32 (1.1 instead of 1.100000023841858)
            Value::Float(format!("{f}").parse::<f64>().unwrap_or(f as f64))
        }
        Conv::Float8 => Value::Float(pt::float8_from_sql(b).ok()?),
        Conv::Numeric => {
            let s = numeric_text(b)?;
            if s.ends_with("NaN") || s.ends_with("Infinity") { Value::text(s) } else { Value::decimal(s) }
        }
        Conv::Money => Value::text(money_text(b)?),
        Conv::Date => match date_from_days(pt::date_from_sql(b).ok()?) {
            Ok(d) => Value::Date(d),
            Err(t) => Value::text(t),
        },
        Conv::Time => {
            let us = pt::time_from_sql(b).ok()?;
            match time_from_micros(us) {
                Some(t) => Value::Time(t),
                None => Value::text(time_text_micros(us)),
            }
        }
        Conv::TimeTz => Value::text(timetz_text(b)?),
        Conv::Timestamp => match timestamp_from_micros(pt::timestamp_from_sql(b).ok()?) {
            Ok(t) => Value::DateTime(t),
            Err(t) => Value::text(t),
        },
        Conv::TimestampTz => match timestamp_from_micros(pt::timestamp_from_sql(b).ok()?) {
            Ok(t) => Value::DateTimeTz(DateTime::from_naive_utc_and_offset(t, FixedOffset::east_opt(0)?)),
            Err(t) => Value::text(t),
        },
        Conv::Interval => Value::text(interval_text(b)?),
        Conv::Uuid => Value::Uuid(Uuid::from_bytes(pt::uuid_from_sql(b).ok()?)),
        Conv::Json => Value::json(String::from_utf8_lossy(b).into_owned()),
        Conv::Jsonb => {
            let (v, rest) = b.split_first()?;
            if *v != 1 {
                return None;
            }
            Value::json(String::from_utf8_lossy(rest).into_owned())
        }
        Conv::Bytea => Value::bytes(b.to_vec()),
        Conv::Text | Conv::Enum | Conv::Xml => Value::text(String::from_utf8_lossy(b).into_owned()),
        Conv::Char => Value::text(char_text(b)),
        Conv::Geometry => match ewkb::ewkb_to_ewkt(b) {
            Ok(t) => Value::text(t),
            Err(_) => return None,
        },
        Conv::Other => return None,
        other => Value::text(element_text(other, b)?),
    })
}

/// PostgreSQL text output of a binary encoded value (used for array, range
/// and composite elements and for types carried as text).
pub(crate) fn element_text(conv: &Conv, b: &[u8]) -> Option<String> {
    Some(match conv {
        Conv::Bool => (if pt::bool_from_sql(b).ok()? { "t" } else { "f" }).to_string(),
        Conv::Int2 => pt::int2_from_sql(b).ok()?.to_string(),
        Conv::Int4 => pt::int4_from_sql(b).ok()?.to_string(),
        Conv::Int8 => pt::int8_from_sql(b).ok()?.to_string(),
        Conv::Oid => pt::oid_from_sql(b).ok()?.to_string(),
        Conv::Xid8 => u64::from_be_bytes(b.try_into().ok()?).to_string(),
        Conv::Float4 => pg_float_text(format!("{}", pt::float4_from_sql(b).ok()?).parse().ok()?),
        Conv::Float8 => pg_float_text(pt::float8_from_sql(b).ok()?),
        Conv::Numeric => numeric_text(b)?,
        Conv::Money => money_text(b)?,
        Conv::Date => match date_from_days(pt::date_from_sql(b).ok()?) {
            Ok(d) => date_text(d),
            Err(t) => t,
        },
        Conv::Time => time_text_micros(pt::time_from_sql(b).ok()?),
        Conv::TimeTz => timetz_text(b)?,
        Conv::Timestamp => match timestamp_from_micros(pt::timestamp_from_sql(b).ok()?) {
            Ok(t) => timestamp_text(t),
            Err(t) => t,
        },
        Conv::TimestampTz => match timestamp_from_micros(pt::timestamp_from_sql(b).ok()?) {
            Ok(t) => {
                let text = timestamp_text(t);
                let (d, bc) = split_bc_text(&text);
                format!("{d}+00{bc}")
            }
            Err(t) => t,
        },
        Conv::Interval => interval_text(b)?,
        Conv::Uuid => Uuid::from_bytes(pt::uuid_from_sql(b).ok()?).hyphenated().to_string(),
        Conv::Json | Conv::Text | Conv::Enum | Conv::Xml => String::from_utf8_lossy(b).into_owned(),
        Conv::Jsonb => {
            let (v, rest) = b.split_first()?;
            if *v != 1 {
                return None;
            }
            String::from_utf8_lossy(rest).into_owned()
        }
        Conv::Char => char_text(b),
        Conv::Bytea => format!("\\x{}", hex::encode(b)),
        Conv::Inet | Conv::Cidr => {
            let inet = pt::inet_from_sql(b).ok()?;
            let full = if inet.addr().is_ipv4() { 32 } else { 128 };
            if *conv == Conv::Inet && inet.netmask() == full {
                inet.addr().to_string()
            } else {
                format!("{}/{}", inet.addr(), inet.netmask())
            }
        }
        Conv::MacAddr | Conv::MacAddr8 => {
            let expected = if *conv == Conv::MacAddr { 6 } else { 8 };
            if b.len() != expected {
                return None;
            }
            b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(":")
        }
        Conv::Bit => {
            let v = pt::varbit_from_sql(b).ok()?;
            let mut s = String::with_capacity(v.len());
            for i in 0..v.len() {
                let byte = v.bytes()[i / 8];
                s.push(if byte & (0x80 >> (i % 8)) != 0 { '1' } else { '0' });
            }
            s
        }
        Conv::Range(sub) => range_text(sub, b)?,
        Conv::Multirange(sub) => {
            let n = be_i32(b, 0)?;
            if n < 0 {
                return None;
            }
            let mut pos = 4usize;
            let mut parts = Vec::with_capacity((n as usize).min(1024));
            for _ in 0..n {
                let len = usize::try_from(be_i32(b, pos)?).ok()?;
                pos += 4;
                let r = b.get(pos..pos.checked_add(len)?)?;
                pos += len;
                parts.push(range_text(sub, r)?);
            }
            format!("{{{}}}", parts.join(","))
        }
        Conv::Array(elem) => array_text(elem, b)?,
        Conv::Composite(fields) => composite_text(fields, b)?,
        Conv::Hstore => {
            let mut it = pt::hstore_from_sql(b).ok()?;
            let mut parts = Vec::new();
            while let Some((k, v)) = it.next().ok()? {
                let v = match v {
                    Some(v) => format!("\"{}\"", escape_backslash_quote(v)),
                    None => "NULL".to_string(),
                };
                parts.push(format!("\"{}\"=>{v}", escape_backslash_quote(k)));
            }
            parts.join(", ")
        }
        Conv::Geometry => hex::encode_upper(b),
        Conv::Other => return None,
    })
}

fn char_text(b: &[u8]) -> String {
    match b.first() {
        None | Some(0) => String::new(),
        Some(c) if c.is_ascii() => (*c as char).to_string(),
        Some(c) => format!("\\{c:03o}"),
    }
}

fn split_bc_text(s: &str) -> (&str, &str) {
    match s.strip_suffix(" BC") {
        Some(r) => (r, " BC"),
        None => (s, ""),
    }
}

fn escape_backslash_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Exact decimal text of a binary NUMERIC.
pub(crate) fn numeric_text(b: &[u8]) -> Option<String> {
    let ndigits = be_i16(b, 0)?;
    let weight = be_i16(b, 2)? as i32;
    let sign = be_u16(b, 4)?;
    let dscale = be_u16(b, 6)? as i32;
    match sign {
        0xC000 => return Some("NaN".into()),
        0xD000 => return Some("Infinity".into()),
        0xF000 => return Some("-Infinity".into()),
        0x0000 | 0x4000 => {}
        _ => return None,
    }
    if ndigits < 0 {
        return None;
    }
    let ndigits = ndigits as usize;
    let mut digits = Vec::with_capacity(ndigits);
    for i in 0..ndigits {
        let d = be_i16(b, 8 + 2 * i)?;
        if !(0..10000).contains(&d) {
            return None;
        }
        digits.push(d);
    }
    let digit = |idx: i32| -> i16 {
        if idx >= 0 && (idx as usize) < ndigits { digits[idx as usize] } else { 0 }
    };
    let mut s = String::with_capacity(ndigits * 4 + 4);
    if sign == 0x4000 {
        s.push('-');
    }
    if weight < 0 {
        s.push('0');
    } else {
        for i in 0..=weight {
            let d = digit(i);
            if i == 0 {
                let _ = write!(s, "{d}");
            } else {
                let _ = write!(s, "{d:04}");
            }
        }
    }
    if dscale > 0 {
        s.push('.');
        let mut produced = 0;
        let mut i = weight + 1;
        while produced < dscale {
            let chunk = format!("{:04}", digit(i));
            for c in chunk.chars() {
                if produced < dscale {
                    s.push(c);
                    produced += 1;
                }
            }
            i += 1;
        }
    }
    Some(s)
}

/// Money as a plain decimal with two fraction digits (`lc_monetary` is not known).
fn money_text(b: &[u8]) -> Option<String> {
    let cents = be_i64(b, 0)?;
    if b.len() != 8 {
        return None;
    }
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    Some(format!("{sign}{}.{:02}", abs / 100, abs % 100))
}

fn date_from_days(days: i32) -> Result<NaiveDate, String> {
    match days {
        i32::MAX => Err("infinity".into()),
        i32::MIN => Err("-infinity".into()),
        d => pg_epoch_date()
            .checked_add_signed(TimeDelta::days(d as i64))
            .ok_or_else(|| format!("(date {d})")),
    }
}

/// PostgreSQL text for a date (`0044-03-15 BC` for years before 1).
pub(crate) fn date_text(d: NaiveDate) -> String {
    if d.year() <= 0 {
        format!("{:04}-{:02}-{:02} BC", 1 - d.year(), d.month(), d.day())
    } else {
        format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day())
    }
}

fn time_from_micros(us: i64) -> Option<NaiveTime> {
    if !(0..86_400_000_000).contains(&us) {
        return None;
    }
    NaiveTime::from_num_seconds_from_midnight_opt((us / 1_000_000) as u32, ((us % 1_000_000) * 1000) as u32)
}

fn fraction_text(micros: i64) -> String {
    if micros == 0 {
        String::new()
    } else {
        let f = format!(".{micros:06}");
        f.trim_end_matches('0').to_string()
    }
}

fn time_text_micros(us: i64) -> String {
    let us = us.max(0);
    let secs = us / 1_000_000;
    format!(
        "{:02}:{:02}:{:02}{}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60,
        fraction_text(us % 1_000_000)
    )
}

/// PostgreSQL text for a time of day.
pub(crate) fn time_text(t: NaiveTime) -> String {
    let us = t.num_seconds_from_midnight() as i64 * 1_000_000 + (t.nanosecond() / 1000) as i64;
    time_text_micros(us)
}

fn offset_text(east_secs: i32) -> String {
    let sign = if east_secs < 0 { '-' } else { '+' };
    let a = east_secs.unsigned_abs();
    let (h, m, s) = (a / 3600, (a / 60) % 60, a % 60);
    if s != 0 {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    } else if m != 0 {
        format!("{sign}{h:02}:{m:02}")
    } else {
        format!("{sign}{h:02}")
    }
}

fn timetz_text(b: &[u8]) -> Option<String> {
    if b.len() != 12 {
        return None;
    }
    let us = be_i64(b, 0)?;
    let zone_west = be_i32(b, 8)?;
    Some(format!("{}{}", time_text_micros(us), offset_text(-zone_west)))
}

fn timestamp_from_micros(us: i64) -> Result<NaiveDateTime, String> {
    match us {
        i64::MAX => Err("infinity".into()),
        i64::MIN => Err("-infinity".into()),
        u => pg_epoch()
            .checked_add_signed(TimeDelta::microseconds(u))
            .ok_or_else(|| format!("(timestamp {u})")),
    }
}

/// PostgreSQL text for a timestamp without time zone.
pub(crate) fn timestamp_text(t: NaiveDateTime) -> String {
    let d = t.date();
    let time = time_text(t.time());
    if d.year() <= 0 {
        format!("{:04}-{:02}-{:02} {time} BC", 1 - d.year(), d.month(), d.day())
    } else {
        format!("{:04}-{:02}-{:02} {time}", d.year(), d.month(), d.day())
    }
}

/// Interval text in the `postgres` IntervalStyle.
fn interval_text(b: &[u8]) -> Option<String> {
    if b.len() != 16 {
        return None;
    }
    Some(format_interval(be_i64(b, 0)?, be_i32(b, 8)?, be_i32(b, 12)?))
}

/// Format an interval exactly like PostgreSQL's `EncodeInterval` with
/// `IntervalStyle = postgres`.
pub(crate) fn format_interval(micros: i64, days: i32, months: i32) -> String {
    let year = (months / 12) as i64;
    let mon = (months % 12) as i64;
    let mday = days as i64;
    let mut time = micros;
    let hour = time / 3_600_000_000;
    time -= hour * 3_600_000_000;
    let min = time / 60_000_000;
    time -= min * 60_000_000;
    let sec = time / 1_000_000;
    let fsec = time - sec * 1_000_000;

    fn add(value: i64, units: &str, out: &mut String, is_zero: &mut bool, is_before: &mut bool) {
        if value == 0 {
            return;
        }
        let _ = write!(
            out,
            "{}{}{} {}{}",
            if !*is_zero { " " } else { "" },
            if *is_before && value > 0 { "+" } else { "" },
            value,
            units,
            if value != 1 { "s" } else { "" }
        );
        *is_before = value < 0;
        *is_zero = false;
    }
    let mut out = String::new();
    let mut is_zero = true;
    let mut is_before = false;
    add(year, "year", &mut out, &mut is_zero, &mut is_before);
    add(mon, "mon", &mut out, &mut is_zero, &mut is_before);
    add(mday, "day", &mut out, &mut is_zero, &mut is_before);
    if is_zero || hour != 0 || min != 0 || sec != 0 || fsec != 0 {
        let minus = hour < 0 || min < 0 || sec < 0 || fsec < 0;
        let _ = write!(
            out,
            "{}{}{:02}:{:02}:{:02}{}",
            if is_zero { "" } else { " " },
            if minus { "-" } else if is_before { "+" } else { "" },
            hour.abs(),
            min.abs(),
            sec.abs(),
            fraction_text(fsec.abs())
        );
    }
    out
}

/// Float text as PostgreSQL prints it (`NaN`, `Infinity`, `1e+300`, `1.5e-07`).
pub(crate) fn pg_float_text(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let a = f.abs();
    if a != 0.0 && !(1e-4..1e15).contains(&a) {
        let s = format!("{f:e}");
        if let Some((m, e)) = s.split_once('e') {
            let (sign, digits) = match e.strip_prefix('-') {
                Some(d) => ('-', d),
                None => ('+', e),
            };
            return format!("{m}e{sign}{digits:0>2}");
        }
        s
    } else {
        format!("{f}")
    }
}

/// Quote an array element like `array_out`.
pub(crate) fn quote_array_element(s: &str) -> String {
    let needs = s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars().any(|c| matches!(c, '{' | '}' | '"' | '\\' | ',') || c.is_ascii_whitespace());
    if needs { format!("\"{}\"", escape_backslash_quote(s)) } else { s.to_string() }
}

/// Quote a range bound or composite field like `range_out` / `record_out`.
fn quote_record_element(s: &str) -> String {
    let needs = s.is_empty()
        || s.chars().any(|c| matches!(c, '"' | '\\' | '(' | ')' | '[' | ']' | ',') || c.is_ascii_whitespace());
    if !needs {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push(c);
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn array_text(elem: &Conv, b: &[u8]) -> Option<String> {
    let arr = pt::array_from_sql(b).ok()?;
    let dims: Vec<pt::ArrayDimension> = arr.dimensions().collect().ok()?;
    if dims.is_empty() || dims.iter().any(|d| d.len == 0) {
        return Some("{}".into());
    }
    let mut values = Vec::new();
    let mut it = arr.values();
    while let Some(v) = it.next().ok()? {
        values.push(match v {
            None => "NULL".to_string(),
            Some(raw) => quote_array_element(&element_text(elem, raw).unwrap_or_else(|| format!("\\x{}", hex::encode(raw)))),
        });
    }
    let mut out = String::new();
    if dims.iter().any(|d| d.lower_bound != 1) {
        for d in &dims {
            let _ = write!(out, "[{}:{}]", d.lower_bound, d.lower_bound as i64 + d.len as i64 - 1);
        }
        out.push('=');
    }
    let mut idx = 0usize;
    write_array_level(&dims, 0, &values, &mut idx, &mut out)?;
    Some(out)
}

fn write_array_level(
    dims: &[pt::ArrayDimension],
    level: usize,
    values: &[String],
    idx: &mut usize,
    out: &mut String,
) -> Option<()> {
    out.push('{');
    let len = dims.get(level)?.len.max(0) as usize;
    for i in 0..len {
        if i > 0 {
            out.push(',');
        }
        if level + 1 == dims.len() {
            out.push_str(values.get(*idx)?);
            *idx += 1;
        } else {
            write_array_level(dims, level + 1, values, idx, out)?;
        }
    }
    out.push('}');
    Some(())
}

fn range_text(sub: &Conv, b: &[u8]) -> Option<String> {
    match pt::range_from_sql(b).ok()? {
        pt::Range::Empty => Some("empty".into()),
        pt::Range::Nonempty(lo, hi) => {
            let bound = |v: Option<&[u8]>| -> Option<String> {
                match v {
                    None => Some(String::new()),
                    Some(raw) => Some(quote_record_element(&element_text(sub, raw)?)),
                }
            };
            let (open, lo_txt) = match lo {
                pt::RangeBound::Inclusive(v) => ('[', bound(v)?),
                pt::RangeBound::Exclusive(v) => ('(', bound(v)?),
                pt::RangeBound::Unbounded => ('(', String::new()),
            };
            let (close, hi_txt) = match hi {
                pt::RangeBound::Inclusive(v) => (']', bound(v)?),
                pt::RangeBound::Exclusive(v) => (')', bound(v)?),
                pt::RangeBound::Unbounded => (')', String::new()),
            };
            Some(format!("{open}{lo_txt},{hi_txt}{close}"))
        }
    }
}

fn composite_text(fields: &[Conv], b: &[u8]) -> Option<String> {
    let n = be_i32(b, 0)?;
    if n < 0 {
        return None;
    }
    let mut pos = 4usize;
    let mut parts = Vec::with_capacity((n as usize).min(1024));
    for i in 0..n as usize {
        let _type_oid = be_i32(b, pos)?;
        let len = be_i32(b, pos + 4)?;
        pos += 8;
        if len < 0 {
            parts.push(String::new());
            continue;
        }
        let len = len as usize;
        let raw = b.get(pos..pos.checked_add(len)?)?;
        pos += len;
        let conv = fields.get(i).unwrap_or(&Conv::Other);
        let text = element_text(conv, raw).unwrap_or_else(|| format!("\\x{}", hex::encode(raw)));
        parts.push(quote_record_element(&text));
    }
    Some(format!("({})", parts.join(",")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric_bytes(ndigits: i16, weight: i16, sign: u16, dscale: u16, digits: &[i16]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&ndigits.to_be_bytes());
        v.extend_from_slice(&weight.to_be_bytes());
        v.extend_from_slice(&sign.to_be_bytes());
        v.extend_from_slice(&dscale.to_be_bytes());
        for d in digits {
            v.extend_from_slice(&d.to_be_bytes());
        }
        v
    }

    #[test]
    fn numeric_binary() {
        // 12345.678 = [1, 2345, 6780] weight 1, dscale 3
        assert_eq!(numeric_text(&numeric_bytes(3, 1, 0, 3, &[1, 2345, 6780])).unwrap(), "12345.678");
        // -0.0005 = [5] weight -1 => 0.0005, dscale 4
        assert_eq!(numeric_text(&numeric_bytes(1, -1, 0x4000, 4, &[5])).unwrap(), "-0.0005");
        // 0.00000001 = [1] weight -2, dscale 8; [100] would be 0.000001
        assert_eq!(numeric_text(&numeric_bytes(1, -2, 0, 8, &[1])).unwrap(), "0.00000001");
        assert_eq!(numeric_text(&numeric_bytes(1, -2, 0, 8, &[100])).unwrap(), "0.00000100");
        // 100000000 = [1] weight 2
        assert_eq!(numeric_text(&numeric_bytes(1, 2, 0, 0, &[1])).unwrap(), "100000000");
        // 0.00 with dscale 2 and no digits
        assert_eq!(numeric_text(&numeric_bytes(0, 0, 0, 2, &[])).unwrap(), "0.00");
        assert_eq!(numeric_text(&numeric_bytes(0, 0, 0xC000, 0, &[])).unwrap(), "NaN");
        assert_eq!(numeric_text(&numeric_bytes(0, 0, 0xD000, 0, &[])).unwrap(), "Infinity");
        assert!(numeric_text(&numeric_bytes(2, 0, 0, 0, &[1])).is_none());
        assert_eq!(
            binary_value(&Conv::Numeric, Some(&numeric_bytes(2, 0, 0, 2, &[3, 1400]))),
            Value::decimal("3.14")
        );
    }

    #[test]
    fn intervals() {
        assert_eq!(format_interval(0, 0, 0), "00:00:00");
        assert_eq!(format_interval(4 * 3_600_000_000 + 5 * 60_000_000 + 6_789_000, 3, 14), "1 year 2 mons 3 days 04:05:06.789");
        assert_eq!(format_interval(2 * 3_600_000_000, -1, 0), "-1 days +02:00:00");
        assert_eq!(format_interval(-90_000_000, 0, 0), "-00:01:30");
        assert_eq!(format_interval(0, 1, 0), "1 day");
        assert_eq!(format_interval(0, 0, -13), "-1 years -1 mons");
    }

    #[test]
    fn text_parsing() {
        assert_eq!(text_value(&Conv::Bool, "t"), Value::Bool(true));
        assert_eq!(text_value(&Conv::Int8, "-9223372036854775808"), Value::Int(i64::MIN));
        assert_eq!(text_value(&Conv::Oid, "4294967295"), Value::Int(4294967295));
        assert!(matches!(text_value(&Conv::Float8, "NaN"), Value::Float(f) if f.is_nan()));
        assert_eq!(text_value(&Conv::Float8, "-Infinity"), Value::Float(f64::NEG_INFINITY));
        assert_eq!(text_value(&Conv::Numeric, "NaN"), Value::text("NaN"));
        assert_eq!(text_value(&Conv::Numeric, "-12.50"), Value::decimal("-12.50"));
        assert_eq!(text_value(&Conv::Numeric, "Infinity"), Value::text("Infinity"));
        assert_eq!(text_value(&Conv::Date, "infinity"), Value::text("infinity"));
        assert_eq!(
            text_value(&Conv::Date, "0044-03-15 BC"),
            Value::Date(NaiveDate::from_ymd_opt(-43, 3, 15).unwrap())
        );
        assert_eq!(text_value(&Conv::Time, "24:00:00"), Value::text("24:00:00"));
        assert_eq!(
            text_value(&Conv::Time, "13:14:15.5"),
            Value::Time(NaiveTime::from_hms_milli_opt(13, 14, 15, 500).unwrap())
        );
        let tz = text_value(&Conv::TimestampTz, "2024-01-02 03:04:05.123456+05:30");
        match tz {
            Value::DateTimeTz(d) => {
                assert_eq!(d.offset().local_minus_utc(), 5 * 3600 + 1800);
                assert_eq!(d.naive_local().nanosecond(), 123_456_000);
            }
            other => panic!("{other:?}"),
        }
        let tz = text_value(&Conv::TimestampTz, "1900-01-01 00:00:00+00:53:28");
        assert!(matches!(tz, Value::DateTimeTz(d) if d.offset().local_minus_utc() == 53 * 60 + 28));
        let tz = text_value(&Conv::TimestampTz, "2024-06-01 10:00:00-07");
        assert!(matches!(tz, Value::DateTimeTz(d) if d.offset().local_minus_utc() == -7 * 3600));
        assert_eq!(text_value(&Conv::Bytea, "\\x00ff"), Value::bytes(vec![0u8, 255]));
        assert_eq!(parse_bytea_text("a\\\\b\\001").unwrap(), b"a\\b\x01".to_vec());
        assert_eq!(
            text_value(&Conv::Geometry, "0101000020E6100000000000000000F03F0000000000000040"),
            Value::text("SRID=4326;POINT(1 2)")
        );
        assert_eq!(text_value(&Conv::Array(Box::new(Conv::Int4)), "{1,2}"), Value::text("{1,2}"));
        assert_eq!(
            text_value(&Conv::Uuid, "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"),
            Value::Uuid(Uuid::parse_str("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11").unwrap())
        );
    }

    #[test]
    fn binary_scalars() {
        assert_eq!(binary_value(&Conv::Int4, Some(&7i32.to_be_bytes())), Value::Int(7));
        assert_eq!(binary_value(&Conv::Int4, None), Value::Null);
        assert_eq!(binary_value(&Conv::Float4, Some(&1.1f32.to_be_bytes())), Value::Float(1.1));
        assert_eq!(
            binary_value(&Conv::Date, Some(&0i32.to_be_bytes())),
            Value::Date(NaiveDate::from_ymd_opt(2000, 1, 1).unwrap())
        );
        assert_eq!(binary_value(&Conv::Date, Some(&i32::MAX.to_be_bytes())), Value::text("infinity"));
        assert_eq!(binary_value(&Conv::Timestamp, Some(&i64::MIN.to_be_bytes())), Value::text("-infinity"));
        let ts = binary_value(&Conv::TimestampTz, Some(&1_500_000i64.to_be_bytes()));
        assert_eq!(ts.to_text(), "2000-01-01 00:00:01.5+00");
        let mut tz = Vec::new();
        tz.extend_from_slice(&(3_600_000_000i64).to_be_bytes());
        tz.extend_from_slice(&(-5 * 3600 - 1800i32).to_be_bytes());
        assert_eq!(binary_value(&Conv::TimeTz, Some(&tz)), Value::text("01:00:00+05:30"));
        let mut jb = vec![1u8];
        jb.extend_from_slice(b"{\"a\": 1}");
        assert_eq!(binary_value(&Conv::Jsonb, Some(&jb)), Value::json("{\"a\": 1}"));
        assert_eq!(binary_value(&Conv::Money, Some(&(-12345i64).to_be_bytes())), Value::text("-123.45"));
        assert_eq!(binary_value(&Conv::MacAddr, Some(&[8, 0, 0x2b, 1, 2, 3])), Value::text("08:00:2b:01:02:03"));
        // inet 192.168.1.5/24 and /32
        assert_eq!(binary_value(&Conv::Inet, Some(&[2, 24, 0, 4, 192, 168, 1, 5])), Value::text("192.168.1.5/24"));
        assert_eq!(binary_value(&Conv::Inet, Some(&[2, 32, 0, 4, 10, 0, 0, 1])), Value::text("10.0.0.1"));
        assert_eq!(binary_value(&Conv::Cidr, Some(&[2, 32, 1, 4, 10, 0, 0, 1])), Value::text("10.0.0.1/32"));
        // bit(5) 10110
        let mut bits = 5i32.to_be_bytes().to_vec();
        bits.push(0b1011_0000);
        assert_eq!(binary_value(&Conv::Bit, Some(&bits)), Value::text("10110"));
        assert_eq!(binary_value(&Conv::Other, Some(&[1, 2])), Value::text("(binary) 0102"));
        assert_eq!(binary_value(&Conv::Char, Some(b"r")), Value::text("r"));
    }

    fn array_bytes(elem_oid: u32, dims: &[(i32, i32)], vals: &[Option<Vec<u8>>]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(dims.len() as i32).to_be_bytes());
        v.extend_from_slice(&(vals.iter().any(Option::is_none) as i32).to_be_bytes());
        v.extend_from_slice(&elem_oid.to_be_bytes());
        for (len, lb) in dims {
            v.extend_from_slice(&len.to_be_bytes());
            v.extend_from_slice(&lb.to_be_bytes());
        }
        for x in vals {
            match x {
                None => v.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(b) => {
                    v.extend_from_slice(&(b.len() as i32).to_be_bytes());
                    v.extend_from_slice(b);
                }
            }
        }
        v
    }

    #[test]
    fn binary_arrays() {
        let conv = Conv::Array(Box::new(Conv::Int4));
        let a = array_bytes(23, &[(3, 1)], &[Some(1i32.to_be_bytes().to_vec()), None, Some(3i32.to_be_bytes().to_vec())]);
        assert_eq!(binary_value(&conv, Some(&a)), Value::text("{1,NULL,3}"));
        let m = array_bytes(
            23,
            &[(2, 1), (2, 1)],
            &[1, 2, 3, 4].iter().map(|i: &i32| Some(i.to_be_bytes().to_vec())).collect::<Vec<_>>(),
        );
        assert_eq!(binary_value(&conv, Some(&m)), Value::text("{{1,2},{3,4}}"));
        let lb = array_bytes(23, &[(2, 0)], &[Some(5i32.to_be_bytes().to_vec()), Some(6i32.to_be_bytes().to_vec())]);
        assert_eq!(binary_value(&conv, Some(&lb)), Value::text("[0:1]={5,6}"));
        let t = Conv::Array(Box::new(Conv::Text));
        let ta = array_bytes(25, &[(4, 1)], &[Some(b"a b".to_vec()), Some(b"".to_vec()), Some(b"q\"x".to_vec()), Some(b"NULL".to_vec())]);
        assert_eq!(binary_value(&t, Some(&ta)), Value::text("{\"a b\",\"\",\"q\\\"x\",\"NULL\"}"));
        let empty = array_bytes(23, &[], &[]);
        assert_eq!(binary_value(&conv, Some(&empty)), Value::text("{}"));
    }

    #[test]
    fn binary_ranges() {
        // [1,10)
        let mut r = vec![0x02u8]; // lower inclusive
        r.extend_from_slice(&4i32.to_be_bytes());
        r.extend_from_slice(&1i32.to_be_bytes());
        r.extend_from_slice(&4i32.to_be_bytes());
        r.extend_from_slice(&10i32.to_be_bytes());
        let conv = Conv::Range(Box::new(Conv::Int4));
        assert_eq!(binary_value(&conv, Some(&r)), Value::text("[1,10)"));
        assert_eq!(binary_value(&conv, Some(&[0x01])), Value::text("empty"));
        // (,5] : lower unbounded (0x08) + upper inclusive (0x04)
        let mut u = vec![0x08u8 | 0x04];
        u.extend_from_slice(&4i32.to_be_bytes());
        u.extend_from_slice(&5i32.to_be_bytes());
        assert_eq!(binary_value(&conv, Some(&u)), Value::text("(,5]"));
        let mut mr = 2i32.to_be_bytes().to_vec();
        for part in [&r[..], &u[..]] {
            mr.extend_from_slice(&(part.len() as i32).to_be_bytes());
            mr.extend_from_slice(part);
        }
        assert_eq!(binary_value(&Conv::Multirange(Box::new(Conv::Int4)), Some(&mr)), Value::text("{[1,10),(,5]}"));
    }

    #[test]
    fn binary_composite() {
        let mut c = 2i32.to_be_bytes().to_vec();
        c.extend_from_slice(&23u32.to_be_bytes());
        c.extend_from_slice(&4i32.to_be_bytes());
        c.extend_from_slice(&42i32.to_be_bytes());
        c.extend_from_slice(&25u32.to_be_bytes());
        c.extend_from_slice(&5i32.to_be_bytes());
        c.extend_from_slice(b"a, b\"");
        let conv = Conv::Composite(vec![Conv::Int4, Conv::Text]);
        assert_eq!(binary_value(&conv, Some(&c)), Value::text("(42,\"a, b\"\"\")"));
    }

    #[test]
    fn float_text() {
        assert_eq!(pg_float_text(1e300), "1e+300");
        assert_eq!(pg_float_text(1.5e-7), "1.5e-07");
        assert_eq!(pg_float_text(0.25), "0.25");
        assert_eq!(pg_float_text(f64::NAN), "NaN");
    }
}
