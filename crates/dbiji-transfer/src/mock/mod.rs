//! Mock (test) data generation for a table (FR-D8).

mod words;

use crate::convert::{ConvertOptions, convert_value};
use crate::export::cancellable;
use crate::progress::Progress;
use crate::writer::{BatchWriter, OnError, RowError, TableTarget, TransactionScope};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Utc};
use dbiji_core::ddl::{ColumnDef, TableDef};
use dbiji_core::driver::{Session, query_all, query_scalar};
use dbiji_core::{DataKind, DbError, DbResult, ErrorKind, ObjectRef, Row, Value};
use rand::rngs::StdRng;
use rand::seq::IndexedRandom;
use rand::{RngExt, SeedableRng};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use words::*;

/// How values of one column are generated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GeneratorKind {
    /// Leave the column out of the `INSERT` (auto-increment, generated, defaults).
    #[default]
    Skip,
    /// Always NULL.
    Null,
    /// A fixed value (text, converted to the column type).
    Constant {
        /// The value.
        value: String,
    },
    /// `start`, `start + step`, ... With `after_max` the sequence continues
    /// after the largest value already in the table.
    Sequence {
        /// First value.
        start: i64,
        /// Increment.
        step: i64,
        /// Start after `MAX(column)`.
        #[serde(default)]
        after_max: bool,
    },
    /// Uniform integer in `min..=max`.
    IntRange {
        /// Smallest value.
        min: i64,
        /// Largest value.
        max: i64,
    },
    /// Uniform floating point number rounded to `decimals`.
    FloatRange {
        /// Smallest value.
        min: f64,
        /// Largest value.
        max: f64,
        /// Fraction digits.
        decimals: u32,
    },
    /// Uniform exact decimal with `scale` fraction digits.
    DecimalRange {
        /// Smallest value.
        min: f64,
        /// Largest value.
        max: f64,
        /// Fraction digits.
        scale: u32,
    },
    /// Boolean that is true with probability `true_ratio`.
    Bool {
        /// Probability of `true` (0..1).
        true_ratio: f64,
    },
    /// Date in `from..=to`.
    DateRange {
        /// Earliest date.
        from: NaiveDate,
        /// Latest date.
        to: NaiveDate,
    },
    /// Timestamp in `from..=to` (also used for times of day).
    DateTimeRange {
        /// Earliest timestamp.
        from: NaiveDateTime,
        /// Latest timestamp.
        to: NaiveDateTime,
    },
    /// Random (v4) UUID.
    Uuid,
    /// Lorem ipsum text between `min` and `max` characters.
    Text {
        /// Minimum length.
        min: usize,
        /// Maximum length.
        max: usize,
    },
    /// First name.
    FirstName,
    /// Last name.
    LastName,
    /// First and last name.
    FullName,
    /// Login name such as `ana.santos42`.
    UserName,
    /// E-mail address at an `example.*` domain.
    Email,
    /// Phone number in the fictional 555 range.
    Phone,
    /// City name.
    City,
    /// Country name.
    Country,
    /// Street address.
    StreetAddress,
    /// Company name.
    Company,
    /// URL at an `example.*` domain.
    Url,
    /// Private IPv4 address.
    IpV4,
    /// One lorem ipsum word.
    Word,
    /// `min..=max` lorem ipsum words.
    Words {
        /// Minimum number of words.
        min: usize,
        /// Maximum number of words.
        max: usize,
    },
    /// One sentence.
    Sentence,
    /// A few sentences.
    Paragraph,
    /// One of the given values.
    Enum {
        /// Candidates.
        values: Vec<String>,
    },
    /// A small JSON object.
    Json,
    /// Random bytes.
    Bytes {
        /// Minimum length.
        min: usize,
        /// Maximum length.
        max: usize,
    },
    /// Text from a pattern: `#` digit, `?` upper case letter, `*` letter or
    /// digit, `\` escapes the next character.
    Pattern {
        /// The pattern.
        pattern: String,
    },
    /// A value sampled from a column of another table. Generators with the
    /// same `group` and table take their values from the same referenced row
    /// (composite foreign keys).
    ForeignKey {
        /// Referenced table.
        table: ObjectRef,
        /// Referenced column.
        column: String,
        /// Group of columns sharing one referenced row.
        #[serde(default)]
        group: Option<String>,
    },
}

/// Generator of one column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ColumnGenerator {
    /// Column name.
    pub column: String,
    /// Value generator.
    pub generator: GeneratorKind,
    /// Probability of NULL (ignored for NOT NULL columns).
    pub null_ratio: f64,
    /// Make text values unique by appending the row number.
    pub unique: bool,
}

impl ColumnGenerator {
    /// Generator for `column`.
    pub fn new(column: impl Into<String>, generator: GeneratorKind) -> Self {
        ColumnGenerator { column: column.into(), generator, null_ratio: 0.0, unique: false }
    }
}

/// Whether existing rows are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MockMode {
    /// Add rows.
    #[default]
    Append,
    /// Delete existing rows first.
    TruncateFirst,
}

/// A mock data run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MockConfig {
    /// Target table definition (from `Session::table_definition`).
    pub table: TableDef,
    /// Rows to generate.
    pub rows: u64,
    /// Generators; columns without one are left out of the `INSERT`.
    pub generators: Vec<ColumnGenerator>,
    /// Rows per batch (one transaction each).
    pub batch_size: usize,
    /// Seed for reproducible data.
    pub seed: Option<u64>,
    /// Append or replace existing rows.
    pub mode: MockMode,
    /// Handling of rows the database rejects.
    pub on_error: OnError,
}

impl Default for MockConfig {
    fn default() -> Self {
        MockConfig { table: TableDef::default(), rows: 100, generators: Vec::new(), batch_size: 500, seed: None, mode: MockMode::Append, on_error: OnError::Abort }
    }
}

/// Result of a mock data run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MockSummary {
    /// Rows inserted.
    pub rows_inserted: u64,
    /// Rows rejected by the database.
    pub rows_failed: u64,
    /// Rejected rows.
    pub errors: Vec<RowError>,
    /// Warnings.
    pub warnings: Vec<String>,
    /// Wall clock time in milliseconds.
    pub duration_ms: u64,
}

// ---------------------------------------------------------------------------
// type helpers

/// `varchar(40)` → 40 for character types.
pub(crate) fn text_limit(c: &ColumnDef) -> Option<usize> {
    if !matches!(c.kind, DataKind::Text | DataKind::Enum) {
        return None;
    }
    let t = c.data_type.to_ascii_lowercase();
    let inner = t.split_once('(')?.1.split(')').next()?;
    inner.split(',').next()?.trim().parse::<usize>().ok().filter(|n| *n > 0)
}

/// `numeric(10,2)` → (10, Some(2)).
pub(crate) fn numeric_precision(c: &ColumnDef) -> Option<(u32, Option<u32>)> {
    let t = c.data_type.to_ascii_lowercase();
    let inner = t.split_once('(')?.1.split(')').next()?;
    let mut parts = inner.split(',').map(|p| p.trim().parse::<u32>().ok());
    let p = parts.next()??;
    Some((p, parts.next().flatten()))
}

/// Values of `enum('a','b')` / `set(...)` types.
fn enum_values(c: &ColumnDef) -> Option<Vec<String>> {
    let t = c.data_type.trim();
    let lower = t.to_ascii_lowercase();
    if !(lower.starts_with("enum(") || lower.starts_with("set(")) {
        return None;
    }
    let inner = &t[t.find('(')? + 1..t.rfind(')')?];
    let values: Vec<String> = inner.split(',').map(|v| v.trim().trim_matches('\'').trim_matches('"').replace("''", "'")).filter(|v| !v.is_empty()).collect();
    (!values.is_empty()).then_some(values)
}

fn int_upper_bound(c: &ColumnDef) -> i64 {
    let t = c.data_type.to_ascii_lowercase();
    if t.starts_with("tinyint") {
        if t.contains("unsigned") { 255 } else { 127 }
    } else if t.starts_with("smallint") || t.starts_with("int2") {
        32_767
    } else {
        10_000
    }
}

/// Pick generators for every column of `table` from names, types and constraints.
///
/// Auto-increment, identity, generated and sequence-defaulted columns are
/// skipped; foreign key columns sample the referenced table; single-column
/// primary or unique keys get sequences (numbers) or unique suffixes (texts);
/// text lengths and numeric precision of the column types are respected.
pub fn auto_generators(table: &TableDef) -> Vec<ColumnGenerator> {
    let now = Utc::now().naive_utc();
    let two_years_ago = now - Duration::days(730);
    let today = now.date();
    let year_start = NaiveDate::from_ymd_opt(today.year() - 5, 1, 1).unwrap_or(today);
    table
        .columns
        .iter()
        .map(|c| {
            let n = c.name.to_ascii_lowercase();
            let has = |w: &str| n == w || n.starts_with(&format!("{w}_")) || n.ends_with(&format!("_{w}")) || n.contains(&format!("_{w}_"));
            let unique_key = table.primary_key.iter().chain(table.unique_keys.iter()).any(|k| k.columns.len() == 1 && k.columns[0] == c.name);
            let mut g = ColumnGenerator::new(&c.name, GeneratorKind::Skip);
            let sequence_default = c.default.as_deref().is_some_and(|d| d.to_ascii_lowercase().contains("nextval("));
            if c.auto_increment || c.generated.is_some() || sequence_default {
                return g;
            }
            if let Some(fk) = table.foreign_keys.iter().find(|fk| fk.columns.iter().any(|x| x == &c.name))
                && let Some(pos) = fk.columns.iter().position(|x| x == &c.name)
                && let Some(ref_col) = fk.ref_columns.get(pos)
            {
                let schema = fk.ref_schema.clone().or_else(|| table.schema.clone());
                g.generator = GeneratorKind::ForeignKey {
                    table: ObjectRef::table(schema, fk.ref_table.clone()),
                    column: ref_col.clone(),
                    group: (fk.columns.len() > 1).then(|| fk.name.clone().unwrap_or_else(|| fk.ref_table.clone())),
                };
                return g;
            }
            let limit = text_limit(c);
            let textual = matches!(c.kind, DataKind::Text | DataKind::Enum);
            let numeric = c.kind.is_numeric();
            let temporal = matches!(c.kind, DataKind::Date | DataKind::DateTime | DataKind::DateTimeTz);
            let gen_kind = if c.kind == DataKind::Uuid || ((has("uuid") || has("guid")) && textual) {
                GeneratorKind::Uuid
            } else if textual && (has("email") || has("mail")) {
                GeneratorKind::Email
            } else if textual && matches!(n.as_str(), "first_name" | "firstname" | "fname" | "given_name" | "forename") {
                GeneratorKind::FirstName
            } else if textual && matches!(n.as_str(), "last_name" | "lastname" | "lname" | "surname" | "family_name") {
                GeneratorKind::LastName
            } else if textual && (has("username") || has("login") || n == "user_name") {
                GeneratorKind::UserName
            } else if textual
                && (n == "name" || matches!(n.as_str(), "full_name" | "fullname" | "display_name" | "contact_name" | "customer_name" | "person_name" | "employee_name" | "author"))
            {
                GeneratorKind::FullName
            } else if textual && (has("company") || has("organization") || has("organisation") || has("employer") || n == "company_name") {
                GeneratorKind::Company
            } else if textual && (has("phone") || has("mobile") || has("tel") || has("fax")) {
                GeneratorKind::Phone
            } else if textual && (has("city") || has("town")) {
                GeneratorKind::City
            } else if textual && has("country") {
                if limit.is_some_and(|l| l <= 3) { GeneratorKind::Enum { values: COUNTRY_CODES.iter().map(|s| s.to_string()).collect() } } else { GeneratorKind::Country }
            } else if textual && (has("address") || has("street")) {
                GeneratorKind::StreetAddress
            } else if textual && (has("url") || has("website") || has("homepage") || has("link")) {
                GeneratorKind::Url
            } else if c.kind == DataKind::Network || (textual && (has("ip") || has("ipv4") || n.contains("ip_address"))) {
                GeneratorKind::IpV4
            } else if textual && (has("zip") || has("postcode") || has("postal")) {
                GeneratorKind::Pattern { pattern: "#####".into() }
            } else if textual && has("status") {
                GeneratorKind::Enum { values: STATUSES.iter().map(|s| s.to_string()).collect() }
            } else if textual && (has("gender") || has("sex")) {
                GeneratorKind::Enum { values: GENDERS.iter().map(|s| s.to_string()).collect() }
            } else if textual && (has("color") || has("colour")) {
                GeneratorKind::Enum { values: COLORS.iter().map(|s| s.to_string()).collect() }
            } else if textual && has("currency") {
                GeneratorKind::Enum { values: CURRENCIES.iter().map(|s| s.to_string()).collect() }
            } else if textual && (has("password") || has("hash") || has("token") || has("secret")) {
                GeneratorKind::Pattern { pattern: "*".repeat(limit.unwrap_or(32).min(32)) }
            } else if textual && ["description", "comment", "comments", "notes", "note", "bio", "body", "content", "summary", "message", "remarks"].iter().any(|w| has(w)) {
                if limit.is_none_or(|l| l > 200) { GeneratorKind::Paragraph } else { GeneratorKind::Sentence }
            } else if textual && (has("title") || has("subject") || has("headline")) {
                GeneratorKind::Words { min: 2, max: 5 }
            } else if numeric && (has("lat") || has("latitude")) {
                GeneratorKind::FloatRange { min: -90.0, max: 90.0, decimals: 6 }
            } else if numeric && (has("lng") || has("lon") || has("longitude")) {
                GeneratorKind::FloatRange { min: -180.0, max: 180.0, decimals: 6 }
            } else if c.kind == DataKind::Int && has("age") {
                GeneratorKind::IntRange { min: 18, max: 90 }
            } else if c.kind == DataKind::Int && has("year") {
                GeneratorKind::IntRange { min: 1990, max: today.year() as i64 }
            } else if numeric && ["price", "amount", "total", "cost", "salary", "balance", "fee", "subtotal"].iter().any(|w| has(w)) {
                let scale = numeric_precision(c).and_then(|(_, s)| s).unwrap_or(2);
                GeneratorKind::DecimalRange { min: 1.0, max: 1000.0, scale: if c.kind == DataKind::Int { 0 } else { scale } }
            } else if c.kind == DataKind::Int && ["qty", "quantity", "count", "stock"].iter().any(|w| has(w)) {
                GeneratorKind::IntRange { min: 0, max: 100 }
            } else if temporal && (has("birth") || has("dob") || has("birthday") || has("birthdate")) {
                GeneratorKind::DateRange { from: NaiveDate::from_ymd_opt(1950, 1, 1).unwrap_or(today), to: NaiveDate::from_ymd_opt(2005, 12, 31).unwrap_or(today) }
            } else if temporal && c.kind != DataKind::Date {
                GeneratorKind::DateTimeRange { from: two_years_ago, to: now }
            } else {
                match c.kind {
                    DataKind::Bool => GeneratorKind::Bool { true_ratio: 0.5 },
                    DataKind::Int if unique_key => GeneratorKind::Sequence { start: 1, step: 1, after_max: true },
                    DataKind::Int => GeneratorKind::IntRange { min: 1, max: int_upper_bound(c) },
                    DataKind::Decimal => {
                        let (p, s) = numeric_precision(c).map(|(p, s)| (p, s.unwrap_or(0))).unwrap_or((10, 2));
                        let int_digits = p.saturating_sub(s).min(6) as i32;
                        let max = (10f64.powi(int_digits) - 1.0).max(0.0);
                        GeneratorKind::DecimalRange { min: 0.0, max: max.max(if s > 0 { 0.9 } else { 1.0 }), scale: s }
                    }
                    DataKind::Float => GeneratorKind::FloatRange { min: 0.0, max: 1000.0, decimals: 2 },
                    DataKind::Date => GeneratorKind::DateRange { from: year_start, to: today },
                    DataKind::DateTime | DataKind::DateTimeTz | DataKind::Time => GeneratorKind::DateTimeRange { from: two_years_ago, to: now },
                    DataKind::Json => GeneratorKind::Json,
                    DataKind::Binary => GeneratorKind::Bytes { min: 8, max: 32 },
                    DataKind::Enum => match enum_values(c) {
                        Some(values) => GeneratorKind::Enum { values },
                        None => GeneratorKind::Word,
                    },
                    DataKind::Text => match limit {
                        Some(l) if l < 5 => GeneratorKind::Pattern { pattern: "?".repeat(l) },
                        Some(l) => GeneratorKind::Text { min: 5.min(l), max: l.min(40) },
                        None => GeneratorKind::Words { min: 1, max: 4 },
                    },
                    _ if c.nullable => GeneratorKind::Null,
                    _ => GeneratorKind::Skip,
                }
            };
            g.generator = gen_kind;
            g.unique = unique_key && textual && !matches!(g.generator, GeneratorKind::Uuid | GeneratorKind::Enum { .. });
            g
        })
        .collect()
}

// ---------------------------------------------------------------------------
// generation

fn int_in(rng: &mut StdRng, a: i64, b: i64) -> i64 {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    rng.random_range(lo..=hi)
}

fn usize_in(rng: &mut StdRng, a: usize, b: usize) -> usize {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    rng.random_range(lo..=hi)
}

fn float_in(rng: &mut StdRng, a: f64, b: f64) -> f64 {
    if !a.is_finite() || !b.is_finite() {
        return 0.0;
    }
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    lo + (hi - lo) * rng.random::<f64>()
}

fn chance(rng: &mut StdRng, p: f64) -> bool {
    let p = if p.is_nan() { 0.0 } else { p.clamp(0.0, 1.0) };
    rng.random_bool(p)
}

fn pick<'a>(rng: &mut StdRng, items: &'a [&'a str]) -> &'a str {
    items.choose(rng).copied().unwrap_or("")
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn words(rng: &mut StdRng, n: usize) -> String {
    (0..n).map(|_| pick(rng, LOREM)).collect::<Vec<_>>().join(" ")
}

fn sentence(rng: &mut StdRng) -> String {
    let n = usize_in(rng, 6, 14);
    format!("{}.", capitalize(&words(rng, n)))
}

fn pattern(rng: &mut StdRng, p: &str) -> String {
    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(p.len());
    let mut chars = p.chars();
    while let Some(c) = chars.next() {
        match c {
            '#' => out.push(char::from(b'0' + rng.random_range(0..10u8))),
            '?' => out.push(char::from(b'A' + rng.random_range(0..26u8))),
            '*' => out.push(char::from(ALNUM[rng.random_range(0..ALNUM.len())])),
            '\\' => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            c => out.push(c),
        }
    }
    out
}

fn decimal_text(units: i128, scale: u32) -> String {
    if scale == 0 {
        return units.to_string();
    }
    let neg = units < 0;
    let digits = units.unsigned_abs().to_string();
    let scale = scale as usize;
    let padded = if digits.len() <= scale { format!("{}{digits}", "0".repeat(scale + 1 - digits.len())) } else { digits };
    let (int_part, frac) = padded.split_at(padded.len() - scale);
    format!("{}{int_part}.{frac}", if neg { "-" } else { "" })
}

struct State {
    rng: StdRng,
    sequences: Vec<i64>,
    /// Sampled referenced rows by group key.
    fk_samples: BTreeMap<String, Vec<Row>>,
}

fn fk_key(table: &ObjectRef, column: &str, group: &Option<String>) -> String {
    match group {
        Some(g) => format!("{}#{g}", table.display_path()),
        None => format!("{}.{column}", table.display_path()),
    }
}

/// Generate the raw value of one column for row `row_no` (1-based).
fn generate(kind: &GeneratorKind, slot: usize, row_no: u64, st: &mut State, fk_pick: &BTreeMap<String, usize>, fk_cols: &BTreeMap<String, Vec<String>>) -> Value {
    let rng = &mut st.rng;
    match kind {
        GeneratorKind::Skip | GeneratorKind::Null => Value::Null,
        GeneratorKind::Constant { value } => Value::text(value.clone()),
        GeneratorKind::Sequence { step, .. } => {
            let v = st.sequences.get(slot).copied().unwrap_or(1);
            if let Some(s) = st.sequences.get_mut(slot) {
                *s = s.saturating_add(*step);
            }
            Value::Int(v)
        }
        GeneratorKind::IntRange { min, max } => Value::Int(int_in(rng, *min, *max)),
        GeneratorKind::FloatRange { min, max, decimals } => {
            let f = float_in(rng, *min, *max);
            let m = 10f64.powi((*decimals).min(15) as i32);
            Value::Float((f * m).round() / m)
        }
        GeneratorKind::DecimalRange { min, max, scale } => {
            let scale = (*scale).min(18);
            let m = 10f64.powi(scale as i32);
            let lo = (min * m).ceil();
            let hi = (max * m).floor();
            let (lo, hi) = if lo.is_finite() && hi.is_finite() { (lo as i128, hi as i128) } else { (0, 0) };
            let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
            let units = rng.random_range(lo..=hi);
            Value::decimal(decimal_text(units, scale))
        }
        GeneratorKind::Bool { true_ratio } => Value::Bool(chance(rng, *true_ratio)),
        GeneratorKind::DateRange { from, to } => {
            let days = to.signed_duration_since(*from).num_days();
            Value::Date(from.checked_add_signed(Duration::days(int_in(rng, 0, days))).unwrap_or(*from))
        }
        GeneratorKind::DateTimeRange { from, to } => {
            let secs = to.signed_duration_since(*from).num_seconds();
            Value::DateTime(from.checked_add_signed(Duration::seconds(int_in(rng, 0, secs))).unwrap_or(*from))
        }
        GeneratorKind::Uuid => {
            let mut b = [0u8; 16];
            rng.fill(&mut b[..]);
            Value::Uuid(uuid::Builder::from_random_bytes(b).into_uuid())
        }
        GeneratorKind::Text { min, max } => {
            let target = usize_in(rng, *min, (*max).max(*min));
            let mut s = String::new();
            while s.chars().count() < target {
                if !s.is_empty() {
                    s.push(' ');
                }
                s.push_str(pick(rng, LOREM));
            }
            let mut s: String = s.chars().take(target).collect();
            if s.ends_with(' ') {
                s.pop();
                s.push('x');
            }
            Value::text(capitalize(&s))
        }
        GeneratorKind::FirstName => Value::text(pick(rng, FIRST_NAMES)),
        GeneratorKind::LastName => Value::text(pick(rng, LAST_NAMES)),
        GeneratorKind::FullName => Value::text(format!("{} {}", pick(rng, FIRST_NAMES), pick(rng, LAST_NAMES))),
        GeneratorKind::UserName => {
            let first = pick(rng, FIRST_NAMES).to_lowercase();
            let last = pick(rng, LAST_NAMES).to_lowercase();
            let sep = if chance(rng, 0.5) { "." } else { "_" };
            Value::text(format!("{first}{sep}{last}{}", int_in(rng, 1, 99)))
        }
        GeneratorKind::Email => {
            let first = pick(rng, FIRST_NAMES).to_lowercase();
            let last = pick(rng, LAST_NAMES).to_lowercase();
            Value::text(format!("{first}.{last}{}@{}", int_in(rng, 1, 999), pick(rng, DOMAINS)))
        }
        GeneratorKind::Phone => Value::text(pattern(rng, "+1 555-01##-####")),
        GeneratorKind::City => Value::text(pick(rng, CITIES)),
        GeneratorKind::Country => Value::text(pick(rng, COUNTRIES)),
        GeneratorKind::StreetAddress => Value::text(format!("{} {} {}", int_in(rng, 1, 9999), pick(rng, STREET_WORDS), pick(rng, STREET_SUFFIXES))),
        GeneratorKind::Company => Value::text(format!("{} {} {}", pick(rng, COMPANY_WORDS), pick(rng, COMPANY_NOUNS), pick(rng, COMPANY_SUFFIXES))),
        GeneratorKind::Url => Value::text(format!("https://{}.{}/{}", pick(rng, LOREM), pick(rng, DOMAINS), pick(rng, LOREM))),
        GeneratorKind::IpV4 => {
            let v = match int_in(rng, 0, 2) {
                0 => format!("10.{}.{}.{}", int_in(rng, 0, 255), int_in(rng, 0, 255), int_in(rng, 1, 254)),
                1 => format!("172.{}.{}.{}", int_in(rng, 16, 31), int_in(rng, 0, 255), int_in(rng, 1, 254)),
                _ => format!("192.168.{}.{}", int_in(rng, 0, 255), int_in(rng, 1, 254)),
            };
            Value::text(v)
        }
        GeneratorKind::Word => Value::text(pick(rng, LOREM)),
        GeneratorKind::Words { min, max } => {
            let n = usize_in(rng, (*min).max(1), (*max).max(1));
            Value::text(capitalize(&words(rng, n)))
        }
        GeneratorKind::Sentence => Value::text(sentence(rng)),
        GeneratorKind::Paragraph => {
            let n = usize_in(rng, 3, 6);
            Value::text((0..n).map(|_| sentence(rng)).collect::<Vec<_>>().join(" "))
        }
        GeneratorKind::Enum { values } => match values.choose(rng) {
            Some(v) => Value::text(v.clone()),
            None => Value::Null,
        },
        GeneratorKind::Json => {
            let word = pick(rng, LOREM);
            let score = (float_in(rng, 0.0, 100.0) * 100.0).round() / 100.0;
            let active = chance(rng, 0.5);
            Value::json(format!("{{\"id\":{row_no},\"tag\":\"{word}\",\"score\":{score},\"active\":{active}}}"))
        }
        GeneratorKind::Bytes { min, max } => {
            let n = usize_in(rng, *min, (*max).max(*min)).min(1 << 20);
            let mut b = vec![0u8; n];
            rng.fill(&mut b[..]);
            Value::bytes(b)
        }
        GeneratorKind::Pattern { pattern: p } => Value::text(pattern(rng, p)),
        GeneratorKind::ForeignKey { table, column, group } => {
            let key = fk_key(table, column, group);
            let Some(rows) = st.fk_samples.get(&key) else { return Value::Null };
            let Some(&idx) = fk_pick.get(&key) else { return Value::Null };
            let pos = fk_cols.get(&key).and_then(|cols| cols.iter().position(|c| c == column)).unwrap_or(0);
            rows.get(idx).and_then(|r| r.get(pos)).cloned().unwrap_or(Value::Null)
        }
    }
}

/// Make a text unique by appending `_n` (`.n` before the `@` of e-mail
/// addresses) and fit it into `limit` characters by shortening the part before
/// the suffix. The suffix always follows the last separator, so different
/// `n` give different texts.
fn make_unique(v: Value, n: u64, limit: Option<usize>) -> Value {
    let Value::Text(s) = v else { return v };
    let (base, tail) = match s.split_once('@') {
        Some((local, domain)) => (local.to_string(), format!(".{n}@{domain}")),
        None => (s.to_string(), format!("_{n}")),
    };
    let text = match limit {
        Some(l) => {
            let keep = l.saturating_sub(tail.chars().count());
            format!("{}{tail}", base.chars().take(keep).collect::<String>())
        }
        None => format!("{base}{tail}"),
    };
    Value::text(text)
}

/// Fit a text into `limit` characters; e-mail addresses keep their domain.
fn fit_limit(v: Value, limit: Option<usize>) -> Value {
    let (Value::Text(s), Some(limit)) = (&v, limit) else { return v };
    if s.chars().count() <= limit {
        return v;
    }
    if let Some((local, domain)) = s.split_once('@') {
        let tail = domain.chars().count() + 1;
        if tail < limit {
            let local: String = local.chars().take(limit - tail).collect();
            return Value::text(format!("{local}@{domain}"));
        }
    }
    Value::text(s.chars().take(limit).collect::<String>())
}

/// Generate `config.rows` rows and insert them into `config.table`.
pub async fn run_mock(config: &MockConfig, session: &mut dyn Session, progress: &Progress, cancel: &CancellationToken) -> DbResult<MockSummary> {
    let started = Instant::now();
    let dialect = session.dialect();
    let table_ref = ObjectRef::table(config.table.schema.clone(), config.table.name.clone());
    let item = table_ref.display_path();
    let mut summary = MockSummary::default();

    // Active generators with their column definitions.
    let mut active: Vec<(&ColumnGenerator, &ColumnDef)> = Vec::new();
    for g in &config.generators {
        if g.generator == GeneratorKind::Skip {
            continue;
        }
        let col = config.table.columns.iter().find(|c| c.name == g.column).ok_or_else(|| DbError::config(format!("Column '{}' does not exist in {item}", g.column)))?;
        if active.iter().any(|(a, _)| a.column == g.column) {
            return Err(DbError::config(format!("Column '{}' has two generators", g.column)));
        }
        active.push((g, col));
    }
    if active.is_empty() {
        return Err(DbError::config("No column has a generator"));
    }
    let handle = session.cancel_handle();
    let rng = match config.seed {
        Some(s) => StdRng::seed_from_u64(s),
        None => StdRng::from_rng(&mut rand::rng()),
    };

    // Truncate first (before sequences look at MAX()).
    if config.mode == MockMode::TruncateFirst {
        let truncate = dialect.truncate_table(&table_ref);
        if let Err(e) = cancellable(dbiji_core::driver::execute_update(session, &truncate, &[]), cancel, handle.clone()).await {
            if e.kind == ErrorKind::Cancelled {
                return Err(e);
            }
            let delete = format!("DELETE FROM {}", dialect.qualified_ref(&table_ref));
            cancellable(dbiji_core::driver::execute_update(session, &delete, &[]), cancel, handle.clone()).await?;
        }
    }

    // Unique texts continue after the rows already in the table.
    let mut unique_offset = 0u64;
    if active.iter().any(|(g, _)| g.unique) {
        let sql = format!("SELECT COUNT(*) FROM {}", dialect.qualified_ref(&table_ref));
        let n = cancellable(query_scalar(session, &sql, &[]), cancel, handle.clone()).await?;
        unique_offset = n.as_i64().and_then(|n| u64::try_from(n).ok()).unwrap_or(0);
    }

    // Sequences.
    let mut sequences = vec![0i64; active.len()];
    for (i, (g, col)) in active.iter().enumerate() {
        if let GeneratorKind::Sequence { start, step, after_max } = &g.generator {
            sequences[i] = *start;
            if *after_max {
                let sql = format!("SELECT MAX({}) FROM {}", dialect.quote_ident(&col.name), dialect.qualified_ref(&table_ref));
                let max = cancellable(query_scalar(session, &sql, &[]), cancel, handle.clone()).await?;
                if let Some(m) = max.as_i64() {
                    sequences[i] = sequences[i].max(m.saturating_add(*step));
                }
            }
        }
    }

    // Foreign key samples.
    let mut fk_samples: BTreeMap<String, Vec<Row>> = BTreeMap::new();
    let mut fk_cols: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (g, _) in &active {
        if let GeneratorKind::ForeignKey { table, column, group } = &g.generator {
            let key = fk_key(table, column, group);
            let cols = fk_cols.entry(key).or_default();
            if !cols.contains(column) {
                cols.push(column.clone());
            }
        }
    }
    for (key, cols) in &fk_cols {
        let Some((table, _)) = active.iter().find_map(|(g, _)| match &g.generator {
            GeneratorKind::ForeignKey { table, column, group } if &fk_key(table, column, group) == key => Some((table, column)),
            _ => None,
        }) else {
            continue;
        };
        let list = cols.iter().map(|c| dialect.quote_ident(c)).collect::<Vec<_>>().join(", ");
        let sql = dialect.limit_query(&format!("SELECT DISTINCT {list} FROM {}", dialect.qualified_ref(table)), 10_000, 0);
        let (_, rows) = cancellable(query_all(session, &sql, &[]), cancel, handle.clone()).await?;
        let rows: Vec<Row> = rows.into_iter().filter(|r| r.iter().all(|v| !v.is_null())).collect();
        if rows.is_empty() {
            let required =
                active.iter().any(|(g, c)| matches!(&g.generator, GeneratorKind::ForeignKey { table, column, group } if &fk_key(table, column, group) == key) && !c.nullable);
            if required {
                return Err(DbError::config(format!("The referenced table {} has no rows to reference", table.display_path())));
            }
            summary.warnings.push(format!("{} has no rows; the referencing columns are NULL", table.display_path()));
        }
        fk_samples.insert(key.clone(), rows);
    }

    let mut st = State { rng, sequences, fk_samples };
    let target = TableTarget { table_sql: dialect.qualified_ref(&table_ref), columns: active.iter().map(|(_, c)| dialect.quote_ident(&c.name)).collect(), key_index: Vec::new() };
    let limits: Vec<Option<usize>> = active.iter().map(|(_, c)| text_limit(c)).collect();
    let convert = ConvertOptions::default();
    progress.set_total_rows(Some(config.rows));
    progress.item_started(&item);
    let batch_size = config.batch_size.max(1);
    let mut writer = BatchWriter::new(session, target, TransactionScope::PerBatch, config.on_error, batch_size, item.clone());
    writer.begin().await?;
    let outcome = async {
        let mut produced = 0u64;
        while produced < config.rows {
            if cancel.is_cancelled() {
                return Err(DbError::cancelled());
            }
            let n = (config.rows - produced).min(batch_size as u64);
            let mut batch = Vec::with_capacity(n as usize);
            for _ in 0..n {
                produced += 1;
                let mut fk_pick: BTreeMap<String, usize> = BTreeMap::new();
                for (key, rows) in &st.fk_samples {
                    if !rows.is_empty() {
                        let idx = st.rng.random_range(0..rows.len());
                        fk_pick.insert(key.clone(), idx);
                    }
                }
                let mut row: Row = Vec::with_capacity(active.len());
                for (slot, (g, col)) in active.iter().enumerate() {
                    let null = col.nullable && g.null_ratio > 0.0 && chance(&mut st.rng, g.null_ratio);
                    let raw = if null { Value::Null } else { generate(&g.generator, slot, produced, &mut st, &fk_pick, &fk_cols) };
                    let raw = if g.unique { make_unique(raw, unique_offset + produced, limits[slot]) } else { fit_limit(raw, limits[slot]) };
                    let value = convert_value(&raw, col.kind, &convert).map_err(|e| DbError::config(format!("Column '{}': generated value does not fit: {e}", col.name)))?;
                    row.push(value);
                }
                batch.push((produced, row));
            }
            writer.write(batch, progress).await?;
            writer.end_batch().await?;
            progress.add_rows(n);
        }
        Ok(())
    }
    .await;
    progress.item_finished(&item);
    match outcome {
        Ok(()) => writer.finish().await?,
        Err(e) => {
            writer.abort().await;
            if e.kind == ErrorKind::Cancelled
                && let Some(h) = handle
            {
                let _ = h.cancel().await;
            }
            return Err(crate::error::with_item(e, &item));
        }
    }
    summary.rows_inserted = writer.written;
    summary.rows_failed = writer.failed;
    summary.errors = std::mem::take(&mut writer.errors);
    summary.warnings.extend(std::mem::take(&mut writer.warnings));
    summary.duration_ms = started.elapsed().as_millis() as u64;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::ddl::{ForeignKeyDef, KeyDef};

    fn table() -> TableDef {
        let mut t = TableDef::new(None, "customers");
        t.columns.push(ColumnDef::new("id", "integer").not_null().auto_increment());
        t.columns.push(ColumnDef::new("code", "integer").not_null());
        t.columns.push(ColumnDef::new("email", "varchar(30)").not_null());
        t.columns.push(ColumnDef::new("first_name", "varchar(50)"));
        t.columns.push(ColumnDef::new("name", "varchar(80)"));
        t.columns.push(ColumnDef::new("phone", "varchar(20)"));
        t.columns.push(ColumnDef::new("city", "text"));
        t.columns.push(ColumnDef::new("status", "varchar(10)"));
        t.columns.push(ColumnDef::new("balance", "numeric(8,2)"));
        t.columns.push(ColumnDef::new("created_at", "timestamp"));
        t.columns.push(ColumnDef::new("birth_date", "date"));
        t.columns.push(ColumnDef::new("token", "uuid"));
        t.columns.push(ColumnDef::new("country_id", "integer"));
        t.columns.push(ColumnDef::new("flag", "boolean"));
        t.columns.push(ColumnDef::new("notes", "text"));
        t.columns.push(ColumnDef::new("tiny", "tinyint"));
        t.primary_key = Some(KeyDef { name: None, columns: vec!["id".into()] });
        t.unique_keys.push(KeyDef { name: None, columns: vec!["code".into()] });
        t.unique_keys.push(KeyDef { name: None, columns: vec!["email".into()] });
        t.foreign_keys.push(ForeignKeyDef { columns: vec!["country_id".into()], ref_table: "countries".into(), ref_columns: vec!["id".into()], ..Default::default() });
        t
    }

    #[test]
    fn heuristics() {
        let g = auto_generators(&table());
        let by = |n: &str| g.iter().find(|x| x.column == n).unwrap();
        assert_eq!(by("id").generator, GeneratorKind::Skip);
        assert_eq!(by("code").generator, GeneratorKind::Sequence { start: 1, step: 1, after_max: true });
        assert_eq!(by("email").generator, GeneratorKind::Email);
        assert!(by("email").unique);
        assert_eq!(by("first_name").generator, GeneratorKind::FirstName);
        assert_eq!(by("name").generator, GeneratorKind::FullName);
        assert_eq!(by("phone").generator, GeneratorKind::Phone);
        assert_eq!(by("city").generator, GeneratorKind::City);
        assert!(matches!(by("status").generator, GeneratorKind::Enum { .. }));
        assert!(matches!(by("balance").generator, GeneratorKind::DecimalRange { scale: 2, .. }));
        assert!(matches!(by("created_at").generator, GeneratorKind::DateTimeRange { .. }));
        assert!(matches!(by("birth_date").generator, GeneratorKind::DateRange { .. }));
        assert_eq!(by("token").generator, GeneratorKind::Uuid);
        assert!(matches!(by("country_id").generator, GeneratorKind::ForeignKey { .. }));
        assert!(matches!(by("flag").generator, GeneratorKind::Bool { .. }));
        assert_eq!(by("notes").generator, GeneratorKind::Paragraph);
        assert_eq!(by("tiny").generator, GeneratorKind::IntRange { min: 1, max: 127 });
    }

    #[test]
    fn deterministic_with_seed_and_shapes() {
        let mk = || State { rng: StdRng::seed_from_u64(7), sequences: vec![5], fk_samples: BTreeMap::new() };
        let (mut a, mut b) = (mk(), mk());
        let none = BTreeMap::new();
        let none_cols = BTreeMap::new();
        for k in [GeneratorKind::Email, GeneratorKind::Paragraph, GeneratorKind::Uuid, GeneratorKind::IpV4] {
            assert_eq!(generate(&k, 0, 1, &mut a, &none, &none_cols), generate(&k, 0, 1, &mut b, &none, &none_cols));
        }
        let seq = GeneratorKind::Sequence { start: 5, step: 2, after_max: false };
        assert_eq!(generate(&seq, 0, 1, &mut a, &none, &none_cols), Value::Int(5));
        assert_eq!(generate(&seq, 0, 2, &mut a, &none, &none_cols), Value::Int(7));
        let d = generate(&GeneratorKind::DecimalRange { min: -5.0, max: 5.0, scale: 3 }, 0, 1, &mut a, &none, &none_cols);
        let text = d.to_text();
        assert_eq!(text.split('.').nth(1).map(str::len), Some(3), "{text}");
        let p = generate(&GeneratorKind::Pattern { pattern: "AB-##\\#?".into() }, 0, 1, &mut a, &none, &none_cols).to_text().into_owned();
        assert!(p.starts_with("AB-") && p.len() == 7 && p.contains('#'), "{p}");
        let t = generate(&GeneratorKind::Text { min: 10, max: 10 }, 0, 1, &mut a, &none, &none_cols).to_text().into_owned();
        assert_eq!(t.chars().count(), 10, "{t}");
    }

    #[test]
    fn helpers() {
        assert_eq!(decimal_text(5, 2), "0.05");
        assert_eq!(decimal_text(-12345, 2), "-123.45");
        assert_eq!(decimal_text(7, 0), "7");
        assert_eq!(make_unique(Value::text("ann@example.com"), 12, Some(15)).to_text(), ".12@example.com");
        assert_eq!(make_unique(Value::text("ann@example.com"), 12, Some(17)).to_text(), "an.12@example.com");
        assert_eq!(make_unique(Value::text("ann@example.com"), 12, None).to_text(), "ann.12@example.com");
        assert_eq!(make_unique(Value::text("abcdef"), 3, Some(5)).to_text(), "abc_3");
        assert_eq!(fit_limit(Value::text("someone.long@example.com"), Some(16)).to_text(), "some@example.com");
        assert_eq!(fit_limit(Value::text("abcdef"), Some(3)).to_text(), "abc");
        assert_eq!(fit_limit(Value::Int(5), Some(1)), Value::Int(5));
        let c = ColumnDef::new("x", "character varying(12)");
        assert_eq!(text_limit(&c), Some(12));
        assert_eq!(numeric_precision(&ColumnDef::new("n", "numeric(10, 3)")), Some((10, Some(3))));
        let e = ColumnDef { kind: DataKind::Enum, ..ColumnDef::new("e", "enum('a','b''c')") };
        assert_eq!(enum_values(&e), Some(vec!["a".to_string(), "b'c".to_string()]));
    }
}
