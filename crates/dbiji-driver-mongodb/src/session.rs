//! [`MongoSession`]: shell statements run with the official driver. Find and
//! aggregate results stream from the server cursor in batches; the columns
//! are the fields of the first documents.

use crate::dialect::MongoDialect;
use crate::shell::{self, Call, Command, to_shell};
use async_trait::async_trait;
use dbiji_core::admin::{KillMode, Metric, SessionList, TabularData};
use dbiji_core::config::SslMode;
use dbiji_core::ddl::{
    ColumnDef, IndexColumn, IndexDef, KeyDef, SchemaSnapshot, SnapshotOptions, TableDef,
};
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{ServerInfo, parse_version_number};
use dbiji_core::meta::*;
use dbiji_core::plan::{PlanNode, QueryPlan};
use dbiji_core::value::{ColumnMeta, DataKind};
use dbiji_core::{
    CancelHandle, ConnectParams, DbError, DbKind, DbResult, ErrorKind, ExecOptions, QueryCursor,
    ResultHeader, Row, ServerMessage, Session, TransactionMode, Value,
};
use futures::TryStreamExt;
use mongodb::bson::{Bson, Document, doc};
use mongodb::options::ClientOptions;
use mongodb::{Client, Cursor, Database, IndexModel};
use std::collections::VecDeque;
use std::sync::Arc;

/// Documents per server batch.
const BATCH: u32 = 500;
/// Documents read before the columns are decided.
const PEEK: usize = 100;
/// Documents sampled for a collection's fields.
const SAMPLE: i32 = 100;

fn err(e: mongodb::error::Error) -> DbError {
    use mongodb::error::ErrorKind as K;
    let text = e.to_string();
    let kind = match e.kind.as_ref() {
        K::Authentication { .. } => ErrorKind::Auth,
        K::ServerSelection { .. }
        | K::Io(_)
        | K::ConnectionPoolCleared { .. }
        | K::DnsResolve { .. } => ErrorKind::Connection,
        K::InvalidArgument { .. } => ErrorKind::Syntax,
        _ => ErrorKind::Query,
    };
    DbError::new(kind, text)
}

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A connection string for `params`. A single host is reached directly
/// (no replica set discovery, which fails behind Docker, NAT or a tunnel)
/// unless `replicaSet` or `directConnection` says otherwise.
pub(crate) fn uri(params: &ConnectParams) -> String {
    let srv = params.property("srv") == Some("true");
    let hosts = match params.property("hosts").filter(|h| !h.trim().is_empty()) {
        Some(h) => h.to_string(),
        None if srv => params.host.clone(),
        None => format!("{}:{}", params.host, params.port),
    };
    let mut s = String::from(if srv { "mongodb+srv://" } else { "mongodb://" });
    if let Some(u) = params.user.as_deref().filter(|u| !u.is_empty()) {
        s.push_str(&pct(u));
        if let Some(p) = params
            .password
            .as_ref()
            .map(|p| p.expose())
            .filter(|p| !p.is_empty())
        {
            s.push(':');
            s.push_str(&pct(p));
        }
        s.push('@');
    }
    s.push_str(&hosts);
    s.push('/');
    let mut q: Vec<(String, String)> = vec![("appName".into(), params.application_name.clone())];
    let tls = matches!(
        params.ssl.mode,
        SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull
    ) || srv && params.ssl.mode != SslMode::Disable;
    if tls {
        q.push(("tls".into(), "true".into()));
        if params.ssl.mode != SslMode::VerifyFull && !srv {
            q.push(("tlsAllowInvalidCertificates".into(), "true".into()));
        }
    }
    if !srv
        && !hosts.contains(',')
        && params.property("replicaSet").is_none()
        && params.property("directConnection").is_none()
    {
        q.push(("directConnection".into(), "true".into()));
    }
    q.push((
        "connectTimeoutMS".into(),
        params.connect_timeout.as_millis().to_string(),
    ));
    q.push((
        "serverSelectionTimeoutMS".into(),
        params.connect_timeout.as_millis().max(1000).to_string(),
    ));
    if params.user.is_some() && params.property("authSource").is_none() {
        q.push(("authSource".into(), "admin".into()));
    }
    for (k, v) in &params.properties {
        if !matches!(
            k.as_str(),
            "srv" | "hosts" | "application_name" | "time_zone"
        ) {
            q.push((k.clone(), v.clone()));
        }
    }
    s.push('?');
    s.push_str(
        &q.iter()
            .map(|(k, v)| format!("{}={}", pct(k), pct(v)))
            .collect::<Vec<_>>()
            .join("&"),
    );
    s
}

// ------------------------------------------------------------- conversion

pub(crate) fn value(b: &Bson) -> Value {
    match b {
        Bson::Null | Bson::Undefined => Value::Null,
        Bson::Boolean(v) => Value::Bool(*v),
        Bson::Int32(n) => Value::Int(*n as i64),
        Bson::Int64(n) => Value::Int(*n),
        Bson::Double(f) => Value::Float(*f),
        Bson::Decimal128(d) => Value::Decimal(d.to_string().into()),
        Bson::String(s) | Bson::Symbol(s) => Value::Text(s.as_str().into()),
        Bson::ObjectId(o) => Value::Text(o.to_hex().into()),
        Bson::DateTime(d) => {
            use chrono::TimeZone;
            chrono::Utc
                .timestamp_millis_opt(d.timestamp_millis())
                .single()
                .map_or(Value::Null, |t| Value::DateTimeTz(t.fixed_offset()))
        }
        Bson::Binary(bin)
            if bin.subtype == mongodb::bson::spec::BinarySubtype::Uuid && bin.bytes.len() == 16 =>
        {
            uuid_from(&bin.bytes)
                .map_or_else(|| Value::Bytes(bin.bytes.clone().into()), Value::Uuid)
        }
        Bson::Binary(bin) => Value::Bytes(bin.bytes.clone().into()),
        Bson::RegularExpression(r) => Value::Text(format!("/{}/{}", r.pattern, r.options).into()),
        Bson::Timestamp(t) => Value::Text(format!("Timestamp({}, {})", t.time, t.increment).into()),
        Bson::Document(_) | Bson::Array(_) => {
            Value::Json(b.clone().into_relaxed_extjson().to_string().into())
        }
        other => Value::Text(to_shell(other).into()),
    }
}

fn uuid_from(b: &[u8]) -> Option<uuid::Uuid> {
    uuid::Uuid::from_slice(b).ok()
}

fn kind_of(b: &Bson) -> (DataKind, &'static str) {
    match b {
        Bson::Boolean(_) => (DataKind::Bool, "bool"),
        Bson::Int32(_) => (DataKind::Int, "int"),
        Bson::Int64(_) => (DataKind::Int, "long"),
        Bson::Double(_) => (DataKind::Float, "double"),
        Bson::Decimal128(_) => (DataKind::Decimal, "decimal"),
        Bson::String(_) => (DataKind::Text, "string"),
        Bson::ObjectId(_) => (DataKind::Text, "objectId"),
        Bson::DateTime(_) => (DataKind::DateTimeTz, "date"),
        Bson::Binary(_) => (DataKind::Binary, "binData"),
        Bson::Document(_) => (DataKind::Json, "object"),
        Bson::Array(_) => (DataKind::Array, "array"),
        Bson::Timestamp(_) => (DataKind::Text, "timestamp"),
        Bson::RegularExpression(_) => (DataKind::Text, "regex"),
        _ => (DataKind::Other, "null"),
    }
}

/// The fields of `docs` in order of appearance, typed by their first non-null value.
fn columns(docs: &[Document]) -> Vec<ColumnMeta> {
    let mut out: Vec<(String, Option<&Bson>)> = Vec::new();
    for d in docs {
        for (k, v) in d {
            match out.iter_mut().find(|(n, _)| n == k) {
                Some((_, slot)) => {
                    if slot.is_none_or(|s| matches!(s, Bson::Null)) {
                        *slot = Some(v);
                    }
                }
                None => out.push((k.clone(), Some(v))),
            }
        }
    }
    out.into_iter()
        .map(|(name, v)| {
            let (kind, ty) = v.map(kind_of).unwrap_or((DataKind::Other, "null"));
            ColumnMeta::new(name, ty, kind)
        })
        .collect()
}

fn row(d: &Document, cols: &[ColumnMeta]) -> Row {
    Row::from(
        cols.iter()
            .map(|c| d.get(&c.name).map_or(Value::Null, value))
            .collect::<Vec<_>>(),
    )
}

/// A command's reply document as (field, value) rows, or its cursor's first batch.
fn reply_rows(d: Document) -> (Vec<ColumnMeta>, Vec<Row>) {
    if let Ok(c) = d.get_document("cursor")
        && let Ok(batch) = c.get_array("firstBatch")
    {
        let docs: Vec<Document> = batch
            .iter()
            .filter_map(|b| b.as_document().cloned())
            .collect();
        let cols = columns(&docs);
        let rows = docs.iter().map(|d| row(d, &cols)).collect();
        return (cols, rows);
    }
    let cols = vec![
        ColumnMeta::new("field", "string", DataKind::Text),
        ColumnMeta::new("value", "", DataKind::Other),
    ];
    let rows = d
        .iter()
        .map(|(k, v)| Row::from(vec![Value::Text(k.as_str().into()), value(v)]))
        .collect();
    (cols, rows)
}

enum Source {
    Done,
    Cursor(Box<Cursor<Document>>),
}

struct MongoCursor {
    header: Option<ResultHeader>,
    columns: Vec<ColumnMeta>,
    buffered: VecDeque<Row>,
    source: Source,
}

impl MongoCursor {
    fn rows(columns: Vec<ColumnMeta>, rows: Vec<Row>) -> Self {
        MongoCursor {
            header: Some(ResultHeader::Rows(columns.clone())),
            columns,
            buffered: rows.into(),
            source: Source::Done,
        }
    }
    fn update(tag: &str, n: Option<u64>) -> Self {
        MongoCursor {
            header: Some(ResultHeader::Update {
                rows_affected: n,
                last_insert_id: None,
                tag: Some(tag.into()),
            }),
            columns: Vec::new(),
            buffered: VecDeque::new(),
            source: Source::Done,
        }
    }
    /// The first documents decide the columns; the rest stream.
    async fn stream(mut cursor: Cursor<Document>) -> DbResult<Self> {
        let mut first = Vec::new();
        while first.len() < PEEK {
            match cursor.try_next().await.map_err(err)? {
                Some(d) => first.push(d),
                None => {
                    let cols = columns(&first);
                    let rows = first.iter().map(|d| row(d, &cols)).collect();
                    return Ok(Self::rows(cols, rows));
                }
            }
        }
        let cols = columns(&first);
        let rows: Vec<Row> = first.iter().map(|d| row(d, &cols)).collect();
        Ok(MongoCursor {
            header: Some(ResultHeader::Rows(cols.clone())),
            columns: cols,
            buffered: rows.into(),
            source: Source::Cursor(Box::new(cursor)),
        })
    }
}

#[async_trait]
impl QueryCursor for MongoCursor {
    async fn next_result(&mut self) -> DbResult<Option<ResultHeader>> {
        Ok(self.header.take())
    }

    async fn fetch(&mut self, max_rows: usize) -> DbResult<Vec<Row>> {
        let want = max_rows.max(1);
        let mut out: Vec<Row> = Vec::with_capacity(want.min(4096));
        while out.len() < want {
            if let Some(r) = self.buffered.pop_front() {
                out.push(r);
                continue;
            }
            let Source::Cursor(c) = &mut self.source else {
                break;
            };
            match c.try_next().await.map_err(err)? {
                Some(d) => out.push(row(&d, &self.columns)),
                None => {
                    self.source = Source::Done;
                    break;
                }
            }
        }
        Ok(out)
    }

    async fn close(&mut self) -> DbResult<()> {
        self.source = Source::Done;
        self.buffered.clear();
        Ok(())
    }

    fn take_messages(&mut self) -> Vec<ServerMessage> {
        Vec::new()
    }
}

pub struct MongoSession {
    client: Client,
    db: String,
    info: ServerInfo,
    dialect: Arc<MongoDialect>,
    mode: TransactionMode,
    closed: bool,
}

fn doc_arg(args: &[Bson], i: usize) -> DbResult<Document> {
    match args.get(i) {
        None | Some(Bson::Null) => Ok(Document::new()),
        Some(Bson::Document(d)) => Ok(d.clone()),
        Some(other) => Err(DbError::syntax(format!(
            "expected a document, got {}",
            to_shell(other)
        ))),
    }
}

fn int_arg(args: &[Bson], i: usize) -> DbResult<i64> {
    match args.get(i) {
        Some(Bson::Int32(n)) => Ok(*n as i64),
        Some(Bson::Int64(n)) => Ok(*n),
        Some(Bson::Double(n)) => Ok(*n as i64),
        _ => Err(DbError::syntax("expected a number")),
    }
}

fn str_arg(args: &[Bson], i: usize) -> DbResult<String> {
    match args.get(i) {
        Some(Bson::String(s)) => Ok(s.clone()),
        _ => Err(DbError::syntax("expected a string")),
    }
}

/// `find` with its chained cursor methods.
struct FindSpec {
    filter: Document,
    projection: Option<Document>,
    sort: Option<Document>,
    limit: Option<i64>,
    skip: Option<u64>,
}

fn find_spec(first: &Call, rest: &[Call]) -> DbResult<(FindSpec, Vec<String>)> {
    let mut spec = FindSpec {
        filter: doc_arg(&first.args, 0)?,
        projection: first
            .args
            .get(1)
            .map(|_| doc_arg(&first.args, 1))
            .transpose()?,
        sort: None,
        limit: None,
        skip: None,
    };
    let mut tail = Vec::new();
    for c in rest {
        match c.name.as_str() {
            "sort" => spec.sort = Some(doc_arg(&c.args, 0)?),
            "limit" => spec.limit = Some(int_arg(&c.args, 0)?),
            "skip" => spec.skip = Some(int_arg(&c.args, 0)?.max(0) as u64),
            "projection" => spec.projection = Some(doc_arg(&c.args, 0)?),
            "pretty" | "toArray" | "batchSize" | "maxTimeMS" | "allowDiskUse" | "comment"
            | "hint" | "collation" => {}
            other => tail.push(other.to_string()),
        }
    }
    if first.name == "findOne" {
        spec.limit = Some(1);
    }
    Ok((spec, tail))
}

impl MongoSession {
    pub async fn connect(params: &ConnectParams) -> DbResult<Self> {
        let opts = ClientOptions::parse(uri(params)).await.map_err(err)?;
        let client = Client::with_options(opts).map_err(err)?;
        let db = params
            .database
            .clone()
            .filter(|d| !d.trim().is_empty())
            .unwrap_or_else(|| "test".into());
        let build = client
            .database(&db)
            .run_command(doc! {"buildInfo": 1})
            .await
            .map_err(err)?;
        let version = build.get_str("version").unwrap_or_default().to_string();
        let product = if build.get_document("ferretdb").is_ok() || build.get_str("ferretdb").is_ok()
        {
            "FerretDB"
        } else {
            "MongoDB"
        };
        let info = ServerInfo {
            product: product.into(),
            version_number: parse_version_number(&version),
            version,
            session_id: None,
        };
        Ok(MongoSession {
            client,
            db,
            info,
            dialect: Arc::new(MongoDialect::new()),
            mode: TransactionMode::Auto,
            closed: false,
        })
    }

    fn database(&self, name: Option<&str>) -> Database {
        self.client
            .database(name.filter(|n| !n.is_empty()).unwrap_or(&self.db))
    }

    async fn run(&mut self, cmd: Command) -> DbResult<Box<dyn QueryCursor>> {
        let (db, coll, calls) = match cmd {
            Command::Use(d) => {
                self.db = d.clone();
                return Ok(Box::new(MongoCursor::update(&format!("use {d}"), None)));
            }
            Command::ShowDbs => {
                let r = self
                    .client
                    .database("admin")
                    .run_command(doc! {"listDatabases": 1})
                    .await
                    .map_err(err)?;
                let dbs: Vec<Document> = r
                    .get_array("databases")
                    .map(|a| a.iter().filter_map(|d| d.as_document().cloned()).collect())
                    .unwrap_or_default();
                let cols = columns(&dbs);
                let rows = dbs.iter().map(|d| row(d, &cols)).collect();
                return Ok(Box::new(MongoCursor::rows(cols, rows)));
            }
            Command::ShowCollections => {
                let names = self
                    .database(None)
                    .list_collection_names()
                    .await
                    .map_err(err)?;
                let rows = names
                    .into_iter()
                    .map(|n| Row::from(vec![Value::Text(n.into())]))
                    .collect();
                return Ok(Box::new(MongoCursor::rows(
                    vec![ColumnMeta::new("name", "string", DataKind::Text)],
                    rows,
                )));
            }
            Command::Chain {
                db,
                collection,
                calls,
            } => (db, collection, calls),
        };
        let database = self.database(db.as_deref());
        let first = &calls[0];
        let rest = &calls[1..];
        let Some(coll_name) = coll else {
            // Database methods.
            let reply = match first.name.as_str() {
                "runCommand" => database
                    .run_command(doc_arg(&first.args, 0)?)
                    .await
                    .map_err(err)?,
                "adminCommand" => self
                    .client
                    .database("admin")
                    .run_command(doc_arg(&first.args, 0)?)
                    .await
                    .map_err(err)?,
                "stats" => database
                    .run_command(doc! {"dbStats": 1})
                    .await
                    .map_err(err)?,
                "serverStatus" => database
                    .run_command(doc! {"serverStatus": 1})
                    .await
                    .map_err(err)?,
                "version" => {
                    let v = database
                        .run_command(doc! {"buildInfo": 1})
                        .await
                        .map_err(err)?;
                    doc! {"version": v.get_str("version").unwrap_or_default()}
                }
                "getName" => doc! {"name": database.name()},
                "getCollectionNames" => {
                    let names = database.list_collection_names().await.map_err(err)?;
                    let rows = names
                        .into_iter()
                        .map(|n| Row::from(vec![Value::Text(n.into())]))
                        .collect();
                    return Ok(Box::new(MongoCursor::rows(
                        vec![ColumnMeta::new("name", "string", DataKind::Text)],
                        rows,
                    )));
                }
                "createCollection" => {
                    database
                        .create_collection(str_arg(&first.args, 0)?)
                        .await
                        .map_err(err)?;
                    return Ok(Box::new(MongoCursor::update("createCollection", None)));
                }
                "dropDatabase" => {
                    database.drop().await.map_err(err)?;
                    return Ok(Box::new(MongoCursor::update("dropDatabase", None)));
                }
                other => {
                    return Err(DbError::unsupported(format!(
                        "db.{other}() is not supported; use db.runCommand({{…}})"
                    )));
                }
            };
            let (cols, rows) = reply_rows(reply);
            return Ok(Box::new(MongoCursor::rows(cols, rows)));
        };
        let coll = database.collection::<Document>(&coll_name);
        let one = |n: u64, name: &str| {
            MongoCursor::rows(
                vec![ColumnMeta::new(name, "long", DataKind::Int)],
                vec![Row::from(vec![Value::Int(n as i64)])],
            )
        };
        Ok(Box::new(match first.name.as_str() {
            "find" | "findOne" => {
                let (spec, tail) = find_spec(first, rest)?;
                if tail
                    .iter()
                    .any(|t| t == "count" || t == "size" || t == "itcount")
                {
                    let mut c = coll.count_documents(spec.filter);
                    if let Some(l) = spec.limit.filter(|l| *l > 0) {
                        c = c.limit(l as u64);
                    }
                    if let Some(s) = spec.skip {
                        c = c.skip(s);
                    }
                    one(c.await.map_err(err)?, "count")
                } else if tail.iter().any(|t| t == "explain") {
                    let mut f = doc! {"find": &coll_name, "filter": spec.filter};
                    if let Some(s) = spec.sort {
                        f.insert("sort", s);
                    }
                    let r = database
                        .run_command(doc! {"explain": f, "verbosity": "queryPlanner"})
                        .await
                        .map_err(err)?;
                    let (cols, rows) = reply_rows(r);
                    MongoCursor::rows(cols, rows)
                } else if let Some(t) = tail.first() {
                    return Err(DbError::unsupported(format!(
                        ".{t}() is not supported after find()"
                    )));
                } else {
                    let mut f = coll.find(spec.filter).batch_size(BATCH);
                    if let Some(p) = spec.projection {
                        f = f.projection(p);
                    }
                    if let Some(s) = spec.sort {
                        f = f.sort(s);
                    }
                    if let Some(l) = spec.limit {
                        f = f.limit(l);
                    }
                    if let Some(s) = spec.skip {
                        f = f.skip(s);
                    }
                    MongoCursor::stream(f.await.map_err(err)?).await?
                }
            }
            "aggregate" => {
                let pipeline: Vec<Document> = match first.args.first() {
                    Some(Bson::Array(a)) => a
                        .iter()
                        .map(|s| {
                            s.as_document().cloned().ok_or_else(|| {
                                DbError::syntax("a pipeline stage must be a document")
                            })
                        })
                        .collect::<DbResult<_>>()?,
                    // Stages as separate arguments (the old shell's form).
                    Some(Bson::Document(_)) => first
                        .args
                        .iter()
                        .filter_map(|a| a.as_document().cloned())
                        .collect(),
                    _ => Vec::new(),
                };
                if rest.iter().any(|c| c.name == "explain") {
                    let r = database.run_command(doc! {"explain": {"aggregate": &coll_name, "pipeline": pipeline, "cursor": {}}, "verbosity": "queryPlanner"}).await.map_err(err)?;
                    let (cols, rows) = reply_rows(r);
                    MongoCursor::rows(cols, rows)
                } else {
                    MongoCursor::stream(
                        coll.aggregate(pipeline)
                            .batch_size(BATCH)
                            .await
                            .map_err(err)?,
                    )
                    .await?
                }
            }
            "countDocuments" | "count" => one(
                coll.count_documents(doc_arg(&first.args, 0)?)
                    .await
                    .map_err(err)?,
                "count",
            ),
            "estimatedDocumentCount" => {
                one(coll.estimated_document_count().await.map_err(err)?, "count")
            }
            "distinct" => {
                let values = coll
                    .distinct(str_arg(&first.args, 0)?, doc_arg(&first.args, 1)?)
                    .await
                    .map_err(err)?;
                MongoCursor::rows(
                    vec![ColumnMeta::new("value", "", DataKind::Other)],
                    values.iter().map(|v| Row::from(vec![value(v)])).collect(),
                )
            }
            "insertOne" => {
                coll.insert_one(doc_arg(&first.args, 0)?)
                    .await
                    .map_err(err)?;
                MongoCursor::update("insertOne", Some(1))
            }
            "insertMany" => {
                let docs: Vec<Document> = match first.args.first() {
                    Some(Bson::Array(a)) => {
                        a.iter().filter_map(|d| d.as_document().cloned()).collect()
                    }
                    _ => return Err(DbError::syntax("insertMany takes an array of documents")),
                };
                let r = coll.insert_many(docs).await.map_err(err)?;
                MongoCursor::update("insertMany", Some(r.inserted_ids.len() as u64))
            }
            "updateOne" | "updateMany" | "replaceOne" => {
                let filter = doc_arg(&first.args, 0)?;
                let upsert = doc_arg(&first.args, 2)
                    .ok()
                    .and_then(|o| o.get_bool("upsert").ok())
                    .unwrap_or(false);
                let r = match (first.name.as_str(), first.args.get(1)) {
                    ("replaceOne", _) => {
                        coll.replace_one(filter, doc_arg(&first.args, 1)?)
                            .upsert(upsert)
                            .await
                    }
                    ("updateOne", Some(Bson::Array(p))) => {
                        coll.update_one(
                            filter,
                            p.iter()
                                .filter_map(|d| d.as_document().cloned())
                                .collect::<Vec<_>>(),
                        )
                        .upsert(upsert)
                        .await
                    }
                    ("updateMany", Some(Bson::Array(p))) => {
                        coll.update_many(
                            filter,
                            p.iter()
                                .filter_map(|d| d.as_document().cloned())
                                .collect::<Vec<_>>(),
                        )
                        .upsert(upsert)
                        .await
                    }
                    ("updateOne", _) => {
                        coll.update_one(filter, doc_arg(&first.args, 1)?)
                            .upsert(upsert)
                            .await
                    }
                    _ => {
                        coll.update_many(filter, doc_arg(&first.args, 1)?)
                            .upsert(upsert)
                            .await
                    }
                }
                .map_err(err)?;
                MongoCursor::update(
                    &first.name,
                    Some(r.modified_count + u64::from(r.upserted_id.is_some())),
                )
            }
            "deleteOne" => MongoCursor::update(
                "deleteOne",
                Some(
                    coll.delete_one(doc_arg(&first.args, 0)?)
                        .await
                        .map_err(err)?
                        .deleted_count,
                ),
            ),
            "deleteMany" | "remove" => MongoCursor::update(
                "deleteMany",
                Some(
                    coll.delete_many(doc_arg(&first.args, 0)?)
                        .await
                        .map_err(err)?
                        .deleted_count,
                ),
            ),
            "drop" => {
                coll.drop().await.map_err(err)?;
                MongoCursor::update("drop", None)
            }
            "createIndex" => {
                let mut opts = mongodb::options::IndexOptions::default();
                let o = doc_arg(&first.args, 1)?;
                opts.name = o.get_str("name").ok().map(str::to_string);
                opts.unique = o.get_bool("unique").ok();
                opts.sparse = o.get_bool("sparse").ok();
                coll.create_index(
                    IndexModel::builder()
                        .keys(doc_arg(&first.args, 0)?)
                        .options(opts)
                        .build(),
                )
                .await
                .map_err(err)?;
                MongoCursor::update("createIndex", None)
            }
            "dropIndex" => {
                coll.drop_index(str_arg(&first.args, 0)?)
                    .await
                    .map_err(err)?;
                MongoCursor::update("dropIndex", None)
            }
            "getIndexes" => {
                let models: Vec<IndexModel> = coll
                    .list_indexes()
                    .await
                    .map_err(err)?
                    .try_collect()
                    .await
                    .map_err(err)?;
                let rows = models
                    .iter()
                    .map(|m| {
                        let o = m.options.as_ref();
                        Row::from(vec![
                            Value::Text(o.and_then(|o| o.name.clone()).unwrap_or_default().into()),
                            Value::Json(
                                Bson::Document(m.keys.clone())
                                    .into_relaxed_extjson()
                                    .to_string()
                                    .into(),
                            ),
                            Value::Bool(o.and_then(|o| o.unique).unwrap_or(false)),
                        ])
                    })
                    .collect();
                MongoCursor::rows(
                    vec![
                        ColumnMeta::new("name", "string", DataKind::Text),
                        ColumnMeta::new("key", "object", DataKind::Json),
                        ColumnMeta::new("unique", "bool", DataKind::Bool),
                    ],
                    rows,
                )
            }
            "stats" => {
                let (cols, rows) = reply_rows(
                    database
                        .run_command(doc! {"collStats": &coll_name})
                        .await
                        .map_err(err)?,
                );
                MongoCursor::rows(cols, rows)
            }
            "renameCollection" => {
                let to = format!("{}.{}", database.name(), str_arg(&first.args, 0)?);
                self.client.database("admin").run_command(doc! {"renameCollection": format!("{}.{coll_name}", database.name()), "to": to}).await.map_err(err)?;
                MongoCursor::update("renameCollection", None)
            }
            other => {
                return Err(DbError::unsupported(format!(
                    ".{other}() is not supported; use db.runCommand({{…}})"
                )));
            }
        }))
    }

    /// Fields of a collection from a sample of its documents.
    async fn sample_fields(&self, db: &str, coll: &str) -> DbResult<Vec<ColumnMeta>> {
        let c = self.client.database(db).collection::<Document>(coll);
        let docs: Vec<Document> = c
            .aggregate(vec![doc! {"$sample": {"size": SAMPLE}}])
            .await
            .map_err(err)?
            .try_collect()
            .await
            .map_err(err)?;
        Ok(columns(&docs))
    }

    fn db_name(&self, schema: Option<&str>) -> String {
        schema
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.db)
            .to_string()
    }
}

#[async_trait]
impl Session for MongoSession {
    fn kind(&self) -> DbKind {
        DbKind::MongoDb
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
        self.client
            .database("admin")
            .run_command(doc! {"ping": 1})
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
        _params: &[Value],
        _opts: &ExecOptions,
    ) -> DbResult<Box<dyn QueryCursor>> {
        let cmd = shell::parse(sql).map_err(DbError::syntax)?;
        self.run(cmd).await
    }

    fn transaction_mode(&self) -> TransactionMode {
        self.mode
    }

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
            "Rolling back (each statement is applied at once)",
        ))
    }

    async fn current_database(&mut self) -> DbResult<Option<String>> {
        Ok(None)
    }

    async fn current_schema(&mut self) -> DbResult<Option<String>> {
        Ok(Some(self.db.clone()))
    }

    async fn set_current_schema(&mut self, schema: &str) -> DbResult<()> {
        self.db = schema.to_string();
        Ok(())
    }

    async fn list_databases(&mut self) -> DbResult<Vec<DatabaseInfo>> {
        Ok(Vec::new())
    }

    /// The databases (`listDatabases`; without that privilege the connection's own).
    async fn list_schemas(&mut self) -> DbResult<Vec<SchemaInfo>> {
        let names = match self.client.list_database_names().await {
            Ok(n) => n,
            Err(_) => vec![self.db.clone()],
        };
        let mut out: Vec<SchemaInfo> = names
            .into_iter()
            .map(|n| SchemaInfo {
                is_current: n == self.db,
                is_system: matches!(n.as_str(), "admin" | "config" | "local"),
                name: n,
                owner: None,
                comment: None,
            })
            .collect();
        if !out.iter().any(|s| s.is_current) {
            out.push(SchemaInfo {
                name: self.db.clone(),
                is_current: true,
                is_system: false,
                owner: None,
                comment: None,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn list_objects(
        &mut self,
        schema: &SchemaRef,
        kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        let db = self.db_name(schema.schema.as_deref());
        if !matches!(kind, ObjectKind::Table | ObjectKind::View) {
            return Ok(Vec::new());
        }
        let specs: Vec<mongodb::results::CollectionSpecification> = self
            .client
            .database(&db)
            .list_collections()
            .await
            .map_err(err)?
            .try_collect()
            .await
            .map_err(err)?;
        let mut out: Vec<ObjectInfo> = specs
            .into_iter()
            .filter(|s| {
                (s.collection_type == mongodb::results::CollectionType::View)
                    == (kind == ObjectKind::View)
            })
            .map(|s| {
                let mut o = ObjectInfo::new(ObjectRef::new(kind, Some(db.clone()), s.name.clone()));
                o.is_system = s.name.starts_with("system.");
                if s.collection_type == mongodb::results::CollectionType::Timeseries {
                    o.detail = Some("time series".into());
                }
                o
            })
            .collect();
        out.sort_by(|a, b| a.obj.name.cmp(&b.obj.name));
        Ok(out)
    }

    async fn list_children(
        &mut self,
        parent: &ObjectRef,
        kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        let db = self.db_name(parent.schema.as_deref());
        match kind {
            ObjectKind::Column => Ok(self
                .sample_fields(&db, &parent.name)
                .await?
                .into_iter()
                .map(|c| {
                    ObjectInfo::new(ObjectRef::child(ObjectKind::Column, parent, c.name))
                        .with_detail(c.type_name)
                })
                .collect()),
            ObjectKind::Index => {
                let c = self
                    .client
                    .database(&db)
                    .collection::<Document>(&parent.name);
                let models: Vec<IndexModel> = c
                    .list_indexes()
                    .await
                    .map_err(err)?
                    .try_collect()
                    .await
                    .map_err(err)?;
                Ok(models
                    .into_iter()
                    .map(|m| {
                        let name = m
                            .options
                            .as_ref()
                            .and_then(|o| o.name.clone())
                            .unwrap_or_default();
                        let unique = m.options.as_ref().and_then(|o| o.unique).unwrap_or(false);
                        let keys = to_shell(&Bson::Document(m.keys));
                        ObjectInfo::new(ObjectRef::child(ObjectKind::Index, parent, name))
                            .with_detail(if unique {
                                format!("{keys} unique")
                            } else {
                                keys
                            })
                    })
                    .collect())
            }
            _ => Ok(Vec::new()),
        }
    }

    async fn table_definition(&mut self, table: &ObjectRef) -> DbResult<TableDef> {
        let db = self.db_name(table.schema.as_deref());
        let mut t = TableDef::new(Some(db.clone()), table.name.clone());
        t.columns = self
            .sample_fields(&db, &table.name)
            .await?
            .into_iter()
            .enumerate()
            .map(|(i, c)| ColumnDef {
                ordinal: i as u32 + 1,
                kind: c.kind,
                ..ColumnDef::new(c.name, c.type_name)
            })
            .collect();
        if t.columns.iter().any(|c| c.name == "_id") {
            t.primary_key = Some(KeyDef {
                name: Some("_id_".into()),
                columns: vec!["_id".into()],
            });
        }
        let c = self
            .client
            .database(&db)
            .collection::<Document>(&table.name);
        if let Ok(cursor) = c.list_indexes().await {
            let models: Vec<IndexModel> = cursor.try_collect().await.unwrap_or_default();
            for m in models {
                let o = m.options.unwrap_or_default();
                t.indexes.push(IndexDef {
                    name: o.name.unwrap_or_default(),
                    columns: m
                        .keys
                        .iter()
                        .map(|(k, v)| IndexColumn {
                            name: k.clone(),
                            is_expression: false,
                            descending: matches!(
                                v,
                                Bson::Int32(-1) | Bson::Int64(-1) | Bson::Double(-1.0)
                            ),
                        })
                        .collect(),
                    unique: o.unique.unwrap_or(false),
                    primary: false,
                    ..Default::default()
                });
            }
        }
        Ok(t)
    }

    async fn object_ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        let t = self.table_definition(obj).await?;
        Ok(self.dialect.ddl().create_table(&t, false).join("\n"))
    }

    async fn object_properties(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        let db = self.db_name(obj.schema.as_deref());
        let s = self
            .client
            .database(&db)
            .run_command(doc! {"collStats": &obj.name})
            .await
            .map_err(err)?;
        let get = |k: &str| {
            s.get(k)
                .map(|v| value(v).to_text().into_owned())
                .unwrap_or_default()
        };
        Ok(vec![
            Property::new("Documents", get("count")),
            Property::new("Size (bytes)", get("size")),
            Property::new("Storage size (bytes)", get("storageSize")),
            Property::new("Average document (bytes)", get("avgObjSize")),
            Property::new("Indexes", get("nindexes")),
            Property::new("Index size (bytes)", get("totalIndexSize")),
        ])
    }

    async fn search_metadata(&mut self, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
        let needle = search.pattern.trim().trim_matches('*').to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for s in self.list_schemas().await? {
            if s.is_system || !search.schemas.is_empty() && !search.schemas.contains(&s.name) {
                continue;
            }
            for o in self
                .list_objects(&SchemaRef::schema(s.name.clone()), ObjectKind::Table)
                .await?
            {
                if o.obj.name.to_lowercase().contains(&needle) {
                    out.push(o);
                }
            }
            if out.len() >= search.limit {
                break;
            }
        }
        out.truncate(search.limit);
        Ok(out)
    }

    async fn schema_snapshot(
        &mut self,
        schema: &SchemaRef,
        _opts: &SnapshotOptions,
    ) -> DbResult<SchemaSnapshot> {
        let db = self.db_name(schema.schema.as_deref());
        let tables = self
            .list_objects(&SchemaRef::schema(db.clone()), ObjectKind::Table)
            .await?
            .into_iter()
            .map(|o| TableDef::new(Some(db.clone()), o.obj.name))
            .collect();
        Ok(SchemaSnapshot {
            kind: Some(DbKind::MongoDb),
            schema: Some(db),
            tables,
            ..Default::default()
        })
    }

    /// The query planner's winning plan of a `find` or `aggregate`.
    async fn explain(&mut self, sql: &str, _analyze: bool) -> DbResult<QueryPlan> {
        let Command::Chain {
            db,
            collection: Some(coll),
            calls,
        } = shell::parse(sql).map_err(DbError::syntax)?
        else {
            return Err(DbError::unsupported(
                "Explain works on find() and aggregate()",
            ));
        };
        let database = self.database(db.as_deref());
        let cmd = match calls[0].name.as_str() {
            "find" | "findOne" => {
                let (spec, _) = find_spec(&calls[0], &calls[1..])?;
                let mut f = doc! {"find": &coll, "filter": spec.filter};
                if let Some(s) = spec.sort {
                    f.insert("sort", s);
                }
                f
            }
            "aggregate" => {
                doc! {"aggregate": &coll, "pipeline": calls[0].args.first().cloned().unwrap_or(Bson::Array(Vec::new())), "cursor": {}}
            }
            "countDocuments" | "count" => {
                doc! {"count": &coll, "query": doc_arg(&calls[0].args, 0)?}
            }
            _ => {
                return Err(DbError::unsupported(
                    "Explain works on find() and aggregate()",
                ));
            }
        };
        let r = database
            .run_command(doc! {"explain": cmd, "verbosity": "queryPlanner"})
            .await
            .map_err(err)?;
        let planner = r.get_document("queryPlanner").ok().cloned().or_else(|| {
            r.get_array("stages")
                .ok()?
                .first()?
                .as_document()?
                .get_document("$cursor")
                .ok()?
                .get_document("queryPlanner")
                .ok()
                .cloned()
        });
        let roots = planner
            .and_then(|p| p.get_document("winningPlan").ok().cloned())
            .map(|w| vec![plan_node(&w)])
            .unwrap_or_default();
        Ok(QueryPlan {
            roots,
            text: serde_json::to_string_pretty(&Bson::Document(r.clone()).into_relaxed_extjson())
                .unwrap_or_default(),
            raw: None,
            planning_ms: None,
            execution_ms: None,
            analyzed: false,
        })
    }

    async fn list_sessions(&mut self) -> DbResult<SessionList> {
        let r = self
            .client
            .database("admin")
            .run_command(doc! {"currentOp": 1})
            .await
            .map_err(err)?;
        let ops: Vec<Document> = r
            .get_array("inprog")
            .map(|a| a.iter().filter_map(|d| d.as_document().cloned()).collect())
            .unwrap_or_default();
        let mut data = TabularData::new(
            [
                "opid",
                "client",
                "desc",
                "ns",
                "op",
                "secs_running",
                "command",
            ]
            .map(String::from)
            .to_vec(),
        );
        for o in &ops {
            data.rows.push(vec![
                o.get("opid").map_or(Value::Null, value),
                o.get("client").map_or(Value::Null, value),
                o.get("desc").map_or(Value::Null, value),
                o.get("ns").map_or(Value::Null, value),
                o.get("op").map_or(Value::Null, value),
                o.get("secs_running").map_or(Value::Null, value),
                o.get("command").map_or(Value::Null, value),
            ]);
        }
        Ok(SessionList {
            data,
            id_column: 0,
            query_column: Some(6),
            state_column: Some(4),
            blocked_by_column: None,
        })
    }

    async fn kill_session(&mut self, id: &str, _mode: KillMode) -> DbResult<()> {
        let op: Bson = id
            .parse::<i64>()
            .map(Bson::Int64)
            .unwrap_or_else(|_| Bson::String(id.to_string()));
        self.client
            .database("admin")
            .run_command(doc! {"killOp": 1, "op": op})
            .await
            .map(|_| ())
            .map_err(err)
    }

    async fn server_metrics(&mut self) -> DbResult<Vec<Metric>> {
        let s = self
            .client
            .database("admin")
            .run_command(doc! {"serverStatus": 1})
            .await
            .map_err(err)?;
        let num = |path: &[&str]| -> Option<f64> {
            let mut d = &s;
            for k in &path[..path.len() - 1] {
                d = d.get_document(k).ok()?;
            }
            match d.get(path[path.len() - 1])? {
                Bson::Int32(n) => Some(*n as f64),
                Bson::Int64(n) => Some(*n as f64),
                Bson::Double(n) => Some(*n),
                _ => None,
            }
        };
        let mut out = Vec::new();
        if let Some(v) = num(&["connections", "current"]) {
            out.push(Metric::gauge(
                "Connections",
                "connections.current",
                "Connections",
                v,
                "",
            ));
        }
        if let Some(v) = num(&["mem", "resident"]) {
            out.push(Metric::gauge(
                "Memory",
                "mem.resident",
                "Resident memory",
                v * 1024.0 * 1024.0,
                "bytes",
            ));
        }
        for op in ["insert", "query", "update", "delete", "getmore", "command"] {
            if let Some(v) = num(&["opcounters", op]) {
                out.push(Metric::counter(
                    "Operations",
                    &format!("opcounters.{op}"),
                    op,
                    v,
                    "",
                ));
            }
        }
        for (k, label) in [("bytesIn", "Network in"), ("bytesOut", "Network out")] {
            if let Some(v) = num(&["network", k]) {
                out.push(Metric::counter(
                    "Network",
                    &format!("network.{k}"),
                    label,
                    v,
                    "bytes",
                ));
            }
        }
        Ok(out)
    }
}

fn plan_node(d: &Document) -> PlanNode {
    let mut n = PlanNode::new(d.get_str("stage").unwrap_or("STAGE"));
    n.index = d.get_str("indexName").ok().map(str::to_string);
    n.condition = d
        .get_document("filter")
        .ok()
        .map(|f| to_shell(&Bson::Document(f.clone())));
    if let Ok(k) = d.get_document("keyPattern") {
        n.properties
            .push(("Key pattern".into(), to_shell(&Bson::Document(k.clone()))));
    }
    if let Ok(dir) = d.get_str("direction") {
        n.properties.push(("Direction".into(), dir.into()));
    }
    if let Ok(i) = d.get_document("inputStage") {
        n.children.push(plan_node(i));
    }
    if let Ok(a) = d.get_array("inputStages") {
        n.children
            .extend(a.iter().filter_map(|x| x.as_document()).map(plan_node));
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::config::ConnectionConfig;

    #[test]
    fn connection_strings() {
        let mut cfg = ConnectionConfig::new(DbKind::MongoDb, "x");
        cfg.host = "db.local".into();
        cfg.user = Some("app".into());
        cfg.ssl.mode = SslMode::Disable;
        let p = ConnectParams::from_config(&cfg, Some(dbiji_core::Secret::new("p@ss:w/rd")));
        let u = uri(&p);
        assert!(
            u.starts_with("mongodb://app:p%40ss%3Aw%2Frd@db.local:27017/?appName=DBiji"),
            "{u}"
        );
        assert!(
            u.contains("directConnection=true") && u.contains("authSource=admin"),
            "{u}"
        );
        cfg.properties.insert("srv".into(), "true".into());
        cfg.ssl.mode = SslMode::Prefer;
        let u = uri(&ConnectParams::from_config(&cfg, None));
        assert!(
            u.starts_with("mongodb+srv://app@db.local/?")
                && u.contains("tls=true")
                && !u.contains("directConnection"),
            "{u}"
        );
    }

    #[test]
    fn documents_become_rows() {
        let docs = vec![
            doc! {"_id": 1, "name": "a", "tags": ["x"]},
            doc! {"_id": 2, "age": 30, "name": Bson::Null},
        ];
        let cols = columns(&docs);
        assert_eq!(
            cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["_id", "name", "tags", "age"]
        );
        assert_eq!(cols[2].kind, DataKind::Array);
        let r = row(&docs[1], &cols);
        assert_eq!(
            r.to_vec(),
            vec![Value::Int(2), Value::Null, Value::Null, Value::Int(30)]
        );
        assert_eq!(
            value(&Bson::Document(doc! {"a": {"b": 1}})),
            Value::Json("{\"a\":{\"b\":1}}".into())
        );
    }
}
