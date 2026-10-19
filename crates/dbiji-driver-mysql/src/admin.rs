//! Session manager, lock manager and server metrics.

use crate::convert::read;
use crate::session::MySqlSession;
use dbiji_core::admin::{KillMode, LockList, Metric, SessionList, TabularData};
use dbiji_core::driver::query_tabular;
use dbiji_core::{DbError, DbResult, ErrorKind};
use std::collections::HashMap;

const SESSIONS_MYSQL8: &str = "SELECT p.ID, p.USER, p.HOST, p.DB, p.COMMAND, p.TIME, p.STATE, p.INFO, \
    (SELECT GROUP_CONCAT(DISTINCT bt.PROCESSLIST_ID ORDER BY bt.PROCESSLIST_ID) \
       FROM performance_schema.data_lock_waits w \
       JOIN performance_schema.threads rt ON rt.THREAD_ID = w.REQUESTING_THREAD_ID \
       JOIN performance_schema.threads bt ON bt.THREAD_ID = w.BLOCKING_THREAD_ID \
      WHERE rt.PROCESSLIST_ID = p.ID) AS BLOCKED_BY \
    FROM information_schema.PROCESSLIST p ORDER BY p.ID";

const SESSIONS_INNODB_LOCK_WAITS: &str = "SELECT p.ID, p.USER, p.HOST, p.DB, p.COMMAND, p.TIME, p.STATE, p.INFO, \
    (SELECT GROUP_CONCAT(DISTINCT b.trx_mysql_thread_id ORDER BY b.trx_mysql_thread_id) \
       FROM information_schema.INNODB_LOCK_WAITS w \
       JOIN information_schema.INNODB_TRX r ON r.trx_id = w.requesting_trx_id \
       JOIN information_schema.INNODB_TRX b ON b.trx_id = w.blocking_trx_id \
      WHERE r.trx_mysql_thread_id = p.ID) AS BLOCKED_BY \
    FROM information_schema.PROCESSLIST p ORDER BY p.ID";

const SESSIONS_PLAIN: &str = "SELECT ID, USER, HOST, DB, COMMAND, TIME, STATE, INFO FROM information_schema.PROCESSLIST ORDER BY ID";

const LOCKS_MYSQL8: &str = "SELECT t.PROCESSLIST_ID AS SESSION_ID, l.ENGINE_TRANSACTION_ID AS TRX_ID, l.OBJECT_SCHEMA, \
    l.OBJECT_NAME, l.PARTITION_NAME, l.INDEX_NAME, l.LOCK_TYPE, l.LOCK_MODE, l.LOCK_STATUS, l.LOCK_DATA, \
    (SELECT GROUP_CONCAT(DISTINCT bt.PROCESSLIST_ID) FROM performance_schema.data_lock_waits w \
       JOIN performance_schema.threads bt ON bt.THREAD_ID = w.BLOCKING_THREAD_ID \
      WHERE w.REQUESTING_ENGINE_LOCK_ID = l.ENGINE_LOCK_ID) AS BLOCKED_BY, \
    l.LOCK_STATUS = 'GRANTED' AS GRANTED \
    FROM performance_schema.data_locks l LEFT JOIN performance_schema.threads t ON t.THREAD_ID = l.THREAD_ID \
    UNION ALL \
    SELECT t.PROCESSLIST_ID, NULL, m.OBJECT_SCHEMA, m.OBJECT_NAME, NULL, NULL, CONCAT('METADATA ', m.OBJECT_TYPE), \
    m.LOCK_TYPE, m.LOCK_STATUS, m.LOCK_DURATION, NULL, m.LOCK_STATUS = 'GRANTED' \
    FROM performance_schema.metadata_locks m LEFT JOIN performance_schema.threads t ON t.THREAD_ID = m.OWNER_THREAD_ID \
    WHERE m.OBJECT_TYPE IN ('TABLE', 'SCHEMA', 'FUNCTION', 'PROCEDURE', 'TRIGGER', 'EVENT') \
      AND m.OBJECT_SCHEMA NOT IN ('performance_schema', 'information_schema', 'mysql', 'sys') \
    ORDER BY 1, 3, 4";

const LOCKS_INNODB: &str = "SELECT t.trx_mysql_thread_id AS SESSION_ID, l.lock_trx_id AS TRX_ID, l.lock_table AS OBJECT_NAME, \
    l.lock_index AS INDEX_NAME, l.lock_type AS LOCK_TYPE, l.lock_mode AS LOCK_MODE, \
    IF(EXISTS(SELECT 1 FROM information_schema.INNODB_LOCK_WAITS w WHERE w.requested_lock_id = l.lock_id), 'WAITING', 'GRANTED') AS LOCK_STATUS, \
    l.lock_data AS LOCK_DATA, \
    (SELECT GROUP_CONCAT(DISTINCT bt.trx_mysql_thread_id) FROM information_schema.INNODB_LOCK_WAITS w \
       JOIN information_schema.INNODB_TRX bt ON bt.trx_id = w.blocking_trx_id WHERE w.requested_lock_id = l.lock_id) AS BLOCKED_BY, \
    NOT EXISTS(SELECT 1 FROM information_schema.INNODB_LOCK_WAITS w WHERE w.requested_lock_id = l.lock_id) AS GRANTED \
    FROM information_schema.INNODB_LOCKS l LEFT JOIN information_schema.INNODB_TRX t ON t.trx_id = l.lock_trx_id \
    ORDER BY 1, 3";

const LOCKS_TRX: &str = "SELECT trx_mysql_thread_id AS SESSION_ID, trx_id AS TRX_ID, trx_state AS STATE, trx_started AS STARTED, \
    trx_wait_started AS WAIT_STARTED, trx_rows_locked AS ROWS_LOCKED, trx_tables_locked AS TABLES_LOCKED, \
    trx_lock_structs AS LOCK_STRUCTS, trx_query AS QUERY, trx_state <> 'LOCK WAIT' AS GRANTED \
    FROM information_schema.INNODB_TRX ORDER BY trx_mysql_thread_id";

fn sessions(data: TabularData) -> SessionList {
    SessionList {
        id_column: data.column_index("ID").unwrap_or(0),
        query_column: data.column_index("INFO"),
        state_column: data.column_index("STATE"),
        blocked_by_column: data.column_index("BLOCKED_BY"),
        data,
    }
}

fn locks(data: TabularData) -> LockList {
    LockList {
        session_column: data.column_index("SESSION_ID").unwrap_or(0),
        blocked_by_column: data.column_index("BLOCKED_BY"),
        granted_column: data.column_index("GRANTED"),
        data,
    }
}

/// Metric definitions: (status variable, key, label, group, unit, counter?).
const METRICS: &[(&str, &str, &str, &str, &str, bool)] = &[
    (
        "Threads_connected",
        "connections.connected",
        "Connected threads",
        "Connections",
        "",
        false,
    ),
    (
        "Threads_running",
        "connections.running",
        "Running threads",
        "Connections",
        "",
        false,
    ),
    (
        "Max_used_connections",
        "connections.max_used",
        "Max used connections",
        "Connections",
        "",
        false,
    ),
    (
        "Connections",
        "connections.total",
        "Connection attempts",
        "Connections",
        "",
        true,
    ),
    (
        "Aborted_connects",
        "connections.aborted_connects",
        "Aborted connects",
        "Connections",
        "",
        true,
    ),
    (
        "Aborted_clients",
        "connections.aborted_clients",
        "Aborted clients",
        "Connections",
        "",
        true,
    ),
    (
        "Questions",
        "queries.questions",
        "Questions",
        "Queries",
        "",
        true,
    ),
    ("Queries", "queries.total", "Queries", "Queries", "", true),
    (
        "Com_select",
        "queries.select",
        "SELECT",
        "Queries",
        "",
        true,
    ),
    (
        "Com_insert",
        "queries.insert",
        "INSERT",
        "Queries",
        "",
        true,
    ),
    (
        "Com_update",
        "queries.update",
        "UPDATE",
        "Queries",
        "",
        true,
    ),
    (
        "Com_delete",
        "queries.delete",
        "DELETE",
        "Queries",
        "",
        true,
    ),
    (
        "Com_commit",
        "transactions.commit",
        "Commits",
        "Transactions",
        "",
        true,
    ),
    (
        "Com_rollback",
        "transactions.rollback",
        "Rollbacks",
        "Transactions",
        "",
        true,
    ),
    (
        "Slow_queries",
        "queries.slow",
        "Slow queries",
        "Queries",
        "",
        true,
    ),
    (
        "Bytes_received",
        "network.received",
        "Bytes received",
        "Network",
        "bytes",
        true,
    ),
    (
        "Bytes_sent",
        "network.sent",
        "Bytes sent",
        "Network",
        "bytes",
        true,
    ),
    (
        "Innodb_buffer_pool_read_requests",
        "innodb.buffer_pool_read_requests",
        "Buffer pool read requests",
        "InnoDB",
        "",
        true,
    ),
    (
        "Innodb_buffer_pool_reads",
        "innodb.buffer_pool_reads",
        "Buffer pool disk reads",
        "InnoDB",
        "",
        true,
    ),
    (
        "Innodb_row_lock_waits",
        "innodb.row_lock_waits",
        "Row lock waits",
        "InnoDB",
        "",
        true,
    ),
    (
        "Innodb_row_lock_current_waits",
        "innodb.row_lock_current_waits",
        "Current row lock waits",
        "InnoDB",
        "",
        false,
    ),
    (
        "Innodb_rows_read",
        "innodb.rows_read",
        "Rows read",
        "InnoDB",
        "rows",
        true,
    ),
    (
        "Innodb_rows_inserted",
        "innodb.rows_inserted",
        "Rows inserted",
        "InnoDB",
        "rows",
        true,
    ),
    (
        "Innodb_rows_updated",
        "innodb.rows_updated",
        "Rows updated",
        "InnoDB",
        "rows",
        true,
    ),
    (
        "Innodb_rows_deleted",
        "innodb.rows_deleted",
        "Rows deleted",
        "InnoDB",
        "rows",
        true,
    ),
    (
        "Innodb_buffer_pool_pages_dirty",
        "innodb.pages_dirty",
        "Dirty pages",
        "InnoDB",
        "",
        false,
    ),
    (
        "Created_tmp_tables",
        "tmp.tables",
        "Temporary tables",
        "Temporary tables",
        "",
        true,
    ),
    (
        "Created_tmp_disk_tables",
        "tmp.disk_tables",
        "On-disk temporary tables",
        "Temporary tables",
        "",
        true,
    ),
    // DBeaver's "InnoDB memory" and "Key efficiency" dashboards.
    (
        "Innodb_buffer_pool_bytes_data",
        "innodb_memory.data",
        "Buffer pool data",
        "InnoDB memory",
        "bytes",
        false,
    ),
    (
        "Innodb_buffer_pool_pages_total",
        "innodb_memory.pages_total",
        "Buffer pool pages",
        "InnoDB memory",
        "",
        false,
    ),
    (
        "Innodb_buffer_pool_pages_free",
        "innodb_memory.pages_free",
        "Free pages",
        "InnoDB memory",
        "",
        false,
    ),
    (
        "Key_read_requests",
        "keys.read_requests",
        "Key read requests",
        "Key efficiency",
        "",
        true,
    ),
    (
        "Key_reads",
        "keys.reads",
        "Key reads from disk",
        "Key efficiency",
        "",
        true,
    ),
    (
        "Key_write_requests",
        "keys.write_requests",
        "Key write requests",
        "Key efficiency",
        "",
        true,
    ),
    (
        "Key_writes",
        "keys.writes",
        "Key writes to disk",
        "Key efficiency",
        "",
        true,
    ),
    ("Uptime", "server.uptime", "Uptime", "Server", "s", false),
];

/// Build metrics from `SHOW GLOBAL STATUS` values.
pub fn metrics_from_status(status: &HashMap<String, f64>) -> Vec<Metric> {
    let mut out = Vec::new();
    for (var, key, label, group, unit, counter) in METRICS {
        if let Some(v) = status.get(&var.to_ascii_lowercase()) {
            out.push(if *counter {
                Metric::counter(group, key, label, *v, unit)
            } else {
                Metric::gauge(group, key, label, *v, unit)
            });
        }
    }
    let requests = status.get("innodb_buffer_pool_read_requests").copied();
    let reads = status.get("innodb_buffer_pool_reads").copied();
    if let (Some(req), Some(rd)) = (requests, reads) {
        let ratio = if req > 0.0 {
            ((1.0 - rd / req) * 100.0).clamp(0.0, 100.0)
        } else {
            100.0
        };
        out.push(Metric::gauge(
            "InnoDB",
            "innodb.buffer_pool_hit_ratio",
            "Buffer pool hit ratio",
            ratio,
            "%",
        ));
    }
    let key_req = status.get("key_read_requests").copied();
    let key_reads = status.get("key_reads").copied();
    if let (Some(req), Some(rd)) = (key_req, key_reads) {
        let ratio = if req > 0.0 {
            ((1.0 - rd / req) * 100.0).clamp(0.0, 100.0)
        } else {
            100.0
        };
        out.push(Metric::gauge(
            "Key efficiency",
            "keys.cache_hit_ratio",
            "Key cache hit ratio",
            ratio,
            "%",
        ));
    }
    out
}

impl MySqlSession {
    pub(crate) async fn admin_sessions(&mut self) -> DbResult<SessionList> {
        let first = if self.profile.has_data_locks() {
            SESSIONS_MYSQL8
        } else if self.profile.has_innodb_locks_views() {
            SESSIONS_INNODB_LOCK_WAITS
        } else {
            SESSIONS_PLAIN
        };
        match query_tabular(self, first, &[]).await {
            Ok(d) => Ok(sessions(d)),
            Err(e) if e.kind == ErrorKind::Connection => Err(e),
            Err(e) if first != SESSIONS_PLAIN => {
                tracing::debug!("session list with blocking information failed: {e}");
                Ok(sessions(query_tabular(self, SESSIONS_PLAIN, &[]).await?))
            }
            Err(e) => Err(e),
        }
    }

    pub(crate) async fn admin_kill(&mut self, id: &str, mode: KillMode) -> DbResult<()> {
        let id: u64 = id
            .trim()
            .parse()
            .map_err(|_| DbError::config(format!("Invalid session id '{id}'")))?;
        let sql = match mode {
            KillMode::CancelQuery => format!("KILL QUERY {id}"),
            KillMode::Terminate => format!("KILL {id}"),
        };
        self.exec_simple(&sql).await
    }

    pub(crate) async fn admin_locks(&mut self) -> DbResult<LockList> {
        let mut candidates: Vec<&str> = Vec::new();
        if self.profile.has_data_locks() {
            candidates.push(LOCKS_MYSQL8);
        }
        if self.profile.has_innodb_locks_views() {
            candidates.push(LOCKS_INNODB);
        }
        candidates.push(LOCKS_TRX);
        let mut last_err = None;
        for sql in candidates {
            match query_tabular(self, sql, &[]).await {
                Ok(d) => return Ok(locks(d)),
                Err(e) if e.kind == ErrorKind::Connection => return Err(e),
                Err(e) => {
                    tracing::debug!("lock query failed, trying the next variant: {e}");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            DbError::unsupported("No lock information is available on this server")
        }))
    }

    pub(crate) async fn admin_metrics(&mut self) -> DbResult<Vec<Metric>> {
        let rows = self.rows("SHOW GLOBAL STATUS", Vec::new()).await?;
        let mut status = HashMap::with_capacity(rows.len());
        for r in &rows {
            if let (Some(k), Some(v)) = (read::text(r, 0), read::float(r, 1)) {
                status.insert(k.to_ascii_lowercase(), v);
            }
        }
        Ok(metrics_from_status(&status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::admin::MetricKind;

    #[test]
    fn metric_mapping() {
        let mut s = HashMap::new();
        s.insert("threads_connected".to_string(), 5.0);
        s.insert("questions".to_string(), 1000.0);
        s.insert("innodb_buffer_pool_read_requests".to_string(), 1000.0);
        s.insert("innodb_buffer_pool_reads".to_string(), 10.0);
        s.insert("uptime".to_string(), 60.0);
        s.insert("key_read_requests".to_string(), 200.0);
        s.insert("key_reads".to_string(), 50.0);
        s.insert("innodb_buffer_pool_bytes_data".to_string(), 1048576.0);
        let m = metrics_from_status(&s);
        let get = |k: &str| m.iter().find(|x| x.key == k).cloned().unwrap();
        assert_eq!(get("connections.connected").kind, MetricKind::Gauge);
        assert_eq!(get("queries.questions").kind, MetricKind::Counter);
        assert!((get("innodb.buffer_pool_hit_ratio").value - 99.0).abs() < 1e-9);
        assert_eq!(get("innodb.buffer_pool_hit_ratio").unit, "%");
        assert_eq!(get("server.uptime").unit, "s");
        assert!((get("keys.cache_hit_ratio").value - 75.0).abs() < 1e-9);
        assert_eq!(get("keys.read_requests").kind, MetricKind::Counter);
        assert_eq!(get("innodb_memory.data").unit, "bytes");
        assert!(m.iter().all(|x| x.key != "queries.slow"));
    }

    #[test]
    fn list_columns() {
        let d = TabularData::new(vec![
            "ID".into(),
            "USER".into(),
            "STATE".into(),
            "INFO".into(),
            "BLOCKED_BY".into(),
        ]);
        let s = sessions(d);
        assert_eq!(
            (
                s.id_column,
                s.state_column,
                s.query_column,
                s.blocked_by_column
            ),
            (0, Some(2), Some(3), Some(4))
        );
        let d = TabularData::new(vec![
            "SESSION_ID".into(),
            "TRX_ID".into(),
            "BLOCKED_BY".into(),
            "GRANTED".into(),
        ]);
        let l = locks(d);
        assert_eq!(
            (l.session_column, l.blocked_by_column, l.granted_column),
            (0, Some(2), Some(3))
        );
    }
}
