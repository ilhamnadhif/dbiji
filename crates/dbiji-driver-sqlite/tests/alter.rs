//! `alter_table` and the single-change DDL methods, executed against real
//! databases: native statements where possible, the 12 step rebuild
//! otherwise, with data, indexes, triggers, views and foreign keys preserved.

mod common;

use common::*;
use dbiji_core::ddl::*;
use dbiji_core::driver::{execute_update, query_all};
use dbiji_core::meta::ObjectRef;
use dbiji_core::{Dialect, Session, Value};
use dbiji_driver_sqlite::{SqliteDialect, SqliteSession};

fn ddl() -> SqliteDialect {
    SqliteDialect::new()
}

async fn def(s: &mut SqliteSession, schema: &str, name: &str) -> TableDef {
    s.table_definition(&ObjectRef::table(Some(schema.into()), name))
        .await
        .unwrap_or_else(|e| panic!("{name}: {e:?}"))
}

/// Execute statements one by one; returns the rows of row-returning statements.
async fn apply(s: &mut SqliteSession, stmts: &[String]) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for stmt in stmts {
        let (cols, rows) = query_all(s, stmt, &[])
            .await
            .unwrap_or_else(|e| panic!("{stmt}: {e:?}\nscript: {stmts:#?}"));
        if !cols.is_empty() {
            out.extend(rows);
        }
    }
    out
}

fn is_rebuild(stmts: &[String]) -> bool {
    stmts.iter().any(|s| s == "PRAGMA foreign_keys = OFF")
}

/// name, type, nullable, default, auto increment, generated, collation
type ColumnShape = (
    String,
    String,
    bool,
    Option<String>,
    bool,
    Option<String>,
    Option<String>,
);
/// name, columns, referenced table, referenced columns, on update, on delete, deferrable
type FkShape = (
    Option<String>,
    Vec<String>,
    String,
    Vec<String>,
    FkAction,
    FkAction,
    bool,
);

/// Comparable structure of a table (ignores stored SQL texts and statistics).
#[derive(Debug, PartialEq)]
struct Shape {
    name: String,
    columns: Vec<ColumnShape>,
    pk: Option<KeyDef>,
    uniques: Vec<KeyDef>,
    checks: Vec<(Option<String>, String)>,
    fks: Vec<FkShape>,
    indexes: Vec<(String, bool, Vec<IndexColumn>, Option<String>)>,
    triggers: Vec<String>,
    options: Vec<(String, String)>,
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn shape(t: &TableDef) -> Shape {
    let mut indexes: Vec<_> = t
        .indexes
        .iter()
        .filter(|i| !i.name.starts_with("sqlite_autoindex_"))
        .map(|i| {
            (
                i.name.clone(),
                i.unique,
                i.columns.clone(),
                i.predicate.as_deref().map(squash),
            )
        })
        .collect();
    indexes.sort_by(|a, b| a.0.cmp(&b.0));
    let mut triggers: Vec<String> = t.triggers.iter().map(|t| t.name.clone()).collect();
    triggers.sort();
    Shape {
        name: t.name.clone(),
        columns: t
            .columns
            .iter()
            .map(|c| {
                (
                    c.name.clone(),
                    c.data_type.to_uppercase(),
                    c.nullable,
                    c.default.clone(),
                    c.auto_increment,
                    c.generated.as_deref().map(squash),
                    c.collation.as_deref().map(str::to_uppercase),
                )
            })
            .collect(),
        pk: t.primary_key.clone(),
        uniques: t.unique_keys.clone(),
        checks: t
            .checks
            .iter()
            .map(|c| (c.name.clone(), squash(&c.expression)))
            .collect(),
        fks: t
            .foreign_keys
            .iter()
            .map(|f| {
                (
                    f.name.clone(),
                    f.columns.clone(),
                    f.ref_table.clone(),
                    f.ref_columns.clone(),
                    f.on_update,
                    f.on_delete,
                    f.deferrable,
                )
            })
            .collect(),
        indexes,
        triggers,
        options: t
            .options
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

const SHOP: &str = r#"
CREATE TABLE products (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  price INT DEFAULT 0,
  sku TEXT UNIQUE,
  note TEXT
);
CREATE INDEX ix_products_name ON products (name);
CREATE TABLE price_log (product_id INT, old_price, new_price);
CREATE TRIGGER trg_price AFTER UPDATE OF price ON products BEGIN
  INSERT INTO price_log VALUES (NEW.id, OLD.price, NEW.price);
END;
CREATE VIEW v_products AS SELECT id, name, price FROM products;
CREATE TABLE lines (id INTEGER PRIMARY KEY, product_id INT NOT NULL REFERENCES products (id) ON DELETE CASCADE, qty INT);
INSERT INTO products (name, price, sku, note) VALUES ('apple', 3, 'A1', 'red'), ('pear', 4, 'P1', NULL), ('plum', 5, 'P2', 'blue');
INSERT INTO lines (product_id, qty) VALUES (1, 10), (2, 20), (3, 30);
"#;

async fn shop() -> SqliteSession {
    let mut s = memory().await;
    exec(&mut s, SHOP).await;
    s
}

#[tokio::test]
async fn type_change_rebuild_preserves_everything() {
    let mut s = shop().await;
    let from = def(&mut s, "main", "products").await;
    let mut to = from.clone();
    to.columns
        .iter_mut()
        .find(|c| c.name == "price")
        .unwrap()
        .data_type = "REAL".into();
    to.checks.push(CheckDef {
        name: Some("ck_price".into()),
        expression: "price >= 0".into(),
    });
    let stmts = ddl().ddl().alter_table(&from, &to, &[]);
    assert!(is_rebuild(&stmts), "{stmts:#?}");
    let violations = apply(&mut s, &stmts).await;
    assert!(violations.is_empty(), "{violations:?}");
    let after = def(&mut s, "main", "products").await;
    assert_eq!(shape(&after), shape(&to));
    // data, ids and affinity
    let rows = rows(
        &mut s,
        "SELECT id, name, price, typeof(price), sku FROM products ORDER BY id",
    )
    .await;
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[2],
        vec![
            Value::Int(3),
            Value::text("plum"),
            Value::Float(5.0),
            Value::text("real"),
            Value::text("P2")
        ]
    );
    // trigger, view, child foreign key and the unique constraint still work
    exec(&mut s, "UPDATE products SET price = 9 WHERE id = 1").await;
    assert_eq!(int(&mut s, "SELECT count(*) FROM price_log").await, 1);
    assert_eq!(int(&mut s, "SELECT count(*) FROM v_products").await, 3);
    assert!(
        execute_update(
            &mut s,
            "INSERT INTO products (name, sku) VALUES ('x', 'A1')",
            &[]
        )
        .await
        .is_err()
    );
    assert!(
        execute_update(&mut s, "UPDATE products SET price = -1 WHERE id = 2", &[])
            .await
            .is_err(),
        "new CHECK enforced"
    );
    exec(&mut s, "DELETE FROM products WHERE id = 3").await;
    assert_eq!(
        int(&mut s, "SELECT count(*) FROM lines").await,
        2,
        "ON DELETE CASCADE from the child table"
    );
    assert_eq!(int(&mut s, "PRAGMA foreign_keys").await, 1);
    assert!(rows_of_check(&mut s).await.is_empty());
    assert!(!s.in_transaction());
}

async fn rows_of_check(s: &mut SqliteSession) -> Vec<Vec<Value>> {
    rows(s, "PRAGMA foreign_key_check").await
}

#[tokio::test]
async fn rename_with_rebuild_updates_views_and_triggers() {
    let mut s = shop().await;
    let from = def(&mut s, "main", "products").await;
    let mut to = from.clone();
    to.name = "items".into();
    {
        let name = to.columns.iter_mut().find(|c| c.name == "name").unwrap();
        name.name = "title".into();
        name.collation = Some("NOCASE".into());
    }
    {
        let price = to.columns.iter_mut().find(|c| c.name == "price").unwrap();
        price.name = "cost".into();
        price.data_type = "NUMERIC(8,2)".into();
    }
    let stmts = ddl().ddl().alter_table(
        &from,
        &to,
        &[
            ("name".into(), "title".into()),
            ("price".into(), "cost".into()),
        ],
    );
    assert!(is_rebuild(&stmts));
    apply(&mut s, &stmts).await;
    let after = def(&mut s, "main", "items").await;
    let names: Vec<&str> = after.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["id", "title", "cost", "sku", "note"]);
    assert_eq!(
        after.column("title").unwrap().collation.as_deref(),
        Some("NOCASE")
    );
    assert_eq!(after.column("cost").unwrap().data_type, "NUMERIC(8,2)");
    // the index on `name` followed the rename
    let ix = after
        .indexes
        .iter()
        .find(|i| i.name == "ix_products_name")
        .unwrap();
    assert_eq!(ix.columns[0].name, "title");
    // the view and the trigger were rewritten by SQLite's native renames
    assert_eq!(
        rows(&mut s, "SELECT * FROM v_products ORDER BY id")
            .await
            .len(),
        3
    );
    let view_sql = scalar(
        &mut s,
        "SELECT sql FROM sqlite_schema WHERE name = 'v_products'",
    )
    .await;
    assert!(
        view_sql.as_str().unwrap().contains("title")
            && view_sql.as_str().unwrap().contains("items"),
        "{view_sql:?}"
    );
    exec(&mut s, "UPDATE items SET cost = 7 WHERE id = 2").await;
    assert_eq!(int(&mut s, "SELECT count(*) FROM price_log").await, 1);
    // the child foreign key follows the table rename
    let lines = def(&mut s, "main", "lines").await;
    assert_eq!(lines.foreign_keys[0].ref_table, "items");
    assert_eq!(
        rows(&mut s, "SELECT title FROM items WHERE title = 'APPLE'")
            .await
            .len(),
        1,
        "NOCASE applied"
    );
}

#[tokio::test]
async fn native_changes_avoid_a_rebuild() {
    let mut s = shop().await;
    let from = def(&mut s, "main", "products").await;
    let mut to = from.clone();
    to.name = "goods".into();
    to.columns
        .iter_mut()
        .find(|c| c.name == "note")
        .unwrap()
        .name = "remark".into();
    let mut stock = ColumnDef::new("stock", "INTEGER").not_null();
    stock.default = Some("0".into());
    to.columns.push(stock);
    to.columns.push(ColumnDef::new("vat", "REAL"));
    to.indexes.push(IndexDef {
        name: "ix_goods_price".into(),
        columns: vec![IndexColumn {
            name: "price".into(),
            is_expression: false,
            descending: true,
        }],
        predicate: Some("price > 0".into()),
        ..Default::default()
    });
    let stmts = ddl()
        .ddl()
        .alter_table(&from, &to, &[("note".into(), "remark".into())]);
    assert!(!is_rebuild(&stmts), "{stmts:#?}");
    assert_eq!(stmts.first().map(String::as_str), Some("BEGIN"));
    apply(&mut s, &stmts).await;
    let after = def(&mut s, "main", "goods").await;
    assert_eq!(shape(&after), shape(&to));
    assert_eq!(
        rows(&mut s, "SELECT remark, stock, vat FROM goods WHERE id = 1").await,
        vec![vec![Value::text("red"), Value::Int(0), Value::Null]]
    );

    // dropping a column that only an index uses
    let from = after;
    let mut to = from.clone();
    to.columns.retain(|c| c.name != "vat" && c.name != "price");
    to.indexes.retain(|i| i.name != "ix_goods_price");
    to.triggers.clear(); // the trigger watches `price`
    let stmts = ddl().ddl().alter_table(&from, &to, &[]);
    assert!(!is_rebuild(&stmts), "{stmts:#?}");
    assert!(stmts.iter().any(|s| s == "DROP TRIGGER main.trg_price"));
    assert!(stmts.iter().any(|s| s.ends_with("DROP COLUMN price")));
    // the view selects `price`: SQLite refuses to drop it while the view exists
    exec(&mut s, "DROP VIEW v_products").await;
    apply(&mut s, &stmts).await;
    let after = def(&mut s, "main", "goods").await;
    assert_eq!(shape(&after), shape(&to));
    assert_eq!(int(&mut s, "SELECT count(*) FROM goods").await, 3);
}

#[tokio::test]
async fn column_order_and_removed_keys() {
    let mut s = shop().await;
    let from = def(&mut s, "main", "products").await;
    // a new column in the middle
    let mut to = from.clone();
    let mut brand = ColumnDef::new("brand", "TEXT");
    brand.default = Some("'generic'".into());
    to.columns.insert(2, brand);
    // and the UNIQUE constraint removed
    to.unique_keys.clear();
    let stmts = ddl().ddl().alter_table(&from, &to, &[]);
    assert!(is_rebuild(&stmts));
    apply(&mut s, &stmts).await;
    let after = def(&mut s, "main", "products").await;
    assert_eq!(shape(&after), shape(&to));
    assert_eq!(after.columns[2].name, "brand");
    assert_eq!(
        scalar(&mut s, "SELECT brand FROM products WHERE id = 2").await,
        Value::text("generic")
    );
    exec(
        &mut s,
        "INSERT INTO products (name, sku) VALUES ('dup', 'A1')",
    )
    .await;

    // dropping a primary key column rebuilds; the rowid table keeps its rowids
    let from = def(&mut s, "main", "lines").await;
    let mut to = from.clone();
    to.columns.retain(|c| c.name != "id");
    to.primary_key = None;
    let stmts = ddl().ddl().alter_table(&from, &to, &[]);
    assert!(is_rebuild(&stmts));
    apply(&mut s, &stmts).await;
    assert_eq!(shape(&def(&mut s, "main", "lines").await), shape(&to));
    assert_eq!(
        rows(&mut s, "SELECT rowid, qty FROM lines ORDER BY rowid").await[2],
        vec![Value::Int(3), Value::Int(30)]
    );
}

#[tokio::test]
async fn rowids_and_autoincrement_counters_survive() {
    let mut s = memory().await;
    exec(
        &mut s,
        "CREATE TABLE plain (a TEXT, b INT);
         INSERT INTO plain VALUES ('x', 1), ('y', 2), ('z', 3), ('w', 4);
         DELETE FROM plain WHERE b IN (1, 3);
         CREATE TABLE auto (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT);
         INSERT INTO auto (v) VALUES ('a'), ('b'), ('c');
         DELETE FROM auto WHERE id = 3;",
    )
    .await;
    let from = def(&mut s, "main", "plain").await;
    let mut to = from.clone();
    to.columns[1].data_type = "TEXT".into();
    apply(&mut s, &ddl().ddl().alter_table(&from, &to, &[])).await;
    assert_eq!(
        rows(&mut s, "SELECT rowid, a, b FROM plain ORDER BY rowid").await,
        vec![
            vec![Value::Int(2), Value::text("y"), Value::text("2")],
            vec![Value::Int(4), Value::text("w"), Value::text("4")]
        ]
    );

    let from = def(&mut s, "main", "auto").await;
    assert!(from.column("id").unwrap().auto_increment);
    let mut to = from.clone();
    to.columns[1].nullable = false;
    let stmts = ddl().ddl().alter_table(&from, &to, &[]);
    assert!(is_rebuild(&stmts));
    apply(&mut s, &stmts).await;
    exec(&mut s, "INSERT INTO auto (v) VALUES ('d')").await;
    assert_eq!(
        scalar(&mut s, "SELECT max(id) FROM auto").await,
        Value::Int(4),
        "AUTOINCREMENT never reuses 3"
    );
    assert_eq!(shape(&def(&mut s, "main", "auto").await), shape(&to));
}

#[tokio::test]
async fn swapping_column_names() {
    let mut s = memory().await;
    exec(
        &mut s,
        "CREATE TABLE t (a INT, b TEXT); INSERT INTO t VALUES (1, 'one')",
    )
    .await;
    let from = def(&mut s, "main", "t").await;
    let mut to = from.clone();
    to.columns[0].name = "b".into();
    to.columns[1].name = "a".into();
    let stmts = ddl().ddl().alter_table(
        &from,
        &to,
        &[("a".into(), "b".into()), ("b".into(), "a".into())],
    );
    assert!(!is_rebuild(&stmts), "{stmts:#?}");
    apply(&mut s, &stmts).await;
    assert_eq!(
        rows(&mut s, "SELECT b, a FROM t").await,
        vec![vec![Value::Int(1), Value::text("one")]]
    );
    // case-only rename
    let from = def(&mut s, "main", "t").await;
    let mut to = from.clone();
    to.columns[0].name = "B".into();
    apply(
        &mut s,
        &ddl()
            .ddl()
            .alter_table(&from, &to, &[("b".into(), "B".into())]),
    )
    .await;
    assert_eq!(def(&mut s, "main", "t").await.columns[0].name, "B");
}

#[tokio::test]
async fn foreign_key_checks_and_failed_rebuilds() {
    let mut s = memory().await;
    exec(&mut s, "CREATE TABLE p (id INTEGER PRIMARY KEY); CREATE TABLE c (pid INT, v TEXT); INSERT INTO p VALUES (1); INSERT INTO c VALUES (1, 'ok'), (2, NULL)").await;
    let from = def(&mut s, "main", "c").await;
    // adding a foreign key violated by existing rows: reported by foreign_key_check
    let mut to = from.clone();
    to.foreign_keys.push(ForeignKeyDef {
        columns: vec!["pid".into()],
        ref_table: "p".into(),
        ref_columns: vec!["id".into()],
        ..Default::default()
    });
    let violations = apply(&mut s, &ddl().ddl().alter_table(&from, &to, &[])).await;
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert_eq!(violations[0][0], Value::text("c"));
    assert_eq!(def(&mut s, "main", "c").await.foreign_keys.len(), 1);

    // a NOT NULL that existing rows violate fails inside the transaction
    let from = def(&mut s, "main", "c").await;
    let mut to = from.clone();
    to.columns[1].nullable = false;
    let stmts = ddl().ddl().alter_table(&from, &to, &[]);
    let mut failed = false;
    for stmt in &stmts {
        if execute_update(&mut s, stmt, &[]).await.is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed);
    assert!(s.in_transaction());
    exec(&mut s, "ROLLBACK; PRAGMA foreign_keys = ON").await;
    assert_eq!(
        int(&mut s, "SELECT count(*) FROM c").await,
        2,
        "the original table is intact"
    );
    assert!(def(&mut s, "main", "c").await.columns[1].nullable);
}

#[tokio::test]
async fn single_change_methods() {
    let mut s = memory().await;
    exec(&mut s, "CREATE TABLE p (id INTEGER PRIMARY KEY, code TEXT); CREATE TABLE t (a INT, b TEXT, c REAL); INSERT INTO t VALUES (1, 'x', 1.5), (2, 'y', 2.5); INSERT INTO p VALUES (1, 'p1'), (2, 'p2')").await;
    let d = ddl();
    let g = d.ddl();
    let t = def(&mut s, "main", "t").await;

    let col = ColumnDef::new("d", "TEXT").default_value("'n/a'");
    let stmts = g.add_column(&t, &col);
    assert_eq!(
        stmts,
        vec!["ALTER TABLE main.t ADD COLUMN d TEXT DEFAULT 'n/a'"]
    );
    apply(&mut s, &stmts).await;
    let t = def(&mut s, "main", "t").await;
    // a column with a non-constant default needs a rebuild
    let created = ColumnDef::new("created", "TEXT").default_value("CURRENT_TIMESTAMP");
    let stmts = g.add_column(&t, &created);
    assert!(is_rebuild(&stmts));
    apply(&mut s, &stmts).await;
    let t = def(&mut s, "main", "t").await;
    assert!(t.column("created").is_some());

    exec(
        &mut s,
        &g.add_primary_key(
            &t,
            &KeyDef {
                name: Some("pk_t".into()),
                columns: vec!["a".into()],
            },
        ),
    )
    .await;
    let t = def(&mut s, "main", "t").await;
    assert_eq!(
        t.primary_key,
        Some(KeyDef {
            name: Some("pk_t".into()),
            columns: vec!["a".into()]
        })
    );
    let check = CheckDef {
        name: Some("ck_c".into()),
        expression: "c > 0".into(),
    };
    exec(&mut s, &g.add_check(&t, &check)).await;
    let t = def(&mut s, "main", "t").await;
    assert_eq!(t.checks, vec![check.clone()]);
    exec(&mut s, &g.drop_check(&t, &check)).await;
    let t = def(&mut s, "main", "t").await;
    assert!(t.checks.is_empty());
    let fk = ForeignKeyDef {
        name: Some("fk_a".into()),
        columns: vec!["a".into()],
        ref_table: "p".into(),
        ref_columns: vec!["id".into()],
        on_delete: FkAction::Cascade,
        ..Default::default()
    };
    exec(&mut s, &g.add_foreign_key(&t, &fk)).await;
    let t = def(&mut s, "main", "t").await;
    assert_eq!(t.foreign_keys.len(), 1);
    assert_eq!(t.foreign_keys[0].on_delete, FkAction::Cascade);
    exec(&mut s, "DELETE FROM p WHERE id = 2").await;
    assert_eq!(
        int(&mut s, "SELECT count(*) FROM t").await,
        1,
        "the new FK cascades"
    );
    exec(&mut s, &g.drop_foreign_key(&t, &t.foreign_keys[0].clone())).await;
    let t = def(&mut s, "main", "t").await;
    assert!(t.foreign_keys.is_empty());
    let uq = KeyDef {
        name: Some("uq_b".into()),
        columns: vec!["b".into()],
    };
    exec(&mut s, &g.add_unique_key(&t, &uq)).await;
    assert!(
        execute_update(&mut s, "INSERT INTO t (a, b) VALUES (9, 'x')", &[])
            .await
            .is_err()
    );
    let t = def(&mut s, "main", "t").await;
    assert!(t.indexes.iter().any(|i| i.name == "uq_b" && i.unique));
    exec(&mut s, &g.drop_unique_key(&t, &uq)).await;
    exec(&mut s, "INSERT INTO t (a, b) VALUES (9, 'x')").await;
    let t = def(&mut s, "main", "t").await;
    exec(
        &mut s,
        &g.drop_primary_key(&t, t.primary_key.as_ref().unwrap()),
    )
    .await;
    let t = def(&mut s, "main", "t").await;
    assert!(t.primary_key.is_none());
    let script = g.drop_column(&t, "created");
    exec(&mut s, &script).await;
    let t = def(&mut s, "main", "t").await;
    assert!(t.column("created").is_none());
    assert_eq!(int(&mut s, "SELECT count(*) FROM t").await, 2);
    // index and trigger helpers
    let idx = IndexDef {
        name: "ix_lower_b".into(),
        columns: vec![IndexColumn {
            name: "lower(b)".into(),
            is_expression: true,
            descending: false,
        }],
        ..Default::default()
    };
    exec(&mut s, &g.create_index(&t, &idx)).await;
    let t = def(&mut s, "main", "t").await;
    let read = t.indexes.iter().find(|i| i.name == "ix_lower_b").unwrap();
    assert_eq!(read.columns, idx.columns);
    exec(&mut s, &g.drop_index(&t, read)).await;
    let trig = TriggerDef {
        name: "tr_t".into(),
        timing: "BEFORE".into(),
        events: vec!["DELETE".into()],
        body: Some("SELECT RAISE(ABORT, 'no deletes')".into()),
        ..Default::default()
    };
    apply(&mut s, &g.create_trigger(&t, &trig)).await;
    assert!(execute_update(&mut s, "DELETE FROM t", &[]).await.is_err());
    exec(&mut s, &g.drop_trigger(&t, &trig)).await;
    exec(&mut s, "DELETE FROM t").await;
}

#[tokio::test]
async fn generated_columns_and_table_options() {
    let mut s = memory().await;
    exec(
        &mut s,
        "CREATE TABLE g (a INT, b INT); INSERT INTO g VALUES (1, 2)",
    )
    .await;
    let from = def(&mut s, "main", "g").await;
    let mut to = from.clone();
    let mut sum = ColumnDef::new("sum", "INT");
    sum.generated = Some("a + b".into());
    to.columns.push(sum);
    // VIRTUAL generated columns can be added natively
    let stmts = ddl().ddl().alter_table(&from, &to, &[]);
    assert!(!is_rebuild(&stmts), "{stmts:#?}");
    apply(&mut s, &stmts).await;
    assert_eq!(int(&mut s, "SELECT sum FROM g").await, 3);
    // STORED ones need a rebuild, so do WITHOUT ROWID and STRICT
    let from = def(&mut s, "main", "g").await;
    let mut to = from.clone();
    to.options.insert("generated.sum".into(), "stored".into());
    to.primary_key = Some(KeyDef {
        name: None,
        columns: vec!["a".into()],
    });
    to.columns[0].nullable = false;
    to.options.insert("without_rowid".into(), "true".into());
    to.options.insert("strict".into(), "true".into());
    let stmts = ddl().ddl().alter_table(&from, &to, &[]);
    assert!(is_rebuild(&stmts));
    apply(&mut s, &stmts).await;
    let after = def(&mut s, "main", "g").await;
    assert_eq!(shape(&after), shape(&to));
    assert_eq!(int(&mut s, "SELECT sum FROM g").await, 3);
    assert!(
        execute_update(&mut s, "INSERT INTO g (a, b) VALUES ('x', 1)", &[])
            .await
            .is_err(),
        "STRICT enforced"
    );
}

#[tokio::test]
async fn attached_temp_and_virtual_tables() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = connect_with(
        ":memory:",
        &[("attach.aux", &file_in(dir.path(), "attached.db"))],
    )
    .await;
    exec(
        &mut s,
        "CREATE TABLE aux.t (id INTEGER PRIMARY KEY, v TEXT);
         CREATE INDEX aux.ix_v ON t (v);
         CREATE TABLE main.t (other INT);
         INSERT INTO aux.t (v) VALUES ('a'), ('b');
         CREATE TEMP TABLE tt (x INT, y INT);
         INSERT INTO tt VALUES (1, 2);
         CREATE VIRTUAL TABLE docs USING fts5(body);",
    )
    .await;
    // a table in an attached database (main has a table with the same name)
    let from = def(&mut s, "aux", "t").await;
    let mut to = from.clone();
    to.columns[1].data_type = "VARCHAR(10)".into();
    to.columns[1].nullable = false;
    apply(&mut s, &ddl().ddl().alter_table(&from, &to, &[])).await;
    let after = def(&mut s, "aux", "t").await;
    assert_eq!(shape(&after), shape(&to));
    assert_eq!(int(&mut s, "SELECT count(*) FROM aux.t").await, 2);
    assert_eq!(
        def(&mut s, "main", "t").await.columns.len(),
        1,
        "main.t untouched"
    );
    // temp tables
    let from = def(&mut s, "temp", "tt").await;
    let mut to = from.clone();
    to.columns.remove(1);
    to.columns[0].data_type = "TEXT".into();
    apply(&mut s, &ddl().ddl().alter_table(&from, &to, &[])).await;
    assert_eq!(shape(&def(&mut s, "temp", "tt").await), shape(&to));
    assert_eq!(
        scalar(&mut s, "SELECT x FROM temp.tt").await,
        Value::text("1")
    );
    // virtual tables can only be renamed
    let from = def(&mut s, "main", "docs").await;
    let mut to = from.clone();
    to.name = "documents".into();
    let stmts = ddl().ddl().alter_table(&from, &to, &[]);
    assert_eq!(stmts, vec!["ALTER TABLE main.docs RENAME TO documents"]);
    apply(&mut s, &stmts).await;
    exec(&mut s, "INSERT INTO documents VALUES ('hello world')").await;
    assert_eq!(
        int(
            &mut s,
            "SELECT count(*) FROM documents WHERE documents MATCH 'hello'"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn cross_database_type_mapping_creates_valid_tables() {
    let mut s = memory().await;
    let g = SqliteDialect::new();
    let mut t = TableDef::new(None, "migrated");
    for (name, ty, kind) in [
        ("id", "bigserial", dbiji_core::DataKind::Int),
        ("flag", "boolean", dbiji_core::DataKind::Bool),
        ("doc", "jsonb", dbiji_core::DataKind::Json),
        ("raw", "bytea", dbiji_core::DataKind::Binary),
        ("uid", "uuid", dbiji_core::DataKind::Uuid),
        ("amount", "numeric(12,2)", dbiji_core::DataKind::Decimal),
        ("tags", "text[]", dbiji_core::DataKind::Array),
        (
            "at",
            "timestamp with time zone",
            dbiji_core::DataKind::DateTimeTz,
        ),
    ] {
        let mut c = ColumnDef::new(
            name,
            g.ddl().map_type(dbiji_core::DbKind::Postgres, ty, kind),
        );
        c.kind = kind;
        t.columns.push(c);
    }
    t.columns[0].auto_increment = true;
    t.primary_key = Some(KeyDef {
        name: None,
        columns: vec!["id".into()],
    });
    apply(&mut s, &g.ddl().create_table(&t, true)).await;
    let read = def(&mut s, "main", "migrated").await;
    let types: Vec<&str> = read.columns.iter().map(|c| c.data_type.as_str()).collect();
    assert_eq!(
        types,
        vec![
            "INTEGER",
            "INTEGER",
            "TEXT",
            "BLOB",
            "TEXT",
            "NUMERIC(12,2)",
            "TEXT",
            "TEXT"
        ]
    );
    assert!(read.columns[0].auto_increment);
}
