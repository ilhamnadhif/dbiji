//! [`CassandraSession`] over the `scylla` driver (Cassandra and ScyllaDB).

use crate::dialect::{CqlDialect, qualified, quote};
use async_trait::async_trait;
use chrono::{NaiveDate, NaiveTime, TimeZone, Utc};
use dbiji_core::config::SslMode;
use dbiji_core::ddl::*;
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{ServerInfo, parse_version_number};
use dbiji_core::meta::*;
use dbiji_core::plan::QueryPlan;
use dbiji_core::value::{ColumnMeta, DataKind};
use dbiji_core::{
    CancelHandle, ConnectParams, DbError, DbKind, DbResult, ErrorKind, ExecOptions, QueryCursor,
    ResultHeader, Row, ServerMessage, Session, TransactionMode, Value,
};
use futures::StreamExt;
use scylla::client::pager::TypedRowStream;
use scylla::client::session::Session as Scylla;
use scylla::client::session_builder::SessionBuilder;
use scylla::frame::response::result::{CollectionType, ColumnType, NativeType};
use scylla::statement::unprepared::Statement;
use scylla::value::{CqlDate, CqlTime, CqlTimestamp, CqlValue, Row as CqlRow};
use std::sync::Arc;
use std::time::Duration;

/// Rows per page requested from the server.
const PAGE_SIZE: i32 = 1000;

fn err(e: impl std::fmt::Display) -> DbError {
    let text = e.to_string();
    let lower = text.to_ascii_lowercase();
    let kind = if lower.contains("syntax error")
        || lower.contains("syntaxerror")
        || lower.contains("no viable alternative")
        || lower.contains("mismatched input")
    {
        ErrorKind::Syntax
    } else if lower.contains("authentication")
        || lower.contains("bad credentials")
        || lower.contains("username and/or password")
    {
        ErrorKind::Auth
    } else if lower.contains("could not connect")
        || lower.contains("connection")
            && (lower.contains("refused")
                || lower.contains("broken")
                || lower.contains("closed")
                || lower.contains("pool"))
        || lower.contains("timed out") && lower.contains("connect")
    {
        ErrorKind::Connection
    } else if lower.contains("timeout") || lower.contains("timed out") {
        ErrorKind::Timeout
    } else {
        ErrorKind::Query
    };
    DbError::new(kind, text)
}

// ------------------------------------------------------------- conversions

fn type_name(t: &ColumnType<'_>) -> String {
    match t {
        ColumnType::Native(n) => native_name(n).into(),
        ColumnType::Collection { frozen, typ } => {
            let inner = match typ {
                CollectionType::List(t) => format!("list<{}>", type_name(t)),
                CollectionType::Set(t) => format!("set<{}>", type_name(t)),
                CollectionType::Map(k, v) => format!("map<{}, {}>", type_name(k), type_name(v)),
                _ => "collection".into(),
            };
            if *frozen {
                format!("frozen<{inner}>")
            } else {
                inner
            }
        }
        ColumnType::Vector { typ, dimensions } => {
            format!("vector<{}, {dimensions}>", type_name(typ))
        }
        ColumnType::UserDefinedType { definition, .. } => definition.name.to_string(),
        ColumnType::Tuple(ts) => format!(
            "tuple<{}>",
            ts.iter().map(type_name).collect::<Vec<_>>().join(", ")
        ),
        _ => "unknown".into(),
    }
}

fn native_name(n: &NativeType) -> &'static str {
    match n {
        NativeType::Ascii => "ascii",
        NativeType::Boolean => "boolean",
        NativeType::Blob => "blob",
        NativeType::Counter => "counter",
        NativeType::Date => "date",
        NativeType::Decimal => "decimal",
        NativeType::Double => "double",
        NativeType::Duration => "duration",
        NativeType::Float => "float",
        NativeType::Int => "int",
        NativeType::BigInt => "bigint",
        NativeType::Text => "text",
        NativeType::Timestamp => "timestamp",
        NativeType::Inet => "inet",
        NativeType::SmallInt => "smallint",
        NativeType::TinyInt => "tinyint",
        NativeType::Time => "time",
        NativeType::Timeuuid => "timeuuid",
        NativeType::Uuid => "uuid",
        NativeType::Varint => "varint",
        _ => "unknown",
    }
}

fn kind_of(t: &ColumnType<'_>) -> DataKind {
    match t {
        ColumnType::Native(n) => match n {
            NativeType::Ascii | NativeType::Text => DataKind::Text,
            NativeType::Boolean => DataKind::Bool,
            NativeType::Blob => DataKind::Binary,
            NativeType::Counter
            | NativeType::Int
            | NativeType::BigInt
            | NativeType::SmallInt
            | NativeType::TinyInt => DataKind::Int,
            NativeType::Varint | NativeType::Decimal => DataKind::Decimal,
            NativeType::Double | NativeType::Float => DataKind::Float,
            NativeType::Date => DataKind::Date,
            NativeType::Time => DataKind::Time,
            NativeType::Timestamp => DataKind::DateTimeTz,
            NativeType::Duration => DataKind::Interval,
            NativeType::Inet => DataKind::Network,
            NativeType::Uuid | NativeType::Timeuuid => DataKind::Uuid,
            _ => DataKind::Other,
        },
        ColumnType::Collection { .. } | ColumnType::Vector { .. } | ColumnType::Tuple(_) => {
            DataKind::Array
        }
        ColumnType::UserDefinedType { .. } => DataKind::Json,
        _ => DataKind::Other,
    }
}

/// DataKind of a `system_schema.columns.type` text.
fn kind_of_text(t: &str) -> DataKind {
    match t.trim() {
        "ascii" | "text" | "varchar" => DataKind::Text,
        "boolean" => DataKind::Bool,
        "blob" => DataKind::Binary,
        "counter" | "int" | "bigint" | "smallint" | "tinyint" => DataKind::Int,
        "varint" | "decimal" => DataKind::Decimal,
        "double" | "float" => DataKind::Float,
        "date" => DataKind::Date,
        "time" => DataKind::Time,
        "timestamp" => DataKind::DateTimeTz,
        "duration" => DataKind::Interval,
        "inet" => DataKind::Network,
        "uuid" | "timeuuid" => DataKind::Uuid,
        t if t.starts_with("list<")
            || t.starts_with("set<")
            || t.starts_with("map<")
            || t.starts_with("frozen<")
            || t.starts_with("tuple<")
            || t.starts_with("vector<") =>
        {
            DataKind::Array
        }
        _ => DataKind::Other,
    }
}

/// CQL dates count days from 2^31 days before the Unix epoch.
const DATE_EPOCH: i64 = 1 << 31;

fn date(d: CqlDate) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(1970, 1, 1)?
        .checked_add_signed(chrono::Duration::days(d.0 as i64 - DATE_EPOCH))
}

fn cql_date(d: NaiveDate) -> Option<CqlDate> {
    let days = (d - NaiveDate::from_ymd_opt(1970, 1, 1)?).num_days() + DATE_EPOCH;
    u32::try_from(days).ok().map(CqlDate)
}

/// CQL times are nanoseconds since midnight.
fn time(t: CqlTime) -> Option<NaiveTime> {
    let secs = u32::try_from(t.0 / 1_000_000_000).ok()?;
    NaiveTime::from_num_seconds_from_midnight_opt(secs, (t.0 % 1_000_000_000) as u32)
}

fn cql_time(t: NaiveTime) -> CqlTime {
    use chrono::Timelike;
    CqlTime(
        t.num_seconds_from_midnight() as i64 * 1_000_000_000
            + t.nanosecond().min(999_999_999) as i64,
    )
}

fn json(v: &CqlValue) -> serde_json::Value {
    use serde_json::Value as J;
    match to_value(v) {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(b),
        Value::Int(i) => J::from(i),
        Value::Float(f) => serde_json::Number::from_f64(f).map_or(J::Null, J::Number),
        Value::Json(s) => serde_json::from_str(&s).unwrap_or(J::String(s.into())),
        other => J::String(other.to_text().into_owned()),
    }
}

fn to_value(v: &CqlValue) -> Value {
    match v {
        CqlValue::Ascii(s) | CqlValue::Text(s) => Value::Text(s.as_str().into()),
        CqlValue::Boolean(b) => Value::Bool(*b),
        CqlValue::Blob(b) => Value::Bytes(b.clone().into()),
        CqlValue::Counter(c) => Value::Int(c.0),
        CqlValue::Decimal(d) => Value::Decimal(decimal_text(d).into()),
        CqlValue::Varint(v) => Value::Decimal(varint_text(v).into()),
        CqlValue::Date(d) => date(*d).map_or(Value::Null, Value::Date),
        CqlValue::Double(f) => Value::Float(*f),
        CqlValue::Float(f) => Value::Float(*f as f64),
        CqlValue::Int(i) => Value::Int(*i as i64),
        CqlValue::BigInt(i) => Value::Int(*i),
        CqlValue::SmallInt(i) => Value::Int(*i as i64),
        CqlValue::TinyInt(i) => Value::Int(*i as i64),
        CqlValue::Timestamp(t) => Utc
            .timestamp_millis_opt(t.0)
            .single()
            .map_or(Value::Null, |t| Value::DateTimeTz(t.fixed_offset())),
        CqlValue::Time(t) => time(*t).map_or(Value::Null, Value::Time),
        CqlValue::Duration(d) => {
            Value::Text(format!("{}mo{}d{}ns", d.months, d.days, d.nanoseconds).into())
        }
        CqlValue::Inet(ip) => Value::Text(ip.to_string().into()),
        CqlValue::Uuid(u) => Value::Uuid(*u),
        CqlValue::Timeuuid(u) => Value::Uuid(*u.as_ref()),
        CqlValue::Empty => Value::Null,
        CqlValue::List(items) | CqlValue::Set(items) | CqlValue::Vector(items) => Value::Json(
            serde_json::Value::Array(items.iter().map(json).collect())
                .to_string()
                .into(),
        ),
        CqlValue::Map(pairs) => {
            let all_text = pairs
                .iter()
                .all(|(k, _)| matches!(k, CqlValue::Text(_) | CqlValue::Ascii(_)));
            let j = if all_text {
                serde_json::Value::Object(
                    pairs
                        .iter()
                        .map(|(k, v)| (to_value(k).to_text().into_owned(), json(v)))
                        .collect(),
                )
            } else {
                serde_json::Value::Array(
                    pairs
                        .iter()
                        .map(|(k, v)| serde_json::json!([json(k), json(v)]))
                        .collect(),
                )
            };
            Value::Json(j.to_string().into())
        }
        CqlValue::Tuple(items) => Value::Json(
            serde_json::Value::Array(
                items
                    .iter()
                    .map(|i| i.as_ref().map_or(serde_json::Value::Null, json))
                    .collect(),
            )
            .to_string()
            .into(),
        ),
        CqlValue::UserDefinedType { fields, .. } => Value::Json(
            serde_json::Value::Object(
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_ref().map_or(serde_json::Value::Null, json)))
                    .collect(),
            )
            .to_string()
            .into(),
        ),
        _ => Value::Text(format!("{v:?}").into()),
    }
}

fn varint_text(v: &scylla::value::CqlVarint) -> String {
    let bytes = v.as_signed_bytes_be_slice();
    // Two's complement big-endian to decimal text (small values fast path).
    if bytes.len() <= 16 {
        let mut n: i128 = if bytes.first().is_some_and(|b| b & 0x80 != 0) {
            -1
        } else {
            0
        };
        for b in bytes {
            n = (n << 8) | *b as i128;
        }
        n.to_string()
    } else {
        format!("0x{}", hex::encode(bytes))
    }
}

fn decimal_text(d: &scylla::value::CqlDecimal) -> String {
    let (bytes, scale) = d.as_signed_be_bytes_slice_and_exponent();
    let mut n: i128 = if bytes.first().is_some_and(|b| b & 0x80 != 0) {
        -1
    } else {
        0
    };
    for b in bytes.iter().take(16) {
        n = (n << 8) | *b as i128;
    }
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let s = if scale <= 0 {
        format!("{digits}{}", "0".repeat((-scale) as usize))
    } else {
        let scale = scale as usize;
        let padded = format!("{:0>width$}", digits, width = scale + 1);
        let (int, frac) = padded.split_at(padded.len() - scale);
        format!("{int}.{frac}")
    };
    if neg { format!("-{s}") } else { s }
}

/// A bind value converted to the column type the statement expects.
fn to_cql(v: &Value, t: &ColumnType<'_>) -> DbResult<Option<CqlValue>> {
    if v.is_null() {
        return Ok(None);
    }
    let text = || v.to_text().into_owned();
    let bad = || DbError::query(format!("Cannot use '{}' as {}", v.to_text(), type_name(t)));
    let int = || -> DbResult<i64> {
        match v {
            Value::Int(i) => Ok(*i),
            Value::UInt(u) => i64::try_from(*u).map_err(|_| bad()),
            Value::Bool(b) => Ok(*b as i64),
            Value::Float(f) if f.fract() == 0.0 => Ok(*f as i64),
            _ => text().trim().parse().map_err(|_| bad()),
        }
    };
    let ColumnType::Native(n) = t else {
        // Collections, tuples and UDTs are written as CQL literals instead.
        return Err(DbError::unsupported(format!(
            "Binding a {} value; write it as a CQL literal",
            type_name(t)
        )));
    };
    Ok(Some(match n {
        NativeType::Ascii => CqlValue::Ascii(text()),
        NativeType::Text => CqlValue::Text(text()),
        NativeType::Boolean => CqlValue::Boolean(match v {
            Value::Bool(b) => *b,
            _ => matches!(
                text().trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "yes" | "t"
            ),
        }),
        NativeType::Blob => CqlValue::Blob(match v {
            Value::Bytes(b) => b.to_vec(),
            _ => {
                let t = text();
                hex::decode(t.trim().trim_start_matches("0x")).map_err(|_| bad())?
            }
        }),
        NativeType::Int => CqlValue::Int(i32::try_from(int()?).map_err(|_| bad())?),
        NativeType::BigInt => CqlValue::BigInt(int()?),
        NativeType::SmallInt => CqlValue::SmallInt(i16::try_from(int()?).map_err(|_| bad())?),
        NativeType::TinyInt => CqlValue::TinyInt(i8::try_from(int()?).map_err(|_| bad())?),
        NativeType::Counter => CqlValue::Counter(scylla::value::Counter(int()?)),
        NativeType::Double => CqlValue::Double(match v {
            Value::Float(f) => *f,
            Value::Int(i) => *i as f64,
            _ => text().trim().parse().map_err(|_| bad())?,
        }),
        NativeType::Float => CqlValue::Float(match v {
            Value::Float(f) => *f as f32,
            Value::Int(i) => *i as f32,
            _ => text().trim().parse().map_err(|_| bad())?,
        }),
        NativeType::Uuid | NativeType::Timeuuid => {
            let u = match v {
                Value::Uuid(u) => *u,
                _ => uuid::Uuid::parse_str(text().trim()).map_err(|_| bad())?,
            };
            if *n == NativeType::Timeuuid {
                CqlValue::Timeuuid(u.into())
            } else {
                CqlValue::Uuid(u)
            }
        }
        NativeType::Date => {
            let d = match v {
                Value::Date(d) => *d,
                Value::DateTime(t) => t.date(),
                Value::DateTimeTz(t) => t.date_naive(),
                _ => NaiveDate::parse_from_str(text().trim(), "%Y-%m-%d").map_err(|_| bad())?,
            };
            CqlValue::Date(cql_date(d).ok_or_else(bad)?)
        }
        NativeType::Time => {
            let t = match v {
                Value::Time(t) => *t,
                _ => NaiveTime::parse_from_str(text().trim(), "%H:%M:%S%.f").map_err(|_| bad())?,
            };
            CqlValue::Time(cql_time(t))
        }
        NativeType::Timestamp => {
            let ms = match v {
                Value::DateTimeTz(t) => t.timestamp_millis(),
                Value::DateTime(t) => t.and_utc().timestamp_millis(),
                Value::Date(d) => d
                    .and_hms_opt(0, 0, 0)
                    .map(|t| t.and_utc().timestamp_millis())
                    .unwrap_or(0),
                Value::Int(i) => *i,
                _ => {
                    let s = text();
                    let s = s.trim();
                    chrono::DateTime::parse_from_rfc3339(s)
                        .map(|t| t.timestamp_millis())
                        .or_else(|_| {
                            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
                                .map(|t| t.and_utc().timestamp_millis())
                        })
                        .map_err(|_| bad())?
                }
            };
            CqlValue::Timestamp(CqlTimestamp(ms))
        }
        NativeType::Inet => CqlValue::Inet(text().trim().parse().map_err(|_| bad())?),
        NativeType::Decimal | NativeType::Varint | NativeType::Duration => {
            return Err(DbError::unsupported(format!(
                "Binding a {} value; write it as a CQL literal",
                native_name(n)
            )));
        }
        _ => return Err(bad()),
    }))
}

// ------------------------------------------------------------------ session

fn tls(params: &ConnectParams) -> DbResult<Option<Arc<rustls::ClientConfig>>> {
    if !matches!(
        params.ssl.mode,
        SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull
    ) {
        return Ok(None);
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| DbError::tls(e.to_string()))?;
    let config = if params.ssl.mode == SslMode::VerifyFull {
        let mut roots = rustls::RootCertStore::empty();
        for c in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(c);
        }
        if let Some(ca) = params
            .ssl
            .ca_cert
            .as_deref()
            .filter(|c| !c.trim().is_empty())
        {
            let pem = std::fs::read(ca).map_err(|e| {
                DbError::config(format!("Cannot read the CA certificate {ca}: {e}"))
            })?;
            for c in rustls::pki_types::pem::PemObject::pem_slice_iter(&pem).flatten() {
                let _ = roots.add(c);
            }
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCert(provider)))
            .with_no_client_auth()
    };
    Ok(Some(Arc::new(config)))
}

/// Accepts the server's certificate (require / verify-ca without a CA list).
#[derive(Debug)]
struct AnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &rustls::pki_types::CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &rustls::pki_types::CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Routes every node through the one contact point: nodes announce
/// addresses the client cannot reach behind Docker, NAT or an SSH tunnel.
struct ThroughContactPoint(std::net::SocketAddr);

#[async_trait]
impl scylla::policies::address_translator::AddressTranslator for ThroughContactPoint {
    async fn translate_address(
        &self,
        _peer: &scylla::policies::address_translator::UntranslatedPeer,
    ) -> Result<std::net::SocketAddr, scylla::errors::TranslationError> {
        Ok(self.0)
    }
}

pub struct CassandraSession {
    db: Arc<Scylla>,
    info: ServerInfo,
    dialect: Arc<CqlDialect>,
    mode: TransactionMode,
    closed: bool,
}

impl CassandraSession {
    pub async fn connect(params: &ConnectParams) -> DbResult<Self> {
        let mut b = SessionBuilder::new().connection_timeout(params.connect_timeout);
        let first = format!("{}:{}", params.host, params.port);
        b = b.known_node(&first);
        // More contact points: `hosts` property `h1:9042,h2:9042` (from URLs).
        let several = params.property("hosts").is_some_and(|h| h.contains(','));
        if !several && params.property("direct_nodes") != Some("true") {
            let addr = tokio::net::lookup_host(&first)
                .await
                .map_err(|e| DbError::connection(format!("{first}: {e}")))?
                .next()
                .ok_or_else(|| DbError::connection(format!("{first}: no address")))?;
            b = b
                .address_translator(Arc::new(ThroughContactPoint(addr)))
                .disallow_shard_aware_port(true);
        }
        if let Some(hosts) = params.property("hosts") {
            for h in hosts.split(',').map(str::trim).filter(|h| !h.is_empty()) {
                let h = if h.contains(':') {
                    h.to_string()
                } else {
                    format!("{h}:{}", params.port)
                };
                if h != first {
                    b = b.known_node(h);
                }
            }
        }
        if let Some(user) = params.user.as_deref().filter(|u| !u.is_empty()) {
            b = b.user(
                user,
                params.password.as_ref().map(|p| p.expose()).unwrap_or(""),
            );
        }
        if let Some(dc) = params
            .property("local_datacenter")
            .filter(|d| !d.is_empty())
        {
            b = b.prefer_datacenter(dc.to_string());
        }
        if let Some(cfg) = tls(params)? {
            b = b.tls_context(Some(cfg));
        }
        if let Some(ks) = params.database.as_deref().filter(|k| !k.trim().is_empty()) {
            b = b.use_keyspace(ks.trim(), ks.chars().any(|c| c.is_ascii_uppercase()));
        }
        let db = b.build().await.map_err(err)?;
        // ScyllaDB has system.versions; Cassandra does not.
        let (product, version) = match single_text(&db, "SELECT version FROM system.versions").await
        {
            Some(v) => ("ScyllaDB", v),
            None => (
                "Cassandra",
                single_text(&db, "SELECT release_version FROM system.local")
                    .await
                    .unwrap_or_default(),
            ),
        };
        let info = ServerInfo {
            product: product.into(),
            version_number: parse_version_number(&version),
            version,
            session_id: None,
        };
        Ok(CassandraSession {
            db: Arc::new(db),
            info,
            dialect: Arc::new(CqlDialect::new()),
            mode: if params.auto_commit {
                TransactionMode::Auto
            } else {
                TransactionMode::Manual
            },
            closed: false,
        })
    }

    /// All rows of a (small) query.
    async fn rows(&self, cql: &str, values: &[&str]) -> DbResult<Vec<Vec<Value>>> {
        let result = self.db.query_unpaged(cql, values).await.map_err(err)?;
        let rows = result.into_rows_result().map_err(err)?;
        let mut out = Vec::new();
        for r in rows.rows::<CqlRow>().map_err(err)? {
            let r = r.map_err(err)?;
            out.push(
                r.columns
                    .iter()
                    .map(|c| c.as_ref().map_or(Value::Null, to_value))
                    .collect(),
            );
        }
        Ok(out)
    }

    async fn keyspace(&self, schema: &SchemaRef) -> DbResult<String> {
        match schema
            .schema
            .clone()
            .or_else(|| self.db.get_keyspace().map(|k| k.to_string()))
        {
            Some(k) => Ok(k),
            None => Err(DbError::query("No keyspace selected")),
        }
    }
}

async fn single_text(db: &Scylla, cql: &str) -> Option<String> {
    let rows = db
        .query_unpaged(cql, &[])
        .await
        .ok()?
        .into_rows_result()
        .ok()?;
    let row = rows.rows::<CqlRow>().ok()?.next()?.ok()?;
    row.columns
        .first()?
        .as_ref()
        .map(|v| to_value(v).to_text().into_owned())
}

fn text(v: &Value) -> Option<String> {
    (!v.is_null())
        .then(|| v.to_text().into_owned())
        .filter(|s| !s.is_empty())
}

/// The first word of a statement, upper case.
fn first_word(cql: &str) -> String {
    let mut rest = cql.trim_start();
    loop {
        if let Some(r) = rest.strip_prefix("--").or_else(|| rest.strip_prefix("//")) {
            rest = r
                .split_once('\n')
                .map(|(_, r)| r)
                .unwrap_or("")
                .trim_start();
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r
                .split_once("*/")
                .map(|(_, r)| r)
                .unwrap_or("")
                .trim_start();
        } else {
            break;
        }
    }
    rest.split(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap_or("")
        .to_ascii_uppercase()
}

struct CqlCursor {
    header: Option<ResultHeader>,
    stream: Option<TypedRowStream<CqlRow>>,
}

#[async_trait]
impl QueryCursor for CqlCursor {
    async fn next_result(&mut self) -> DbResult<Option<ResultHeader>> {
        Ok(self.header.take())
    }

    async fn fetch(&mut self, max_rows: usize) -> DbResult<Vec<Row>> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(Vec::new());
        };
        let mut rows = Vec::new();
        while rows.len() < max_rows.max(1) {
            match stream.next().await {
                Some(r) => {
                    let r = r.map_err(err)?;
                    rows.push(Row::from(
                        r.columns
                            .iter()
                            .map(|c| c.as_ref().map_or(Value::Null, to_value))
                            .collect::<Vec<_>>(),
                    ));
                }
                None => {
                    self.stream = None;
                    break;
                }
            }
        }
        Ok(rows)
    }

    async fn close(&mut self) -> DbResult<()> {
        self.stream = None;
        Ok(())
    }

    fn take_messages(&mut self) -> Vec<ServerMessage> {
        Vec::new()
    }
}

fn columns_of(specs: scylla::response::query_result::ColumnSpecs<'_, '_>) -> Vec<ColumnMeta> {
    specs
        .iter()
        .map(|s| {
            let mut m = ColumnMeta::new(s.name(), type_name(s.typ()), kind_of(s.typ()));
            m.source = Some(dbiji_core::value::ColumnSource {
                database: None,
                schema: Some(s.table_spec().ks_name().to_string()),
                table: s.table_spec().table_name().to_string(),
                column: s.name().to_string(),
            });
            m
        })
        .collect()
}

#[async_trait]
impl Session for CassandraSession {
    fn kind(&self) -> DbKind {
        DbKind::Cassandra
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        self.dialect.clone()
    }

    fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        None
    }

    async fn ping(&mut self) -> DbResult<()> {
        self.db
            .query_unpaged("SELECT now() FROM system.local", &[])
            .await
            .map(|_| ())
            .map_err(|e| DbError::connection(e.to_string()))
    }

    async fn close(&mut self) -> DbResult<()> {
        self.closed = true;
        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.closed
    }

    async fn execute(
        &mut self,
        sql: &str,
        params: &[Value],
        opts: &ExecOptions,
    ) -> DbResult<Box<dyn QueryCursor>> {
        let cql = sql.trim().trim_end_matches(';').trim();
        let word = first_word(cql);
        let rows_expected = matches!(word.as_str(), "SELECT" | "DESC" | "DESCRIBE" | "LIST");
        if cql.is_empty() {
            return Ok(Box::new(CqlCursor {
                header: None,
                stream: None,
            }));
        }
        if params.is_empty() {
            let mut st = Statement::new(cql).with_page_size(PAGE_SIZE);
            st.set_request_timeout(opts.timeout);
            if rows_expected {
                let pager = self.db.query_iter(st, &[]).await.map_err(err)?;
                let columns = columns_of(pager.column_specs());
                let stream = pager.rows_stream::<CqlRow>().map_err(err)?;
                return Ok(Box::new(CqlCursor {
                    header: Some(ResultHeader::Rows(columns)),
                    stream: Some(stream),
                }));
            }
            let result = self.db.query_unpaged(st, &[]).await.map_err(err)?;
            return Ok(Box::new(CqlCursor {
                header: Some(update_header(result, &word)?),
                stream: None,
            }));
        }
        // With parameters: prepared, each value converted to its column's type.
        let mut prepared = self.db.prepare(cql).await.map_err(err)?;
        prepared.set_page_size(PAGE_SIZE);
        prepared.set_request_timeout(opts.timeout);
        let specs = prepared.get_variable_col_specs();
        if specs.len() != params.len() {
            return Err(DbError::query(format!(
                "The statement has {} parameters, {} values were given",
                specs.len(),
                params.len()
            )));
        }
        let mut values = Vec::with_capacity(params.len());
        for (i, v) in params.iter().enumerate() {
            values.push(to_cql(
                v,
                specs.get_by_index(i).expect("index in range").typ(),
            )?);
        }
        if rows_expected {
            let pager = self.db.execute_iter(prepared, values).await.map_err(err)?;
            let columns = columns_of(pager.column_specs());
            let stream = pager.rows_stream::<CqlRow>().map_err(err)?;
            return Ok(Box::new(CqlCursor {
                header: Some(ResultHeader::Rows(columns)),
                stream: Some(stream),
            }));
        }
        let result = self
            .db
            .execute_unpaged(&prepared, values)
            .await
            .map_err(err)?;
        Ok(Box::new(CqlCursor {
            header: Some(update_header(result, &word)?),
            stream: None,
        }))
    }

    fn transaction_mode(&self) -> TransactionMode {
        self.mode
    }

    /// No transactions: every statement is applied at once.
    async fn set_transaction_mode(&mut self, mode: TransactionMode) -> DbResult<()> {
        self.mode = mode;
        Ok(())
    }

    fn in_transaction(&self) -> bool {
        false
    }

    async fn commit(&mut self) -> DbResult<()> {
        Ok(())
    }

    async fn rollback(&mut self) -> DbResult<()> {
        Err(DbError::unsupported(
            "Rolling back (Cassandra applies every statement at once)",
        ))
    }

    async fn current_database(&mut self) -> DbResult<Option<String>> {
        Ok(None)
    }

    async fn current_schema(&mut self) -> DbResult<Option<String>> {
        Ok(self.db.get_keyspace().map(|k| k.to_string()))
    }

    async fn set_current_schema(&mut self, schema: &str) -> DbResult<()> {
        self.db
            .use_keyspace(schema, schema.chars().any(|c| c.is_ascii_uppercase()))
            .await
            .map_err(err)
    }

    async fn list_databases(&mut self) -> DbResult<Vec<DatabaseInfo>> {
        Ok(Vec::new())
    }

    async fn list_schemas(&mut self) -> DbResult<Vec<SchemaInfo>> {
        let current = self.db.get_keyspace().map(|k| k.to_string());
        let rows = self
            .rows("SELECT keyspace_name FROM system_schema.keyspaces", &[])
            .await?;
        let mut out: Vec<SchemaInfo> = rows
            .iter()
            .filter_map(|r| text(&r[0]))
            .map(|name| SchemaInfo {
                is_current: current.as_deref() == Some(name.as_str()),
                is_system: name.starts_with("system") || name == "dse_system",
                name,
                owner: None,
                comment: None,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn list_objects(
        &mut self,
        schema: &SchemaRef,
        kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        let ks = self.keyspace(schema).await?;
        let s = Some(ks.clone());
        let (cql, k) = match kind {
            ObjectKind::Table => (
                "SELECT table_name, comment FROM system_schema.tables WHERE keyspace_name = ?",
                ObjectKind::Table,
            ),
            ObjectKind::MaterializedView | ObjectKind::View => (
                "SELECT view_name, base_table_name FROM system_schema.views WHERE keyspace_name = ?",
                ObjectKind::MaterializedView,
            ),
            ObjectKind::Type => (
                "SELECT type_name FROM system_schema.types WHERE keyspace_name = ?",
                ObjectKind::Type,
            ),
            ObjectKind::Function => (
                "SELECT function_name, return_type FROM system_schema.functions WHERE keyspace_name = ?",
                ObjectKind::Function,
            ),
            _ => return Ok(Vec::new()),
        };
        let mut out: Vec<ObjectInfo> = self
            .rows(cql, &[&ks])
            .await?
            .iter()
            .filter_map(|r| {
                let mut o = ObjectInfo::new(ObjectRef::new(k, s.clone(), text(&r[0])?));
                match k {
                    ObjectKind::Table => o = o.with_comment(r.get(1).and_then(text)),
                    ObjectKind::MaterializedView => {
                        o.detail = r.get(1).and_then(text).map(|b| format!("on {b}"))
                    }
                    ObjectKind::Function => o.detail = r.get(1).and_then(text),
                    _ => {}
                }
                Some(o)
            })
            .collect();
        out.sort_by(|a, b| a.obj.name.cmp(&b.obj.name));
        out.dedup_by(|a, b| a.obj.name == b.obj.name);
        Ok(out)
    }

    async fn list_children(
        &mut self,
        parent: &ObjectRef,
        kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        let ks = parent
            .schema
            .clone()
            .ok_or_else(|| DbError::query("The table has no keyspace"))?;
        match kind {
            ObjectKind::Column => {
                let t = self.table_definition(parent).await?;
                Ok(t.columns
                    .iter()
                    .map(|c| {
                        let role = if t.options.get("partition_key").is_some_and(|p| p.split(", ").any(|x| x == c.name)) {
                            " partition key"
                        } else if t.is_pk_column(&c.name) {
                            " clustering"
                        } else {
                            ""
                        };
                        ObjectInfo::new(ObjectRef::child(ObjectKind::Column, parent, c.name.clone())).with_detail(format!("{}{role}", c.data_type))
                    })
                    .collect())
            }
            ObjectKind::Index => Ok(self
                .rows("SELECT index_name, options FROM system_schema.indexes WHERE keyspace_name = ? AND table_name = ?", &[&ks, &parent.name])
                .await?
                .iter()
                .filter_map(|r| {
                    let target = r.get(1).and_then(text).and_then(|o| serde_json::from_str::<serde_json::Value>(&o).ok()).and_then(|j| j.get("target").and_then(|t| t.as_str()).map(str::to_string));
                    let mut o = ObjectInfo::new(ObjectRef::child(ObjectKind::Index, parent, text(&r[0])?));
                    o.detail = target;
                    Some(o)
                })
                .collect()),
            _ => Ok(Vec::new()),
        }
    }

    async fn table_definition(&mut self, table: &ObjectRef) -> DbResult<TableDef> {
        let ks = table
            .schema
            .clone()
            .ok_or_else(|| DbError::query("The table has no keyspace"))?;
        let name = table.name.clone();
        let mut t = TableDef::new(Some(ks.clone()), name.clone());
        let meta = self.rows("SELECT comment FROM system_schema.tables WHERE keyspace_name = ? AND table_name = ?", &[&ks, &name]).await?;
        let views = if meta.is_empty() {
            self.rows("SELECT view_name FROM system_schema.views WHERE keyspace_name = ? AND view_name = ?", &[&ks, &name]).await?
        } else {
            Vec::new()
        };
        if meta.is_empty() && views.is_empty() {
            return Err(DbError::query(format!(
                "Table {} not found",
                qualified(Some(&ks), &name)
            )));
        }
        t.comment = meta.first().and_then(|r| text(&r[0]));
        // (name, kind, position, type, clustering_order)
        let mut cols: Vec<(String, String, i64, String, String)> = self
            .rows("SELECT column_name, kind, position, type, clustering_order FROM system_schema.columns WHERE keyspace_name = ? AND table_name = ?", &[&ks, &name])
            .await?
            .iter()
            .filter_map(|r| {
                let pos = match &r[2] {
                    Value::Int(i) => *i,
                    _ => -1,
                };
                Some((text(&r[0])?, text(&r[1]).unwrap_or_default(), pos, text(&r[3]).unwrap_or_default(), text(&r[4]).unwrap_or_default()))
            })
            .collect();
        let rank = |k: &str| match k {
            "partition_key" => 0,
            "clustering" => 1,
            "static" => 2,
            _ => 3,
        };
        cols.sort_by(|a, b| (rank(&a.1), a.2, &a.0).cmp(&(rank(&b.1), b.2, &b.0)));
        let mut partition = Vec::new();
        let mut clustering = Vec::new();
        let mut order = Vec::new();
        let mut statics = Vec::new();
        for (i, (cname, ckind, _, ctype, corder)) in cols.iter().enumerate() {
            let mut c = ColumnDef::new(cname.clone(), ctype.clone());
            c.kind = kind_of_text(ctype);
            c.nullable = !matches!(ckind.as_str(), "partition_key" | "clustering");
            c.ordinal = i as u32 + 1;
            match ckind.as_str() {
                "partition_key" => partition.push(cname.clone()),
                "clustering" => {
                    clustering.push(cname.clone());
                    order.push(format!(
                        "{} {}",
                        quote(cname),
                        if corder.eq_ignore_ascii_case("desc") {
                            "DESC"
                        } else {
                            "ASC"
                        }
                    ));
                }
                "static" => statics.push(cname.clone()),
                _ => {}
            }
            t.columns.push(c);
        }
        let mut key = partition.clone();
        key.extend(clustering.iter().cloned());
        t.primary_key = Some(KeyDef {
            name: None,
            columns: key,
        });
        t.options
            .insert("partition_key".into(), partition.join(", "));
        if order.iter().any(|o| o.ends_with("DESC")) {
            t.options
                .insert("clustering_order".into(), order.join(", "));
        }
        if !statics.is_empty() {
            t.options.insert("static_columns".into(), statics.join(","));
        }
        for r in self.rows("SELECT index_name, options FROM system_schema.indexes WHERE keyspace_name = ? AND table_name = ?", &[&ks, &name]).await.unwrap_or_default() {
            let Some(iname) = text(&r[0]) else { continue };
            let target = r.get(1).and_then(text).and_then(|o| serde_json::from_str::<serde_json::Value>(&o).ok()).and_then(|j| j.get("target").and_then(|t| t.as_str()).map(str::to_string)).unwrap_or_default();
            let expr = target.contains('(');
            t.indexes.push(IndexDef { name: iname, columns: vec![IndexColumn { name: target, is_expression: expr, descending: false }], method: Some("secondary".into()), ..Default::default() });
        }
        Ok(t)
    }

    async fn view_definition(&mut self, view: &ObjectRef) -> DbResult<ViewDef> {
        let ks = view
            .schema
            .clone()
            .ok_or_else(|| DbError::query("The view has no keyspace"))?;
        let rows = self.rows("SELECT base_table_name, where_clause, include_all_columns FROM system_schema.views WHERE keyspace_name = ? AND view_name = ?", &[&ks, &view.name]).await?;
        let r = rows
            .first()
            .ok_or_else(|| DbError::query(format!("View {} not found", view.name)))?;
        let t = self.table_definition(view).await?;
        let cols = if matches!(r[2], Value::Bool(true)) {
            "*".to_string()
        } else {
            t.columns
                .iter()
                .map(|c| quote(&c.name))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let definition = format!(
            "SELECT {cols} FROM {} WHERE {} {}",
            qualified(Some(&ks), &text(&r[0]).unwrap_or_default()),
            text(&r[1]).unwrap_or_default(),
            t.primary_key
                .as_ref()
                .map(|k| format!(
                    "PRIMARY KEY ({})",
                    k.columns
                        .iter()
                        .map(|c| quote(c))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .unwrap_or_default()
        );
        Ok(ViewDef {
            schema: Some(ks),
            name: view.name.clone(),
            materialized: true,
            definition,
            columns: t.columns,
            comment: None,
            options: Default::default(),
        })
    }

    /// `DESCRIBE` on the server (Cassandra 4.0+, ScyllaDB 5+), else generated.
    async fn object_ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        let what = match obj.kind {
            ObjectKind::Table => "TABLE",
            ObjectKind::MaterializedView | ObjectKind::View => "MATERIALIZED VIEW",
            ObjectKind::Type => "TYPE",
            ObjectKind::Function => "FUNCTION",
            ObjectKind::Index => "INDEX",
            ObjectKind::Schema => "KEYSPACE",
            _ => return Err(DbError::unsupported("DDL of this object")),
        };
        let target = if obj.kind == ObjectKind::Schema {
            quote(&obj.name)
        } else {
            qualified(obj.schema.as_deref(), &obj.name)
        };
        if let Ok(rows) = self.rows(&format!("DESCRIBE {what} {target}"), &[]).await {
            let text: Vec<String> = rows
                .iter()
                .filter_map(|r| r.last().and_then(text))
                .collect();
            if !text.is_empty() {
                return Ok(text.join("\n\n"));
            }
        }
        match obj.kind {
            ObjectKind::Table => {
                let t = self.table_definition(obj).await?;
                Ok(self
                    .dialect
                    .ddl()
                    .create_table(&t, false)
                    .iter()
                    .map(|s| format!("{s};"))
                    .collect::<Vec<_>>()
                    .join("\n\n"))
            }
            ObjectKind::MaterializedView | ObjectKind::View => {
                let v = self.view_definition(obj).await?;
                Ok(self.dialect.ddl().create_view(&v, false).join(";\n") + ";")
            }
            _ => Err(DbError::unsupported("DDL of this object")),
        }
    }

    async fn search_metadata(&mut self, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
        let needle = search.pattern.trim().trim_matches('*').to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let rows = self
            .rows(
                "SELECT keyspace_name, table_name, column_name, type FROM system_schema.columns",
                &[],
            )
            .await?;
        let mut seen = std::collections::BTreeSet::new();
        for r in &rows {
            let (Some(ks), Some(table), Some(col)) = (text(&r[0]), text(&r[1]), text(&r[2])) else {
                continue;
            };
            if ks.starts_with("system")
                || !search.schemas.is_empty() && !search.schemas.contains(&ks)
            {
                continue;
            }
            if search.kinds.contains(&ObjectKind::Table)
                && table.to_lowercase().contains(&needle)
                && seen.insert((ks.clone(), table.clone()))
            {
                out.push(ObjectInfo::new(ObjectRef::table(
                    Some(ks.clone()),
                    table.clone(),
                )));
            }
            if search.kinds.contains(&ObjectKind::Column) && col.to_lowercase().contains(&needle) {
                let parent = ObjectRef::table(Some(ks), table);
                out.push(
                    ObjectInfo::new(ObjectRef::child(ObjectKind::Column, &parent, col))
                        .with_detail(text(&r[3]).unwrap_or_default()),
                );
            }
            if out.len() >= search.limit {
                break;
            }
        }
        Ok(out)
    }

    async fn schema_snapshot(
        &mut self,
        schema: &SchemaRef,
        opts: &SnapshotOptions,
    ) -> DbResult<SchemaSnapshot> {
        let ks = self.keyspace(schema).await?;
        let mut tables = Vec::new();
        for o in self
            .list_objects(&SchemaRef::schema(ks.clone()), ObjectKind::Table)
            .await?
        {
            if opts.columns {
                tables.push(self.table_definition(&o.obj).await?);
            } else {
                tables.push(TableDef::new(Some(ks.clone()), o.obj.name));
            }
        }
        Ok(SchemaSnapshot {
            kind: Some(DbKind::Cassandra),
            database: None,
            schema: Some(ks),
            tables,
            ..Default::default()
        })
    }

    async fn explain(&mut self, _sql: &str, _analyze: bool) -> DbResult<QueryPlan> {
        Err(DbError::unsupported("Explain"))
    }
}

/// Header of a statement without rows (or the rows of a LWT `IF` result).
fn update_header(
    result: scylla::response::query_result::QueryResult,
    word: &str,
) -> DbResult<ResultHeader> {
    let _ = Duration::ZERO;
    if result.is_rows() {
        let rows = result.into_rows_result().map_err(err)?;
        // Lightweight transactions report `[applied]`.
        let applied = rows
            .rows::<CqlRow>()
            .ok()
            .and_then(|mut it| it.next())
            .and_then(|r| r.ok())
            .and_then(|r| r.columns.first().cloned().flatten());
        let tag = match applied {
            Some(CqlValue::Boolean(false)) => format!("{word} (not applied)"),
            _ => word.to_string(),
        };
        return Ok(ResultHeader::Update {
            rows_affected: None,
            last_insert_id: None,
            tag: Some(tag),
        });
    }
    Ok(ResultHeader::Update {
        rows_affected: None,
        last_insert_id: None,
        tag: Some(word.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_convert_both_ways() {
        let int = ColumnType::Native(NativeType::Int);
        assert_eq!(
            to_cql(&Value::Int(5), &int).unwrap(),
            Some(CqlValue::Int(5))
        );
        assert_eq!(
            to_cql(&Value::Text("7".into()), &int).unwrap(),
            Some(CqlValue::Int(7))
        );
        assert!(to_cql(&Value::Int(1 << 40), &int).is_err());
        assert_eq!(to_cql(&Value::Null, &int).unwrap(), None);
        let ts = ColumnType::Native(NativeType::Timestamp);
        assert_eq!(
            to_cql(&Value::Text("2024-01-02 03:04:05".into()), &ts).unwrap(),
            Some(CqlValue::Timestamp(CqlTimestamp(1_704_164_645_000)))
        );
        let u = uuid::Uuid::new_v4();
        assert_eq!(
            to_cql(
                &Value::Text(u.to_string().into()),
                &ColumnType::Native(NativeType::Uuid)
            )
            .unwrap(),
            Some(CqlValue::Uuid(u))
        );
        assert_eq!(
            to_value(&CqlValue::Timestamp(CqlTimestamp(1_704_164_645_000))).to_text(),
            "2024-01-02 03:04:05+00"
        );
        assert_eq!(
            to_value(&CqlValue::List(vec![CqlValue::Int(1), CqlValue::Int(2)])),
            Value::Json("[1,2]".into())
        );
        assert_eq!(
            to_value(&CqlValue::Map(vec![(
                CqlValue::Text("a".into()),
                CqlValue::Int(1)
            )])),
            Value::Json("{\"a\":1}".into())
        );
        assert_eq!(first_word("  -- c\nselect * from t"), "SELECT");
        let day = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        assert_eq!(date(cql_date(day).unwrap()), Some(day));
        assert_eq!(
            cql_date(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()),
            Some(CqlDate(1 << 31))
        );
        let t = NaiveTime::from_hms_nano_opt(13, 4, 5, 6).unwrap();
        assert_eq!(time(cql_time(t)), Some(t));
    }

    #[test]
    fn decimals_and_varints_print_exactly() {
        let d =
            scylla::value::CqlDecimal::from_signed_be_bytes_slice_and_exponent(&[0x30, 0x39], 2);
        assert_eq!(decimal_text(&d), "123.45");
        let d = scylla::value::CqlDecimal::from_signed_be_bytes_slice_and_exponent(&[0xff], 0);
        assert_eq!(decimal_text(&d), "-1");
        let v = scylla::value::CqlVarint::from_signed_bytes_be_slice(&[0x01, 0x00]);
        assert_eq!(varint_text(&v), "256");
    }
}
