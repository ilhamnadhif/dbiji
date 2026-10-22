//! EXPLAIN, session / lock manager, metrics and dialect generated SQL
//! against real servers.

mod common;

use common::*;
use dbiji_core::admin::KillMode;
use dbiji_core::meta::ObjectRef;
use dbiji_core::{Dialect, ErrorKind, Session, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[tokio::test]
async fn explain_plans() {
    for srv in servers() {
        let db = TestDb::create(&srv).await;
        let mut s = db.session().await;
        exec(
            &mut s,
            "CREATE TABLE ea (id INT PRIMARY KEY, v VARCHAR(10), x INT NOT NULL)",
        )
        .await;
        exec(
            &mut s,
            "CREATE TABLE eb (id INT PRIMARY KEY, a_id INT, KEY (a_id))",
        )
        .await;
        exec(&mut s, "INSERT INTO ea VALUES (1,'x',1),(2,'z',2),(3,NULL,3); INSERT INTO eb VALUES (1,1),(2,1),(3,2)").await;
        let sql = "SELECT ea.v, COUNT(*) FROM ea JOIN eb ON eb.a_id = ea.id WHERE ea.x > 0 GROUP BY ea.v ORDER BY 2 DESC";
        let plan = s.explain(sql, false).await.unwrap();
        assert!(!plan.analyzed);
        assert!(
            plan.raw.as_deref().unwrap_or("").contains("query_block"),
            "[{}] {:?}",
            srv.name,
            plan.raw
        );
        assert!(
            !plan.roots.is_empty() && plan.roots[0].count() >= 3,
            "[{}] {:#?}",
            srv.name,
            plan.roots
        );
        let mut tables = Vec::new();
        plan.roots[0].walk(0, &mut |n, _| {
            if let Some(r) = &n.relation {
                tables.push(r.clone());
            }
        });
        assert!(
            tables.contains(&"ea".to_string()) && tables.contains(&"eb".to_string()),
            "{tables:?}"
        );
        if srv.is_mariadb() || srv.is_mysql57(&s) {
            assert!(!plan.text.is_empty());
        } else {
            assert!(
                plan.text.trim_start().starts_with("->"),
                "[mysql] TREE text: {}",
                plan.text
            );
        }
        // EXPLAIN ANALYZE came with MySQL 8.0.18: refused with that reason before.
        if srv.is_mysql57(&s) {
            let e = s.explain(sql, true).await.unwrap_err();
            assert_eq!(e.kind, ErrorKind::Unsupported, "{}", e.full_text());
        } else {
            let analyzed = s.explain(sql, true).await.unwrap();
            assert!(analyzed.analyzed);
            let mut actual = 0;
            analyzed.roots[0].walk(0, &mut |n, _| {
                if n.actual_rows.is_some() || n.actual_total_ms.is_some() {
                    actual += 1;
                }
            });
            assert!(
                actual > 0,
                "[{}] no actual values: {:#?}",
                srv.name,
                analyzed.roots
            );
            assert!(analyzed.execution_ms.is_some());
            // ANALYZE of a data change is rolled back (multi-table form works on both)
            let dml = "UPDATE ea a JOIN eb b ON b.a_id = a.id SET a.v = 'changed'";
            let p = s.explain(dml, true).await.unwrap();
            assert!(p.analyzed);
            assert_eq!(
                query(&mut s, "SELECT COUNT(*) FROM ea WHERE v = 'changed'").await[0][0],
                Value::Int(0),
                "[{}]",
                srv.name
            );
            assert!(!s.in_transaction());
        }
        // plain EXPLAIN of DML
        let p = s
            .explain("DELETE FROM ea WHERE id = 1", false)
            .await
            .unwrap();
        assert!(!p.roots.is_empty());
        assert_eq!(
            query(&mut s, "SELECT COUNT(*) FROM ea").await[0][0],
            Value::Int(3)
        );
        // no tables
        let p = s.explain("SELECT 1", false).await.unwrap();
        assert!(!p.roots.is_empty());
        let e = s.explain("SELEC 1", false).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Syntax);
        s.close().await.unwrap();
        db.drop_now().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_locks_and_kill() {
    for srv in servers() {
        let db = TestDb::create(&srv).await;
        let mut a = db.session().await;
        let mut b = db.session().await;
        let mut c = db.session().await;
        let a_id = a.server_info().session_id.clone().unwrap();
        let b_id = b.server_info().session_id.clone().unwrap();
        exec(
            &mut a,
            "CREATE TABLE lk (id INT PRIMARY KEY, v INT) ENGINE=InnoDB",
        )
        .await;
        exec(&mut a, "INSERT INTO lk VALUES (1, 0), (2, 0)").await;
        exec(&mut a, "START TRANSACTION").await;
        exec(&mut a, "UPDATE lk SET v = 1 WHERE id = 1").await;
        let waiter = tokio::spawn(async move {
            let r = run(&mut b, "UPDATE lk SET v = 2 WHERE id = 1", &[]).await;
            (b, r)
        });

        // the lock wait becomes visible
        let started = Instant::now();
        let mut seen_lock = false;
        let mut seen_session = false;
        while started.elapsed() < Duration::from_secs(10) && !(seen_lock && seen_session) {
            let locks = c.list_locks().await.unwrap();
            for row in &locks.data.rows {
                let sid = row[locks.session_column].to_text().into_owned();
                let blocked_by = locks
                    .blocked_by_column
                    .map(|i| row[i].to_text().into_owned())
                    .unwrap_or_default();
                if sid == b_id && blocked_by.split(',').any(|x| x == a_id) {
                    seen_lock = true;
                    if let Some(g) = locks.granted_column {
                        assert_eq!(
                            row[g].as_bool(),
                            Some(false),
                            "[{}] waiting lock reported as granted",
                            srv.name
                        );
                    }
                }
            }
            let sessions = c.list_sessions().await.unwrap();
            for row in &sessions.data.rows {
                if row[sessions.id_column].to_text() == b_id {
                    if let Some(i) = sessions.blocked_by_column
                        && row[i].to_text().split(',').any(|x| x == a_id)
                    {
                        seen_session = true;
                    }
                    if let Some(q) = sessions.query_column {
                        assert!(row[q].to_text().contains("UPDATE lk"));
                    }
                }
            }
            if !(seen_lock && seen_session) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        assert!(
            seen_lock,
            "[{}] lock wait not reported by list_locks",
            srv.name
        );
        assert!(
            seen_session,
            "[{}] blocking session not reported by list_sessions",
            srv.name
        );
        // all sessions are listed, including our own
        let sessions = c.list_sessions().await.unwrap();
        let own = c.server_info().session_id.clone().unwrap();
        assert!(
            sessions
                .data
                .rows
                .iter()
                .any(|r| r[sessions.id_column].to_text() == own)
        );

        // cancel the waiting statement
        c.kill_session(&b_id, KillMode::CancelQuery).await.unwrap();
        let (mut b, r) = tokio::time::timeout(Duration::from_secs(10), waiter)
            .await
            .expect("waiter finished")
            .unwrap();
        let e = r.expect_err("interrupted");
        assert_eq!(
            e.kind,
            ErrorKind::Cancelled,
            "[{}] {}",
            srv.name,
            e.full_text()
        );
        b.ping().await.unwrap();
        exec(&mut a, "ROLLBACK").await;
        // terminate a session
        c.kill_session(&b_id, KillMode::Terminate).await.unwrap();
        assert!(b.ping().await.is_err());
        assert!(
            c.kill_session("12; DROP TABLE x", KillMode::Terminate)
                .await
                .is_err()
        );

        // metrics
        let metrics = c.server_metrics().await.unwrap();
        let get = |k: &str| metrics.iter().find(|m| m.key == k);
        assert!(get("server.uptime").is_some_and(|m| m.value > 0.0));
        assert!(get("connections.connected").is_some_and(|m| m.value >= 2.0));
        assert!(get("queries.questions").is_some());
        assert!(get("network.sent").is_some_and(|m| m.unit == "bytes"));
        assert!(
            get("innodb.buffer_pool_hit_ratio").is_some_and(|m| (0.0..=100.0).contains(&m.value))
        );
        a.close().await.unwrap();
        c.close().await.unwrap();
        db.drop_now().await;
    }
}

#[tokio::test]
async fn dialect_generated_sql() {
    for srv in servers() {
        let db = TestDb::create(&srv).await;
        let mut s = db.session().await;
        let d = s.mysql_dialect();
        // reserved identifiers are quoted
        exec(
            &mut s,
            "CREATE TABLE `order` (`key` INT PRIMARY KEY, `select` VARCHAR(20), qty INT)",
        )
        .await;
        let t = ObjectRef::table(Some(db.name.clone()), "order");
        let cols = vec![
            d.quote_ident("key"),
            d.quote_ident("select"),
            d.quote_ident("qty"),
        ];
        let up = d
            .upsert(
                &d.qualified_ref(&t),
                &cols,
                &cols[..1],
                &["?".into(), "?".into(), "?".into()],
            )
            .unwrap();
        run(
            &mut s,
            &up,
            &[Value::Int(1), Value::text("a"), Value::Int(1)],
        )
        .await
        .unwrap();
        run(
            &mut s,
            &up,
            &[Value::Int(1), Value::text("b"), Value::Int(2)],
        )
        .await
        .unwrap();
        run(
            &mut s,
            &up,
            &[Value::Int(2), Value::text("c"), Value::Int(3)],
        )
        .await
        .unwrap();
        let rows = query(
            &mut s,
            &d.select_table(&t, None, None, Some("`key`"), Some(10)),
        )
        .await;
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::text("b"), Value::Int(2)],
                vec![Value::Int(2), Value::text("c"), Value::Int(3)]
            ]
        );
        // limit / count wrappers
        let base = "SELECT `key` FROM `order` ORDER BY `key`";
        assert_eq!(
            query(&mut s, &d.limit_query(base, 1, 1)).await,
            vec![vec![Value::Int(2)]]
        );
        assert_eq!(
            query(&mut s, &d.limit_query(&format!("{base} LIMIT 1"), 5, 0))
                .await
                .len(),
            1
        );
        assert_eq!(
            query(&mut s, &d.count_query(base)).await[0][0],
            Value::Int(2)
        );
        // literals round trip
        let values = [
            Value::text("it's a \\ path with \"quotes\" and \0 nul"),
            Value::bytes(vec![0u8, 39, 92, 255]),
            Value::Bool(true),
            Value::decimal("-1.50"),
            Value::Float(0.25),
            Value::Date(chrono::NaiveDate::from_ymd_opt(2024, 5, 6).unwrap()),
        ];
        let sel = values
            .iter()
            .map(|v| d.literal(v, None))
            .collect::<Vec<_>>()
            .join(", ");
        let rows = query(&mut s, &format!("SELECT {sel}")).await;
        assert_eq!(rows[0][0], values[0]);
        assert_eq!(rows[0][1], values[1]);
        assert_eq!(rows[0][2].as_i64(), Some(1));
        assert_eq!(rows[0][3], values[3]);
        assert_eq!(rows[0][4].as_f64(), Some(0.25));
        assert_eq!(rows[0][5].to_text(), "2024-05-06");
        // maintenance statements
        let targets = vec![t.clone()];
        for op in ["analyze", "optimize", "check", "repair", "flush"] {
            let mut opts = BTreeMap::new();
            if op == "check" {
                opts.insert("quick".to_string(), true);
            }
            for stmt in d.maintenance_sql(op, &targets, &opts) {
                run(&mut s, &stmt, &[])
                    .await
                    .unwrap_or_else(|e| panic!("[{}] {stmt}: {}", srv.name, e.full_text()));
            }
        }
        // password change for a temporary user
        let user = format!("dbiji_u_{}", suffix());
        exec(
            &mut s,
            &format!("CREATE USER '{user}'@'%' IDENTIFIED BY 'old-Pw-1'"),
        )
        .await;
        let mut up = srv.params.clone();
        up.user = Some(user.clone());
        up.password = Some("old-Pw-1".into());
        up.database = None;
        let res = async {
            let mut us = srv.try_connect(&up).await.map_err(|e| e.full_text())?;
            let sql = d.change_password_sql(None, "new-Pw-2 'x'").unwrap();
            dbiji_core::driver::execute_update(&mut us, &sql, &[])
                .await
                .map_err(|e| e.full_text())?;
            us.close().await.map_err(|e| e.full_text())?;
            up.password = Some("new-Pw-2 'x'".into());
            let mut us = srv.try_connect(&up).await.map_err(|e| e.full_text())?;
            us.close().await.map_err(|e| e.full_text())?;
            Ok::<(), String>(())
        }
        .await;
        exec(&mut s, &format!("DROP USER '{user}'@'%'")).await;
        res.unwrap_or_else(|e| panic!("[{}] password change: {e}", srv.name));
        s.close().await.unwrap();
        db.drop_now().await;
    }
}
