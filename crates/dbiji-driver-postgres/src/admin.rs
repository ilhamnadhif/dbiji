//! Session manager, lock manager and server health metrics.

use crate::catalog::{self, P, RowExt};
use crate::session::PgSession;
use dbiji_core::admin::{KillMode, LockList, Metric, SessionList};
use dbiji_core::{DbError, DbResult};

const SESSIONS_SQL: &str = "SELECT a.pid, a.usename::text AS user, a.datname::text AS database, a.application_name, \
     a.client_addr::text AS client_addr, a.client_port, a.backend_type, a.state, a.wait_event_type, a.wait_event, \
     NULLIF(array_to_string(pg_catalog.pg_blocking_pids(a.pid), ', '), '') AS blocked_by, \
     a.backend_start, a.xact_start, a.query_start, a.state_change, \
     date_trunc('milliseconds', now() - a.query_start)::text AS query_duration, \
     a.backend_xid::text AS xid, a.backend_xmin::text AS xmin, a.query, (a.pid = pg_catalog.pg_backend_pid()) AS is_current \
     FROM pg_catalog.pg_stat_activity a \
     ORDER BY (a.state = 'active') DESC NULLS LAST, a.query_start DESC NULLS LAST, a.pid";

const LOCKS_SQL: &str = "SELECT l.pid, a.usename::text AS user, a.datname::text AS database, l.locktype, l.mode, l.granted, \
     CASE WHEN l.granted THEN NULL ELSE NULLIF(array_to_string(pg_catalog.pg_blocking_pids(l.pid), ', '), '') END AS blocked_by, \
     CASE WHEN c.oid IS NOT NULL THEN n.nspname || '.' || c.relname ELSE l.relation::text END AS relation, \
     l.page, l.tuple, l.virtualxid, l.transactionid::text AS transactionid, l.virtualtransaction, l.fastpath, \
     l.waitstart, a.state, a.wait_event_type, a.wait_event, date_trunc('milliseconds', now() - a.query_start)::text AS query_duration, a.query \
     FROM pg_catalog.pg_locks l \
     LEFT JOIN pg_catalog.pg_stat_activity a ON a.pid = l.pid \
     LEFT JOIN pg_catalog.pg_class c ON c.oid = l.relation \
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
     WHERE l.pid IS DISTINCT FROM pg_catalog.pg_backend_pid() \
     ORDER BY l.granted, l.pid, l.locktype";

impl PgSession {
    pub(crate) async fn list_sessions_impl(&self) -> DbResult<SessionList> {
        let client = self.client()?;
        if self.is_cockroach() {
            let data = catalog::query_tabular(
                client,
                "SELECT session_id, node_id, user_name AS user, client_address, application_name, session_start, \
                 active_query_start, last_active_query, active_queries, num_txns_executed, status \
                 FROM crdb_internal.cluster_sessions ORDER BY session_start DESC",
                &[],
            )
            .await?;
            let id_column = data.column_index("session_id").unwrap_or(0);
            let query_column = data.column_index("active_queries");
            let state_column = data.column_index("status");
            return Ok(SessionList { data, id_column, query_column, state_column, blocked_by_column: None });
        }
        let data = catalog::query_tabular(client, SESSIONS_SQL, &[]).await?;
        Ok(SessionList {
            id_column: data.column_index("pid").unwrap_or(0),
            query_column: data.column_index("query"),
            state_column: data.column_index("state"),
            blocked_by_column: data.column_index("blocked_by"),
            data,
        })
    }

    pub(crate) async fn kill_session_impl(&self, id: &str, mode: KillMode) -> DbResult<()> {
        let id = id.trim();
        if self.is_cockroach() {
            let quoted = crate::catalog::quote_literal(id);
            let sql = match mode {
                KillMode::Terminate => format!("CANCEL SESSION {quoted}"),
                KillMode::CancelQuery => format!(
                    "CANCEL QUERIES (SELECT query_id FROM [SHOW CLUSTER QUERIES] WHERE session_id = {quoted})"
                ),
            };
            return self.batch(&sql).await;
        }
        let pid: i32 = id.parse().map_err(|_| DbError::config(format!("'{id}' is not a backend process id")))?;
        let func = match mode {
            KillMode::CancelQuery => "pg_catalog.pg_cancel_backend($1)",
            KillMode::Terminate => "pg_catalog.pg_terminate_backend($1)",
        };
        let row = self.q1(&format!("SELECT {func}"), &[P::Int4(pid)]).await?;
        match row {
            Some(r) if r.b(0)? => Ok(()),
            _ => Err(DbError::query(format!(
                "Session {pid} was not signalled (it no longer exists or you lack the privilege)"
            ))),
        }
    }

    pub(crate) async fn list_locks_impl(&self) -> DbResult<LockList> {
        let client = self.client()?;
        if self.is_cockroach() {
            let data = catalog::query_tabular(
                client,
                "SELECT session_id, txn_id::text AS txn_id, database_name, schema_name, table_name, index_name, \
                 lock_strength, granted, contended, duration::text AS duration \
                 FROM crdb_internal.cluster_locks ORDER BY granted, table_name",
                &[],
            )
            .await?;
            return Ok(LockList {
                session_column: data.column_index("session_id").unwrap_or(0),
                blocked_by_column: None,
                granted_column: data.column_index("granted"),
                data,
            });
        }
        let sql = if self.pg_major() >= 14 { LOCKS_SQL.to_string() } else { LOCKS_SQL.replace("l.waitstart, ", "") };
        let data = catalog::query_tabular(client, &sql, &[]).await?;
        Ok(LockList {
            session_column: data.column_index("pid").unwrap_or(0),
            blocked_by_column: data.column_index("blocked_by"),
            granted_column: data.column_index("granted"),
            data,
        })
    }

    pub(crate) async fn server_metrics_impl(&self) -> DbResult<Vec<Metric>> {
        if self.is_cockroach() {
            let row = self
                .q1(
                    "SELECT (SELECT count(*) FROM crdb_internal.cluster_sessions)::float8, \
                     (SELECT count(*) FROM crdb_internal.cluster_queries)::float8, \
                     (SELECT count(*) FROM crdb_internal.cluster_transactions)::float8",
                    &[],
                )
                .await?;
            let mut out = Vec::new();
            if let Some(r) = row {
                let f = |i: usize| r.try_get::<_, Option<f64>>(i).ok().flatten().unwrap_or(0.0);
                out.push(Metric::gauge("Connections", "connections.total", "Sessions", f(0), ""));
                out.push(Metric::gauge("Connections", "connections.active", "Active queries", f(1), ""));
                out.push(Metric::gauge("Transactions", "xact.open", "Open transactions", f(2), ""));
            }
            return Ok(out);
        }
        let row = self
            .q1(
                "SELECT \
                 (SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE backend_type = 'client backend')::float8, \
                 (SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE state = 'active' AND backend_type = 'client backend')::float8, \
                 (SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE state = 'idle')::float8, \
                 (SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE state = 'idle in transaction')::float8, \
                 (SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE state = 'idle in transaction (aborted)')::float8, \
                 (SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE wait_event_type = 'Lock')::float8, \
                 current_setting('max_connections')::float8, \
                 sum(s.xact_commit)::float8, sum(s.xact_rollback)::float8, sum(s.blks_read)::float8, sum(s.blks_hit)::float8, \
                 sum(s.tup_returned)::float8, sum(s.tup_fetched)::float8, sum(s.tup_inserted)::float8, sum(s.tup_updated)::float8, \
                 sum(s.tup_deleted)::float8, sum(s.temp_files)::float8, sum(s.temp_bytes)::float8, sum(s.deadlocks)::float8, \
                 sum(s.conflicts)::float8, \
                 pg_catalog.pg_database_size(current_database())::float8, \
                 (SELECT count(*) FROM pg_catalog.pg_locks)::float8, \
                 (SELECT count(*) FROM pg_catalog.pg_locks WHERE NOT granted)::float8, \
                 (SELECT s2.blks_hit::float8 / NULLIF(s2.blks_hit + s2.blks_read, 0) * 100 FROM pg_catalog.pg_stat_database s2 \
                  WHERE s2.datname = current_database()) \
                 FROM pg_catalog.pg_stat_database s",
                &[],
            )
            .await?
            .ok_or_else(|| DbError::internal("no statistics returned"))?;
        let f = |i: usize| row.try_get::<_, Option<f64>>(i).ok().flatten().unwrap_or(0.0);
        let (hit, read) = (f(10), f(9));
        let ratio = if hit + read > 0.0 { hit / (hit + read) * 100.0 } else { 100.0 };
        let db_ratio = row.try_get::<_, Option<f64>>(23).ok().flatten();
        Ok(vec![
            Metric::gauge("Connections", "connections.total", "Client connections", f(0), ""),
            Metric::gauge("Connections", "connections.active", "Active", f(1), ""),
            Metric::gauge("Connections", "connections.idle", "Idle", f(2), ""),
            Metric::gauge("Connections", "connections.idle_in_transaction", "Idle in transaction", f(3), ""),
            Metric::gauge("Connections", "connections.idle_in_transaction_aborted", "Idle in transaction (aborted)", f(4), ""),
            Metric::gauge("Connections", "connections.waiting", "Waiting for locks", f(5), ""),
            Metric::gauge("Connections", "connections.max", "Max connections", f(6), ""),
            Metric::counter("Transactions", "xact.commit", "Commits", f(7), ""),
            Metric::counter("Transactions", "xact.rollback", "Rollbacks", f(8), ""),
            Metric::counter("Transactions", "xact.deadlocks", "Deadlocks", f(18), ""),
            Metric::counter("Transactions", "xact.conflicts", "Recovery conflicts", f(19), ""),
            Metric::counter("I/O", "io.blks_read", "Blocks read", read, "blocks"),
            Metric::counter("I/O", "io.blks_hit", "Blocks hit", hit, "blocks"),
            Metric::gauge("I/O", "io.cache_hit_ratio", "Cache hit ratio (server)", ratio, "%"),
            Metric::gauge("I/O", "io.cache_hit_ratio_db", "Cache hit ratio (database)", db_ratio.unwrap_or(100.0), "%"),
            Metric::counter("Tuples", "tuples.returned", "Returned", f(11), "rows"),
            Metric::counter("Tuples", "tuples.fetched", "Fetched", f(12), "rows"),
            Metric::counter("Tuples", "tuples.inserted", "Inserted", f(13), "rows"),
            Metric::counter("Tuples", "tuples.updated", "Updated", f(14), "rows"),
            Metric::counter("Tuples", "tuples.deleted", "Deleted", f(15), "rows"),
            Metric::counter("Temp", "temp.files", "Temporary files", f(16), ""),
            Metric::counter("Temp", "temp.bytes", "Temporary bytes", f(17), "bytes"),
            Metric::gauge("Size", "db.size", "Database size", f(20), "bytes"),
            Metric::gauge("Locks", "locks.total", "Locks", f(21), ""),
            Metric::gauge("Locks", "locks.waiting", "Waiting locks", f(22), ""),
        ])
    }
}
