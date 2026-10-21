//! [`MySqlSession`]: one live MySQL / MariaDB connection.

use crate::connect::{self, ConnSettings, Killer};
use crate::convert::{self, Charset};
use crate::cursor::{self, ConnReturn, Job};
use crate::dialect::MySqlDialect;
use crate::error;
use crate::flavor::{Flavor, ServerProfile, parse_server_version};
use crate::sqltext;
use async_trait::async_trait;
use dbiji_core::admin::{KillMode, LockList, Metric, SessionList};
use dbiji_core::ddl::{
    RoutineDef, SchemaSnapshot, SequenceDef, SnapshotOptions, TableDef, ViewDef,
};
use dbiji_core::driver::ServerInfo;
use dbiji_core::meta::{
    DatabaseInfo, Dependency, MetadataSearch, ObjectInfo, ObjectKind, ObjectRef, Property,
    SchemaInfo, SchemaRef,
};
use dbiji_core::plan::QueryPlan;
use dbiji_core::{
    CancelHandle, ConnectParams, DbError, DbKind, DbResult, Dialect, ExecOptions, QueryCursor,
    Session, TransactionMode, Value,
};
use mysql_async::consts::StatusFlags;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Params, Row as MyRow, Value as MyValue};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{oneshot, watch};

/// A UTC time `?` (given twice) in the session's time zone. `CONVERT_TZ` knows the
/// zone's DST rules; without the time zone tables a named zone gives NULL, and the
/// current offset is used.
pub(crate) const SESSION_TIME_SQL: &str = "CAST(COALESCE(CONVERT_TZ(?, '+00:00', @@session.time_zone), TIMESTAMPADD(SECOND, TIMESTAMPDIFF(SECOND, UTC_TIMESTAMP(), NOW()), ?)) AS CHAR)";

/// Where the connection currently is.
enum Slot {
    /// Available.
    Idle(Conn),
    /// Owned by a cursor worker; comes back through the receiver.
    Busy(oneshot::Receiver<ConnReturn>),
    /// The connection is unusable.
    Broken(DbError),
    /// `close()` was called.
    Closed,
}

/// Out-of-band statement cancellation (`KILL QUERY` from a side connection).
pub struct MySqlCancelHandle {
    killer: Arc<Killer>,
}

#[async_trait]
impl CancelHandle for MySqlCancelHandle {
    async fn cancel(&self) -> DbResult<()> {
        self.killer.kill_query().await
    }
}

/// One live connection to a MySQL or MariaDB server.
pub struct MySqlSession {
    slot: Slot,
    pub(crate) profile: ServerProfile,
    info: ServerInfo,
    dialect: Arc<MySqlDialect>,
    killer: Arc<Killer>,
    cancel: Arc<MySqlCancelHandle>,
    tx_mode: TransactionMode,
    in_tx: cursor::TxState,
    settings: ConnSettings,
    /// Lets `close()` interrupt the worker of an open cursor.
    abort: Option<watch::Sender<bool>>,
    tls: bool,
    connect_timeout: Duration,
}

impl MySqlSession {
    /// Open a session (`configured` is the flavor of the driver used; the
    /// actual flavor is detected from the server).
    pub(crate) async fn connect(
        params: &ConnectParams,
        configured: Flavor,
    ) -> DbResult<MySqlSession> {
        let settings = connect::settings(params)?;
        let opened = connect::open_connection(params).await?;
        let mut conn = opened.conn;
        for stmt in &settings.setup {
            let bytes = match convert::encode_text(&settings.wire_charset, stmt) {
                Ok(b) => b,
                Err(e) => {
                    let _ = conn.disconnect().await;
                    return Err(e);
                }
            };
            if let Err(e) = conn.query_drop(bytes).await {
                let err = error::map_error(e, Some(stmt));
                let _ = conn.disconnect().await;
                return Err(err.with_context(format!("Session setup statement failed: {stmt}")));
            }
        }
        let row: Option<MyRow> = match conn
            .query_first("SELECT VERSION(), CONNECTION_ID(), @@sql_mode, @@lower_case_table_names, @@version_comment")
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let err = error::map_error(e, None);
                let _ = conn.disconnect().await;
                return Err(err);
            }
        };
        let (version, conn_id, sql_mode, lctn, comment) = match &row {
            Some(r) => (
                convert::read::text_or_empty(r, 0),
                convert::read::int(r, 1)
                    .map(|v| v as u32)
                    .unwrap_or(conn.id()),
                convert::read::text_or_empty(r, 2),
                convert::read::int(r, 3).unwrap_or(0) as u32,
                convert::read::text_or_empty(r, 4),
            ),
            None => (String::new(), conn.id(), String::new(), 0, String::new()),
        };
        let flavor = if version.is_empty() {
            configured
        } else {
            Flavor::detect(&format!("{version} {comment}"))
        };
        let mut version_number = parse_server_version(&version);
        if version_number == (0, 0, 0) {
            let (a, b, c) = conn.server_version();
            version_number = (a as u32, b as u32, c as u32);
        }
        let profile = ServerProfile {
            flavor,
            version: version_number,
            sql_mode: sql_mode.to_ascii_uppercase(),
            lower_case_table_names: lctn,
        };
        let product = if comment.to_ascii_lowercase().contains("percona") {
            "Percona Server".to_string()
        } else {
            flavor.product().to_string()
        };
        let info = ServerInfo {
            product,
            version: if version.is_empty() {
                format!(
                    "{}.{}.{}",
                    version_number.0, version_number.1, version_number.2
                )
            } else {
                version
            },
            version_number,
            session_id: Some(conn_id.to_string()),
        };
        // Side connections (KILL QUERY) do not select a database, so they keep
        // working when the session's database is dropped or renamed.
        let side_opts: mysql_async::Opts = mysql_async::OptsBuilder::from_opts(opened.opts.clone())
            .db_name(None::<String>)
            .into();
        let killer = Killer::new(side_opts, conn_id, params.connect_timeout);
        let cancel = Arc::new(MySqlCancelHandle {
            killer: killer.clone(),
        });
        let in_tx = conn
            .last_ok_packet()
            .map(|ok| {
                ok.status_flags()
                    .contains(StatusFlags::SERVER_STATUS_IN_TRANS)
            })
            .unwrap_or(false);
        Ok(MySqlSession {
            slot: Slot::Idle(conn),
            dialect: Arc::new(MySqlDialect::for_server(profile.clone())),
            profile,
            info,
            killer,
            cancel,
            tx_mode: if params.auto_commit {
                TransactionMode::Auto
            } else {
                TransactionMode::Manual
            },
            in_tx: Arc::new(AtomicBool::new(in_tx)),
            settings,
            abort: None,
            tls: opened.tls,
            connect_timeout: params.connect_timeout,
        })
    }

    /// Server flavor and version.
    pub fn profile(&self) -> &ServerProfile {
        &self.profile
    }

    /// The connection is encrypted with TLS.
    pub fn is_tls(&self) -> bool {
        self.tls
    }

    /// The concrete dialect of this session.
    pub fn mysql_dialect(&self) -> Arc<MySqlDialect> {
        self.dialect.clone()
    }

    /// Connect timeout of this session (used for side connections).
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    fn apply_return(&mut self, r: ConnReturn) {
        self.abort = None;
        self.slot = match (r.conn, r.broken) {
            (Some(c), _) => Slot::Idle(c),
            (None, Some(e)) => Slot::Broken(e),
            (None, None) => {
                Slot::Broken(DbError::connection("The connection to the server was lost"))
            }
        };
    }

    /// Wait until the connection is available (a cursor worker may still own it).
    async fn wait_idle(&mut self) -> DbResult<()> {
        loop {
            match &mut self.slot {
                Slot::Idle(_) => return Ok(()),
                Slot::Busy(rx) => {
                    let r = rx.await;
                    match r {
                        Ok(ret) => self.apply_return(ret),
                        Err(_) => {
                            self.abort = None;
                            self.slot = Slot::Broken(DbError::connection(
                                "The connection was lost while a statement was running",
                            ));
                        }
                    }
                }
                Slot::Broken(e) => {
                    let mut err = e.clone();
                    err.kind = dbiji_core::ErrorKind::Connection;
                    return Err(err);
                }
                Slot::Closed => return Err(DbError::connection("The session is closed")),
            }
        }
    }

    /// The idle connection.
    pub(crate) async fn conn(&mut self) -> DbResult<&mut Conn> {
        self.wait_idle().await?;
        match &mut self.slot {
            Slot::Idle(c) => Ok(c),
            _ => Err(DbError::internal("The connection is not available")),
        }
    }

    fn handle_err(&mut self, e: mysql_async::Error, sql: &str) -> DbError {
        let fatal = error::is_fatal(&e);
        let err = error::map_error(e, Some(sql));
        if fatal {
            self.slot = Slot::Broken(err.clone());
        }
        err
    }

    /// Text parameter in the connection character set.
    pub(crate) fn p(&self, s: &str) -> MyValue {
        match convert::encode_text(&self.settings.wire_charset, s) {
            Ok(b) => MyValue::Bytes(b),
            Err(_) => MyValue::Bytes(s.as_bytes().to_vec()),
        }
    }

    /// Run an internal query and collect all rows of its first result.
    pub(crate) async fn rows(&mut self, sql: &str, params: Vec<MyValue>) -> DbResult<Vec<MyRow>> {
        // The text goes in the connection's character set (as exec_internal does): `café` with latin1.
        let bytes = convert::encode_text(&self.settings.wire_charset, sql)?;
        let res = {
            let conn = self.conn().await?;
            if params.is_empty() {
                conn.query::<MyRow, _>(bytes).await
            } else {
                conn.exec::<MyRow, _, _>(sql, Params::Positional(params))
                    .await
            }
        };
        res.map_err(|e| self.handle_err(e, sql))
    }

    /// Run an internal text query returning column names and rows (SHOW statements).
    pub(crate) async fn rows_with_columns(
        &mut self,
        sql: &str,
    ) -> DbResult<(Vec<String>, Vec<MyRow>)> {
        let bytes = convert::encode_text(&self.settings.wire_charset, sql)?;
        let res = {
            let conn = self.conn().await?;
            async {
                let mut qr = conn.query_iter(bytes).await?;
                let cols: Vec<String> = qr
                    .columns_ref()
                    .iter()
                    .map(|c| c.name_str().into_owned())
                    .collect();
                let rows: Vec<MyRow> = qr.collect().await?;
                qr.drop_result().await?;
                Ok::<_, mysql_async::Error>((cols, rows))
            }
            .await
        };
        res.map_err(|e| self.handle_err(e, sql))
    }

    /// First row of an internal query.
    pub(crate) async fn row(&mut self, sql: &str, params: Vec<MyValue>) -> DbResult<Option<MyRow>> {
        Ok(self.rows(sql, params).await?.into_iter().next())
    }

    /// Like [`rows`](Self::rows) but an error means "feature not available" (missing
    /// catalog table or privilege) and yields `None`.
    pub(crate) async fn try_rows(
        &mut self,
        sql: &str,
        params: Vec<MyValue>,
    ) -> DbResult<Option<Vec<MyRow>>> {
        match self.rows(sql, params).await {
            Ok(r) => Ok(Some(r)),
            Err(e) if e.kind == dbiji_core::ErrorKind::Connection => Err(e),
            Err(e) => {
                tracing::debug!("optional catalog query failed: {e}");
                Ok(None)
            }
        }
    }

    /// Execute a statement without results and refresh the transaction state.
    pub(crate) async fn exec_simple(&mut self, sql: &str) -> DbResult<()> {
        self.exec_internal(sql).await?;
        self.refresh_tx();
        Ok(())
    }

    /// Execute a statement without results, leaving the transaction
    /// indicator alone (catalog helpers must not make browsing metadata in
    /// manual-commit mode look like a pending transaction).
    pub(crate) async fn exec_internal(&mut self, sql: &str) -> DbResult<()> {
        let bytes = convert::encode_text(&self.settings.wire_charset, sql)?;
        let res = {
            let conn = self.conn().await?;
            conn.query_drop(bytes).await
        };
        res.map_err(|e| self.handle_err(e, sql))
    }

    fn refresh_tx(&mut self) {
        if let Slot::Idle(c) = &self.slot
            && let Some(ok) = c.last_ok_packet()
        {
            self.in_tx.store(
                ok.status_flags()
                    .contains(StatusFlags::SERVER_STATUS_IN_TRANS),
                Ordering::Release,
            );
        }
    }

    /// `DATABASE()`.
    pub(crate) async fn current_db(&mut self) -> DbResult<Option<String>> {
        let row = self.row("SELECT DATABASE()", Vec::new()).await?;
        Ok(row.and_then(|r| convert::read::non_empty(&r, 0)))
    }

    /// The schema to use for metadata calls: the given one or the current database.
    pub(crate) async fn schema_or_current(&mut self, schema: Option<&str>) -> DbResult<String> {
        match schema.filter(|s| !s.is_empty()) {
            Some(s) => Ok(s.to_string()),
            None => self.current_db().await?.ok_or_else(|| {
                DbError::config("No database selected; specify the database (schema) of the object")
            }),
        }
    }
}

#[async_trait]
impl Session for MySqlSession {
    fn kind(&self) -> DbKind {
        self.profile.flavor.kind()
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
        let res = {
            let conn = self.conn().await?;
            conn.ping().await
        };
        res.map_err(|e| {
            let mut err = self.handle_err(e, "");
            err.kind = dbiji_core::ErrorKind::Connection;
            if !matches!(self.slot, Slot::Broken(_)) {
                self.slot = Slot::Broken(err.clone());
            }
            err
        })
    }

    async fn close(&mut self) -> DbResult<()> {
        if let Some(abort) = self.abort.take() {
            let _ = abort.send(true);
        }
        if matches!(self.slot, Slot::Busy(_)) {
            let _ = self.wait_idle().await;
        }
        match std::mem::replace(&mut self.slot, Slot::Closed) {
            Slot::Idle(conn) => conn.disconnect().await.map_err(error::err),
            _ => Ok(()),
        }
    }

    fn is_closed(&self) -> bool {
        match &self.slot {
            Slot::Idle(c) => c.is_disconnected(),
            Slot::Busy(_) => false,
            Slot::Broken(_) | Slot::Closed => true,
        }
    }

    async fn execute(
        &mut self,
        sql: &str,
        params: &[Value],
        opts: &ExecOptions,
    ) -> DbResult<Box<dyn QueryCursor>> {
        self.wait_idle().await?;
        let cs: Charset = self.settings.wire_charset;
        let encoded = convert::encode_text(&cs, sql)?;
        let params = if params.is_empty() {
            None
        } else {
            if sqltext::has_named_params(sql) {
                return Err(DbError::query(
                    "The statement contains ':name' style text outside of string literals, which the MySQL client \
                     library treats as a named parameter; use '?' placeholders (or quote the text) when binding parameters",
                ));
            }
            // A time with a time zone: MySQL ≥ 8.0.19 converts a value with an offset
            // itself; before (and MariaDB), it is given in the session's time zone, as
            // that zone had it at that instant (DST), converted by the server in one query.
            let offset_literal = self.profile.is_mysql() && self.profile.at_least(8, 0, 19);
            let mut local: std::collections::HashMap<chrono::NaiveDateTime, chrono::NaiveDateTime> =
                std::collections::HashMap::new();
            if !offset_literal {
                let mut utc: Vec<chrono::NaiveDateTime> = params
                    .iter()
                    .filter_map(|v| match v {
                        Value::DateTimeTz(dt) => Some(dt.naive_utc()),
                        _ => None,
                    })
                    .collect();
                utc.sort();
                utc.dedup();
                if !utc.is_empty() {
                    let sql = format!("SELECT {}", vec![SESSION_TIME_SQL; utc.len()].join(", "));
                    let args: Vec<MyValue> = utc
                        .iter()
                        .flat_map(|t| {
                            let text = t.format("%Y-%m-%d %H:%M:%S%.6f").to_string();
                            [
                                MyValue::Bytes(text.clone().into_bytes()),
                                MyValue::Bytes(text.into_bytes()),
                            ]
                        })
                        .collect();
                    if let Some(row) = self.row(&sql, args).await? {
                        for (i, t) in utc.iter().enumerate() {
                            if let Some(l) =
                                row.get::<Option<String>, _>(i).flatten().and_then(|s| {
                                    chrono::NaiveDateTime::parse_from_str(
                                        &s,
                                        "%Y-%m-%d %H:%M:%S%.f",
                                    )
                                    .ok()
                                })
                            {
                                local.insert(*t, l);
                            }
                        }
                    }
                }
            }
            Some(
                params
                    .iter()
                    .map(|v| match v {
                        Value::DateTimeTz(dt) if offset_literal => Ok(MyValue::Bytes(
                            dt.format("%Y-%m-%d %H:%M:%S%.6f%:z")
                                .to_string()
                                .into_bytes(),
                        )),
                        Value::DateTimeTz(dt) => match local.get(&dt.naive_utc()) {
                            Some(l) => convert::to_param(&Value::DateTime(*l), &cs),
                            None => convert::to_param(v, &cs),
                        },
                        _ => convert::to_param(v, &cs),
                    })
                    .collect::<DbResult<Vec<_>>>()?,
            )
        };
        let conn = match std::mem::replace(&mut self.slot, Slot::Closed) {
            Slot::Idle(c) => c,
            other => {
                self.slot = other;
                return Err(DbError::internal("The connection is not available"));
            }
        };
        let job = Job {
            sql: encoded,
            sql_text: sql.to_string(),
            params,
            timeout: opts.timeout.filter(|t| !t.is_zero()),
            tag: sqltext::command_tag(sql),
        };
        let (abort_tx, abort_rx) = watch::channel(false);
        let (cursor, ret) =
            cursor::spawn(conn, job, self.killer.clone(), abort_rx, self.in_tx.clone());
        self.slot = Slot::Busy(ret);
        self.abort = Some(abort_tx);
        Ok(Box::new(cursor))
    }

    fn transaction_mode(&self) -> TransactionMode {
        self.tx_mode
    }

    async fn set_transaction_mode(&mut self, mode: TransactionMode) -> DbResult<()> {
        match mode {
            TransactionMode::Manual => self.exec_simple("SET autocommit = 0").await?,
            TransactionMode::Auto => {
                if self.in_tx.load(Ordering::Acquire) {
                    self.exec_simple("COMMIT").await?;
                }
                self.exec_simple("SET autocommit = 1").await?;
            }
        }
        self.tx_mode = mode;
        Ok(())
    }

    fn in_transaction(&self) -> bool {
        self.in_tx.load(Ordering::Acquire)
    }

    async fn commit(&mut self) -> DbResult<()> {
        self.exec_simple("COMMIT").await?;
        self.in_tx.store(false, Ordering::Release);
        Ok(())
    }

    async fn rollback(&mut self) -> DbResult<()> {
        self.exec_simple("ROLLBACK").await?;
        self.in_tx.store(false, Ordering::Release);
        Ok(())
    }

    async fn current_database(&mut self) -> DbResult<Option<String>> {
        self.current_db().await
    }

    async fn current_schema(&mut self) -> DbResult<Option<String>> {
        self.current_db().await
    }

    async fn set_current_schema(&mut self, schema: &str) -> DbResult<()> {
        let sql = format!("USE {}", crate::ddl::quote(schema));
        self.exec_simple(&sql).await
    }

    async fn set_current_database(&mut self, database: &str) -> DbResult<()> {
        self.set_current_schema(database).await
    }

    async fn list_databases(&mut self) -> DbResult<Vec<DatabaseInfo>> {
        // MySQL databases are presented as schemas (see `list_schemas`).
        Ok(Vec::new())
    }

    async fn list_schemas(&mut self) -> DbResult<Vec<SchemaInfo>> {
        self.meta_list_schemas().await
    }

    async fn list_objects(
        &mut self,
        schema: &SchemaRef,
        kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        self.meta_list_objects(schema, kind).await
    }

    async fn list_children(
        &mut self,
        parent: &ObjectRef,
        kind: ObjectKind,
    ) -> DbResult<Vec<ObjectInfo>> {
        self.meta_list_children(parent, kind).await
    }

    async fn table_definition(&mut self, table: &ObjectRef) -> DbResult<TableDef> {
        self.meta_table_definition(table).await
    }

    async fn view_definition(&mut self, view: &ObjectRef) -> DbResult<ViewDef> {
        self.meta_view_definition(view).await
    }

    async fn routine_definition(&mut self, routine: &ObjectRef) -> DbResult<RoutineDef> {
        self.meta_routine_definition(routine).await
    }

    async fn sequence_definition(&mut self, sequence: &ObjectRef) -> DbResult<SequenceDef> {
        self.meta_sequence_definition(sequence).await
    }

    async fn object_ddl(&mut self, obj: &ObjectRef) -> DbResult<String> {
        self.meta_object_ddl(obj).await
    }

    async fn object_properties(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        self.meta_object_properties(obj).await
    }

    async fn object_dependencies(&mut self, obj: &ObjectRef) -> DbResult<Vec<Dependency>> {
        self.meta_object_dependencies(obj).await
    }

    async fn object_statistics(&mut self, obj: &ObjectRef) -> DbResult<Vec<Property>> {
        self.meta_object_statistics(obj).await
    }

    async fn search_metadata(&mut self, search: &MetadataSearch) -> DbResult<Vec<ObjectInfo>> {
        self.meta_search(search).await
    }

    async fn schema_snapshot(
        &mut self,
        schema: &SchemaRef,
        opts: &SnapshotOptions,
    ) -> DbResult<SchemaSnapshot> {
        self.meta_snapshot(schema, opts).await
    }

    async fn explain(&mut self, sql: &str, analyze: bool) -> DbResult<QueryPlan> {
        self.run_explain(sql, analyze).await
    }

    async fn list_sessions(&mut self) -> DbResult<SessionList> {
        self.admin_sessions().await
    }

    async fn kill_session(&mut self, id: &str, mode: KillMode) -> DbResult<()> {
        self.admin_kill(id, mode).await
    }

    async fn list_locks(&mut self) -> DbResult<LockList> {
        self.admin_locks().await
    }

    async fn server_metrics(&mut self) -> DbResult<Vec<Metric>> {
        self.admin_metrics().await
    }
}
