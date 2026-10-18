//! Data transfer tasks (export, import, migration) run through
//! `dbiji-transfer`, plus the session factory shared with the transfer tabs.
//!
//! Saved transfer tasks store the serialised `dbiji-transfer` job as their
//! `config` (an [`ExportJob`], [`ImportJob`] or [`MigrationJob`]); settings
//! that are not part of a job ([`ExportTaskExtras`]) live in the same JSON
//! object under their own keys, which the job deserialisers ignore.

// `DbError` (the error type of dbiji-core) is 144 bytes, so every `DbResult`
// trips this lint; boxing it is a decision for dbiji-core.
#![allow(clippy::result_large_err)]

use crate::conn::{LiveConnection, init_session};
use crate::taskexec::AppTaskExecutor;
use async_trait::async_trait;
use dbiji_core::config::ConnectionConfig;
use dbiji_core::driver::{ConnectParams, Driver, TransactionMode};
use dbiji_core::meta::ObjectRef;
use dbiji_core::{DbError, DbKind, DbResult, Dialect, Session, tr, trf};
use dbiji_tasks::{RunContext, RunOutcome};
use dbiji_transfer::{
    ExportItem, ExportJob, ExportJobSummary, ImportJob, MessageLevel, MigrationJob, Progress, SessionFactory, run_export_job_with, run_import_job, run_migration,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Row errors of an import copied into the task log.
const LOGGED_ROW_ERRORS: usize = 50;

// ------------------------------------------------------------------ sessions

/// Opens the sessions of a transfer for one connection.
///
/// Every session is a new connection made by the driver with the effective
/// parameters of the connection (tunnel endpoint, resolved password) and the
/// connection's initialisation (default schema, init SQL). Sessions run in
/// auto-commit mode: the transfer engine manages its own transactions.
pub struct AppSessionFactory {
    driver: Arc<dyn Driver>,
    params: ConnectParams,
    config: ConnectionConfig,
    /// An already open session handed out by the first [`SessionFactory::open`].
    first: Mutex<Option<Box<dyn Session>>>,
}

impl AppSessionFactory {
    /// Factory over explicit parameters; `database` overrides the database
    /// the sessions connect to (servers with several databases).
    pub fn new(driver: Arc<dyn Driver>, params: ConnectParams, config: ConnectionConfig, database: Option<&str>) -> Self {
        let mut params = params;
        params.auto_commit = true;
        if let Some(db) = database.map(str::trim).filter(|d| !d.is_empty()) {
            params.database = Some(db.to_string());
        }
        AppSessionFactory { driver, params, config, first: Mutex::new(None) }
    }

    /// Sessions of a connected connection (used by the transfer tabs).
    pub fn from_live(live: &LiveConnection, database: Option<&str>) -> Self {
        Self::new(live.driver.clone(), live.params().clone(), live.config.clone(), database)
    }

    /// Hand out `session` first, before opening new ones.
    pub fn with_session(self, session: Box<dyn Session>) -> Self {
        *self.first.lock() = Some(session);
        self
    }

    /// Database the sessions connect to.
    pub fn database(&self) -> Option<&str> {
        self.params.database.as_deref()
    }
}

#[async_trait]
impl SessionFactory for AppSessionFactory {
    async fn open(&self) -> DbResult<Box<dyn Session>> {
        let first = self.first.lock().take();
        if let Some(mut s) = first
            && !s.is_closed()
        {
            if s.transaction_mode() == TransactionMode::Manual {
                // Switching to auto-commit commits what the init SQL left open.
                s.set_transaction_mode(TransactionMode::Auto).await?;
            }
            return Ok(s);
        }
        let timeout = self.params.connect_timeout + Duration::from_secs(5);
        let mut s = tokio::time::timeout(timeout, self.driver.connect(&self.params))
            .await
            .map_err(|_| DbError::timeout(trf("Connection timed out after {}s", &[&timeout.as_secs()])))??;
        init_session(s.as_mut(), &self.config, &self.params).await?;
        Ok(s)
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        self.driver.dialect()
    }
}

/// Database the objects live in (first one that names a database).
pub fn database_of<'a>(objects: impl IntoIterator<Item = &'a ObjectRef>) -> Option<String> {
    objects.into_iter().find_map(|o| o.database.clone()).filter(|d| !d.trim().is_empty())
}

/// Database of the table items of an export job.
pub fn export_database(job: &ExportJob) -> Option<String> {
    database_of(job.items.iter().filter_map(|i| match i {
        ExportItem::Table { table, .. } => Some(table),
        ExportItem::Query { .. } => None,
    }))
}

// ------------------------------------------------------------- task configs

/// Export settings that are not part of [`ExportJob`], stored next to the
/// job's fields in the task configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportTaskExtras {
    /// Database product the SQL format writes statements for; `None` uses the
    /// product of the source connection.
    pub sql_dialect: Option<DbKind>,
}

fn merge_json(base: serde_json::Value, extra: serde_json::Value) -> serde_json::Value {
    match (base, extra) {
        (serde_json::Value::Object(mut a), serde_json::Value::Object(b)) => {
            for (k, v) in b {
                if !v.is_null() {
                    a.insert(k, v);
                }
            }
            serde_json::Value::Object(a)
        }
        (a, _) => a,
    }
}

/// Configuration of an export task (`TaskKind::Export::config`).
pub fn export_task_config(job: &ExportJob, extras: &ExportTaskExtras) -> DbResult<serde_json::Value> {
    let base = serde_json::to_value(job).map_err(|e| DbError::internal(e.to_string()))?;
    let extra = serde_json::to_value(extras).map_err(|e| DbError::internal(e.to_string()))?;
    Ok(merge_json(base, extra))
}

/// Read an export task configuration.
pub fn parse_export_config(config: &serde_json::Value) -> DbResult<(ExportJob, ExportTaskExtras)> {
    let job: ExportJob = serde_json::from_value(config.clone()).map_err(|e| DbError::config(trf("Invalid export task configuration: {}", &[&e])))?;
    let extras: ExportTaskExtras = serde_json::from_value(config.clone()).unwrap_or_default();
    Ok((job, extras))
}

/// Configuration of an import task (`TaskKind::Import::config`).
pub fn import_task_config(job: &ImportJob) -> DbResult<serde_json::Value> {
    serde_json::to_value(job).map_err(|e| DbError::internal(e.to_string()))
}

/// Configuration of a migration task (`TaskKind::Migration::config`).
pub fn migration_task_config(job: &MigrationJob) -> DbResult<serde_json::Value> {
    serde_json::to_value(job).map_err(|e| DbError::internal(e.to_string()))
}

// --------------------------------------------------------------- reporting

/// Copy new transfer messages and the counters into the task run.
fn report(ctx: &RunContext, progress: &Progress) {
    for m in progress.take_messages() {
        let text = match &m.item {
            Some(item) => format!("{item}: {}", m.text),
            None => m.text,
        };
        let level = match m.level {
            MessageLevel::Info => dbiji_tasks::MessageLevel::Info,
            MessageLevel::Warning => dbiji_tasks::MessageLevel::Warning,
            MessageLevel::Error => dbiji_tasks::MessageLevel::Error,
        };
        ctx.log(level, text);
    }
    let s = progress.snapshot();
    ctx.progress(s.rows, s.total_rows, s.active_items.first().map(String::as_str));
}

/// Await `fut`, reporting the progress to the run twice a second.
async fn drive<T>(ctx: &RunContext, progress: &Progress, fut: impl Future<Output = DbResult<T>>) -> DbResult<T> {
    tokio::pin!(fut);
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let result = loop {
        tokio::select! {
            r = &mut fut => break r,
            _ = tick.tick() => report(ctx, progress),
        }
    };
    report(ctx, progress);
    result
}

/// Files written by the successful items of an export.
pub fn export_output_files(summary: &ExportJobSummary) -> Vec<PathBuf> {
    summary.items.iter().filter(|i| i.error.is_none()).filter_map(|i| i.file.clone()).collect()
}

// ------------------------------------------------------------------ runners

/// Run a saved export (`TaskKind::Export`).
pub async fn export(exec: &AppTaskExecutor, ctx: &RunContext, connection_id: &str, config: &serde_json::Value) -> DbResult<RunOutcome> {
    let (job, extras) = parse_export_config(config)?;
    if job.items.is_empty() {
        return Err(DbError::config(tr("The export task has no objects to export")));
    }
    let database = export_database(&job);
    let sql_dialect = extras.sql_dialect.map(|k| exec.registry.dialect(k));
    let c = exec.connection(connection_id).await?;
    let same_db = database.is_none() || database == c.params.database;
    let factory = AppSessionFactory::new(c.driver.clone(), c.params.clone(), c.config.clone(), database.as_deref());
    // The rest of `c` (the tunnel) lives until the end of this function.
    let mut session = c.session;
    let factory = if same_db {
        factory.with_session(session)
    } else {
        let _ = session.close().await;
        factory
    };
    let factory: Arc<dyn SessionFactory> = Arc::new(factory);
    let progress = Progress::shared();
    let total = job.items.len();
    ctx.info(trf("Exporting {} object(s) to {}", &[&total, &job.output_folder.display()]));
    let summary = drive(ctx, &progress, run_export_job_with(job, factory, sql_dialect, progress.clone(), ctx.cancel.clone())).await?;
    let failed: Vec<String> = summary.items.iter().filter_map(|i| i.error.as_ref().map(|e| format!("{}: {e}", i.name))).collect();
    let files = export_output_files(&summary);
    if !failed.is_empty() {
        return Err(DbError::query(trf("{} of {} object(s) failed: {}", &[&failed.len(), &total, &failed.join("; ")])));
    }
    Ok(RunOutcome { rows: Some(summary.rows), message: Some(trf("{} rows exported to {} file(s)", &[&summary.rows, &files.len()])), output_files: files })
}

/// Run a saved import (`TaskKind::Import`).
pub async fn import(exec: &AppTaskExecutor, ctx: &RunContext, connection_id: &str, config: &serde_json::Value) -> DbResult<RunOutcome> {
    let job: ImportJob = serde_json::from_value(config.clone()).map_err(|e| DbError::config(trf("Invalid import task configuration: {}", &[&e])))?;
    if job.mapping.target.name.trim().is_empty() {
        return Err(DbError::config(tr("The import task has no target table")));
    }
    let database = database_of([&job.mapping.target]);
    let c = exec.connection(connection_id).await?;
    if c.config.read_only {
        return Err(DbError::read_only(trf("The connection \"{}\" is read-only", &[&c.config.name])));
    }
    let same_db = database.is_none() || database == c.params.database;
    let factory = AppSessionFactory::new(c.driver.clone(), c.params.clone(), c.config.clone(), database.as_deref());
    let mut session = c.session;
    let factory = if same_db {
        factory.with_session(session)
    } else {
        let _ = session.close().await;
        factory
    };
    let factory: Arc<dyn SessionFactory> = Arc::new(factory);
    let progress = Progress::shared();
    let target = job.mapping.target.display_path();
    ctx.info(trf("Importing into {}", &[&target]));
    let summary = drive(ctx, &progress, run_import_job(job, factory, progress.clone(), ctx.cancel.clone())).await?;
    for e in summary.errors.iter().take(LOGGED_ROW_ERRORS) {
        let at = match &e.column {
            Some(c) => trf("Row {}, column {}", &[&e.row, c]),
            None => trf("Row {}", &[&e.row]),
        };
        ctx.warn(format!("{at}: {}", e.message));
    }
    for w in &summary.warnings {
        ctx.warn(w.clone());
    }
    Ok(RunOutcome { rows: Some(summary.rows_written), output_files: Vec::new(), message: Some(trf("{} rows imported, {} failed", &[&summary.rows_written, &summary.rows_failed])) })
}

/// Run a saved migration (`TaskKind::Migration`).
pub async fn migrate(exec: &AppTaskExecutor, ctx: &RunContext, source: &str, target: &str, config: &serde_json::Value) -> DbResult<RunOutcome> {
    let job: MigrationJob = serde_json::from_value(config.clone()).map_err(|e| DbError::config(trf("Invalid migration task configuration: {}", &[&e])))?;
    if job.tables.is_empty() {
        return Err(DbError::config(tr("The migration task has no tables")));
    }
    let database = database_of(job.tables.iter());
    let a = exec.connection(source).await?;
    let b = exec.connection(target).await?;
    if b.config.read_only {
        return Err(DbError::read_only(trf("The connection \"{}\" is read-only", &[&b.config.name])));
    }
    let same_db = database.is_none() || database == a.params.database;
    let src = AppSessionFactory::new(a.driver.clone(), a.params.clone(), a.config.clone(), database.as_deref());
    let mut src_session = a.session;
    let src = if same_db {
        src.with_session(src_session)
    } else {
        let _ = src_session.close().await;
        src
    };
    let dst = AppSessionFactory::new(b.driver.clone(), b.params.clone(), b.config.clone(), None).with_session(b.session);
    let (src, dst): (Arc<dyn SessionFactory>, Arc<dyn SessionFactory>) = (Arc::new(src), Arc::new(dst));
    let progress = Progress::new();
    let tables = job.tables.len();
    ctx.info(trf("Migrating {} table(s)", &[&tables]));
    let summary = drive(ctx, &progress, run_migration(job, src, dst, &progress, &ctx.cancel)).await?;
    let failed: Vec<String> = summary
        .tables
        .iter()
        .filter_map(|t| {
            let name = t.source.as_ref().map(ObjectRef::display_path).unwrap_or_default();
            t.error.as_ref().map(|e| format!("{name}: {e}"))
        })
        .collect();
    if !failed.is_empty() {
        return Err(DbError::query(trf("{} of {} table(s) failed: {}", &[&failed.len(), &tables, &failed.join("; ")])));
    }
    let failed_rows: u64 = summary.tables.iter().map(|t| t.rows_failed).sum();
    Ok(RunOutcome {
        rows: Some(summary.rows),
        output_files: Vec::new(),
        message: Some(if failed_rows > 0 {
            trf("{} rows copied into {} table(s), {} rows failed", &[&summary.rows, &tables, &failed_rows])
        } else {
            trf("{} rows copied into {} table(s)", &[&summary.rows, &tables])
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_transfer::ExportFormat;

    #[test]
    fn export_config_roundtrip_keeps_extras() {
        let job = ExportJob { items: vec![ExportItem::Query { sql: "select 1".into(), name: Some("one".into()) }], format: ExportFormat::Sql, ..Default::default() };
        let extras = ExportTaskExtras { sql_dialect: Some(DbKind::MySql) };
        let value = export_task_config(&job, &extras).unwrap();
        let (back, back_extras) = parse_export_config(&value).unwrap();
        assert_eq!(back, job);
        assert_eq!(back_extras, extras);
        // A plain job (no extras) is accepted as well.
        let plain = serde_json::to_value(&job).unwrap();
        let (_, none) = parse_export_config(&plain).unwrap();
        assert_eq!(none, ExportTaskExtras::default());
    }

    #[test]
    fn database_of_objects() {
        let a = ObjectRef::table(Some("public".into()), "a");
        let b = ObjectRef::table(Some("public".into()), "b").with_database(Some("other".into()));
        assert_eq!(database_of([&a]), None);
        assert_eq!(database_of([&a, &b]).as_deref(), Some("other"));
    }
}
