//! Executes saved tasks (FR-A3) for the in-app scheduler and for headless
//! runs (`dbiji --run-task`). Connections are read from `connections.json`
//! at every run and opened with saved credentials only.

use crate::conn::{HeadlessConnection, NetConnector, SecretSource, connect_saved};
use dbiji_config::{AppPaths, ConnectionStore};
use dbiji_core::ddl::SnapshotOptions;
use dbiji_core::driver::{TransactionMode, execute_update};
use dbiji_core::meta::SchemaRef;
use dbiji_core::registry::DriverRegistry;
use dbiji_core::{DbError, DbResult, Session};
use dbiji_tasks::{RunContext, RunOutcome, TaskDef, TaskExecutor, TaskKind};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub struct AppTaskExecutor {
    pub paths: AppPaths,
    pub registry: Arc<DriverRegistry>,
    pub secrets: Arc<dyn SecretSource>,
    pub net: Arc<dyn NetConnector>,
}

/// Configuration of a schema compare task (`TaskKind::SchemaCompare::config`).
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct CompareTaskConfig {
    pub source_connection: String,
    pub source_schema: Option<String>,
    pub target_connection: String,
    pub target_schema: Option<String>,
    /// Drop target objects missing in the source.
    pub drop_removed: bool,
    /// Execute the migration script on the target.
    pub apply: bool,
    /// Write the migration script here.
    pub script_path: Option<PathBuf>,
    /// Write an HTML report here.
    pub report_path: Option<PathBuf>,
}

impl AppTaskExecutor {
    async fn open(&self, id: &str) -> DbResult<HeadlessConnection> {
        let store = ConnectionStore::load(&self.paths)?;
        connect_saved(&self.registry, &store, self.secrets.as_ref(), self.net.clone(), id).await
    }

    async fn sql_script(
        &self,
        ctx: &RunContext,
        connection_id: &str,
        database: Option<&str>,
        schema: Option<&str>,
        sql: Option<&str>,
        script_path: Option<&PathBuf>,
        stop_on_error: bool,
        commit_at_end: bool,
    ) -> DbResult<RunOutcome> {
        let text = match (sql.filter(|s| !s.trim().is_empty()), script_path) {
            (Some(s), _) => s.to_string(),
            (None, Some(p)) => std::fs::read_to_string(p).map_err(|e| DbError::io(format!("{}: {e}", p.display())))?,
            (None, None) => return Err(DbError::config("The task has no SQL")),
        };
        let mut c = self.open(connection_id).await?;
        let r = run_script(ctx, c.session.as_mut(), &text, database, schema, stop_on_error, commit_at_end).await;
        let _ = c.session.close().await;
        r
    }

    async fn tool(
        &self,
        ctx: &RunContext,
        connection_id: &str,
        restore: Option<&dbiji_core::admin::RestoreOptions>,
        backup: Option<&dbiji_core::admin::BackupOptions>,
    ) -> DbResult<RunOutcome> {
        // Backup tools connect themselves; open (and close) a connection
        // anyway to establish tunnels and verify the credentials.
        let c = self.open(connection_id).await?;
        let spec = match (backup, restore) {
            (Some(b), _) => c.driver.backup_command(&c.params, b)?,
            (_, Some(r)) => c.driver.restore_command(&c.params, r)?,
            _ => return Err(DbError::internal("nothing to run")),
        };
        ctx.info(spec.display());
        let mut lines = 0u64;
        crate::tools::run_command(spec, ctx.cancel.clone(), |l| {
            lines += 1;
            ctx.info(l);
        })
        .await?;
        let mut out = RunOutcome::default();
        if let Some(b) = backup {
            out.output_files.push(b.output.clone());
            out.message = Some(format!("Backup written to {}", b.output.display()));
        } else {
            out.message = Some("Restore completed".into());
        }
        let _ = lines;
        drop(c);
        Ok(out)
    }

    async fn compare(&self, ctx: &RunContext, config: &serde_json::Value) -> DbResult<RunOutcome> {
        let cfg: CompareTaskConfig = serde_json::from_value(config.clone()).map_err(|e| DbError::config(format!("Invalid compare task: {e}")))?;
        let mut a = self.open(&cfg.source_connection).await?;
        let mut b = self.open(&cfg.target_connection).await?;
        let sa = a.session.schema_snapshot(&SchemaRef::new(None, cfg.source_schema.clone()), &SnapshotOptions::full()).await?;
        let sb = b.session.schema_snapshot(&SchemaRef::new(None, cfg.target_schema.clone()), &SnapshotOptions::full()).await?;
        let diff = dbiji_compare::schema_diff(&sa, &sb, &dbiji_compare::CompareOptions::default());
        let t = diff.summary.total;
        ctx.info(format!("{} added, {} removed, {} modified", t.added, t.removed, t.modified));
        let opts = dbiji_compare::ScriptOptions { drop_removed: cfg.drop_removed, include_comments: true, ..Default::default() };
        let dialect = b.session.dialect();
        let script = dbiji_compare::migration_script(&diff, &sa, &sb, dialect.as_ref(), &opts);
        let mut out = RunOutcome::default();
        if let Some(p) = &cfg.script_path {
            std::fs::write(p, script.iter().map(|s| format!("{s};\n")).collect::<String>()).map_err(|e| DbError::io(e.to_string()))?;
            out.output_files.push(p.clone());
        }
        if let Some(p) = &cfg.report_path {
            let html = dbiji_compare::html_report(&diff, cfg.source_schema.as_deref().unwrap_or(""), cfg.target_schema.as_deref().unwrap_or(""));
            std::fs::write(p, html).map_err(|e| DbError::io(e.to_string()))?;
            out.output_files.push(p.clone());
        }
        if cfg.apply && !script.is_empty() {
            if let Some(s) = &cfg.target_schema {
                b.session.set_current_schema(s).await?;
            }
            for (i, sql) in script.iter().enumerate() {
                if ctx.is_cancelled() {
                    return Err(DbError::cancelled());
                }
                execute_update(b.session.as_mut(), sql, &[]).await.map_err(|e| e.with_context(format!("statement {}", i + 1)))?;
                ctx.progress(i as u64 + 1, Some(script.len() as u64), None);
            }
            ctx.info(format!("{} statement(s) applied", script.len()));
        }
        out.message = Some(if diff.has_differences() { format!("{} difference(s)", t.changed()) } else { "The schemas are identical".into() });
        let _ = a.session.close().await;
        let _ = b.session.close().await;
        Ok(out)
    }
}

/// Run a script statement by statement on a headless session.
pub async fn run_script(
    ctx: &RunContext,
    s: &mut dyn Session,
    text: &str,
    database: Option<&str>,
    schema: Option<&str>,
    stop_on_error: bool,
    commit_at_end: bool,
) -> DbResult<RunOutcome> {
    if let Some(db) = database.filter(|d| !d.is_empty()) {
        s.set_current_database(db).await?;
    }
    if let Some(sc) = schema.filter(|d| !d.is_empty()) {
        s.set_current_schema(sc).await?;
    }
    let syntax = s.dialect().syntax().clone();
    let spans = dbiji_sql::split_statements(text, &syntax, &dbiji_sql::SplitOptions::default());
    let total = spans.len() as u64;
    let mut rows = 0u64;
    let mut failed = 0usize;
    for (i, span) in spans.iter().enumerate() {
        if ctx.is_cancelled() {
            return Err(DbError::cancelled());
        }
        let stmt = span.text(text);
        if span.is_client_command || stmt.trim_start().starts_with('@') {
            ctx.warn(format!("Client command skipped in task: {}", stmt.lines().next().unwrap_or("")));
            continue;
        }
        match execute_update(s, stmt, &[]).await {
            Ok(n) => rows += n.unwrap_or(0),
            Err(e) => {
                failed += 1;
                ctx.error(format!("Statement {}: {}", i + 1, e.full_text()));
                if stop_on_error {
                    if s.transaction_mode() == TransactionMode::Manual {
                        let _ = s.rollback().await;
                    }
                    return Err(e.with_context(format!("statement {}", i + 1)));
                }
            }
        }
        ctx.progress(i as u64 + 1, Some(total), None);
    }
    if commit_at_end && s.transaction_mode() == TransactionMode::Manual {
        s.commit().await?;
    }
    Ok(RunOutcome {
        rows: Some(rows),
        output_files: Vec::new(),
        message: Some(if failed == 0 { format!("{total} statement(s) executed") } else { format!("{total} statement(s), {failed} failed") }),
    })
}

#[async_trait::async_trait]
impl TaskExecutor for AppTaskExecutor {
    async fn execute(&self, task: &TaskDef, ctx: RunContext) -> Result<RunOutcome, String> {
        let r = match &task.kind {
            TaskKind::SqlScript { connection_id, database, schema, sql, script_path, stop_on_error, commit_at_end } => {
                self.sql_script(&ctx, connection_id, database.as_deref(), schema.as_deref(), sql.as_deref(), script_path.as_ref(), *stop_on_error, *commit_at_end).await
            }
            TaskKind::Shell { command } => {
                tokio::select! {
                    r = dbiji_net::run_shell_command(command, Duration::from_secs(24 * 3600)) => r.map(|out| {
                        for l in out.lines().take(200) {
                            ctx.info(l.to_string());
                        }
                        RunOutcome::default()
                    }),
                    _ = ctx.cancel.cancelled() => Err(DbError::cancelled()),
                }
            }
            TaskKind::Backup { connection_id, options } => self.tool(&ctx, connection_id, None, Some(options)).await,
            TaskKind::Restore { connection_id, options } => self.tool(&ctx, connection_id, Some(options), None).await,
            TaskKind::SchemaCompare { config } => self.compare(&ctx, config).await,
            TaskKind::Export { connection_id, config } => crate::transferexec::export(self, &ctx, connection_id, config).await,
            TaskKind::Import { connection_id, config } => crate::transferexec::import(self, &ctx, connection_id, config).await,
            TaskKind::Migration { source_connection_id, target_connection_id, config } => {
                crate::transferexec::migrate(self, &ctx, source_connection_id, target_connection_id, config).await
            }
            TaskKind::Composite { .. } => Err(DbError::internal("composite tasks are run by the scheduler")),
        };
        r.map_err(|e| e.full_text())
    }
}

impl AppTaskExecutor {
    /// Open a saved connection (used by the transfer task runners).
    pub async fn connection(&self, id: &str) -> DbResult<HeadlessConnection> {
        self.open(id).await
    }
}
