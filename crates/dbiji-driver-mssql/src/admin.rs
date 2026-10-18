//! Session manager, lock manager and server metrics for SQL Server
//! (dynamic management views; `VIEW SERVER STATE` is needed to see other
//! sessions).

use crate::session::SqlServerSession;
use dbiji_core::admin::*;
use dbiji_core::{DbError, DbResult, Session, Value};

const SESSIONS_SQL: &str = "SELECT s.session_id, s.login_name, s.host_name, s.program_name, DB_NAME(s.database_id) AS database_name, \
    COALESCE(r.status, s.status) AS status, r.command, r.wait_type, r.wait_time, \
    NULLIF(r.blocking_session_id, 0) AS blocking_session_id, \
    r.cpu_time, r.total_elapsed_time, s.open_transaction_count, s.login_time, s.last_request_start_time, \
    s.reads, s.writes, s.memory_usage, \
    CASE WHEN r.sql_handle IS NULL THEN NULL ELSE SUBSTRING(t.text, (r.statement_start_offset / 2) + 1, \
      ((CASE r.statement_end_offset WHEN -1 THEN DATALENGTH(t.text) ELSE r.statement_end_offset END - r.statement_start_offset) / 2) + 1) END AS current_statement, \
    t.text AS batch_text, CASE WHEN s.session_id = @@SPID THEN 1 ELSE 0 END AS is_current \
    FROM sys.dm_exec_sessions s \
    LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id \
    OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t \
    WHERE s.is_user_process = 1 \
    ORDER BY s.session_id";

const SESSION_COLUMNS: &[&str] = &[
    "session_id",
    "login_name",
    "host_name",
    "program_name",
    "database_name",
    "status",
    "command",
    "wait_type",
    "wait_time_ms",
    "blocking_session_id",
    "cpu_time_ms",
    "elapsed_ms",
    "open_transactions",
    "login_time",
    "last_request_start",
    "reads",
    "writes",
    "memory_pages",
    "current_statement",
    "batch_text",
    "is_current",
];

/// Sessions with their running request.
pub async fn list_sessions(s: &mut SqlServerSession) -> DbResult<SessionList> {
    let rows = s.rows(SESSIONS_SQL, &[]).await?;
    Ok(SessionList {
        data: TabularData { columns: SESSION_COLUMNS.iter().map(|c| c.to_string()).collect(), rows },
        id_column: 0,
        query_column: Some(18),
        state_column: Some(5),
        blocked_by_column: Some(9),
    })
}

/// `KILL <spid>` (Terminate). SQL Server cannot cancel the statement of
/// another session without terminating it.
pub async fn kill_session(s: &mut SqlServerSession, id: &str, mode: KillMode) -> DbResult<()> {
    let spid: i64 = id.trim().parse().map_err(|_| DbError::config(format!("'{id}' is not a SQL Server session id")))?;
    if mode == KillMode::CancelQuery {
        return Err(DbError::unsupported("SQL Server cannot cancel the statement of another session without ending it; use Terminate (KILL)"));
    }
    if s.server_info().session_id.as_deref() == Some(spid.to_string().as_str()) {
        return Err(DbError::config("Refusing to KILL the current session"));
    }
    s.batch(&format!("KILL {spid}")).await
}

const LOCKS_SQL: &str = "SELECT l.request_session_id, l.resource_type, DB_NAME(l.resource_database_id) AS database_name, \
    CASE WHEN l.resource_type = 'OBJECT' THEN \
      OBJECT_SCHEMA_NAME(l.resource_associated_entity_id, l.resource_database_id) + N'.' + \
      OBJECT_NAME(l.resource_associated_entity_id, l.resource_database_id) END AS object_name, \
    l.resource_associated_entity_id, l.resource_description, l.request_mode, l.request_status, l.request_owner_type, \
    CAST(CASE WHEN l.request_status = 'GRANT' THEN 1 ELSE 0 END AS bit) AS granted, \
    NULLIF(w.blocking_session_id, 0) AS blocking_session_id, w.wait_duration_ms \
    FROM sys.dm_tran_locks l \
    LEFT JOIN sys.dm_os_waiting_tasks w ON w.resource_address = l.lock_owner_address \
    WHERE NOT (l.request_session_id = @@SPID AND l.resource_type = 'DATABASE') \
    ORDER BY l.request_session_id, l.resource_type";

/// Locks with their waiting / blocking sessions.
pub async fn list_locks(s: &mut SqlServerSession) -> DbResult<LockList> {
    let rows = s.rows(LOCKS_SQL, &[]).await?;
    let columns = [
        "session_id",
        "resource_type",
        "database_name",
        "object_name",
        "resource_id",
        "resource_description",
        "mode",
        "status",
        "owner_type",
        "granted",
        "blocking_session_id",
        "wait_ms",
    ];
    Ok(LockList { data: TabularData { columns: columns.iter().map(|c| c.to_string()).collect(), rows }, session_column: 0, blocked_by_column: Some(10), granted_column: Some(9) })
}

/// (object suffix, counter, instance or "", group, key, label, kind, unit)
type CounterSpec = (&'static str, &'static str, &'static str, &'static str, &'static str, &'static str, MetricKind, &'static str);

const COUNTERS: &[CounterSpec] = &[
    ("SQL Statistics", "Batch Requests/sec", "", "Workload", "batch_requests", "Batch requests", MetricKind::Counter, ""),
    ("SQL Statistics", "SQL Compilations/sec", "", "Workload", "compilations", "SQL compilations", MetricKind::Counter, ""),
    ("SQL Statistics", "SQL Re-Compilations/sec", "", "Workload", "recompilations", "SQL re-compilations", MetricKind::Counter, ""),
    ("General Statistics", "User Connections", "", "Connections", "connections.user", "User connections", MetricKind::Gauge, ""),
    ("General Statistics", "Logins/sec", "", "Connections", "connections.logins", "Logins", MetricKind::Counter, ""),
    ("General Statistics", "Processes blocked", "", "Locks", "locks.blocked_processes", "Blocked processes", MetricKind::Gauge, ""),
    ("Buffer Manager", "Page life expectancy", "", "Memory", "buffer.page_life_expectancy", "Page life expectancy", MetricKind::Gauge, "s"),
    ("Buffer Manager", "Page reads/sec", "", "I/O", "io.page_reads", "Page reads", MetricKind::Counter, ""),
    ("Buffer Manager", "Page writes/sec", "", "I/O", "io.page_writes", "Page writes", MetricKind::Counter, ""),
    ("Databases", "Transactions/sec", "_Total", "Transactions", "xact.total", "Transactions", MetricKind::Counter, ""),
    ("Locks", "Lock Waits/sec", "_Total", "Locks", "locks.waits", "Lock waits", MetricKind::Counter, ""),
    ("Locks", "Number of Deadlocks/sec", "_Total", "Locks", "locks.deadlocks", "Deadlocks", MetricKind::Counter, ""),
    ("Memory Manager", "Total Server Memory (KB)", "", "Memory", "memory.total", "Total server memory", MetricKind::Gauge, "bytes"),
    ("Memory Manager", "Target Server Memory (KB)", "", "Memory", "memory.target", "Target server memory", MetricKind::Gauge, "bytes"),
];

/// Health counters from `sys.dm_os_performance_counters`.
pub async fn server_metrics(s: &mut SqlServerSession) -> DbResult<Vec<Metric>> {
    let sql = "SELECT RTRIM(object_name), RTRIM(counter_name), RTRIM(instance_name), cntr_value \
               FROM sys.dm_os_performance_counters \
               WHERE counter_name IN (N'Batch Requests/sec', N'SQL Compilations/sec', N'SQL Re-Compilations/sec', \
               N'User Connections', N'Logins/sec', N'Processes blocked', N'Page life expectancy', N'Page reads/sec', \
               N'Page writes/sec', N'Transactions/sec', N'Lock Waits/sec', N'Number of Deadlocks/sec', \
               N'Total Server Memory (KB)', N'Target Server Memory (KB)', N'Buffer cache hit ratio', N'Buffer cache hit ratio base')";
    let rows = s.rows(sql, &[]).await?;
    Ok(metrics_from_rows(&rows))
}

/// Turn counter rows `(object, counter, instance, value)` into metrics.
pub fn metrics_from_rows(rows: &[Vec<Value>]) -> Vec<Metric> {
    let find = |object: &str, counter: &str, instance: &str| -> Option<f64> {
        rows.iter()
            .find(|r| {
                let obj = r.first().map(|v| v.to_text().into_owned()).unwrap_or_default();
                let cnt = r.get(1).map(|v| v.to_text().into_owned()).unwrap_or_default();
                let inst = r.get(2).map(|v| v.to_text().into_owned()).unwrap_or_default();
                obj.ends_with(object) && cnt.eq_ignore_ascii_case(counter) && (instance.is_empty() || inst == instance)
            })
            .and_then(|r| r.get(3))
            .and_then(Value::as_f64)
    };
    let mut out = Vec::new();
    for (object, counter, instance, group, key, label, kind, unit) in COUNTERS {
        if let Some(mut v) = find(object, counter, instance) {
            if counter.ends_with("(KB)") {
                v *= 1024.0;
            }
            out.push(match kind {
                MetricKind::Gauge => Metric::gauge(group, key, label, v, unit),
                MetricKind::Counter => Metric::counter(group, key, label, v, unit),
            });
        }
    }
    if let (Some(hit), Some(base)) = (find("Buffer Manager", "Buffer cache hit ratio", ""), find("Buffer Manager", "Buffer cache hit ratio base", ""))
        && base > 0.0
    {
        out.push(Metric::gauge("Memory", "buffer.cache_hit_ratio", "Buffer cache hit ratio", hit / base * 100.0, "%"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(o: &str, c: &str, i: &str, v: i64) -> Vec<Value> {
        vec![Value::text(o), Value::text(c), Value::text(i), Value::Int(v)]
    }

    #[test]
    fn metrics() {
        let rows = vec![
            row("SQLServer:SQL Statistics", "Batch Requests/sec", "", 100),
            row("SQLServer:General Statistics", "User Connections", "", 7),
            row("MSSQL$INST:Buffer Manager", "Buffer cache hit ratio", "", 90),
            row("MSSQL$INST:Buffer Manager", "Buffer cache hit ratio base", "", 100),
            row("SQLServer:Databases", "Transactions/sec", "master", 3),
            row("SQLServer:Databases", "Transactions/sec", "_Total", 50),
            row("SQLServer:Memory Manager", "Total Server Memory (KB)", "", 2),
        ];
        let m = metrics_from_rows(&rows);
        let get = |k: &str| m.iter().find(|x| x.key == k).cloned().unwrap();
        assert_eq!(get("batch_requests").kind, MetricKind::Counter);
        assert_eq!(get("connections.user").value, 7.0);
        assert_eq!(get("connections.user").kind, MetricKind::Gauge);
        assert_eq!(get("buffer.cache_hit_ratio").value, 90.0);
        assert_eq!(get("xact.total").value, 50.0);
        assert_eq!(get("memory.total").value, 2048.0);
        assert!(m.iter().all(|x| x.key != "locks.deadlocks"));
    }
}
