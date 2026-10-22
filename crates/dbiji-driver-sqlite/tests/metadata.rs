//! Navigator listings, definitions, DDL, properties, statistics,
//! dependencies, search, snapshots and the DDL round trip.

mod common;

use common::*;
use dbiji_core::ddl::*;
use dbiji_core::meta::*;
use dbiji_core::{Dialect, Session};
use dbiji_driver_sqlite::ddl::options;
use dbiji_driver_sqlite::{SqliteDialect, SqliteSession};

const SCHEMA: &str = r#"
CREATE TABLE customers (
  id INTEGER PRIMARY KEY AUTOINCREMENT, -- customer id
  email TEXT NOT NULL COLLATE NOCASE,
  name TEXT DEFAULT 'anonymous',
  score REAL DEFAULT 0 CHECK (score >= 0),
  created DATETIME DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT uq_email UNIQUE (email)
);
CREATE TABLE orders (
  id INTEGER PRIMARY KEY,
  customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
  total NUMERIC(10,2) NOT NULL DEFAULT 0,
  status TEXT CHECK (status IN ('new', 'paid', 'shipped')),
  doubled REAL GENERATED ALWAYS AS (total * 2) STORED,
  note TEXT,
  CONSTRAINT ck_total CHECK (total >= 0)
);
CREATE TABLE order_items (
  order_id INTEGER NOT NULL,
  line INTEGER NOT NULL,
  sku TEXT NOT NULL,
  qty INTEGER NOT NULL DEFAULT 1,
  PRIMARY KEY (order_id, line),
  CONSTRAINT fk_order FOREIGN KEY (order_id) REFERENCES orders (id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED
) WITHOUT ROWID;
CREATE TABLE tags (tag TEXT PRIMARY KEY, weight INT) STRICT;
CREATE TABLE self_ref (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES self_ref);
CREATE INDEX ix_orders_status ON orders (status) WHERE status IS NOT NULL;
CREATE UNIQUE INDEX ix_customers_lower_name ON customers (lower(name), id DESC);
CREATE INDEX ix_items_sku ON order_items (sku COLLATE NOCASE);
CREATE VIEW v_order_totals (customer, total) AS
  SELECT c.email, sum(o.total) FROM customers c JOIN orders o ON o.customer_id = c.id GROUP BY c.email;
CREATE TRIGGER trg_orders_paid AFTER UPDATE OF status ON orders FOR EACH ROW WHEN NEW.status = 'paid'
BEGIN
  UPDATE customers SET score = score + 1 WHERE id = NEW.customer_id;
END;
CREATE VIRTUAL TABLE docs USING fts5(title, body);
INSERT INTO customers(email, name) VALUES ('a@x', 'Alice'), ('b@x', 'Bob');
INSERT INTO orders(customer_id, total, status) VALUES (1, 10, 'new'), (1, 5.5, 'paid'), (2, 7, NULL);
INSERT INTO order_items VALUES (1, 1, 'A', 2), (1, 2, 'B', 1);
INSERT INTO docs VALUES ('hello', 'world');
ANALYZE;
"#;

async fn sample() -> SqliteSession {
    let mut s = memory().await;
    exec(&mut s, SCHEMA).await;
    s
}

fn main_schema() -> SchemaRef {
    SchemaRef::schema("main")
}

fn table(name: &str) -> ObjectRef {
    ObjectRef::table(Some("main".into()), name)
}

fn names(list: &[ObjectInfo]) -> Vec<String> {
    list.iter().map(|o| o.obj.name.clone()).collect()
}

#[tokio::test]
async fn lists_schemas_and_objects() {
    let mut s = sample().await;
    let schemas = s.list_schemas().await.unwrap();
    assert_eq!(
        schemas.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        vec!["main", "temp"]
    );
    assert!(schemas[0].is_current && schemas[1].is_system);

    let tables = s
        .list_objects(&main_schema(), ObjectKind::Table)
        .await
        .unwrap();
    assert_eq!(
        names(&tables),
        vec![
            "customers",
            "docs",
            "order_items",
            "orders",
            "self_ref",
            "tags"
        ]
    );
    // Sizes from dbstat: every table takes at least one page (a virtual
    // table's rows are in its shadow tables).
    for t in &tables {
        let is_virtual = t
            .detail
            .as_deref()
            .is_some_and(|d| d.starts_with("virtual"));
        assert_eq!(t.size_bytes.is_some_and(|b| b > 0), !is_virtual, "{t:?}");
    }
    let by_name = |n: &str| tables.iter().find(|t| t.obj.name == n).unwrap().clone();
    assert_eq!(by_name("customers").row_estimate, Some(2));
    assert_eq!(
        by_name("docs").detail.as_deref(),
        Some("virtual table (fts5)")
    );
    assert_eq!(
        by_name("order_items").detail.as_deref(),
        Some("WITHOUT ROWID")
    );
    assert_eq!(by_name("tags").detail.as_deref(), Some("STRICT"));

    assert_eq!(
        names(
            &s.list_objects(&main_schema(), ObjectKind::View)
                .await
                .unwrap()
        ),
        vec!["v_order_totals"]
    );
    let indexes = s
        .list_objects(&main_schema(), ObjectKind::Index)
        .await
        .unwrap();
    assert_eq!(
        names(&indexes),
        vec![
            "ix_customers_lower_name",
            "ix_items_sku",
            "ix_orders_status"
        ]
    );
    assert_eq!(indexes[2].obj.table.as_deref(), Some("orders"));
    assert!(
        indexes[2].detail.as_deref().unwrap().contains("partial"),
        "{:?}",
        indexes[2].detail
    );
    let triggers = s
        .list_objects(&main_schema(), ObjectKind::Trigger)
        .await
        .unwrap();
    assert_eq!(names(&triggers), vec!["trg_orders_paid"]);
    assert_eq!(
        triggers[0].detail.as_deref(),
        Some("AFTER UPDATE OF status ON orders")
    );
    assert!(
        s.list_objects(&main_schema(), ObjectKind::Sequence)
            .await
            .unwrap()
            .is_empty()
    );
    // unqualified means main; unknown schemas fail
    assert_eq!(
        s.list_objects(&SchemaRef::default(), ObjectKind::View)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        s.list_objects(&SchemaRef::schema("nope"), ObjectKind::Table)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn system_objects_on_request() {
    let mut s = connect_with(":memory:", &[("show_system_objects", "true")]).await;
    exec(&mut s, SCHEMA).await;
    let tables = s
        .list_objects(&main_schema(), ObjectKind::Table)
        .await
        .unwrap();
    let system: Vec<String> = tables
        .iter()
        .filter(|t| t.is_system)
        .map(|t| t.obj.name.clone())
        .collect();
    assert!(
        system.contains(&"sqlite_sequence".to_string()),
        "{system:?}"
    );
    assert!(system.contains(&"sqlite_stat1".to_string()));
    assert!(system.iter().any(|n| n.starts_with("docs_")), "{system:?}");
    let indexes = s
        .list_objects(&main_schema(), ObjectKind::Index)
        .await
        .unwrap();
    assert!(
        indexes
            .iter()
            .any(|i| i.obj.name == "sqlite_autoindex_customers_1" && i.is_system)
    );
}

#[tokio::test]
async fn lists_table_children() {
    let mut s = sample().await;
    let t = table("customers");
    let cols = s.list_children(&t, ObjectKind::Column).await.unwrap();
    assert_eq!(
        names(&cols),
        vec!["id", "email", "name", "score", "created"]
    );
    assert_eq!(cols[0].detail.as_deref(), Some("INTEGER PK"));
    assert_eq!(cols[1].detail.as_deref(), Some("TEXT NOT NULL"));
    assert_eq!(cols[0].obj.table.as_deref(), Some("customers"));
    let idx = s.list_children(&t, ObjectKind::Index).await.unwrap();
    assert_eq!(
        names(&idx),
        vec!["sqlite_autoindex_customers_1", "ix_customers_lower_name"]
    );
    assert!(
        idx[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("unique constraint")
    );
    let pk = s.list_children(&t, ObjectKind::PrimaryKey).await.unwrap();
    assert_eq!(names(&pk), vec!["PRIMARY KEY"]);
    assert_eq!(
        names(&s.list_children(&t, ObjectKind::UniqueKey).await.unwrap()),
        vec!["uq_email"]
    );
    let checks = s
        .list_children(&t, ObjectKind::CheckConstraint)
        .await
        .unwrap();
    assert_eq!(names(&checks), vec!["customers_check1"]);
    assert_eq!(checks[0].detail.as_deref(), Some("score >= 0"));
    let fks = s
        .list_children(&table("orders"), ObjectKind::ForeignKey)
        .await
        .unwrap();
    assert_eq!(names(&fks), vec!["orders_fk1"]);
    assert_eq!(
        fks[0].detail.as_deref(),
        Some("(customer_id) → customers (id)")
    );
    assert_eq!(
        names(
            &s.list_children(&table("order_items"), ObjectKind::ForeignKey)
                .await
                .unwrap()
        ),
        vec!["fk_order"]
    );
    assert_eq!(
        names(
            &s.list_children(&table("orders"), ObjectKind::Trigger)
                .await
                .unwrap()
        ),
        vec!["trg_orders_paid"]
    );
    // columns of views and virtual tables
    let view_cols = s
        .list_children(
            &ObjectRef::new(ObjectKind::View, None, "v_order_totals"),
            ObjectKind::Column,
        )
        .await
        .unwrap();
    assert_eq!(names(&view_cols), vec!["customer", "total"]);
    assert_eq!(
        names(
            &s.list_children(&table("docs"), ObjectKind::Column)
                .await
                .unwrap()
        ),
        vec!["title", "body"]
    );
}

#[tokio::test]
async fn table_definitions() {
    let mut s = sample().await;
    let c = s.table_definition(&table("customers")).await.unwrap();
    assert_eq!(c.schema.as_deref(), Some("main"));
    assert_eq!(c.table_kind, TableKind::Table);
    let col = |n: &str| c.column(n).unwrap().clone();
    assert!(col("id").auto_increment);
    assert_eq!(col("id").comment.as_deref(), Some("customer id"));
    assert_eq!(col("id").ordinal, 1);
    assert_eq!(col("email").collation.as_deref(), Some("NOCASE"));
    assert!(!col("email").nullable);
    assert_eq!(col("name").default.as_deref(), Some("'anonymous'"));
    assert_eq!(col("score").default.as_deref(), Some("0"));
    assert_eq!(col("created").default.as_deref(), Some("CURRENT_TIMESTAMP"));
    assert_eq!(col("created").kind, dbiji_core::DataKind::DateTime);
    assert_eq!(
        c.primary_key,
        Some(KeyDef {
            name: None,
            columns: vec!["id".into()]
        })
    );
    assert_eq!(
        c.unique_keys,
        vec![KeyDef {
            name: Some("uq_email".into()),
            columns: vec!["email".into()]
        }]
    );
    assert_eq!(
        c.checks,
        vec![CheckDef {
            name: None,
            expression: "score >= 0".into()
        }]
    );
    assert_eq!(c.row_estimate, Some(2));
    let expr_idx = c
        .indexes
        .iter()
        .find(|i| i.name == "ix_customers_lower_name")
        .unwrap();
    assert!(expr_idx.unique);
    assert_eq!(expr_idx.columns.len(), 2);
    assert!(expr_idx.columns[0].is_expression);
    assert_eq!(expr_idx.columns[0].name, "lower(name)");
    assert_eq!(expr_idx.columns[1].name, "id");
    assert!(expr_idx.columns[1].descending);
    let auto = c
        .indexes
        .iter()
        .find(|i| i.name == "sqlite_autoindex_customers_1")
        .unwrap();
    assert!(auto.unique && auto.definition.is_none());
    assert_eq!(c.referenced_by.len(), 1);
    assert_eq!(c.referenced_by[0].table, "orders");
    assert_eq!(c.referenced_by[0].columns, vec!["customer_id"]);

    let o = s.table_definition(&table("orders")).await.unwrap();
    let doubled = o.column("doubled").unwrap();
    assert_eq!(doubled.generated.as_deref(), Some("total * 2"));
    assert_eq!(doubled.default, None);
    assert_eq!(
        o.options.get("generated.doubled").map(String::as_str),
        Some("stored")
    );
    assert_eq!(o.column("total").unwrap().data_type, "NUMERIC(10,2)");
    assert_eq!(o.foreign_keys.len(), 1);
    let fk = &o.foreign_keys[0];
    assert_eq!(
        (
            fk.ref_table.as_str(),
            fk.ref_columns.clone(),
            fk.on_delete,
            fk.deferrable
        ),
        (
            "customers",
            vec!["id".to_string()],
            FkAction::Cascade,
            false
        )
    );
    assert_eq!(o.checks.len(), 2);
    assert_eq!(o.checks[1].name.as_deref(), Some("ck_total"));
    let partial = o
        .indexes
        .iter()
        .find(|i| i.name == "ix_orders_status")
        .unwrap();
    assert_eq!(partial.predicate.as_deref(), Some("status IS NOT NULL"));
    assert_eq!(o.triggers.len(), 1);
    let trg = &o.triggers[0];
    assert_eq!(
        (trg.timing.as_str(), trg.events.clone()),
        ("AFTER", vec!["UPDATE OF status".to_string()])
    );
    assert!(trg.body.as_deref().unwrap().starts_with("UPDATE customers"));
    assert!(trg.definition.starts_with("CREATE TRIGGER trg_orders_paid"));
    assert_eq!(
        o.referenced_by
            .iter()
            .map(|f| f.name.clone())
            .collect::<Vec<_>>(),
        vec![Some("fk_order".to_string())]
    );

    let items = s.table_definition(&table("order_items")).await.unwrap();
    assert_eq!(
        items.primary_key.as_ref().unwrap().columns,
        vec!["order_id", "line"]
    );
    assert_eq!(
        items
            .options
            .get(options::WITHOUT_ROWID)
            .map(String::as_str),
        Some("true")
    );
    let fk = &items.foreign_keys[0];
    assert_eq!(fk.name.as_deref(), Some("fk_order"));
    assert!(fk.deferrable);
    assert_eq!(fk.on_update, FkAction::NoAction);

    let selfref = s.table_definition(&table("self_ref")).await.unwrap();
    assert_eq!(
        selfref.foreign_keys[0].ref_columns,
        vec!["id"],
        "implicit parent key is resolved"
    );
    let tags = s.table_definition(&table("tags")).await.unwrap();
    assert_eq!(
        tags.options.get(options::STRICT).map(String::as_str),
        Some("true")
    );
    let docs = s.table_definition(&table("docs")).await.unwrap();
    assert_eq!(
        docs.options
            .get(options::VIRTUAL_MODULE)
            .map(String::as_str),
        Some("fts5")
    );
    assert_eq!(docs.columns.len(), 2);
    // unqualified lookup and missing tables
    assert_eq!(
        s.table_definition(&ObjectRef::table(None, "TAGS"))
            .await
            .unwrap()
            .name,
        "tags"
    );
    assert_eq!(
        s.table_definition(&table("missing"))
            .await
            .unwrap_err()
            .kind,
        dbiji_core::ErrorKind::Query
    );
}

#[tokio::test]
async fn views_ddl_and_properties() {
    let mut s = sample().await;
    let v = s
        .view_definition(&ObjectRef::new(
            ObjectKind::View,
            Some("main".into()),
            "v_order_totals",
        ))
        .await
        .unwrap();
    assert!(
        v.definition.starts_with("SELECT c.email, sum(o.total)"),
        "{}",
        v.definition
    );
    assert_eq!(
        v.options.get(options::VIEW_COLUMNS).map(String::as_str),
        Some("(customer, total)")
    );
    assert_eq!(v.columns.len(), 2);

    let ddl = s.object_ddl(&table("orders")).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE orders ("), "{ddl}");
    assert!(
        ddl.contains("CREATE INDEX ix_orders_status ON orders (status) WHERE status IS NOT NULL;")
    );
    assert!(ddl.contains("CREATE TRIGGER trg_orders_paid"));
    assert!(ddl.contains("END;"));
    let all = s
        .object_ddl(&ObjectRef::new(ObjectKind::Schema, None, "main"))
        .await
        .unwrap();
    for needle in [
        "CREATE TABLE customers",
        "CREATE VIEW v_order_totals",
        "CREATE VIRTUAL TABLE docs",
        "CREATE TRIGGER",
    ] {
        assert!(all.contains(needle), "{needle}");
    }
    assert!(!all.contains("sqlite_sequence") && !all.contains("docs_data"));
    // the whole schema DDL can be replayed
    let mut copy = memory().await;
    exec(&mut copy, &all).await;
    assert_eq!(
        copy.list_objects(&main_schema(), ObjectKind::Table)
            .await
            .unwrap()
            .len(),
        6
    );
    let auto = s
        .object_ddl(&ObjectRef {
            table: Some("customers".into()),
            ..ObjectRef::new(ObjectKind::Index, None, "sqlite_autoindex_customers_1")
        })
        .await
        .unwrap();
    assert!(auto.starts_with("-- Index sqlite_autoindex_customers_1"));
    let col_ddl = s
        .object_ddl(&ObjectRef::child(
            ObjectKind::Column,
            &table("tags"),
            "weight",
        ))
        .await
        .unwrap();
    assert!(col_ddl.starts_with("CREATE TABLE tags"));

    let props = s.object_properties(&table("order_items")).await.unwrap();
    let get = |n: &str| props.iter().find(|p| p.name == n).map(|p| p.value.clone());
    assert_eq!(get("WITHOUT ROWID").as_deref(), Some("Yes"));
    assert_eq!(get("Columns").as_deref(), Some("4"));
    let props = s
        .object_properties(&ObjectRef::new(ObjectKind::Index, None, "ix_orders_status"))
        .await
        .unwrap();
    assert!(
        props
            .iter()
            .any(|p| p.name == "Partial" && p.value == "Yes")
    );
    let props = s
        .object_properties(&ObjectRef::new(ObjectKind::Database, None, "main"))
        .await
        .unwrap();
    assert!(props.iter().any(|p| p.name == "page_size"));
    assert!(
        props
            .iter()
            .any(|p| p.name == "foreign_keys" && p.value == "1")
    );

    let stats = s.object_statistics(&table("customers")).await.unwrap();
    assert!(
        stats
            .iter()
            .any(|p| p.name == "Row count" && p.value == "2"),
        "{stats:?}"
    );
    assert!(
        stats
            .iter()
            .any(|p| p.name == "Row count (estimate, ANALYZE)" && p.value == "2")
    );
    assert!(stats.iter().any(|p| p.name == "Pages"));
    let stats = s
        .object_statistics(&ObjectRef::new(ObjectKind::Database, None, "main"))
        .await
        .unwrap();
    assert!(stats.iter().any(|p| p.name == "page_count"));
}

#[tokio::test]
async fn dependencies() {
    let mut s = sample().await;
    let deps = s.object_dependencies(&table("customers")).await.unwrap();
    let used_by: Vec<(String, Option<String>)> = deps
        .iter()
        .filter(|d| d.direction == DependencyDirection::UsedBy)
        .map(|d| (d.target.name.clone(), d.kind.clone()))
        .collect();
    assert!(
        used_by.contains(&("orders".into(), Some("foreign key".into()))),
        "{used_by:?}"
    );
    assert!(used_by.contains(&("v_order_totals".into(), Some("view".into()))));
    assert!(used_by.contains(&("trg_orders_paid".into(), Some("trigger".into()))));
    let deps = s
        .object_dependencies(&ObjectRef::new(ObjectKind::View, None, "v_order_totals"))
        .await
        .unwrap();
    let on: Vec<String> = deps
        .iter()
        .filter(|d| d.direction == DependencyDirection::DependsOn)
        .map(|d| d.target.name.clone())
        .collect();
    assert_eq!(on, vec!["customers", "orders"]);
    let deps = s.object_dependencies(&table("orders")).await.unwrap();
    assert!(
        deps.iter()
            .any(|d| d.direction == DependencyDirection::DependsOn && d.target.name == "customers")
    );
    assert!(
        deps.iter()
            .any(|d| d.direction == DependencyDirection::UsedBy && d.target.name == "order_items")
    );
}

#[tokio::test]
async fn metadata_search() {
    let mut s = sample().await;
    let search = MetadataSearch {
        pattern: "order".into(),
        kinds: vec![
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::Column,
            ObjectKind::Index,
        ],
        ..Default::default()
    };
    let hits = s.search_metadata(&search).await.unwrap();
    let found: Vec<(ObjectKind, String)> = hits
        .iter()
        .map(|h| (h.obj.kind, h.obj.name.clone()))
        .collect();
    for expected in [
        (ObjectKind::Table, "orders"),
        (ObjectKind::Table, "order_items"),
        (ObjectKind::View, "v_order_totals"),
        (ObjectKind::Index, "ix_orders_status"),
        (ObjectKind::Column, "order_id"),
    ] {
        assert!(
            found.contains(&(expected.0, expected.1.to_string())),
            "{expected:?} in {found:?}"
        );
    }
    let col = hits.iter().find(|h| h.obj.name == "order_id").unwrap();
    assert_eq!(col.obj.table.as_deref(), Some("order_items"));
    let wild = MetadataSearch {
        pattern: "*_ID".into(),
        kinds: vec![ObjectKind::Column],
        ..Default::default()
    };
    let hits = s.search_metadata(&wild).await.unwrap();
    assert!(
        hits.iter()
            .all(|h| h.obj.name.to_lowercase().ends_with("_id"))
            && hits.len() >= 3,
        "{hits:?}"
    );
    let sensitive = MetadataSearch {
        case_sensitive: true,
        ..wild.clone()
    };
    assert!(s.search_metadata(&sensitive).await.unwrap().is_empty());
    let limited = MetadataSearch {
        pattern: String::new(),
        limit: 3,
        ..Default::default()
    };
    assert_eq!(s.search_metadata(&limited).await.unwrap().len(), 3);
    let comments = MetadataSearch {
        pattern: "customer id".into(),
        kinds: vec![ObjectKind::Column],
        search_comments: true,
        ..Default::default()
    };
    let hits = s.search_metadata(&comments).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].comment.as_deref(), Some("customer id"));
}

#[tokio::test]
async fn schema_snapshots() {
    let mut s = sample().await;
    let snap = s
        .schema_snapshot(&main_schema(), &SnapshotOptions::full())
        .await
        .unwrap();
    assert_eq!(snap.kind, Some(dbiji_core::DbKind::Sqlite));
    assert_eq!(snap.tables.len(), 6);
    assert_eq!(snap.views.len(), 1);
    assert!(snap.views[0].definition.starts_with("SELECT"));
    let customers = snap.table("customers").unwrap();
    assert_eq!(customers.referenced_by.len(), 1);
    assert!(!customers.indexes.is_empty());
    let orders = snap.table("orders").unwrap();
    assert_eq!(orders.triggers.len(), 1);
    assert_eq!(orders.referenced_by.len(), 1);
    let light = s
        .schema_snapshot(&main_schema(), &SnapshotOptions::light())
        .await
        .unwrap();
    assert!(
        light
            .tables
            .iter()
            .all(|t| t.indexes.is_empty() && t.triggers.is_empty())
    );
    assert_eq!(light.table("orders").unwrap().foreign_keys.len(), 1);
    assert!(light.views[0].definition.is_empty());
    assert_eq!(light.views[0].columns.len(), 2);
}

#[tokio::test]
async fn snapshot_of_many_tables() {
    let mut s = memory().await;
    let mut script = String::new();
    for i in 0..300 {
        script.push_str(&format!(
            "CREATE TABLE t{i} (id INTEGER PRIMARY KEY, ref_id INT REFERENCES t{} (id), v TEXT NOT NULL DEFAULT '', CHECK (length(v) < 100));\n\
             CREATE INDEX ix_t{i}_v ON t{i} (v);\n",
            (i + 1) % 300
        ));
    }
    exec(&mut s, &script).await;
    let started = std::time::Instant::now();
    let snap = s
        .schema_snapshot(&main_schema(), &SnapshotOptions::full())
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(snap.tables.len(), 300);
    assert!(snap.tables.iter().all(|t| t.columns.len() == 3
        && t.foreign_keys.len() == 1
        && t.referenced_by.len() == 1
        && t.indexes.len() == 1
        && t.checks.len() == 1));
    assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");
}

#[tokio::test]
async fn temp_schema_objects() {
    let mut s = memory().await;
    exec(&mut s, "CREATE TEMP TABLE scratch (id INTEGER PRIMARY KEY, v TEXT); CREATE TEMP VIEW tv AS SELECT v FROM scratch").await;
    let temp = SchemaRef::schema("temp");
    assert_eq!(
        names(&s.list_objects(&temp, ObjectKind::Table).await.unwrap()),
        vec!["scratch"]
    );
    let def = s
        .table_definition(&ObjectRef::table(None, "scratch"))
        .await
        .unwrap();
    assert_eq!(def.schema.as_deref(), Some("temp"));
    assert_eq!(def.table_kind, TableKind::Temporary);
    let ddl = s
        .object_ddl(&ObjectRef::table(Some("temp".into()), "scratch"))
        .await
        .unwrap();
    assert!(ddl.starts_with("CREATE TEMP TABLE scratch"), "{ddl}");
    let vddl = s
        .object_ddl(&ObjectRef::new(ObjectKind::View, Some("temp".into()), "tv"))
        .await
        .unwrap();
    assert!(vddl.starts_with("CREATE TEMP VIEW tv"), "{vddl}");
}

/// Definition without the parts that legitimately differ between copies.
fn normalized(mut t: TableDef) -> TableDef {
    t.schema = None;
    t.referenced_by.clear();
    t.row_estimate = None;
    t.size_bytes = None;
    for fk in &mut t.foreign_keys {
        fk.schema = None;
        fk.ref_schema = None;
    }
    t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    t.triggers.sort_by(|a, b| a.name.cmp(&b.name));
    t
}

#[tokio::test]
async fn ddl_round_trip_into_an_attached_database() {
    let mut s = sample().await;
    exec(&mut s, "ATTACH DATABASE ':memory:' AS copy").await;
    let dialect = SqliteDialect::new();
    let tables = s
        .list_objects(&main_schema(), ObjectKind::Table)
        .await
        .unwrap();
    for t in &tables {
        let original = s.table_definition(&t.obj).await.unwrap();
        let mut target = original.clone();
        target.schema = Some("copy".into());
        for stmt in dialect.ddl().create_table(&target, true) {
            exec(&mut s, &stmt).await;
        }
        let copied = s
            .table_definition(&ObjectRef::table(
                Some("copy".into()),
                original.name.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(
            normalized(copied),
            normalized(original.clone()),
            "round trip of {}",
            original.name
        );
    }
    // data can flow between the copies (constraints are enforced in the copy)
    exec(
        &mut s,
        "INSERT INTO copy.customers SELECT * FROM main.customers",
    )
    .await;
    assert_eq!(int(&mut s, "SELECT count(*) FROM copy.customers").await, 2);
    let err = dbiji_core::driver::execute_update(
        &mut s,
        "INSERT INTO copy.customers(email) VALUES ('A@X')",
        &[],
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.code.as_deref(),
        Some("SQLITE_CONSTRAINT_UNIQUE"),
        "COLLATE NOCASE unique key survived"
    );
}

#[tokio::test]
async fn properties_of_columns_and_constraints() {
    let mut s = sample().await;
    let orders = table("orders");
    let has = |props: &[Property], name: &str, value: &str| {
        props.iter().any(|p| p.name == name && p.value == value)
    };
    let col = ObjectRef::child(ObjectKind::Column, &orders, "doubled");
    let props = s.object_properties(&col).await.unwrap();
    assert!(has(&props, "Generated", "total * 2 (STORED)"), "{props:?}");
    assert!(has(&props, "Table", "orders"));
    assert!(s.object_dependencies(&col).await.unwrap().is_empty());
    let fk = ObjectRef::child(ObjectKind::ForeignKey, &orders, "orders_fk1");
    let props = s.object_properties(&fk).await.unwrap();
    assert!(has(&props, "References", "customers (id)"), "{props:?}");
    assert!(has(&props, "ON DELETE", "CASCADE"));
    let deps = s.object_dependencies(&fk).await.unwrap();
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0].target.name, "customers");
    let ck = ObjectRef::child(ObjectKind::CheckConstraint, &orders, "ck_total");
    assert!(has(
        &s.object_properties(&ck).await.unwrap(),
        "Expression",
        "total >= 0"
    ));
    let unnamed = ObjectRef::child(ObjectKind::CheckConstraint, &orders, "orders_check1");
    let props = s.object_properties(&unnamed).await.unwrap();
    assert!(
        props
            .iter()
            .any(|p| p.name == "Expression" && p.value.starts_with("status IN")),
        "{props:?}"
    );
    let pk = ObjectRef::child(ObjectKind::PrimaryKey, &table("order_items"), "PRIMARY KEY");
    assert!(has(
        &s.object_properties(&pk).await.unwrap(),
        "Columns",
        "order_id, line"
    ));
    let uq = ObjectRef::child(ObjectKind::UniqueKey, &table("customers"), "uq_email");
    assert!(has(
        &s.object_properties(&uq).await.unwrap(),
        "Columns",
        "email"
    ));
    let id = ObjectRef::child(ObjectKind::Column, &table("customers"), "id");
    let props = s.object_properties(&id).await.unwrap();
    assert!(
        has(&props, "Primary key position", "1")
            && has(&props, "AUTOINCREMENT", "Yes")
            && has(&props, "Comment", "customer id")
    );
    let missing = ObjectRef::child(ObjectKind::Column, &orders, "nope");
    assert_eq!(
        s.object_properties(&missing).await.unwrap_err().kind,
        dbiji_core::ErrorKind::Query
    );
    let ix = s
        .object_properties(&ObjectRef::new(ObjectKind::Index, None, "ix_orders_status"))
        .await
        .unwrap();
    assert!(has(&ix, "Table", "orders"));
}
