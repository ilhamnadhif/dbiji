//! MongoDB [`Session`]: connection, shell statements, catalog and plans.
//!
//! One session is one `mongodb::Client` (the driver pools its own sockets).
//! `execute` parses the shell statement ([`crate::shell`]) and runs it as a
//! server command; finds and aggregations stream through a [`MongoCursor`]
//! that takes the column set from the first documents it sees.

use crate::dialect::MongoDialect;
use crate::error::{map_error, map_error_with};
use crate::shell::{self, ShellCommand, ShowWhat};
use crate::types::{self, Columns};
use async_trait::async_trait;
use dbiji_core::ddl::*;
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::{ServerInfo, parse_version_number};
use dbiji_core::meta::*;
use dbiji_core::plan::{PlanNode, QueryPlan};
use dbiji_core::{CancelHandle, ColumnMeta, ConnectParams, DbError, DbKind, DbResult, ErrorKind, ExecOptions, QueryCursor, ResultHeader, Row, ServerMessage, Session, TransactionMode, Value};
use futures::TryStreamExt;
use mongodb::bson::{Bson, Document, doc};
use mongodb::options::{ClientOptions, Credential, FindOptions, ServerAddress, Tls, TlsOptions};
use mongodb::{Client, Collection, Database};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

/// Documents read to derive the columns of a result (a find result may be
/// larger than memory; the header needs a sample first).
const COLUMN_SAMPLE: usize = 500;

/// Documents a catalog query samples to learn a collection's fields.
pub(crate) const FIELD_SAMPLE: i64 = 200;

/// One MongoDB session.
pub struct MongoSession {
    client: Client,
    database: String,
    dialect: Arc<MongoDialect>,
    info: ServerInfo,
    read_only: bool,
    closed: bool,
}

/// How a connection encrypts its traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TlsChoice {
    /// Plain TCP.
    Off,
    /// TLS without certificate validation (`require`).
    Insecure,
    /// TLS with chain and host name validation (`verify-ca`, `verify-full`).
    Verify,
}

impl MongoSession {
    /// Connect with the parameters resolved by the application.
    ///
    /// MongoDB has no in-band TLS negotiation: the handshake happens before
    /// the first byte of the protocol. `prefer` therefore tries one mode and
    /// then the other — plain first for a direct host, TLS first for an
    /// `mongodb+srv://` seed list (the URI specification makes TLS the default
    /// there).
    pub async fn connect(params: &ConnectParams) -> DbResult<MongoSession> {
        let choices = tls_choices(params);
        // Two attempts must not take twice the connect timeout: a mistyped host
        // would otherwise wait 2× before the error appears.
        let attempt_timeout = params.connect_timeout.max(Duration::from_millis(500)) / choices.len() as u32;
        let mut last = None;
        // The database the user is checked in: the URI rule, the connection's
        // database, else `admin`.
        let source = auth_source(params);
        for (i, choice) in choices.iter().enumerate() {
            match Self::open(params, *choice, attempt_timeout, source.as_deref()).await {
                Ok(session) => return Ok(session),
                Err(e) => {
                    // A user created in `admin` (Atlas, or the first user of a
                    // `mongod`) is not found in the database the connection
                    // opens: check the other place once before giving up.
                    if let (ErrorKind::Auth, Some(fallback)) = (e.kind, auth_source_fallback(params))
                        && let Ok(session) = Self::open(params, *choice, attempt_timeout, Some(fallback)).await
                    {
                        tracing::debug!("MongoDB authenticated the user in {fallback}, not in {}", source.as_deref().unwrap_or("admin"));
                        return Ok(session);
                    }
                    if i + 1 == choices.len() || !worth_retrying(&e) {
                        return Err(e);
                    }
                    tracing::debug!("MongoDB with {:?} failed ({}); trying the other TLS mode", choice, e.message);
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| DbError::connection("Could not connect to MongoDB")))
    }

    async fn open(params: &ConnectParams, tls: TlsChoice, timeout: Duration, source: Option<&str>) -> DbResult<MongoSession> {
        let mut options = options_for(params, tls, timeout).await?;
        options.app_name = Some(params.application_name.clone());
        options.connect_timeout = Some(timeout);
        options.server_selection_timeout = Some(timeout);
        options.tls = Some(match tls {
            TlsChoice::Off => Tls::Disabled,
            TlsChoice::Insecure => {
                let mut t = TlsOptions::default();
                // `require` trusts any certificate, as the SQL and MySQL drivers do.
                t.allow_invalid_certificates = Some(true);
                Tls::Enabled(t)
            }
            TlsChoice::Verify => {
                let mut t = TlsOptions::default();
                t.allow_invalid_certificates = Some(false);
                t.ca_file_path = params.ssl.ca_cert.as_deref().filter(|c| !c.is_empty()).map(std::path::PathBuf::from);
                Tls::Enabled(t)
            }
        });
        options.credential = credential(params, source);
        if let Some(direct) = params.property("directConnection").or(params.property("direct_connection")) {
            options.direct_connection = Some(direct == "true" || direct == "1");
        }
        if let Some(rs) = params.property("replicaSet").filter(|v| !v.is_empty()) {
            options.repl_set_name = Some(rs.to_string());
        }
        let client = Client::with_options(options).map_err(|e| map_error(&e).with_context("Connecting to MongoDB"))?;
        let admin = client.database("admin");
        // A round trip now, so a TLS, authentication or version failure is
        // reported for the attempt that caused it instead of on the first
        // statement.
        admin.run_command(doc! {"ping": 1}).await.map_err(|e| map_error_with(&e, "Connecting to MongoDB"))?;
        let build = admin.run_command(doc! {"buildInfo": 1}).await.map_err(|e| map_error_with(&e, "Reading the server version"))?;
        let version = build.get_str("version").unwrap_or("").to_string();
        let info = ServerInfo { product: "MongoDB".into(), version_number: parse_version_number(&version), version, session_id: None };
        let database = params.database.clone().filter(|d| !d.is_empty()).unwrap_or_else(|| "test".into());
        Ok(MongoSession { client, database, dialect: Arc::new(MongoDialect::new()), info, read_only: params.read_only, closed: false })
    }

    fn db(&self, name: Option<&String>) -> Database {
        self.client.database(name.map(String::as_str).unwrap_or(&self.database))
    }

    fn coll(&self, db: Option<&String>, coll: &str) -> Collection<Document> {
        self.db(db).collection::<Document>(coll)
    }

    fn timeout(&self, opts: &ExecOptions) -> Option<Duration> {
        opts.timeout
    }

    /// Run a shell statement into a result.
    async fn run_statement(&mut self, stmt: &shell::Statement, opts: &ExecOptions) -> DbResult<Outcome> {
        let timeout = self.timeout(opts);
        let cmd = stmt.command.clone();
        match cmd {
            ShellCommand::Find { db, coll, filter, projection, sort, limit, skip } => {
                let mut options = FindOptions::default();
                options.projection = projection;
                options.sort = sort;
                options.limit = limit;
                options.skip = skip.map(|s| s as u64);
                options.batch_size = Some(1000);
                options.max_time = timeout;
                let cursor = self.coll(db.as_ref(), &coll).find(filter).with_options(options).await.map_err(|e| map_error_with(&e, &format!("find {coll}")))?;
                Ok(Outcome::Stream(Box::new(MongoCursor::new(cursor, coll))))
            }
            ShellCommand::FindOne { db, coll, filter, projection } => {
                let mut options = mongodb::options::FindOneOptions::default();
                options.projection = projection;
                options.max_time = timeout;
                let doc = self.coll(db.as_ref(), &coll).find_one(filter).with_options(options).await.map_err(|e| map_error_with(&e, &format!("findOne {coll}")))?;
                Ok(Outcome::Rows(document_table(doc.into_iter().collect())))
            }
            ShellCommand::Aggregate { db, coll, pipeline } => {
                let mut options = mongodb::options::AggregateOptions::default();
                options.batch_size = Some(1000);
                options.max_time = timeout;
                let name = coll.clone().unwrap_or_else(|| self.database.clone());
                let cursor = match &coll {
                    Some(c) => self.coll(db.as_ref(), c).aggregate(pipeline).with_options(options).await,
                    None => self.db(db.as_ref()).aggregate(pipeline).with_options(options).await,
                }
                .map_err(|e| map_error_with(&e, "aggregate"))?;
                Ok(Outcome::Stream(Box::new(MongoCursor::new(cursor, name))))
            }
            ShellCommand::Count { db, coll, filter } => {
                let n = self.coll(db.as_ref(), &coll).count_documents(filter).await.map_err(|e| map_error_with(&e, &format!("count {coll}")))?;
                Ok(Outcome::Rows(single_int("count", n as i64)))
            }
            ShellCommand::EstimatedCount { db, coll } => {
                let n = self.coll(db.as_ref(), &coll).estimated_document_count().await.map_err(|e| map_error_with(&e, "estimatedDocumentCount"))?;
                Ok(Outcome::Rows(single_int("count", n as i64)))
            }
            ShellCommand::Distinct { db, coll, field, filter } => {
                let values = self.coll(db.as_ref(), &coll).distinct(&field, filter).await.map_err(|e| map_error_with(&e, "distinct"))?;
                let sample = values.iter().find(|v| !matches!(v, Bson::Null));
                let column = types::column(&field, sample);
                let rows = values.iter().map(|v| vec![types::to_value(v)]).collect();
                Ok(Outcome::Rows((vec![column], rows)))
            }
            ShellCommand::Insert { db, coll, docs } => {
                let n = docs.len();
                if n == 1 {
                    self.coll(db.as_ref(), &coll).insert_one(docs.into_iter().next().unwrap_or_default()).await.map_err(|e| map_error_with(&e, "insertOne"))?;
                } else if n > 1 {
                    self.coll(db.as_ref(), &coll).insert_many(docs).await.map_err(|e| map_error_with(&e, "insertMany"))?;
                }
                Ok(Outcome::Update { n: Some(n as u64), tag: format!("inserted {n} document{}", if n == 1 { "" } else { "s" }) })
            }
            ShellCommand::Update { db, coll, filter, update, many, upsert } => {
                let c = self.coll(db.as_ref(), &coll);
                let result = if many { c.update_many(filter, update).upsert(upsert).await } else { c.update_one(filter, update).upsert(upsert).await }
                    .map_err(|e| map_error_with(&e, "update"))?;
                Ok(Outcome::Update {
                    n: Some(result.modified_count),
                    tag: format!("matched {}, modified {}{}", result.matched_count, result.modified_count, if result.upserted_id.is_some() { ", upserted" } else { "" }),
                })
            }
            ShellCommand::Delete { db, coll, filter, many } => {
                let c = self.coll(db.as_ref(), &coll);
                let result = if many { c.delete_many(filter).await } else { c.delete_one(filter).await }.map_err(|e| map_error_with(&e, "delete"))?;
                Ok(Outcome::Update { n: Some(result.deleted_count), tag: format!("deleted {}", result.deleted_count) })
            }
            ShellCommand::FindAndModify { db, coll, filter, update, delete } => {
                let c = self.coll(db.as_ref(), &coll);
                let doc = match (delete, update) {
                    (true, _) => c.find_one_and_delete(filter).await,
                    (false, Some(u)) => c.find_one_and_update(filter, u).await,
                    (false, None) => Ok(None),
                }
                .map_err(|e| map_error_with(&e, "findOneAndModify"))?;
                Ok(Outcome::Rows(document_table(doc.into_iter().collect())))
            }
            ShellCommand::CreateCollection { db, coll, options } => {
                let mut cmd = doc! {"create": coll.clone()};
                for (k, v) in options {
                    cmd.insert(k, v);
                }
                self.db(db.as_ref()).run_command(cmd).await.map_err(|e| map_error_with(&e, "createCollection"))?;
                Ok(Outcome::Update { n: None, tag: format!("created collection {coll}") })
            }
            ShellCommand::DropCollection { db, coll } => {
                self.coll(db.as_ref(), &coll).drop().await.map_err(|e| map_error_with(&e, "drop"))?;
                Ok(Outcome::Update { n: None, tag: format!("dropped collection {coll}") })
            }
            ShellCommand::DropDatabase { db } => {
                let name = db.clone().unwrap_or_else(|| self.database.clone());
                self.db(Some(&name)).drop().await.map_err(|e| map_error_with(&e, "dropDatabase"))?;
                Ok(Outcome::Update { n: None, tag: format!("dropped database {name}") })
            }
            ShellCommand::CreateIndex { db, coll, keys, options } => {
                let name = index_name(&keys, &options);
                let mut spec = Document::new();
                spec.insert("key", Bson::Document(keys));
                spec.insert("name", Bson::String(name.clone()));
                for (k, v) in options {
                    spec.insert(k, v);
                }
                let cmd = doc! {"createIndexes": coll.clone(), "indexes": [Bson::Document(spec)]};
                self.db(db.as_ref()).run_command(cmd).await.map_err(|e| map_error_with(&e, "createIndex"))?;
                Ok(Outcome::Update { n: None, tag: format!("index {name} created") })
            }
            ShellCommand::DropIndex { db, coll, name } => {
                let cmd = if name == "*" {
                    doc! {"dropIndexes": coll.clone(), "index": "*"}
                } else {
                    doc! {"dropIndexes": coll.clone(), "index": name.clone()}
                };
                self.db(db.as_ref()).run_command(cmd).await.map_err(|e| map_error_with(&e, "dropIndex"))?;
                Ok(Outcome::Update { n: None, tag: format!("dropped index {name}") })
            }
            ShellCommand::ListIndexes { db, coll } => {
                let indexes = list_indexes(self.db(db.as_ref()), &coll).await?;
                Ok(Outcome::Rows(indexes_table(indexes)))
            }
            ShellCommand::Stats { db, coll } => {
                let cmd = match &coll {
                    Some(c) => doc! {"collStats": c.clone()},
                    None => doc! {"dbStats": 1},
                };
                let out = self.db(db.as_ref()).run_command(cmd).await.map_err(|e| map_error_with(&e, "stats"))?;
                Ok(Outcome::Rows(document_table(vec![out])))
            }
            ShellCommand::RunCommand { db, admin, command } => {
                let database = if admin { self.client.database("admin") } else { self.db(db.as_ref()) };
                let out = database.run_command(command).await.map_err(|e| map_error_with(&e, "command"))?;
                // A command returning a cursor (`listCollections`, `find` in a
                // command document) is read to its end.
                if let Ok(cursor) = out.get_document("cursor") {
                    let ns = cursor.get_str("collection").unwrap_or("").to_string();
                    let mut docs = cursor.get_array("firstBatch").map(|a| a.iter().filter_map(|b| b.as_document().cloned()).collect::<Vec<_>>()).unwrap_or_default();
                    let id = cursor.get("id").cloned().unwrap_or(Bson::Int64(0));
                    if !matches!(id, Bson::Int64(0)) && !matches!(id, Bson::Int32(0)) {
                        docs.extend(drain_cursor(&database, id, &ns).await?);
                    }
                    return Ok(Outcome::Rows(document_table(docs)));
                }
                Ok(Outcome::Rows(document_table(vec![out])))
            }
            ShellCommand::Show { what } => match what {
                ShowWhat::Databases => {
                    let dbs = self.client.list_databases().await.map_err(|e| map_error_with(&e, "listDatabases"))?;
                    let rows = dbs
                        .into_iter()
                        .map(|d| {
                            let mut doc = Document::new();
                            doc.insert("name", d.name);
                            doc.insert("sizeOnDisk", Bson::Int64(d.size_on_disk as i64));
                            doc.insert("empty", d.empty);
                            doc
                        })
                        .collect();
                    Ok(Outcome::Rows(document_table(rows)))
                }
                ShowWhat::Collections => {
                    let names = self.db(None).list_collection_names().await.map_err(|e| map_error_with(&e, "listCollections"))?;
                    Ok(Outcome::Rows(single_text("collection", names)))
                }
            },
            ShellCommand::Use(name) => {
                self.database = name.clone();
                Ok(Outcome::Update { n: None, tag: format!("switched to database {name}") })
            }
        }
    }
}

/// The TLS modes to try, in order. `prefer` means "whichever works": plain
/// first for a direct host, TLS first for a seed list.
fn tls_choices(params: &ConnectParams) -> Vec<TlsChoice> {
    use dbiji_core::config::SslMode;
    match params.ssl.mode {
        SslMode::Disable => vec![TlsChoice::Off],
        SslMode::Require => vec![TlsChoice::Insecure],
        SslMode::VerifyCa | SslMode::VerifyFull => vec![TlsChoice::Verify],
        SslMode::Prefer if is_srv(params) => vec![TlsChoice::Insecure, TlsChoice::Off],
        SslMode::Prefer => vec![TlsChoice::Off, TlsChoice::Insecure],
    }
}

/// A connection made through a `mongodb+srv://` seed list.
fn is_srv(params: &ConnectParams) -> bool {
    params.property("srv").is_some_and(|v| v == "true" || v == "1")
}

/// Is another attempt with the other TLS mode worth it?
///
/// A failed handshake or a closed socket is (a server that requires TLS refuses
/// plaintext, and one that only speaks an old TLS version refuses ours). A
/// refused login, a version mismatch or a DNS failure is conclusive.
fn worth_retrying(err: &DbError) -> bool {
    matches!(err.kind, ErrorKind::Connection | ErrorKind::Tls | ErrorKind::Io) && !err.message.contains("wire version")
}

/// The credential of a connection, or `None` when it names no user (the
/// server then answers as an unauthenticated client). `source` is the database
/// the user is checked in.
fn credential(params: &ConnectParams, source: Option<&str>) -> Option<Credential> {
    let user = params.user.as_deref().filter(|user| !user.is_empty())?;
    let mut credential = Credential::default();
    credential.username = Some(user.to_string());
    credential.password = params.password.as_ref().map(|password| password.expose().to_string());
    credential.source = source.map(str::to_string);
    credential.mechanism = params.property("authMechanism").and_then(|mechanism| mechanism.parse().ok());
    Some(credential)
}

/// The `authSource` property, when the connection sets one.
fn explicit_auth_source(params: &ConnectParams) -> Option<&str> {
    params.property("authSource").or_else(|| params.property("authsource")).filter(|source| !source.is_empty())
}

/// The other database a user may be defined in when the first attempt is
/// refused: `admin`, for a user created there (Atlas, or the first user of a
/// `mongod`) while the connection opens another database.
fn auth_source_fallback(params: &ConnectParams) -> Option<&'static str> {
    let database = params.database.as_deref().filter(|db| !db.is_empty())?;
    (explicit_auth_source(params).is_none() && database != "admin").then_some("admin")
}

/// Database a user is authenticated against: the `authSource` property, else
/// the connection's database — the URI specification's `defaultauthdb` — else
/// `None`, which the driver reads as `admin`.
///
/// A user created with `use shop; db.createUser(…)` is defined in `shop` only:
/// checking the credentials against `admin` answers
/// `SCRAM failure: Authentication failed.` (code 18).
pub(crate) fn auth_source(params: &ConnectParams) -> Option<String> {
    explicit_auth_source(params)
        .map(str::to_string)
        .or_else(|| params.database.clone().filter(|db| !db.is_empty()))
}

/// Driver options for one attempt: the hosts (a seed list through DNS for
/// `mongodb+srv://`, one host otherwise).
async fn options_for(params: &ConnectParams, _tls: TlsChoice, timeout: Duration) -> DbResult<ClientOptions> {
    if is_srv(params) {
        let host = params.host.trim();
        let uri = match params.database.as_deref().filter(|d| !d.is_empty()) {
            Some(db) => format!("mongodb+srv://{host}/{db}"),
            None => format!("mongodb+srv://{host}"),
        };
        // The lookup is DNS: bound it by the connect timeout.
        let parsed = tokio::time::timeout(timeout, ClientOptions::parse(&uri))
            .await
            .map_err(|_| DbError::timeout(format!("Resolving {host} took longer than the connect timeout")))?
            .map_err(|e| map_error_with(&e, &format!("Resolving the seed list of {host}")))?;
        return Ok(parsed);
    }
    let mut options = ClientOptions::default();
    options.hosts = vec![ServerAddress::Tcp { host: params.host.clone(), port: Some(params.port) }];
    Ok(options)
}

/// What a statement produced before it becomes a cursor.
enum Outcome {
    Rows((Vec<ColumnMeta>, Vec<Row>)),
    Stream(Box<MongoCursor>),
    Update { n: Option<u64>, tag: String },
}

/// Streams documents of a find or aggregation as rows.
pub struct MongoCursor {
    cursor: Option<mongodb::Cursor<Document>>,
    columns: Columns,
    pending: VecDeque<Document>,
    started: bool,
    done: bool,
    label: String,
}

impl MongoCursor {
    fn new(cursor: mongodb::Cursor<Document>, label: impl Into<String>) -> Self {
        MongoCursor { cursor: Some(cursor), columns: Columns::new(), pending: VecDeque::new(), started: false, done: false, label: label.into() }
    }

    async fn pull(&mut self, max: usize) -> DbResult<Vec<Row>> {
        let mut out = Vec::with_capacity(max.min(64));
        // Documents sampled to derive the columns come first; the cursor may
        // already be exhausted when the whole result fitted in the sample.
        while out.len() < max {
            if let Some(d) = self.pending.pop_front() {
                out.push(self.columns.row(&d));
                continue;
            }
            let Some(cursor) = self.cursor.as_mut() else {
                self.done = true;
                break;
            };
            match cursor.try_next().await {
                Ok(Some(d)) => {
                    if !self.started {
                        self.pending.push_back(d);
                        continue;
                    }
                    out.push(self.columns.row(&d));
                }
                Ok(None) => {
                    self.done = true;
                    self.cursor = None;
                    break;
                }
                Err(e) => {
                    self.done = true;
                    self.cursor = None;
                    return Err(map_error_with(&e, &format!("reading {}", self.label)));
                }
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl QueryCursor for MongoCursor {
    async fn next_result(&mut self) -> DbResult<Option<ResultHeader>> {
        if self.started {
            return Ok(None);
        }
        self.started = true;
        // Sample the first documents to derive the columns.
        let mut docs = Vec::new();
        if let Some(cursor) = self.cursor.as_mut() {
            while docs.len() < COLUMN_SAMPLE {
                match cursor.try_next().await {
                    Ok(Some(d)) => docs.push(d),
                    Ok(None) => {
                        self.done = true;
                        self.cursor = None;
                        break;
                    }
                    Err(e) => {
                        self.done = true;
                        self.cursor = None;
                        return Err(map_error_with(&e, &format!("reading {}", self.label)));
                    }
                }
            }
        }
        for d in &docs {
            self.columns.observe(d);
        }
        self.pending = docs.into();
        let metas = self.columns.metas("string");
        Ok(Some(ResultHeader::Rows(metas)))
    }

    async fn fetch(&mut self, max_rows: usize) -> DbResult<Vec<Row>> {
        if self.done && self.pending.is_empty() {
            return Ok(Vec::new());
        }
        self.pull(max_rows.max(1)).await
    }

    async fn close(&mut self) -> DbResult<()> {
        self.pending.clear();
        self.cursor = None;
        self.done = true;
        Ok(())
    }

    fn take_messages(&mut self) -> Vec<ServerMessage> {
        Vec::new()
    }
}

// ------------------------------------------------------------------ shaping

/// One column per field of the documents.
fn document_table(docs: Vec<Document>) -> (Vec<ColumnMeta>, Vec<Row>) {
    let mut cols = Columns::new();
    for d in &docs {
        cols.observe(d);
    }
    let rows = docs.iter().map(|d| cols.row(d)).collect();
    (cols.metas("string"), rows)
}

fn single_int(name: &str, n: i64) -> (Vec<ColumnMeta>, Vec<Row>) {
    (vec![ColumnMeta::new(name, "long", dbiji_core::DataKind::Int)], vec![vec![Value::Int(n)]])
}

fn single_text(name: &str, values: Vec<String>) -> (Vec<ColumnMeta>, Vec<Row>) {
    let rows = values.into_iter().map(|v| vec![Value::Text(v.into())]).collect();
    (vec![ColumnMeta::new(name, "string", dbiji_core::DataKind::Text)], rows)
}

/// Index specification name (`a_1_b_-1` unless the options name it).
fn index_name(keys: &Document, options: &Document) -> String {
    if let Ok(name) = options.get_str("name") {
        return name.to_string();
    }
    keys.iter()
        .map(|(k, v)| {
            let dir = match v {
                Bson::Int32(n) => n.to_string(),
                Bson::Int64(n) => n.to_string(),
                Bson::Double(n) => n.to_string(),
                Bson::String(s) => s.clone(),
                _ => "1".into(),
            };
            format!("{k}_{dir}")
        })
        .collect::<Vec<_>>()
        .join("_")
}

/// Read a server cursor to its end (`getMore`).
async fn drain_cursor(db: &Database, id: Bson, collection: &str) -> DbResult<Vec<Document>> {
    let mut out = Vec::new();
    let mut current = id;
    loop {
        let reply = db.run_command(doc! {"getMore": current.clone(), "collection": collection}).await.map_err(|e| map_error_with(&e, "getMore"))?;
        let cursor = reply.get_document("cursor").cloned().unwrap_or_default();
        if let Ok(batch) = cursor.get_array("nextBatch") {
            out.extend(batch.iter().filter_map(|b| b.as_document().cloned()));
        }
        let next = cursor.get("id").cloned().unwrap_or(Bson::Int64(0));
        if matches!(next, Bson::Int64(0)) || matches!(next, Bson::Int32(0)) {
            return Ok(out);
        }
        current = next;
    }
}

// -------------------------------------------------------------------- catalog

/// Indexes of a collection as (name, keys, unique, sparse, definition).
pub(crate) struct IndexInfo {
    pub name: String,
    pub keys: Document,
    pub unique: bool,
    pub sparse: bool,
    pub partial: Option<Document>,
}

pub(crate) async fn list_indexes(db: Database, coll: &str) -> DbResult<Vec<IndexInfo>> {
    let collection = db.collection::<Document>(coll);
    let mut cursor = collection.list_indexes().await.map_err(|e| map_error_with(&e, "listIndexes"))?;
    let mut out = Vec::new();
    loop {
        match cursor.try_next().await {
            Ok(Some(idx)) => {
                let options = idx.options.clone().unwrap_or_default();
                out.push(IndexInfo {
                    name: options.name.clone().unwrap_or_default(),
                    keys: idx.keys.clone(),
                    unique: options.unique.unwrap_or(false),
                    sparse: options.sparse.unwrap_or(false),
                    partial: options.partial_filter_expression.clone(),
                });
            }
            Ok(None) => break,
            Err(e) => return Err(map_error_with(&e, "listIndexes")),
        }
    }
    Ok(out)
}

fn indexes_table(indexes: Vec<IndexInfo>) -> (Vec<ColumnMeta>, Vec<Row>) {
    let cols = vec![
        ColumnMeta::new("name", "string", dbiji_core::DataKind::Text),
        ColumnMeta::new("keys", "object", dbiji_core::DataKind::Json),
        ColumnMeta::new("unique", "bool", dbiji_core::DataKind::Bool),
        ColumnMeta::new("sparse", "bool", dbiji_core::DataKind::Bool),
        ColumnMeta::new("partialFilterExpression", "object", dbiji_core::DataKind::Json),
    ];
    let rows = indexes
        .into_iter()
        .map(|i| {
            vec![
                Value::Text(i.name.into()),
                Value::Json(types::json_text(&Bson::Document(i.keys)).into()),
                Value::Bool(i.unique),
                Value::Bool(i.sparse),
                i.partial.map(|p| Value::Json(types::json_text(&Bson::Document(p)).into())).unwrap_or(Value::Null),
            ]
        })
        .collect();
    (cols, rows)
}

/// Fields of a collection, sampled from its documents: (name, type).
pub(crate) async fn sample_fields(db: &Database, coll: &str) -> DbResult<Vec<(String, String)>> {
    let collection = db.collection::<Document>(coll);
    let mut options = FindOptions::default();
    options.limit = Some(FIELD_SAMPLE);
    options.batch_size = Some(FIELD_SAMPLE as u32);
    let mut cursor = collection.find(doc! {}).with_options(options).await.map_err(|e| map_error_with(&e, "sampling the collection"))?;
    let mut cols = Columns::new();
    loop {
        match cursor.try_next().await {
            Ok(Some(d)) => cols.observe(&d),
            Ok(None) => break,
            Err(e) => return Err(map_error_with(&e, "sampling the collection")),
        }
    }
    Ok(cols.metas("string").into_iter().map(|c| (c.name, c.type_name)).collect())
}

// ------------------------------------------------------------------- session

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
        // Statements carry a `maxTimeMS` from `ExecOptions::timeout`; there is
        // no out-of-band kill that cannot hit another session's operation.
        None
    }

    async fn ping(&mut self) -> DbResult<()> {
        self.client.database("admin").run_command(doc! {"ping": 1}).await.map_err(|e| map_error_with(&e, "ping"))?;
        Ok(())
    }

    async fn close(&mut self) -> DbResult<()> {
        self.closed = true;
        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.closed
    }

    async fn execute(&mut self, sql: &str, params: &[Value], opts: &ExecOptions) -> DbResult<Box<dyn QueryCursor>> {
        if !params.is_empty() {
            return Err(DbError::unsupported("The MongoDB shell has no statement parameters"));
        }
        if self.closed {
            return Err(DbError::connection("The connection is closed"));
        }
        let stmt = shell::parse(sql).map_err(DbError::syntax)?;
        if self.read_only && is_write(&stmt.command) {
            return Err(DbError::read_only("The connection is read-only; this statement changes data"));
        }
        match self.run_statement(&stmt, opts).await? {
            Outcome::Rows((cols, rows)) => Ok(Box::new(dbiji_core::driver::MemoryCursor::rows(cols, rows))),
            Outcome::Update { n, tag } => Ok(Box::new(dbiji_core::driver::MemoryCursor::update(n, Some(tag)))),
            Outcome::Stream(cursor) => Ok(cursor),
        }
    }

    fn transaction_mode(&self) -> TransactionMode {
        TransactionMode::Auto
    }

    async fn set_transaction_mode(&mut self, mode: TransactionMode) -> DbResult<()> {
        match mode {
            TransactionMode::Auto => Ok(()),
            TransactionMode::Manual => Err(DbError::unsupported("MongoDB has no interactive transactions here; each statement commits on its own")),
        }
    }

    fn in_transaction(&self) -> bool {
        false
    }

    async fn commit(&mut self) -> DbResult<()> {
        Err(DbError::unsupported("MongoDB has no transactions in DBiji; each statement commits on its own"))
    }

    async fn rollback(&mut self) -> DbResult<()> {
        Err(DbError::unsupported("MongoDB has no transactions in DBiji; each statement commits on its own"))
    }

    async fn current_database(&mut self) -> DbResult<Option<String>> {
        Ok(Some(self.database.clone()))
    }

    async fn current_schema(&mut self) -> DbResult<Option<String>> {
        Ok(Some(self.database.clone()))
    }

    async fn set_current_schema(&mut self, schema: &str) -> DbResult<()> {
        if schema.trim().is_empty() {
            return Err(DbError::config("The database name is empty"));
        }
        self.database = schema.to_string();
        Ok(())
    }

    async fn set_current_database(&mut self, database: &str) -> DbResult<()> {
        self.set_current_schema(database).await
    }

    async fn list_databases(&mut self) -> DbResult<Vec<DatabaseInfo>> {
        let dbs = self.client.list_databases().await.map_err(|e| map_error_with(&e, "listDatabases"))?;
        Ok(dbs
            .into_iter()
            .map(|d| DatabaseInfo {
                is_current: d.name == self.database,
                is_system: matches!(d.name.as_str(), "admin" | "local" | "config"),
                size_bytes: Some(d.size_on_disk as i64),
                name: d.name,
                ..Default::default()
            })
            .collect())
    }

    async fn list_schemas(&mut self) -> DbResult<Vec<SchemaInfo>> {
        // A MongoDB database is shown as a schema (as MySQL databases are).
        let dbs = self.list_databases().await?;
        Ok(dbs.into_iter().map(|d| SchemaInfo { is_current: d.is_current, is_system: d.is_system, name: d.name, owner: None, comment: None }).collect())
    }

    async fn list_objects(&mut self, schema: &SchemaRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
        let wanted = match kind {
            ObjectKind::View | ObjectKind::MaterializedView => ObjectKind::View,
            ObjectKind::Table => ObjectKind::Table,
            _ => return Ok(Vec::new()),
        };
        let db = self.db(schema.schema.as_ref());
        let mut cursor = db.list_collections().await.map_err(|e| map_error_with(&e, "listCollections"))?;
        let mut out = Vec::new();
        loop {
            let spec = match cursor.try_next().await {
                Ok(Some(s)) => s,
                Ok(None) => break,
                Err(e) => return Err(map_error_with(&e, "listCollections")),
            };
            let object_kind = match spec.collection_type {
                mongodb::results::CollectionType::View => ObjectKind::View,
                _ => ObjectKind::Table,
            };
            if object_kind != wanted {
                continue;
            }
            let detail = if object_kind == ObjectKind::View {
                spec.options.view_on.clone().map(|v| format!("view on {v}"))
            } else if matches!(spec.collection_type, mongodb::results::CollectionType::Timeseries) {
                Some("time series".into())
            } else {
                None
            };
            let mut info = ObjectInfo::new(ObjectRef { kind: object_kind, database: None, schema: schema.schema.clone(), table: None, name: spec.name.clone(), signature: None });
            info.detail = detail;
            out.push(info);
        }
        Ok(out)
    }

    async fn list_children(&mut self, parent: &ObjectRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
        match kind {
            ObjectKind::Column => {
                let db = self.db(parent.schema.as_ref());
                let fields = sample_fields(&db, &parent.name).await?;
                Ok(fields
                    .into_iter()
                    .map(|(name, ty)| {
                        let mut info = ObjectInfo::new(ObjectRef::child(ObjectKind::Column, parent, name));
                        info.detail = Some(ty);
                        info
                    })
                    .collect())
            }
            ObjectKind::Index | ObjectKind::UniqueKey => {
                let db = self.db(parent.schema.as_ref());
                let indexes = list_indexes(db, &parent.name).await?;
                Ok(indexes
                    .into_iter()
                    .map(|i| {
                        let mut info = ObjectInfo::new(ObjectRef::child(ObjectKind::Index, parent, i.name));
                        let keys = i.keys.iter().map(|(k, v)| format!("{k}: {}", dir_text(v))).collect::<Vec<_>>().join(", ");
                        info.detail = Some(if i.unique { format!("unique {{{keys}}}") } else { format!("{{{keys}}}") });
                        info
                    })
                    .collect())
            }
            _ => Ok(Vec::new()),
        }
    }

    async fn table_definition(&mut self, table: &ObjectRef) -> DbResult<TableDef> {
        let db = self.db(table.schema.as_ref());
        let fields = sample_fields(&db, &table.name).await?;
        let indexes = list_indexes(db.clone(), &table.name).await?;
        let mut def = TableDef { schema: table.schema.clone(), name: table.name.clone(), ..Default::default() };
        for (i, (name, ty)) in fields.into_iter().enumerate() {
            let mut c = ColumnDef::new(name, ty);
            c.ordinal = i as u32 + 1;
            c.nullable = true;
            def.columns.push(c);
        }
        for idx in indexes {
            let mut index = IndexDef { name: idx.name, unique: idx.unique, ..Default::default() };
            index.columns = idx.keys.iter().map(|(k, v)| IndexColumn { name: k.clone(), is_expression: false, descending: is_descending(v) }).collect();
            index.primary = index.columns.len() == 1 && index.columns[0].name == "_id";
            if idx.sparse {
                index.method = Some("sparse".into());
            }
            if let Some(p) = idx.partial {
                index.predicate = Some(types::json_text(&Bson::Document(p)));
            }
            def.indexes.push(index);
        }
        if let Some(opts) = collection_spec(&db, &table.name).await? {
            for (k, v) in &opts {
                def.options.insert(k.clone(), types::json_text(v));
            }
        }
        Ok(def)
    }

    async fn view_definition(&mut self, view: &ObjectRef) -> DbResult<ViewDef> {
        let db = self.db(view.schema.as_ref());
        let reply = db.run_command(doc! {"listCollections": 1, "filter": {"name": view.name.clone()}}).await.map_err(|e| map_error_with(&e, "listCollections"))?;
        let first = reply
            .get_document("cursor")
            .ok()
            .and_then(|c| c.get_array("firstBatch").ok())
            .and_then(|b| b.first().and_then(|d| d.as_document().cloned()))
            .ok_or_else(|| DbError::query(format!("The view '{}' does not exist", view.name)))?;
        let options = first.get_document("options").cloned().unwrap_or_default();
        let on = options.get_str("viewOn").unwrap_or("").to_string();
        let pipeline = options.get_array("pipeline").map(|a| a.to_vec()).unwrap_or_default();
        let pipeline_text = pipeline.iter().map(types::json_text).collect::<Vec<_>>().join(", ");
        Ok(ViewDef {
            schema: view.schema.clone(),
            name: view.name.clone(),
            materialized: false,
            definition: format!("db.getCollection({}).aggregate([{pipeline_text}])", crate::ddl::quote(&on)),
            columns: Vec::new(),
            comment: None,
            options: Default::default(),
        })
    }

    async fn object_ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        let db = self.db(obj.schema.as_ref());
        let target = match obj.schema.as_deref().filter(|s| !s.is_empty()) {
            Some(schema) => format!("db.getSiblingDB({}).getCollection({})", crate::ddl::quote(schema), crate::ddl::quote(&obj.name)),
            None => format!("db.getCollection({})", crate::ddl::quote(&obj.name)),
        };
        match obj.kind {
            ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => {
                let options = collection_spec(&db, &obj.name).await?.unwrap_or_default();
                let mut out = String::new();
                let opts = options.iter().map(|(k, v)| format!("{k}: {}", types::json_text(v))).collect::<Vec<_>>().join(", ");
                if obj.kind == ObjectKind::View {
                    let def = self.view_definition(obj).await?;
                    out.push_str(&format!("// view {}\n{}\n", obj.name, def.definition));
                } else if opts.is_empty() {
                    out.push_str(&format!("{target}.__createCollection()\n"));
                    out = out.replace(&format!("{target}.__createCollection()"), &format!("db.createCollection({})", crate::ddl::quote(&obj.name)));
                } else {
                    out.push_str(&format!("db.createCollection({}, {{{opts}}})\n", crate::ddl::quote(&obj.name)));
                }
                for idx in list_indexes(db, &obj.name).await? {
                    let keys = idx.keys.iter().map(|(k, v)| format!("{}: {}", crate::ddl::quote(k), dir_text(v))).collect::<Vec<_>>().join(", ");
                    let mut extra = Vec::new();
                    if !idx.name.is_empty() {
                        extra.push(format!("name: {}", crate::ddl::quote(&idx.name)));
                    }
                    if idx.unique {
                        extra.push("unique: true".into());
                    }
                    if idx.sparse {
                        extra.push("sparse: true".into());
                    }
                    if let Some(p) = idx.partial {
                        extra.push(format!("partialFilterExpression: {}", types::json_text(&Bson::Document(p))));
                    }
                    if idx.name == "_id_" {
                        continue;
                    }
                    out.push_str(&format!("{target}.createIndex({{{keys}}}, {{{}}})\n", extra.join(", ")));
                }
                Ok(out)
            }
            ObjectKind::Index | ObjectKind::UniqueKey => Ok(format!("{target}.createIndex({})\n", crate::ddl::quote(&obj.name))),
            ObjectKind::Database => Ok(format!("db.getSiblingDB({}).createCollection(\"__dbiji\")\n", crate::ddl::quote(&obj.name))),
            _ => Err(DbError::unsupported("No DDL is known for this object in MongoDB")),
        }
    }

    async fn object_properties(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        let db = self.db(obj.schema.as_ref());
        let mut out = vec![Property::new("name", obj.name.clone()), Property::new("type", obj.kind.label().to_string())];
        if let Some(opts) = collection_spec(&db, &obj.name).await? {
            for (k, v) in opts {
                out.push(Property::new(k, types::json_text(&v)));
            }
        }
        let count = db.collection::<Document>(&obj.name).estimated_document_count().await.unwrap_or(0);
        out.push(Property::new("documents (estimated)", count.to_string()));
        Ok(out)
    }

    async fn object_statistics(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        let db = self.db(obj.schema.as_ref());
        let stats = db.run_command(doc! {"collStats": obj.name.clone()}).await.map_err(|e| map_error_with(&e, "collStats"))?;
        let mut out = Vec::new();
        for (k, v) in stats {
            if matches!(v, Bson::Document(_) | Bson::Array(_)) {
                continue;
            }
            out.push(Property::new(k, types::to_value(&v).to_text()));
        }
        Ok(out)
    }

    async fn search_metadata(&mut self, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
        let pattern = search.pattern.to_ascii_lowercase();
        let dbs = if search.schemas.is_empty() { self.list_databases().await?.into_iter().map(|d| d.name).collect::<Vec<_>>() } else { search.schemas.clone() };
        let mut out = Vec::new();
        for name in dbs {
            if search.kinds.iter().all(|k| !matches!(k, ObjectKind::Table | ObjectKind::View)) {
                break;
            }
            let db = self.client.database(&name);
            let names = match db.list_collection_names().await {
                Ok(n) => n,
                Err(_) => continue,
            };
            for coll in names {
                if out.len() >= search.limit.max(1) {
                    return Ok(out);
                }
                let hit = if pattern.is_empty() {
                    true
                } else if search.case_sensitive {
                    coll.contains(&search.pattern)
                } else {
                    coll.to_ascii_lowercase().contains(&pattern)
                };
                if hit {
                    out.push(ObjectInfo::new(ObjectRef { kind: ObjectKind::Table, database: None, schema: Some(name.clone()), table: None, name: coll, signature: None }));
                }
            }
        }
        Ok(out)
    }

    async fn schema_snapshot(&mut self, schema: &SchemaRef, opts: &SnapshotOptions) -> DbResult<SchemaSnapshot> {
        let db = self.db(schema.schema.as_ref());
        let names = db.list_collection_names().await.map_err(|e| map_error_with(&e, "listCollections"))?;
        let mut snap = SchemaSnapshot { kind: Some(DbKind::MongoDb), database: None, schema: schema.schema.clone(), ..Default::default() };
        for name in names {
            let mut def = TableDef { schema: schema.schema.clone(), name: name.clone(), ..Default::default() };
            if opts.columns {
                for (i, (field, ty)) in sample_fields(&db, &name).await?.into_iter().enumerate() {
                    let mut c = ColumnDef::new(field, ty);
                    c.ordinal = i as u32 + 1;
                    c.nullable = true;
                    def.columns.push(c);
                }
            }
            if opts.indexes {
                for idx in list_indexes(db.clone(), &name).await? {
                    let mut index = IndexDef { name: idx.name, unique: idx.unique, ..Default::default() };
                    index.columns = idx.keys.iter().map(|(k, _)| IndexColumn { name: k.clone(), is_expression: false, descending: false }).collect();
                    def.indexes.push(index);
                }
            }
            snap.tables.push(def);
        }
        Ok(snap)
    }

    async fn explain(&mut self, sql: &str, _analyze: bool) -> DbResult<QueryPlan> {
        let stmt = shell::parse(sql).map_err(DbError::syntax)?;
        let command = explain_command(&stmt.command).ok_or_else(|| DbError::unsupported("EXPLAIN is available for find and aggregate statements"))?;
        let db = self.db(statement_db(&stmt.command).as_ref());
        let reply = db.run_command(command).await.map_err(|e| map_error_with(&e, "explain"))?;
        Ok(plan_from_explain(&reply))
    }
}

fn statement_db(cmd: &ShellCommand) -> Option<String> {
    match cmd {
        ShellCommand::Find { db, .. }
        | ShellCommand::FindOne { db, .. }
        | ShellCommand::Aggregate { db, .. }
        | ShellCommand::Count { db, .. }
        | ShellCommand::Distinct { db, .. }
        | ShellCommand::Stats { db, .. } => db.clone(),
        _ => None,
    }
}

/// The `explain` command for a find or aggregation.
fn explain_command(cmd: &ShellCommand) -> Option<Document> {
    let mut inner = Document::new();
    match cmd {
        ShellCommand::Find { coll, filter, sort, limit, skip, projection, .. } => {
            inner.insert("find", coll.clone());
            inner.insert("filter", filter.clone());
            if let Some(s) = sort {
                inner.insert("sort", s.clone());
            }
            if let Some(l) = limit {
                inner.insert("limit", Bson::Int64(*l));
            }
            if let Some(s) = skip {
                inner.insert("skip", Bson::Int64(*s));
            }
            if let Some(p) = projection {
                inner.insert("projection", p.clone());
            }
        }
        ShellCommand::Aggregate { coll, pipeline, .. } => {
            inner.insert("aggregate", coll.clone().unwrap_or_else(|| "1".to_string()));
            inner.insert("pipeline", Bson::Array(pipeline.iter().cloned().map(Bson::Document).collect()));
            inner.insert("cursor", Bson::Document(Document::new()));
        }
        _ => return None,
    }
    Some(doc! {"explain": Bson::Document(inner), "verbosity": "executionStats"})
}

/// Turn an explain reply into the plan model: one node per stage, in order.
fn plan_from_explain(reply: &Document) -> QueryPlan {
    let raw = serde_json::to_string(&Bson::Document(reply.clone())).unwrap_or_default();
    let mut roots = Vec::new();
    let mut planning_ms = None;
    let mut execution_ms = None;
    if let Ok(stages) = reply.get_array("stages") {
        for stage in stages.iter().filter_map(|s| s.as_document()) {
            roots.push(stage_node(stage));
        }
    }
    if let Ok(stats) = reply.get_document("executionStats") {
        execution_ms = stats.get_i64("executionTimeMillis").ok().map(|v| v as f64);
        planning_ms = stats.get_f64("executionTimeMillisEstimate").ok().or(execution_ms);
        let winning = stats.get_document("winningPlan").ok().and_then(|w| w.get_document("queryPlan").cloned().ok().or_else(|| Some(w.clone())));
        if let Some(winning) = winning {
            roots.push(stage_node(&winning));
        }
    }
    if roots.is_empty()
        && let Ok(plan) = reply.get_document("queryPlanner").and_then(|p| p.get_document("winningPlan"))
    {
        roots.push(stage_node(plan));
    }
    let mut plan = QueryPlan { roots, text: String::new(), raw: Some(raw), planning_ms, execution_ms, analyzed: true };
    plan.text = plan.to_text_tree();
    plan
}

fn stage_node(d: &Document) -> PlanNode {
    let stage = d.get_str("stage").unwrap_or("PLAN").to_string();
    let mut node = PlanNode::new(stage);
    if let Ok(ns) = d.get_str("ns") {
        node.relation = Some(ns.to_string());
    }
    if let Ok(index) = d.get_str("indexName") {
        node.index = Some(index.to_string());
    }
    if let Ok(filter) = d.get_document("filter") {
        node.condition = Some(types::json_text(&Bson::Document(filter.clone())));
    }
    if let Ok(n) = d.get_i64("nReturned") {
        node.plan_rows = Some(n as f64);
        node.actual_rows = Some(n as f64);
    }
    if let Ok(ms) = d.get_i64("executionTimeMillisEstimate") {
        node.actual_total_ms = Some(ms as f64);
    }
    if let Ok(input) = d.get_document("inputStage") {
        node.children.push(stage_node(input));
    }
    if let Ok(inputs) = d.get_array("inputStages") {
        for i in inputs {
            if let Some(s) = i.as_document() {
                node.children.push(stage_node(s));
            }
        }
    }
    if let Ok(inner) = d.get_document("queryPlan") {
        node.children.push(stage_node(inner));
    }
    if let Ok(inner) = d.get_document("winningPlan") {
        node.children.push(stage_node(inner));
    }
    node
}

fn is_descending(v: &Bson) -> bool {
    match v {
        Bson::Int32(n) => *n < 0,
        Bson::Int64(n) => *n < 0,
        Bson::Double(n) => *n < 0.0,
        _ => false,
    }
}

fn dir_text(v: &Bson) -> String {
    match v {
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => n.to_string(),
        Bson::Double(n) => n.to_string(),
        Bson::String(s) => format!("\"{s}\""),
        other => types::json_text(other),
    }
}

/// Collection options as reported by `listCollections`.
async fn collection_spec(db: &Database, coll: &str) -> DbResult<Option<Document>> {
    let reply = db.run_command(doc! {"listCollections": 1, "filter": {"name": coll}}).await.map_err(|e| map_error_with(&e, "listCollections"))?;
    let first = reply.get_document("cursor").ok().and_then(|c| c.get_array("firstBatch").ok()).and_then(|b| b.first().and_then(|d| d.as_document().cloned()));
    Ok(first.map(|d| d.get_document("options").cloned().unwrap_or_default()))
}

/// Server commands that only read (`db.runCommand({ping: 1})` on a read-only
/// connection).
const READ_COMMANDS: &[&str] = &[
    "ping",
    "hello",
    "isMaster",
    "buildInfo",
    "serverStatus",
    "listDatabases",
    "listCollections",
    "listIndexes",
    "collStats",
    "dbStats",
    "explain",
    "count",
    "distinct",
    "find",
    "aggregate",
    "connectionStatus",
    "hostInfo",
    "getParameter",
    "replSetGetStatus",
    "usersInfo",
    "rolesInfo",
    "validate",
    "currentOp",
];

fn is_write(cmd: &ShellCommand) -> bool {
    match cmd {
        ShellCommand::Insert { .. }
        | ShellCommand::Update { .. }
        | ShellCommand::Delete { .. }
        | ShellCommand::FindAndModify { .. }
        | ShellCommand::CreateCollection { .. }
        | ShellCommand::DropCollection { .. }
        | ShellCommand::DropDatabase { .. }
        | ShellCommand::CreateIndex { .. }
        | ShellCommand::DropIndex { .. } => true,
        // An unknown command may change anything: it counts as a write.
        ShellCommand::RunCommand { command, .. } => {
            let name = command.keys().next().map(String::as_str).unwrap_or("");
            !READ_COMMANDS.iter().any(|c| c.eq_ignore_ascii_case(name))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(database: Option<&str>, properties: &[(&str, &str)]) -> ConnectParams {
        ConnectParams {
            kind: DbKind::MongoDb,
            host: "localhost".into(),
            port: 27017,
            tls_server_name: None,
            database: database.map(str::to_string),
            user: Some("clerk".into()),
            password: Some(dbiji_core::Secret::new("secret")),
            file_path: None,
            ssl: Default::default(),
            properties: properties.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            application_name: "DBiji".into(),
            connect_timeout: Duration::from_secs(5),
            read_only: false,
            auto_commit: true,
        }
    }

    #[test]
    fn the_credential_authenticates_in_the_uri_database() {
        let p = params(Some("shop"), &[]);
        let source = auth_source(&p);
        let c = credential(&p, source.as_deref()).expect("a credential");
        assert_eq!(c.username.as_deref(), Some("clerk"));
        assert_eq!(c.password.as_deref(), Some("secret"));
        assert_eq!(c.source.as_deref(), Some("shop"));

        // An explicit authSource wins over the database, whatever its spelling.
        let p = params(Some("shop"), &[("authSource", "admin")]);
        assert_eq!(auth_source(&p).as_deref(), Some("admin"));
        let p = params(Some("shop"), &[("authsource", "admin")]);
        assert_eq!(auth_source(&p).as_deref(), Some("admin"));
        // Without either, the driver's own default (admin) applies.
        assert_eq!(auth_source(&params(None, &[])), None);
        // A password without a user names nobody: no credential.
        let mut p = params(Some("shop"), &[]);
        p.user = None;
        assert!(credential(&p, Some("shop")).is_none());
    }

    #[test]
    fn a_refused_login_falls_back_to_admin_once() {
        // The connection opens `shop`: the user may live there (URI rule) or
        // in `admin` (Atlas, the first `mongod` user).
        assert_eq!(auth_source_fallback(&params(Some("shop"), &[])), Some("admin"));
        // `admin` is already the fallback: no second attempt.
        assert_eq!(auth_source_fallback(&params(Some("admin"), &[])), None);
        assert_eq!(auth_source_fallback(&params(None, &[])), None);
        // An explicit authSource is authoritative.
        assert_eq!(auth_source_fallback(&params(Some("shop"), &[("authSource", "users")])), None);
    }
}
