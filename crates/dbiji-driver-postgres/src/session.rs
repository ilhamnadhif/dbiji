//! [`PgSession`]: one live PostgreSQL / CockroachDB connection implementing
//! [`dbiji_core::Session`].

use crate::catalog::{self, P, RowExt};
use crate::connect::{self, PgCancelHandle, SessionShared};
use crate::cursor::{Body, ColumnPlan, CopyBody, ExtendedBody, PgCursor, SimpleBody, StatementTimer};
use crate::dialect::PgDialect;
use crate::error::{map_error, pg};
use crate::params::TextParam;
use crate::sqltext::{CopyKind, ScriptInfo, Tok, tokenize};
use crate::types::{Conv, display_name, typmod_info};
use async_trait::async_trait;
use dbiji_core::admin::*;
use dbiji_core::ddl::*;
use dbiji_core::dialect::Dialect;
use dbiji_core::driver::ServerInfo;
use dbiji_core::meta::*;
use dbiji_core::plan::QueryPlan;
use dbiji_core::{
    CancelHandle, ColumnMeta, ColumnSource, ConnectParams, DbError, DbKind, DbResult, ExecOptions, QueryCursor,
    Session, TransactionMode, Value,
};
use postgres_types::{ToSql, Type};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::OwnedMutexGuard;
use tokio_postgres::Client;
use tokio_postgres::error::SqlState;

/// Server family behind the PostgreSQL protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    Postgres,
    Cockroach,
}

/// Resolved origin of a result column.
#[derive(Debug, Clone)]
pub(crate) struct SourceInfo {
    pub schema: String,
    pub table: String,
    pub column: String,
    pub not_null: bool,
    pub auto_increment: bool,
}

/// A live connection.
pub struct PgSession {
    pub(crate) kind: DbKind,
    pub(crate) flavor: Flavor,
    pub(crate) dialect: Arc<PgDialect>,
    client: Option<Client>,
    pub(crate) shared: Arc<SessionShared>,
    pub(crate) cancel: Arc<PgCancelHandle>,
    pub(crate) info: ServerInfo,
    mode: TransactionMode,
    pub(crate) params: ConnectParams,
    pub(crate) rt: Handle,
    task: Option<tokio::task::JoinHandle<()>>,
    source_cache: HashMap<(u32, i16), Option<SourceInfo>>,
    /// The database never changes during a session.
    database: Option<String>,
}

/// Session settings applied right after connecting.
const SESSION_SETUP: &[&str] = &[
    "SET DateStyle = 'ISO, MDY'",
    "SET IntervalStyle = 'postgres'",
    "SET extra_float_digits = 3",
    "SET bytea_output = 'hex'",
    "SET client_encoding = 'UTF8'",
];

/// Wraps a metadata operation: takes the session gate and, inside an open
/// transaction, protects it with a savepoint so a failing catalog query
/// cannot abort the user's transaction.
macro_rules! guarded {
    ($s:ident, $e:expr) => {{
        let _gate = $s.acquire().await?;
        let sp = $s.meta_begin().await?;
        let r = $e;
        $s.meta_end(sp, r.is_ok()).await;
        r
    }};
}

impl PgSession {
    /// Connect and initialise a session.
    pub(crate) async fn open(params: &ConnectParams, kind: DbKind) -> DbResult<PgSession> {
        let connected = connect::open(params).await?;
        let rt = Handle::current();
        let mut session = PgSession {
            kind,
            // detected from version() while initialising
            flavor: Flavor::Postgres,
            dialect: Arc::new(if kind == DbKind::CockroachDb { PgDialect::cockroach() } else { PgDialect::new() }),
            client: Some(connected.client),
            shared: connected.shared,
            cancel: connected.cancel,
            info: ServerInfo::default(),
            mode: if params.auto_commit { TransactionMode::Auto } else { TransactionMode::Manual },
            params: params.clone(),
            rt,
            task: Some(connected.task),
            source_cache: HashMap::new(),
            database: None,
        };
        if let Err(e) = session.initialize(&connected.props).await {
            session.shutdown().await;
            return Err(e);
        }
        Ok(session)
    }

    async fn initialize(&mut self, props: &connect::Props) -> DbResult<()> {
        let client = self.client()?;
        let rows = client
            .simple_query("SELECT version(), pg_backend_pid(), current_database()")
            .await
            .map_err(pg)?;
        let mut version = String::new();
        let mut pid = None;
        for m in rows {
            if let tokio_postgres::SimpleQueryMessage::Row(r) = m {
                version = r.get(0).unwrap_or_default().to_string();
                pid = r.get(1).map(str::to_string);
                self.database = r.get(2).map(str::to_string);
            }
        }
        if version.contains("CockroachDB") {
            self.flavor = Flavor::Cockroach;
            self.dialect = Arc::new(PgDialect::cockroach());
        }
        let product = if self.flavor == Flavor::Cockroach {
            "CockroachDB".to_string()
        } else if version.starts_with("PostgreSQL") {
            "PostgreSQL".to_string()
        } else {
            version.split_whitespace().next().unwrap_or("PostgreSQL").to_string()
        };
        self.info = ServerInfo {
            version_number: dbiji_core::driver::parse_version_number(&version),
            product,
            version,
            session_id: pid,
        };
        let client = self.client()?;
        match client.batch_execute(&SESSION_SETUP.join(";\n")).await {
            Ok(()) => {}
            Err(e) => {
                // CockroachDB (and some proxies) do not accept every setting: apply what works.
                tracing::debug!("session setup batch failed ({}), applying settings individually", map_error(&e));
                for s in SESSION_SETUP {
                    if let Err(e) = client.batch_execute(s).await {
                        tracing::warn!("session setting not applied ({s}): {}", map_error(&e));
                    }
                }
            }
        }
        if self.params.read_only {
            client
                .batch_execute("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")
                .await
                .map_err(pg)?;
        }
        if let Some(tsa) = props.target_session_attrs.as_deref() {
            self.check_target_session_attrs(tsa).await?;
        }
        Ok(())
    }

    async fn check_target_session_attrs(&self, attrs: &str) -> DbResult<()> {
        let client = self.client()?;
        let scalar = |sql: &'static str| async move {
            let msgs = client.simple_query(sql).await.map_err(pg)?;
            Ok::<String, DbError>(
                msgs.into_iter()
                    .find_map(|m| match m {
                        tokio_postgres::SimpleQueryMessage::Row(r) => r.get(0).map(str::to_string),
                        _ => None,
                    })
                    .unwrap_or_default(),
            )
        };
        let ok = match attrs {
            "read-write" => scalar("SHOW transaction_read_only").await? == "off",
            "read-only" => scalar("SHOW transaction_read_only").await? == "on",
            "primary" => scalar("SELECT pg_is_in_recovery()").await? == "f",
            "standby" => scalar("SELECT pg_is_in_recovery()").await? == "t",
            "any" | "prefer-standby" => true,
            other => {
                return Err(DbError::config(format!("Invalid target_session_attrs value '{other}'")));
            }
        };
        if ok {
            Ok(())
        } else {
            Err(DbError::connection(format!("The server does not satisfy target_session_attrs={attrs}")))
        }
    }

    pub(crate) fn client(&self) -> DbResult<&Client> {
        match &self.client {
            Some(c) if !self.shared.is_closed() => Ok(c),
            _ => Err(self.shared.closed_error()),
        }
    }

    pub(crate) fn is_cockroach(&self) -> bool {
        self.flavor == Flavor::Cockroach
    }

    /// PostgreSQL major version (0 for CockroachDB).
    pub(crate) fn pg_major(&self) -> u32 {
        if self.is_cockroach() { 0 } else { self.info.version_number.0 }
    }

    /// Take the session gate. Fails fast when a cursor is still open (a
    /// programming error on the caller side) and waits for the background
    /// cleanup of dropped cursors.
    pub(crate) async fn acquire(&self) -> DbResult<OwnedMutexGuard<()>> {
        if self.client.is_none() || self.shared.is_closed() {
            return Err(self.shared.closed_error());
        }
        match self.shared.gate.clone().try_lock_owned() {
            Ok(g) => Ok(g),
            Err(_) => {
                if self.shared.cursor_open.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(DbError::internal(
                        "A previous result is still open on this session; close it before running another statement",
                    ));
                }
                match tokio::time::timeout(Duration::from_secs(90), self.shared.gate.clone().lock_owned()).await {
                    Ok(g) => Ok(g),
                    Err(_) => Err(DbError::timeout("Timed out waiting for the previous statement to finish")),
                }
            }
        }
    }

    /// Run statements without results (internal).
    pub(crate) async fn batch(&self, sql: &str) -> DbResult<()> {
        self.client()?.batch_execute(sql).await.map_err(pg)
    }

    /// Catalog query helper.
    pub(crate) async fn q(&self, sql: &str, params: &[P<'_>]) -> DbResult<Vec<tokio_postgres::Row>> {
        catalog::query(self.client()?, sql, params).await
    }

    /// Catalog query returning at most one row.
    pub(crate) async fn q1(&self, sql: &str, params: &[P<'_>]) -> DbResult<Option<tokio_postgres::Row>> {
        catalog::query_opt(self.client()?, sql, params).await
    }

    pub(crate) async fn meta_begin(&self) -> DbResult<bool> {
        if self.shared.in_tx() {
            self.batch("SAVEPOINT dbiji_meta").await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub(crate) async fn meta_end(&self, savepoint: bool, ok: bool) {
        if !savepoint {
            return;
        }
        let sql = if ok {
            "RELEASE SAVEPOINT dbiji_meta"
        } else {
            "ROLLBACK TO SAVEPOINT dbiji_meta; RELEASE SAVEPOINT dbiji_meta"
        };
        if let Err(e) = self.batch(sql).await {
            tracing::warn!("releasing the metadata savepoint failed: {e}");
        }
    }

    async fn shutdown(&mut self) {
        self.client = None;
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
        self.shared.set_closed(None);
    }

    // ------------------------------------------------------------- execution

    async fn implicit_begin(&mut self, script: &ScriptInfo) -> DbResult<()> {
        if self.mode != TransactionMode::Manual || self.shared.in_tx() {
            return Ok(());
        }
        let Some(first) = script.first() else { return Ok(()) };
        if first.tx.is_transaction_control() || first.needs_autocommit {
            return Ok(());
        }
        self.batch("BEGIN").await?;
        self.shared.set_in_tx(true);
        Ok(())
    }

    /// Column plans (metadata + conversion) for prepared statement columns.
    async fn plans_for(&mut self, columns: &[ColInfo], resolve: bool) -> DbResult<Vec<ColumnPlan>> {
        let mut plans: Vec<ColumnPlan> = columns
            .iter()
            .map(|c| {
                let ty = &c.ty;
                let conv = Conv::for_type(ty);
                let tm = typmod_info(ty.oid(), c.typmod);
                let mut meta = ColumnMeta::new(c.name.as_str(), display_name(ty, c.typmod), conv.kind());
                meta.precision = tm.precision;
                meta.scale = tm.scale;
                meta.max_length = tm.max_length;
                ColumnPlan { meta, conv }
            })
            .collect();
        if resolve {
            self.resolve_sources(columns, &mut plans).await?;
        }
        Ok(plans)
    }

    /// Batch-resolve base table / column of result columns (cached per session).
    async fn resolve_sources(&mut self, columns: &[ColInfo], plans: &mut [ColumnPlan]) -> DbResult<()> {
        let keys: Vec<(u32, i16)> = columns
            .iter()
            .filter_map(|c| match (c.table_oid, c.column_id) {
                (Some(t), Some(a)) if a > 0 => Some((t, a)),
                _ => None,
            })
            .collect();
        let missing: Vec<(u32, i16)> = keys.iter().copied().filter(|k| !self.source_cache.contains_key(k)).collect();
        if !missing.is_empty() {
            let oids: Vec<u32> = missing.iter().map(|k| k.0).collect();
            let nums: Vec<i16> = missing.iter().map(|k| k.1).collect();
            let rows = self
                .q(
                    "SELECT k.relid::int8, k.attnum::int8, n.nspname::text, c.relname::text, a.attname::text, a.attnotnull, \
                     (a.attidentity <> '' OR (COALESCE(pg_catalog.pg_get_expr(d.adbin, d.adrelid), '') LIKE 'nextval(%' AND \
                     EXISTS (SELECT 1 FROM pg_catalog.pg_depend od JOIN pg_catalog.pg_class oc ON oc.oid = od.objid AND oc.relkind = 'S' WHERE od.classid = 'pg_catalog.pg_class'::regclass AND od.refclassid = 'pg_catalog.pg_class'::regclass AND od.refobjid = a.attrelid AND od.refobjsubid = a.attnum AND od.deptype = 'a'))) \
                     FROM unnest($1::oid[], $2::int2[]) AS k(relid, attnum) \
                     JOIN pg_catalog.pg_class c ON c.oid = k.relid \
                     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     JOIN pg_catalog.pg_attribute a ON a.attrelid = k.relid AND a.attnum = k.attnum \
                     LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum",
                    &[P::Oids(&oids), P::Int2s(&nums)],
                )
                .await?;
            for k in &missing {
                self.source_cache.insert(*k, None);
            }
            for r in rows {
                let key = (r.i(0)? as u32, r.i(1)? as i16);
                self.source_cache.insert(
                    key,
                    Some(SourceInfo {
                        schema: r.s(2)?,
                        table: r.s(3)?,
                        column: r.s(4)?,
                        not_null: r.b(5)?,
                        auto_increment: r.b(6)?,
                    }),
                );
            }
        }
        for (c, plan) in columns.iter().zip(plans.iter_mut()) {
            if let (Some(t), Some(a)) = (c.table_oid, c.column_id)
                && let Some(Some(src)) = self.source_cache.get(&(t, a))
            {
                plan.meta.source = Some(ColumnSource {
                    database: None,
                    schema: Some(src.schema.clone()),
                    table: src.table.clone(),
                    column: src.column.clone(),
                });
                plan.meta.nullable = Some(!src.not_null);
                plan.meta.auto_increment = Some(src.auto_increment);
            }
        }
        Ok(())
    }

    async fn simple_body(&mut self, sql: &str, script: ScriptInfo, opts: &ExecOptions) -> DbResult<Body> {
        let mut prepared = None;
        let single = script.statements.len() == 1;
        if single && script.statements[0].may_return_rows {
            let res = self.client()?.prepare(sql).await;
            match res {
                Ok(stmt) => {
                    let columns = col_infos(stmt.columns());
                    drop(stmt);
                    prepared = Some(self.plans_for(&columns, opts.resolve_sources).await?);
                }
                Err(e) if is_prepare_unsupported(&e) => {
                    tracing::debug!("statement cannot be described, executing without metadata: {}", map_error(&e));
                }
                Err(e) => return Err(map_error(&e)),
            }
        }
        let stream = self.client()?.simple_query_raw(sql).await.map_err(pg)?;
        Ok(Body::Simple(SimpleBody {
            stream: Box::pin(stream),
            script,
            stmt_index: 0,
            prepared,
            convs: Vec::new(),
            results_seen: 0,
        }))
    }

    async fn extended_body(
        &mut self,
        sql: &str,
        script: ScriptInfo,
        params: &[Value],
        opts: &ExecOptions,
    ) -> DbResult<Body> {
        if script.statements.len() > 1 {
            return Err(DbError::query("A statement with parameters must contain a single SQL command"));
        }
        let info = script.statements.into_iter().next().ok_or_else(|| DbError::query("Empty statement"))?;
        let res = self.client()?.prepare(sql).await;
        let stmt = match res {
            Ok(s) => s,
            Err(e) if e.code() == Some(&SqlState::INDETERMINATE_DATATYPE) => {
                let n = count_placeholders(sql).max(params.len());
                let types = vec![Type::TEXT; n];
                self.client()?.prepare_typed(sql, &types).await.map_err(pg)?
            }
            Err(e) => return Err(map_error(&e)),
        };
        if stmt.params().len() != params.len() {
            return Err(DbError::query(format!(
                "The statement expects {} parameter(s) but {} were supplied",
                stmt.params().len(),
                params.len()
            )));
        }
        let columns = if stmt.columns().is_empty() {
            None
        } else {
            let cols = col_infos(stmt.columns());
            Some(self.plans_for(&cols, opts.resolve_sources).await?)
        };
        let text_params: Vec<TextParam<'_>> = params.iter().map(TextParam).collect();
        let client = self.client()?;
        let stream = client
            .query_raw(&stmt, text_params.iter().map(|p| p as &(dyn ToSql + Sync)))
            .await
            .map_err(pg)?;
        Ok(Body::Extended(ExtendedBody { stream: Box::pin(stream), columns, info, header_done: false }))
    }

    async fn execute_impl(
        &mut self,
        sql: &str,
        params: &[Value],
        opts: &ExecOptions,
        guard: OwnedMutexGuard<()>,
    ) -> DbResult<Box<dyn QueryCursor>> {
        self.shared.clear_notices();
        let script = ScriptInfo::analyze(sql);
        if script.has_copy_from_stdin() {
            return Err(DbError::unsupported(
                "COPY ... FROM STDIN cannot be run from the SQL editor; use the data import tool or COPY FROM a server side file",
            ));
        }
        if script.statements.len() > 1 && script.statements.iter().any(|s| s.copy == CopyKind::ToStdout) {
            return Err(DbError::unsupported("Run COPY ... TO STDOUT as a single statement, not as part of a script"));
        }
        if script
            .statements
            .iter()
            .any(|s| matches!(s.verb.as_str(), "CREATE" | "ALTER" | "DROP" | "COMMENT" | "TRUNCATE"))
        {
            self.source_cache.clear();
        }
        if script.is_empty() {
            return Ok(Box::new(PgCursor::new(Body::Empty, self.shared.clone(), guard, self.cancel.clone(), None, self.rt.clone())));
        }
        self.implicit_begin(&script).await?;
        let timer = opts
            .timeout
            .filter(|t| !t.is_zero())
            .map(|t| StatementTimer::start(&self.rt, t, self.cancel.clone()));
        let copy_out = script.statements.len() == 1 && script.statements[0].copy == CopyKind::ToStdout;
        let body = if !params.is_empty() {
            self.extended_body(sql, script, params, opts).await
        } else if copy_out {
            match self.client()?.copy_out(sql).await {
                Ok(s) => Ok(Body::Copy(CopyBody { stream: Box::pin(s), header_done: false })),
                Err(e) => Err(map_error(&e)),
            }
        } else {
            self.simple_body(sql, script, opts).await
        };
        match body {
            Ok(b) => Ok(Box::new(PgCursor::new(b, self.shared.clone(), guard, self.cancel.clone(), timer, self.rt.clone()))),
            Err(mut e) => {
                if let Some(t) = timer {
                    let fired = t.has_fired();
                    // never release the session while a timeout cancel is still in flight
                    if let Some(pending_cancel) = t.stop() {
                        let _ = pending_cancel.await;
                    }
                    if fired && e.code.as_deref() == Some("57014") {
                        e = DbError::timeout("The statement was cancelled because it exceeded the timeout")
                            .with_code("57014");
                    }
                }
                Err(e)
            }
        }
    }

}

/// Errors after which a statement is executed without `Parse`/`Describe` metadata.
fn is_prepare_unsupported(e: &tokio_postgres::Error) -> bool {
    match e.as_db_error() {
        Some(db) => {
            let code = db.code();
            (code == &SqlState::SYNTAX_ERROR && db.message().contains("multiple commands"))
                || code == &SqlState::DUPLICATE_PSTATEMENT
                || code == &SqlState::INVALID_SQL_STATEMENT_NAME
                || code == &SqlState::FEATURE_NOT_SUPPORTED
        }
        None => false,
    }
}

/// Highest `$n` placeholder number in `sql`.
fn count_placeholders(sql: &str) -> usize {
    let toks = tokenize(sql);
    toks.iter()
        .filter(|t| t.tok == Tok::Param)
        .filter_map(|t| sql.get(t.start + 1..t.end).and_then(|n| n.parse::<usize>().ok()))
        .max()
        .unwrap_or(0)
}

/// The parts of a prepared statement column the driver needs.
pub(crate) struct ColInfo {
    pub name: String,
    pub ty: Type,
    pub typmod: i32,
    pub table_oid: Option<u32>,
    pub column_id: Option<i16>,
}

pub(crate) fn col_infos(cols: &[tokio_postgres::Column]) -> Vec<ColInfo> {
    cols.iter()
        .map(|c| ColInfo {
            name: c.name().to_string(),
            ty: c.type_().clone(),
            typmod: c.type_modifier(),
            table_oid: c.table_oid(),
            column_id: c.column_id(),
        })
        .collect()
}

#[async_trait]
impl Session for PgSession {
    fn kind(&self) -> DbKind {
        self.kind
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        self.dialect.clone()
    }

    fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        Some(self.cancel.clone())
    }

    async fn ping(&mut self) -> DbResult<()> {
        let _g = self.acquire().await?;
        let client = self.client()?;
        match tokio::time::timeout(Duration::from_secs(30), client.check_connection()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                let mut err = map_error(&e);
                if err.kind != dbiji_core::ErrorKind::Connection {
                    err.kind = dbiji_core::ErrorKind::Connection;
                }
                Err(err)
            }
            Err(_) => Err(DbError::connection("The server did not answer the ping within 30s")),
        }
    }

    async fn close(&mut self) -> DbResult<()> {
        self.shutdown().await;
        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.shared.is_closed() || self.client.as_ref().is_none_or(Client::is_closed)
    }

    async fn execute(&mut self, sql: &str, params: &[Value], opts: &ExecOptions) -> DbResult<Box<dyn QueryCursor>> {
        let guard = self.acquire().await?;
        self.execute_impl(sql, params, opts, guard).await
    }

    fn transaction_mode(&self) -> TransactionMode {
        self.mode
    }

    async fn set_transaction_mode(&mut self, mode: TransactionMode) -> DbResult<()> {
        if mode == TransactionMode::Auto && self.shared.in_tx() {
            let _g = self.acquire().await?;
            self.batch("COMMIT").await?;
            self.shared.set_in_tx(false);
        }
        self.mode = mode;
        Ok(())
    }

    fn in_transaction(&self) -> bool {
        self.shared.in_tx()
    }

    async fn commit(&mut self) -> DbResult<()> {
        let _g = self.acquire().await?;
        if !self.shared.in_tx() {
            return Ok(());
        }
        let r = self.batch("COMMIT").await;
        // COMMIT ends the transaction even when it fails (e.g. deferred constraints).
        self.shared.set_in_tx(false);
        r
    }

    async fn rollback(&mut self) -> DbResult<()> {
        let _g = self.acquire().await?;
        if !self.shared.in_tx() {
            return Ok(());
        }
        let r = self.batch("ROLLBACK").await;
        self.shared.set_in_tx(false);
        r
    }

    async fn current_database(&mut self) -> DbResult<Option<String>> {
        if self.database.is_some() {
            return Ok(self.database.clone());
        }
        guarded!(self, async {
            match self.q1("SELECT current_database()::text", &[]).await? {
                Some(r) => r.os(0),
                None => Ok(None),
            }
        }
        .await)
    }

    async fn current_schema(&mut self) -> DbResult<Option<String>> {
        guarded!(self, async {
            match self.q1("SELECT current_schema()::text", &[]).await? {
                Some(r) => r.os(0),
                None => Ok(None),
            }
        }
        .await)
    }

    async fn set_current_schema(&mut self, schema: &str) -> DbResult<()> {
        let _g = self.acquire().await?;
        let sql = if schema == "public" {
            "SET search_path TO public".to_string()
        } else {
            format!("SET search_path TO {}, public", catalog::quote_ident_always(schema))
        };
        self.batch(&sql).await
    }

    async fn set_current_database(&mut self, _database: &str) -> DbResult<()> {
        Err(DbError::unsupported(
            "PostgreSQL cannot switch the database of an open session; open a new session for the other database",
        ))
    }

    async fn list_databases(&mut self) -> DbResult<Vec<DatabaseInfo>> {
        guarded!(self, self.list_databases_impl().await)
    }

    async fn list_schemas(&mut self) -> DbResult<Vec<SchemaInfo>> {
        guarded!(self, self.list_schemas_impl().await)
    }

    async fn list_objects(&mut self, schema: &SchemaRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
        guarded!(self, self.list_objects_impl(schema, kind).await)
    }

    async fn list_children(&mut self, parent: &ObjectRef, kind: ObjectKind) -> DbResult<Vec<ObjectInfo>> {
        guarded!(self, self.list_children_impl(parent, kind).await)
    }

    async fn table_definition(&mut self, table: &ObjectRef) -> DbResult<TableDef> {
        guarded!(self, self.table_definition_impl(table).await)
    }

    async fn view_definition(&mut self, view: &ObjectRef) -> DbResult<ViewDef> {
        guarded!(self, self.view_definition_impl(view).await)
    }

    async fn routine_definition(&mut self, routine: &ObjectRef) -> DbResult<RoutineDef> {
        guarded!(self, self.routine_definition_impl(routine).await)
    }

    async fn sequence_definition(&mut self, sequence: &ObjectRef) -> DbResult<SequenceDef> {
        guarded!(self, self.sequence_definition_impl(sequence).await)
    }

    async fn object_ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        guarded!(self, self.object_ddl_impl(obj).await)
    }

    async fn object_properties(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        guarded!(self, self.object_properties_impl(obj).await)
    }

    async fn object_dependencies(&mut self, obj: &ObjectRef) -> DbResult<Vec<Dependency>> {
        guarded!(self, self.object_dependencies_impl(obj).await)
    }

    async fn object_statistics(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        guarded!(self, self.object_statistics_impl(obj).await)
    }

    async fn search_metadata(&mut self, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
        guarded!(self, self.search_metadata_impl(search).await)
    }

    async fn schema_snapshot(&mut self, schema: &SchemaRef, opts: &SnapshotOptions) -> DbResult<SchemaSnapshot> {
        guarded!(self, self.schema_snapshot_impl(schema, opts).await)
    }

    async fn explain(&mut self, sql: &str, analyze: bool) -> DbResult<QueryPlan> {
        let _g = self.acquire().await?;
        self.explain_impl(sql, analyze).await
    }

    async fn list_sessions(&mut self) -> DbResult<SessionList> {
        guarded!(self, self.list_sessions_impl().await)
    }

    async fn kill_session(&mut self, id: &str, mode: KillMode) -> DbResult<()> {
        guarded!(self, self.kill_session_impl(id, mode).await)
    }

    async fn list_locks(&mut self) -> DbResult<LockList> {
        guarded!(self, self.list_locks_impl().await)
    }

    async fn server_metrics(&mut self) -> DbResult<Vec<Metric>> {
        guarded!(self, self.server_metrics_impl().await)
    }

    async fn debug_routine(&mut self, routine: &ObjectRef, args: &[String]) -> DbResult<Box<dyn DebugSession>> {
        let target = guarded!(self, self.debug_target(routine).await)?;
        crate::debugger::start(&self.params, target, args).await
    }
}

impl Drop for PgSession {
    fn drop(&mut self) {
        // Dropping the client makes the connection task send Terminate and exit.
        self.client = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders() {
        assert_eq!(count_placeholders("select $1, '$5', $3"), 3);
        assert_eq!(count_placeholders("select 1"), 0);
    }
}
