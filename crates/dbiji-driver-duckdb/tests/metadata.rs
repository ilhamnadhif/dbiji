//! Catalog access, DDL generation and EXPLAIN.

mod common;

use common::*;
use dbiji_core::ddl::{ColumnDef, DdlGenerator, KeyDef, SnapshotOptions, TableDef};
use dbiji_core::dialect::Dialect;
use dbiji_core::meta::{MetadataSearch, ObjectKind, ObjectRef, SchemaRef};
use dbiji_core::{Session, Value};
use dbiji_driver_duckdb::{DuckDbDdl, DuckDbDialect, DuckDbSession};

const SCHEMA: &str = "
    CREATE SCHEMA shop;
    CREATE SEQUENCE shop.customers_id_seq;
    CREATE TABLE shop.customers (
        id INTEGER DEFAULT nextval('shop.customers_id_seq') PRIMARY KEY,
        email VARCHAR NOT NULL UNIQUE,
        name VARCHAR DEFAULT 'anon',
        score DECIMAL(5,2) CHECK (score >= 0),
        name_len INTEGER GENERATED ALWAYS AS (length(name)) VIRTUAL
    );
    CREATE TABLE shop.orders (
        id INTEGER PRIMARY KEY,
        customer_id INTEGER REFERENCES shop.customers(id),
        total DOUBLE,
        tags VARCHAR[]
    );
    CREATE INDEX orders_total_idx ON shop.orders (total);
    CREATE VIEW shop.big_orders AS SELECT id, total FROM shop.orders WHERE total > 100;
    CREATE MACRO shop.add_tax(x) AS x * 1.2;
    CREATE TYPE shop.status AS ENUM ('new', 'paid');
    COMMENT ON TABLE shop.customers IS 'People who buy';
    COMMENT ON COLUMN shop.customers.email IS 'Login';
    INSERT INTO shop.customers (email, name, score) VALUES ('a@x', 'Ann', 1.5), ('b@x', 'Bob', 2);
    INSERT INTO shop.orders VALUES (1, 1, 150, ['x']), (2, 2, 20, NULL);
";

async fn shop() -> DuckDbSession {
    let mut s = memory().await;
    exec(&mut s, SCHEMA).await;
    s
}

fn shop_ref() -> SchemaRef {
    SchemaRef::schema("shop")
}

fn names(v: &[dbiji_core::meta::ObjectInfo]) -> Vec<String> {
    v.iter().map(|o| o.obj.name.clone()).collect()
}

#[tokio::test]
async fn navigator_lists() {
    let mut s = shop().await;
    let schemas = s.list_schemas().await.unwrap();
    assert!(schemas.iter().any(|x| x.name == "shop"));
    assert!(schemas.iter().any(|x| x.name == "main" && x.is_current));
    assert_eq!(
        names(
            &s.list_objects(&shop_ref(), ObjectKind::Table)
                .await
                .unwrap()
        ),
        vec!["customers", "orders"]
    );
    let tables = s
        .list_objects(&shop_ref(), ObjectKind::Table)
        .await
        .unwrap();
    assert_eq!(tables[0].comment.as_deref(), Some("People who buy"));
    assert_eq!(tables[0].obj.database, None);
    assert_eq!(
        names(&s.list_objects(&shop_ref(), ObjectKind::View).await.unwrap()),
        vec!["big_orders"]
    );
    let idx = s
        .list_objects(&shop_ref(), ObjectKind::Index)
        .await
        .unwrap();
    assert_eq!(names(&idx), vec!["orders_total_idx"]);
    assert_eq!(idx[0].obj.table.as_deref(), Some("orders"));
    assert_eq!(
        names(
            &s.list_objects(&shop_ref(), ObjectKind::Sequence)
                .await
                .unwrap()
        ),
        vec!["customers_id_seq"]
    );
    let macros = s
        .list_objects(&shop_ref(), ObjectKind::Function)
        .await
        .unwrap();
    assert_eq!(names(&macros), vec!["add_tax"]);
    assert_eq!(macros[0].obj.signature.as_deref(), Some("(x)"));
    assert_eq!(
        names(&s.list_objects(&shop_ref(), ObjectKind::Type).await.unwrap()),
        vec!["status"]
    );
    // system views are hidden
    assert!(
        s.list_objects(&SchemaRef::schema("main"), ObjectKind::View)
            .await
            .unwrap()
            .is_empty()
    );

    let customers = ObjectRef::table(Some("shop".into()), "customers");
    let cols = s
        .list_children(&customers, ObjectKind::Column)
        .await
        .unwrap();
    assert_eq!(
        names(&cols),
        vec!["id", "email", "name", "score", "name_len"]
    );
    assert_eq!(cols[0].detail.as_deref(), Some("INTEGER PK NOT NULL"));
    assert_eq!(cols[1].comment.as_deref(), Some("Login"));
    assert!(cols[4].detail.as_deref().unwrap().contains("GENERATED"));
    assert_eq!(
        s.list_children(&customers, ObjectKind::PrimaryKey)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        s.list_children(&customers, ObjectKind::UniqueKey)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        s.list_children(&customers, ObjectKind::CheckConstraint)
            .await
            .unwrap()
            .len(),
        1
    );
    let orders = ObjectRef::table(Some("shop".into()), "orders");
    let fks = s
        .list_children(&orders, ObjectKind::ForeignKey)
        .await
        .unwrap();
    assert_eq!(fks.len(), 1);
    assert!(fks[0].detail.as_deref().unwrap().contains("REFERENCES"));
    let idx = s.list_children(&orders, ObjectKind::Index).await.unwrap();
    assert_eq!(idx[0].detail.as_deref(), Some("(total)"));
    // views have columns too
    let view = ObjectRef::new(ObjectKind::View, Some("shop".into()), "big_orders");
    assert_eq!(
        names(&s.list_children(&view, ObjectKind::Column).await.unwrap()),
        vec!["id", "total"]
    );
}

#[tokio::test]
async fn table_and_view_definitions() {
    let mut s = shop().await;
    let t = s
        .table_definition(&ObjectRef::table(Some("shop".into()), "customers"))
        .await
        .unwrap();
    assert_eq!(t.schema.as_deref(), Some("shop"));
    assert_eq!(t.comment.as_deref(), Some("People who buy"));
    assert_eq!(
        t.primary_key,
        Some(KeyDef {
            name: None,
            columns: vec!["id".into()]
        })
    );
    assert_eq!(
        t.unique_keys,
        vec![KeyDef {
            name: None,
            columns: vec!["email".into()]
        }]
    );
    assert_eq!(t.checks.len(), 1);
    assert!(t.checks[0].expression.contains("score"), "{:?}", t.checks);
    let id = &t.columns[0];
    assert!(
        id.auto_increment && id.default.is_none() && !id.nullable,
        "{id:?}"
    );
    let email = &t.columns[1];
    assert!(!email.nullable);
    assert_eq!(email.comment.as_deref(), Some("Login"));
    assert_eq!(t.columns[2].default.as_deref(), Some("'anon'"));
    assert_eq!(t.columns[3].data_type, "DECIMAL(5,2)");
    assert!(
        t.columns[4]
            .generated
            .as_deref()
            .is_some_and(|g| g.contains("length")),
        "{:?}",
        t.columns[4]
    );
    assert!(t.columns[4].default.is_none());
    assert_eq!(t.row_estimate, Some(2));
    assert_eq!(t.referenced_by.len(), 1);
    let rb = &t.referenced_by[0];
    assert_eq!(
        (rb.schema.as_deref(), rb.table.as_str()),
        (Some("shop"), "orders")
    );
    assert_eq!(rb.columns, vec!["customer_id"]);

    let o = s
        .table_definition(&ObjectRef::table(Some("shop".into()), "orders"))
        .await
        .unwrap();
    assert_eq!(o.foreign_keys.len(), 1);
    let fk = &o.foreign_keys[0];
    assert_eq!(
        (fk.ref_schema.as_deref(), fk.ref_table.as_str()),
        (Some("shop"), "customers")
    );
    assert_eq!(fk.ref_columns, vec!["id"]);
    assert_eq!(o.indexes.len(), 1);
    assert_eq!(o.indexes[0].columns[0].name, "total");
    assert_eq!(o.columns[3].data_type, "VARCHAR[]");

    let v = s
        .view_definition(&ObjectRef::new(
            ObjectKind::View,
            Some("shop".into()),
            "big_orders",
        ))
        .await
        .unwrap();
    assert!(
        v.definition.to_uppercase().starts_with("SELECT"),
        "{}",
        v.definition
    );
    assert_eq!(v.columns.len(), 2);
    let seq = s
        .sequence_definition(&ObjectRef::new(
            ObjectKind::Sequence,
            Some("shop".into()),
            "customers_id_seq",
        ))
        .await
        .unwrap();
    assert_eq!(seq.increment, Some(1));
    let m = s
        .routine_definition(&ObjectRef::new(
            ObjectKind::Function,
            Some("shop".into()),
            "add_tax",
        ))
        .await
        .unwrap();
    assert!(
        m.definition.starts_with("CREATE MACRO shop.add_tax(x) AS"),
        "{}",
        m.definition
    );
    let missing = s
        .table_definition(&ObjectRef::table(Some("shop".into()), "nope"))
        .await
        .unwrap_err();
    assert_eq!(missing.code.as_deref(), Some("NOT_FOUND"));
}

#[tokio::test]
async fn ddl_and_properties() {
    let mut s = shop().await;
    let ddl = s
        .object_ddl(&ObjectRef::table(Some("shop".into()), "customers"))
        .await
        .unwrap();
    assert!(ddl.contains("CREATE SEQUENCE"), "{ddl}");
    assert!(ddl.contains("CREATE TABLE"), "{ddl}");
    assert!(
        ddl.contains("COMMENT ON TABLE shop.customers IS 'People who buy';"),
        "{ddl}"
    );
    let ddl = s
        .object_ddl(&ObjectRef::table(Some("shop".into()), "orders"))
        .await
        .unwrap();
    assert!(ddl.contains("CREATE INDEX orders_total_idx"), "{ddl}");
    for (kind, name) in [
        (ObjectKind::View, "big_orders"),
        (ObjectKind::Sequence, "customers_id_seq"),
        (ObjectKind::Function, "add_tax"),
        (ObjectKind::Type, "status"),
    ] {
        let ddl = s
            .object_ddl(&ObjectRef::new(kind, Some("shop".into()), name))
            .await
            .unwrap();
        assert!(ddl.starts_with("CREATE"), "{kind:?}: {ddl}");
    }
    let mut idx = ObjectRef::new(ObjectKind::Index, Some("shop".into()), "orders_total_idx");
    idx.table = Some("orders".into());
    assert!(
        s.object_ddl(&idx)
            .await
            .unwrap()
            .starts_with("CREATE INDEX")
    );
    // the generated DDL recreates the schema in another database
    let mut copy = memory().await;
    let mut script = String::from("CREATE SCHEMA shop;\n");
    for (kind, name) in [
        (ObjectKind::Type, "status"),
        (ObjectKind::Function, "add_tax"),
        (ObjectKind::Table, "customers"),
        (ObjectKind::Table, "orders"),
        (ObjectKind::View, "big_orders"),
    ] {
        script.push_str(
            &s.object_ddl(&ObjectRef::new(kind, Some("shop".into()), name))
                .await
                .unwrap(),
        );
    }
    exec(&mut copy, &script).await;
    exec(
        &mut copy,
        "INSERT INTO shop.customers (email) VALUES ('c@x')",
    )
    .await;
    assert_eq!(int(&mut copy, "SELECT id FROM shop.customers").await, 1);

    let props = s
        .object_properties(&ObjectRef::table(Some("shop".into()), "customers"))
        .await
        .unwrap();
    assert!(
        props
            .iter()
            .any(|p| p.name == "Primary key" && p.value == "id")
    );
    let col = ObjectRef::child(
        ObjectKind::Column,
        &ObjectRef::table(Some("shop".into()), "customers"),
        "name",
    );
    let props = s.object_properties(&col).await.unwrap();
    assert!(
        props
            .iter()
            .any(|p| p.name == "Default" && p.value == "'anon'")
    );
    let stats = s
        .object_statistics(&ObjectRef::table(Some("shop".into()), "orders"))
        .await
        .unwrap();
    assert!(
        stats
            .iter()
            .any(|p| p.name == "Estimated rows" && p.value == "2")
    );
    let db_stats = s
        .object_statistics(&ObjectRef::new(ObjectKind::Database, None, "memory"))
        .await
        .unwrap();
    assert!(db_stats.iter().any(|p| p.name == "Block size"));
}

#[tokio::test]
async fn search_and_snapshot() {
    let mut s = shop().await;
    let search = MetadataSearch {
        pattern: "cust*".into(),
        kinds: vec![],
        ..Default::default()
    };
    let found = s.search_metadata(&search).await.unwrap();
    assert!(
        found
            .iter()
            .any(|o| o.obj.kind == ObjectKind::Table && o.obj.name == "customers")
    );
    assert!(
        found
            .iter()
            .any(|o| o.obj.kind == ObjectKind::Column && o.obj.name == "customer_id")
    );
    assert!(found.iter().any(|o| o.obj.kind == ObjectKind::Sequence));
    let comments = MetadataSearch {
        pattern: "login".into(),
        kinds: vec![ObjectKind::Column],
        search_comments: true,
        ..Default::default()
    };
    assert_eq!(
        names(&s.search_metadata(&comments).await.unwrap()),
        vec!["email"]
    );
    let limited = MetadataSearch {
        pattern: "".into(),
        kinds: vec![ObjectKind::Column],
        limit: 3,
        ..Default::default()
    };
    assert_eq!(s.search_metadata(&limited).await.unwrap().len(), 3);

    let snap = s
        .schema_snapshot(&shop_ref(), &SnapshotOptions::full())
        .await
        .unwrap();
    assert_eq!(snap.tables.len(), 2);
    assert_eq!(snap.views.len(), 1);
    assert_eq!(snap.sequences.len(), 1);
    assert_eq!(snap.routines.len(), 1);
    assert_eq!(snap.types.len(), 1);
    assert_eq!(snap.types[0].elements, vec!["new", "paid"]);
    let t = snap.table("customers").unwrap();
    assert_eq!(t.referenced_by.len(), 1);
    let light = s
        .schema_snapshot(&shop_ref(), &SnapshotOptions::light())
        .await
        .unwrap();
    assert!(light.tables[1].indexes.is_empty());
    assert!(light.views[0].definition.is_empty());
}

#[tokio::test]
async fn attached_databases() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_in(dir.path(), "other.duckdb");
    let mut s = memory().await;
    exec(&mut s, &format!("ATTACH '{path}' AS other; CREATE TABLE other.main.items(id INTEGER PRIMARY KEY, label VARCHAR)"))
        .await;
    let r = SchemaRef::new(Some("other".into()), None);
    let tables = s.list_objects(&r, ObjectKind::Table).await.unwrap();
    assert_eq!(names(&tables), vec!["items"]);
    assert_eq!(tables[0].obj.database.as_deref(), Some("other"));
    let def = s.table_definition(&tables[0].obj).await.unwrap();
    assert_eq!(def.columns.len(), 2);
    let d = DuckDbDialect::new();
    assert_eq!(d.qualified_ref(&tables[0].obj), "other.main.items");
    assert!(
        s.object_ddl(&ObjectRef::new(ObjectKind::Database, None, "other"))
            .await
            .unwrap()
            .starts_with("ATTACH")
    );
    let found = s
        .search_metadata(&MetadataSearch {
            pattern: "items".into(),
            kinds: vec![ObjectKind::Table],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found[0].obj.database.as_deref(), Some("other"));
}

#[tokio::test]
async fn generated_ddl_runs() {
    let mut s = memory().await;
    let ddl = DuckDbDdl::new();
    let mut t = TableDef::new(Some("main".into()), "people");
    let mut id = ColumnDef::new("id", "INTEGER").not_null();
    id.auto_increment = true;
    t.columns = vec![
        id,
        ColumnDef::new("name", "VARCHAR"),
        ColumnDef::new("age", "INTEGER"),
    ];
    t.primary_key = Some(KeyDef {
        name: None,
        columns: vec!["id".into()],
    });
    t.comment = Some("persons".into());
    for stmt in ddl.create_table(&t, true) {
        exec(&mut s, &stmt).await;
    }
    exec(
        &mut s,
        "INSERT INTO people (name, age) VALUES ('a', 1), ('b', 2)",
    )
    .await;
    // read back and alter: rename a column, change a type, add a column, add a unique key (rebuild)
    let from = s
        .table_definition(&ObjectRef::table(Some("main".into()), "people"))
        .await
        .unwrap();
    assert!(from.columns[0].auto_increment);
    let mut to = from.clone();
    to.columns[1].name = "full_name".into();
    to.columns[2].data_type = "BIGINT".into();
    to.columns.push(ColumnDef::new("email", "VARCHAR"));
    let stmts = ddl.alter_table(&from, &to, &[("name".into(), "full_name".into())]);
    for stmt in &stmts {
        exec(&mut s, stmt).await;
    }
    assert_eq!(int(&mut s, "SELECT sum(age) FROM people").await, 3);
    assert_eq!(
        scalar(&mut s, "SELECT full_name FROM people WHERE id = 2").await,
        Value::text("b")
    );
    let from = s
        .table_definition(&ObjectRef::table(Some("main".into()), "people"))
        .await
        .unwrap();
    assert_eq!(from.columns[2].data_type, "BIGINT");
    let mut to = from.clone();
    to.unique_keys.push(KeyDef {
        name: None,
        columns: vec!["full_name".into()],
    });
    for stmt in ddl.alter_table(&from, &to, &[]) {
        exec(&mut s, &stmt).await;
    }
    let after = s
        .table_definition(&ObjectRef::table(Some("main".into()), "people"))
        .await
        .unwrap();
    assert_eq!(after.unique_keys.len(), 1);
    assert_eq!(int(&mut s, "SELECT count(*) FROM people").await, 2);
    assert_eq!(after.comment.as_deref(), Some("persons"));
    exec(&mut s, "INSERT INTO people (full_name) VALUES ('c')").await;
    assert_eq!(int(&mut s, "SELECT max(id) FROM people").await, 3);
}

#[tokio::test]
async fn explain_plans() {
    let mut s = shop().await;
    let plan = s
        .explain("SELECT * FROM shop.orders WHERE total > 10", false)
        .await
        .unwrap();
    assert!(!plan.analyzed);
    assert!(!plan.roots.is_empty());
    assert!(!plan.text.is_empty());
    let mut relations = Vec::new();
    for r in &plan.roots {
        r.walk(0, &mut |n, _| relations.extend(n.relation.clone()));
    }
    assert!(relations.iter().any(|r| r.ends_with("orders")), "{plan:?}");
    assert!(plan.roots[0].plan_rows.is_some());
    let analyzed = s
        .explain("SELECT count(*) FROM shop.orders", true)
        .await
        .unwrap();
    assert!(analyzed.analyzed);
    assert!(analyzed.execution_ms.is_some());
    assert!(
        analyzed.roots.iter().any(|r| r.actual_total_ms.is_some()),
        "{analyzed:?}"
    );
    let e = s.explain("SELECT 1; SELECT 2", false).await.unwrap_err();
    assert_eq!(e.kind, dbiji_core::ErrorKind::Query);
}
