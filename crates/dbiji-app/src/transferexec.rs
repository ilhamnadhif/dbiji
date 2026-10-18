//! Data transfer tasks (export, import, migration) run through `dbiji-transfer`.

use crate::taskexec::AppTaskExecutor;
use dbiji_core::{DbError, DbResult};
use dbiji_tasks::{RunContext, RunOutcome};

pub async fn export(exec: &AppTaskExecutor, ctx: &RunContext, connection_id: &str, config: &serde_json::Value) -> DbResult<RunOutcome> {
    let _ = (exec, ctx, connection_id, config);
    Err(DbError::unsupported("Data export tasks are not available in this build"))
}

pub async fn import(exec: &AppTaskExecutor, ctx: &RunContext, connection_id: &str, config: &serde_json::Value) -> DbResult<RunOutcome> {
    let _ = (exec, ctx, connection_id, config);
    Err(DbError::unsupported("Data import tasks are not available in this build"))
}

pub async fn migrate(exec: &AppTaskExecutor, ctx: &RunContext, source: &str, target: &str, config: &serde_json::Value) -> DbResult<RunOutcome> {
    let _ = (exec, ctx, source, target, config);
    Err(DbError::unsupported("Data migration tasks are not available in this build"))
}
