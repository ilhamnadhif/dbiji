//! Logic tests of schema compare with hand-built snapshots and the ANSI DDL
//! generator, including the self-check: applying the planned migration to
//! the target model must make the diff empty.

use crate::*;
use dbiji_core::ddl::*;
use dbiji_core::dialect::GenericDialect;
use dbiji_core::{DbKind, Dialect, ObjectKind};

fn col(name: &str, ty: &str) -> ColumnDef {
    ColumnDef::new(name, ty)
}

fn key(name: &str, cols: &[&str]) -> KeyDef {
    KeyDef { name: Some(name.to_string()), columns: cols.iter().map(|c| c.to_string()).collect() }
}

fn idx(name: &str, cols: &[&str]) -> IndexDef {
    IndexDef { name: name.to_string(), columns: cols.iter().map(|c| IndexColumn::column(*c)).collect(), ..Default::default() }
}

fn fk(name: &str, cols: &[&str], schema: &str, ref_table: &str, ref_cols: &[&str], on_delete: FkAction) -> ForeignKeyDef {
    ForeignKeyDef {
        name: Some(name.to_string()),
        columns: cols.iter().map(|c| c.to_string()).collect(),
        ref_schema: Some(schema.to_string()),
        ref_table: ref_table.to_string(),
        ref_columns: ref_cols.iter().map(|c| c.to_string()).collect(),
        on_delete,
        ..Default::default()
    }
}

fn table<'a>(s: &'a mut SchemaSnapshot, name: &str) -> &'a mut TableDef {
    s.tables.iter_mut().find(|t| t.name == name).expect("fixture table")
}

/// A small but complete shop schema living in `schema`.
fn shop(schema: &str) -> SchemaSnapshot {
    let s = Some(schema.to_string());
    let q = |n: &str| format!("{schema}.{n}");

    let mut users = TableDef::new(s.clone(), "users");
    users.columns =
        vec![col("id", "integer").not_null(), col("email", "varchar(255)").not_null(), col("name", "varchar(100)"), col("created_at", "timestamp").default_value("now()")];
    users.primary_key = Some(key("users_pkey", &["id"]));
    users.unique_keys = vec![key("users_email_key", &["email"])];
    users.indexes = vec![idx("idx_users_name", &["name"])];
    users.comment = Some("Registered users".into());

    let mut orders = TableDef::new(s.clone(), "orders");
    orders.columns = vec![
        col("id", "integer").not_null(),
        col("user_id", "integer").not_null(),
        col("status", &q("order_status")).not_null().default_value("'new'"),
        col("total", "numeric(12,2)").not_null().default_value("0"),
        col("invoice_no", "bigint").default_value(format!("nextval('{}'::regclass)", q("invoice_seq"))),
    ];
    orders.primary_key = Some(key("orders_pkey", &["id"]));
    orders.checks = vec![CheckDef { name: Some("orders_total_check".into()), expression: "(total >= (0)::numeric)".into() }];
    orders.foreign_keys = vec![fk("fk_orders_user", &["user_id"], schema, "users", &["id"], FkAction::Cascade)];
    orders.indexes = vec![idx("idx_orders_user", &["user_id"])];
    orders.triggers = vec![TriggerDef {
        name: "trg_orders_audit".into(),
        timing: "AFTER".into(),
        events: vec!["INSERT".into()],
        for_each_row: true,
        enabled: true,
        definition: format!("CREATE TRIGGER trg_orders_audit AFTER INSERT ON {} FOR EACH ROW EXECUTE FUNCTION {}()", q("orders"), q("audit")),
        ..Default::default()
    }];

    let mut items = TableDef::new(s.clone(), "order_items");
    items.columns =
        vec![col("order_id", "integer").not_null(), col("line_no", "integer").not_null(), col("product", "text").not_null(), col("qty", "integer").not_null().default_value("1")];
    items.primary_key = Some(key("order_items_pkey", &["order_id", "line_no"]));
    items.foreign_keys = vec![fk("fk_items_order", &["order_id"], schema, "orders", &["id"], FkAction::Cascade)];
    items.indexes = vec![idx("idx_items_product", &["product"])];

    let views = vec![
        ViewDef {
            schema: s.clone(),
            name: "v_user_orders".into(),
            definition: format!("SELECT u.id, u.email, count(o.id) AS orders FROM {} u LEFT JOIN {} o ON o.user_id = u.id GROUP BY u.id, u.email", q("users"), q("orders")),
            ..Default::default()
        },
        ViewDef { schema: s.clone(), name: "v_big_orders".into(), definition: format!("SELECT id, email FROM {} WHERE orders > 10", q("v_user_orders")), ..Default::default() },
    ];
    let sequences = vec![SequenceDef { schema: s.clone(), name: "invoice_seq".into(), start: Some(1000), increment: Some(1), ..Default::default() }];
    let routines = vec![
        RoutineDef {
            schema: s.clone(),
            name: "audit".into(),
            kind: Some(ObjectKind::Function),
            arguments: String::new(),
            returns: Some("trigger".into()),
            language: Some("plpgsql".into()),
            definition: format!("CREATE OR REPLACE FUNCTION {}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$", q("audit")),
            comment: None,
        },
        RoutineDef {
            schema: s.clone(),
            name: "order_total".into(),
            kind: Some(ObjectKind::Function),
            arguments: "oid integer".into(),
            returns: Some("numeric".into()),
            language: Some("sql".into()),
            definition: format!(
                "CREATE OR REPLACE FUNCTION {}(oid integer) RETURNS numeric LANGUAGE sql AS $$ SELECT sum(total) FROM {} WHERE id = oid $$",
                q("order_total"),
                q("orders")
            ),
            comment: None,
        },
    ];
    let types = vec![TypeDef {
        schema: s.clone(),
        name: "order_status".into(),
        category: "enum".into(),
        elements: vec!["new".into(), "paid".into(), "shipped".into()],
        definition: format!("CREATE TYPE {} AS ENUM ('new', 'paid', 'shipped')", q("order_status")),
        comment: None,
    }];
    SchemaSnapshot { kind: Some(DbKind::Postgres), database: None, schema: s, tables: vec![users, orders, items], views, sequences, routines, types }
}

/// The production copy, drifted in many ways.
fn drifted() -> SchemaSnapshot {
    let mut t = shop("prod");
    {
        let u = table(&mut t, "users");
        u.columns[1].data_type = "character varying(100)".into();
        u.columns[2].nullable = false;
        u.columns.retain(|c| c.name != "created_at");
        u.columns.push(col("legacy_flag", "boolean"));
    }
    {
        let o = table(&mut t, "orders");
        o.columns[2].default = Some("'pending'::prod.order_status".into());
        o.checks[0].expression = "(total > (0)::numeric)".into();
        o.indexes = vec![idx("idx_orders_status", &["status"])];
        o.foreign_keys[0].on_delete = FkAction::NoAction;
    }
    t.tables.retain(|x| x.name != "order_items");
    let mut logs = TableDef::new(Some("prod".into()), "old_logs");
    logs.columns = vec![col("id", "integer").not_null(), col("msg", "text")];
    logs.primary_key = Some(key("old_logs_pkey", &["id"]));
    t.tables.push(logs);
    t.views.push(ViewDef { schema: Some("prod".into()), name: "v_logs".into(), definition: "SELECT msg FROM prod.old_logs".into(), ..Default::default() });
    t.views[1].definition = "SELECT id, email FROM prod.v_user_orders WHERE orders > 5".into();
    t.sequences[0].increment = Some(10);
    t.sequences.push(SequenceDef { schema: Some("prod".into()), name: "old_seq".into(), ..Default::default() });
    t.routines[1].definition = t.routines[1].definition.replace("WHERE id = oid", "WHERE id = oid AND total > 0");
    t.routines.push(RoutineDef {
        schema: Some("prod".into()),
        name: "legacy_fn".into(),
        kind: Some(ObjectKind::Function),
        returns: Some("integer".into()),
        definition: "CREATE OR REPLACE FUNCTION prod.legacy_fn() RETURNS integer LANGUAGE sql AS $$ SELECT 1 $$".into(),
        ..Default::default()
    });
    t
}

fn item<'a>(d: &'a SchemaDiff, kind: ObjectKind, name: &str) -> &'a DiffItem {
    d.items.iter().find(|i| i.kind == kind && i.name == name).unwrap_or_else(|| panic!("no {kind:?} {name} in diff"))
}

fn child<'a>(i: &'a DiffItem, kind: ObjectKind, name: &str) -> &'a DiffItem {
    i.children.iter().find(|c| c.kind == kind && c.name == name).unwrap_or_else(|| panic!("no child {kind:?} {name} in {}", i.name))
}

fn describe(d: &SchemaDiff) -> String {
    let mut s = String::new();
    for i in d.changed_items() {
        s.push_str(&format!("{:?} {:?} {} {:?}\n", i.status, i.kind, i.name, i.changes));
        for c in &i.children {
            s.push_str(&format!("    {:?} {:?} {} {:?}\n", c.status, c.kind, c.name, c.changes));
        }
    }
    s
}

/// Plan, simulate, re-diff: the migration must leave no differences.
fn self_check(source: &SchemaSnapshot, target: &SchemaSnapshot, opts: &CompareOptions, script: &ScriptOptions, dialect: &dyn Dialect) -> MigrationPlan {
    let diff = schema_diff(source, target, opts);
    let plan = migration_plan(&diff, source, target, dialect, script);
    let applied = apply_plan(&plan, target).unwrap_or_else(|e| panic!("plan does not apply: {e}\n{:#?}", plan.statements()));
    let after = schema_diff(source, &applied, opts);
    assert!(!after.has_differences(), "differences remain after the migration:\n{}\nstatements:\n{}", describe(&after), plan.statements().join(";\n"));
    plan
}

fn pos(stmts: &[String], needle: &str) -> usize {
    stmts.iter().position(|s| s.contains(needle)).unwrap_or_else(|| panic!("no statement containing {needle:?} in\n{}", stmts.join(";\n")))
}

#[test]
fn same_schema_in_two_namespaces_is_identical() {
    let d = schema_diff(&shop("app"), &shop("prod"), &CompareOptions::default());
    assert!(!d.has_differences(), "{}", describe(&d));
    assert_eq!(d.summary.total.changed(), 0);
    assert_eq!(d.summary.total.unchanged, d.items.len());
    assert_eq!(d.items.len(), 3 + 2 + 1 + 2 + 1);
    // types first, then sequences, tables, views, routines
    assert_eq!(d.items[0].kind, ObjectKind::Type);
    assert_eq!(d.items[1].kind, ObjectKind::Sequence);
}

#[test]
fn detects_expected_differences() {
    let d = schema_diff(&shop("app"), &drifted(), &CompareOptions::default());
    let users = item(&d, ObjectKind::Table, "users");
    assert_eq!(users.status, DiffStatus::Modified);
    let email = child(users, ObjectKind::Column, "email");
    assert_eq!(email.changes[0].property, "type");
    assert_eq!(child(users, ObjectKind::Column, "name").changes[0].property, "nullable");
    assert_eq!(child(users, ObjectKind::Column, "created_at").status, DiffStatus::Added);
    assert_eq!(child(users, ObjectKind::Column, "legacy_flag").status, DiffStatus::Removed);
    assert_eq!(users.children.len(), 4, "{}", describe(&d));

    let orders = item(&d, ObjectKind::Table, "orders");
    assert_eq!(child(orders, ObjectKind::Column, "status").changes[0].property, "default");
    assert_eq!(child(orders, ObjectKind::CheckConstraint, "orders_total_check").changes[0].property, "expression");
    assert_eq!(child(orders, ObjectKind::Index, "idx_orders_user").status, DiffStatus::Added);
    assert_eq!(child(orders, ObjectKind::Index, "idx_orders_status").status, DiffStatus::Removed);
    assert_eq!(child(orders, ObjectKind::ForeignKey, "fk_orders_user").changes[0].property, "on_delete");
    // the qualified type and the sequence default are equal across schemas
    assert!(orders.children.iter().all(|c| c.name != "invoice_no"));

    assert_eq!(item(&d, ObjectKind::Table, "order_items").status, DiffStatus::Added);
    assert_eq!(item(&d, ObjectKind::Table, "old_logs").status, DiffStatus::Removed);
    assert_eq!(item(&d, ObjectKind::View, "v_user_orders").status, DiffStatus::Unchanged);
    assert_eq!(item(&d, ObjectKind::View, "v_big_orders").status, DiffStatus::Modified);
    assert_eq!(item(&d, ObjectKind::View, "v_logs").status, DiffStatus::Removed);
    assert_eq!(item(&d, ObjectKind::Sequence, "invoice_seq").changes[0].property, "increment");
    assert_eq!(item(&d, ObjectKind::Sequence, "old_seq").status, DiffStatus::Removed);
    assert_eq!(item(&d, ObjectKind::Function, "order_total").status, DiffStatus::Modified);
    assert_eq!(item(&d, ObjectKind::Function, "audit").status, DiffStatus::Unchanged);
    assert_eq!(item(&d, ObjectKind::Function, "legacy_fn").status, DiffStatus::Removed);
    assert_eq!(item(&d, ObjectKind::Type, "order_status").status, DiffStatus::Unchanged);
    let tables = d.summary.by_kind[&ObjectKind::Table];
    assert_eq!((tables.added, tables.removed, tables.modified, tables.unchanged), (1, 1, 2, 0));
}

#[test]
fn full_sync_script_makes_schemas_identical() {
    let src = shop("app");
    let tgt = drifted();
    let plan = self_check(&src, &tgt, &CompareOptions::default(), &ScriptOptions::full_sync(), &GenericDialect::new());
    let st = plan.statements();
    // qualified with the TARGET schema, never the source schema
    assert!(st.iter().all(|s| !s.contains("app.")), "{}", st.join(";\n"));
    assert!(st.iter().any(|s| s.starts_with("CREATE TABLE prod.order_items")));
    // dependency-safe order
    assert!(pos(&st, "DROP CONSTRAINT fk_orders_user") < pos(&st, "ADD CONSTRAINT fk_orders_user"));
    assert!(pos(&st, "DROP VIEW prod.v_big_orders") < pos(&st, "DROP VIEW prod.v_user_orders"));
    assert!(pos(&st, "DROP VIEW prod.v_user_orders") < pos(&st, "ALTER TABLE prod.users DROP COLUMN legacy_flag"));
    assert!(pos(&st, "DROP VIEW prod.v_logs") < pos(&st, "DROP TABLE prod.old_logs"));
    assert!(pos(&st, "DROP INDEX prod.idx_orders_status") < pos(&st, "CREATE INDEX idx_orders_user"));
    assert!(pos(&st, "CREATE TABLE prod.order_items") < pos(&st, "ADD CONSTRAINT fk_items_order"));
    assert!(pos(&st, "ALTER COLUMN email SET DATA TYPE varchar(255)") < pos(&st, "CREATE VIEW prod.v_user_orders"));
    assert!(pos(&st, "CREATE VIEW prod.v_user_orders") < pos(&st, "CREATE VIEW prod.v_big_orders"));
    assert!(pos(&st, "ALTER SEQUENCE prod.invoice_seq INCREMENT BY 1") < pos(&st, "CREATE TABLE prod.order_items"));
    assert!(st.iter().any(|s| s.contains("ADD COLUMN created_at timestamp DEFAULT now()")));
    assert!(st.iter().any(|s| s.contains("ALTER COLUMN name DROP NOT NULL")));
    assert!(st.iter().any(|s| s.contains("ALTER COLUMN status SET DEFAULT 'new'")));
    assert!(st.iter().any(|s| s.contains("ON DELETE CASCADE") && s.contains("fk_orders_user")));
    assert!(st.iter().any(|s| s == "DROP SEQUENCE prod.old_seq"));
    assert!(st.iter().any(|s| s == "DROP FUNCTION prod.legacy_fn"));
    assert!(st.iter().any(|s| s.starts_with("CREATE OR REPLACE FUNCTION prod.order_total")));
    assert!(!st.iter().any(|s| s.contains("DROP FUNCTION prod.order_total")), "replaceable routines are not dropped");
    // no statement touches unchanged objects
    assert!(!st.iter().any(|s| s.contains("audit")));
    assert!(!st.iter().any(|s| s.contains("idx_users_name") || s.contains("users_email_key")));
    assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
}

#[test]
fn additive_script_keeps_target_only_objects() {
    let src = shop("app");
    let tgt = drifted();
    let diff = schema_diff(&src, &tgt, &CompareOptions::default());
    let plan = migration_plan(&diff, &src, &tgt, &GenericDialect::new(), &ScriptOptions::default());
    let st = plan.statements();
    assert!(!st.iter().any(|s| s.contains("DROP TABLE") || s.contains("DROP COLUMN") || s.contains("DROP SEQUENCE prod.old_seq")));
    assert!(!st.iter().any(|s| s.contains("legacy_fn") || s.contains("idx_orders_status") || s.contains("v_logs")));
    let applied = apply_plan(&plan, &tgt).unwrap();
    let after = schema_diff(&src, &applied, &CompareOptions::default());
    for i in after.changed_items() {
        match i.status {
            DiffStatus::Removed => {}
            DiffStatus::Modified => {
                assert!(i.changes.is_empty(), "{}", describe(&after));
                assert!(i.children.iter().all(|c| c.status == DiffStatus::Removed), "{}", describe(&after));
            }
            other => panic!("unexpected {other:?} {} after additive migration:\n{}", i.name, describe(&after)),
        }
    }
}

#[test]
fn foreign_keys_are_recreated_around_referenced_key_changes() {
    let src = shop("app");
    let mut tgt = shop("prod");
    // the referenced primary key differs only by name
    table(&mut tgt, "users").primary_key = Some(key("users_pk_old", &["id"]));
    let plan = self_check(&src, &tgt, &CompareOptions::default(), &ScriptOptions::full_sync(), &GenericDialect::new());
    let st = plan.statements();
    assert!(pos(&st, "DROP CONSTRAINT fk_orders_user") < pos(&st, "DROP CONSTRAINT users_pk_old"));
    assert!(pos(&st, "ADD CONSTRAINT users_pkey PRIMARY KEY") < pos(&st, "ADD CONSTRAINT fk_orders_user"));
    // with structural matching there is nothing to do
    let opts = CompareOptions { ignore_constraint_names: true, ..Default::default() };
    let d = schema_diff(&src, &tgt, &opts);
    assert!(!d.has_differences(), "{}", describe(&d));
}

#[test]
fn foreign_keys_are_recreated_around_type_changes() {
    let mut src = shop("app");
    let tgt = shop("prod");
    table(&mut src, "users").columns[0].data_type = "bigint".into();
    table(&mut src, "orders").columns[1].data_type = "bigint".into();
    let plan = self_check(&src, &tgt, &CompareOptions::default(), &ScriptOptions::full_sync(), &GenericDialect::new());
    let st = plan.statements();
    assert!(pos(&st, "DROP CONSTRAINT fk_orders_user") < pos(&st, "ALTER TABLE prod.users ALTER COLUMN id SET DATA TYPE bigint"));
    assert!(pos(&st, "ALTER TABLE prod.orders ALTER COLUMN user_id SET DATA TYPE bigint") < pos(&st, "ADD CONSTRAINT fk_orders_user"));
    // order_items references orders.id, which does not change
    assert!(!st.iter().any(|s| s.contains("fk_items_order")));
    // views over users are rebuilt because a column type changes
    assert!(pos(&st, "DROP VIEW prod.v_user_orders") < pos(&st, "ALTER TABLE prod.users ALTER COLUMN id"));
}

#[test]
fn type_aliases_and_custom_normalizer() {
    let mut src = shop("app");
    let mut tgt = shop("prod");
    table(&mut src, "users").columns[0].data_type = "int4".into();
    table(&mut tgt, "users").columns[1].data_type = "character varying(255)".into();
    let d = schema_diff(&src, &tgt, &CompareOptions::default());
    assert!(!d.has_differences(), "{}", describe(&d));
    let strict = CompareOptions { normalize_type_aliases: false, ..Default::default() };
    let d = schema_diff(&src, &tgt, &strict);
    let users = item(&d, ObjectKind::Table, "users");
    assert_eq!(users.children.len(), 2);
    // a custom rule on top of the built-in normalisation
    table(&mut src, "orders").columns[3].data_type = "money".into();
    let d = schema_diff(&src, &tgt, &CompareOptions::default());
    assert!(d.has_differences());
    let custom = CompareOptions::default().with_type_normalizer(|t| if t == "money" { "numeric(12,2)".into() } else { t.to_string() });
    let d = schema_diff(&src, &tgt, &custom);
    assert!(!d.has_differences(), "{}", describe(&d));
}

#[test]
fn table_mapping_and_identifier_case() {
    let src = shop("app");
    let mut tgt = shop("prod");
    table(&mut tgt, "users").name = "customers".into();
    for t in &mut tgt.tables {
        for f in &mut t.foreign_keys {
            if f.ref_table == "users" {
                f.ref_table = "customers".into();
            }
        }
    }
    for v in &mut tgt.views {
        v.definition = v.definition.replace("prod.users", "prod.customers");
    }
    let mut opts = CompareOptions::default();
    let d = schema_diff(&src, &tgt, &opts);
    assert_eq!(item(&d, ObjectKind::Table, "users").status, DiffStatus::Added);
    opts.table_mapping.insert("users".into(), "customers".into());
    let d = schema_diff(&src, &tgt, &opts);
    let users = item(&d, ObjectKind::Table, "users");
    assert_eq!(users.target_name.as_deref(), Some("customers"));
    assert_eq!(users.status, DiffStatus::Unchanged, "{}", describe(&d));
    // the view text still names the source table: only the view differs
    assert_eq!(d.summary.total.changed(), 1, "{}", describe(&d));

    // identifier case
    let mut upper = shop("prod");
    table(&mut upper, "users").columns[1].name = "EMAIL".into();
    let d = schema_diff(&src, &upper, &CompareOptions::default());
    assert!(d.has_differences());
    let ci = CompareOptions { ignore_identifier_case: true, ..Default::default() };
    let d = schema_diff(&src, &upper, &ci);
    let users = item(&d, ObjectKind::Table, "users");
    assert!(users.children.is_empty(), "{}", describe(&d));
}

#[test]
fn known_column_rename_is_scripted_as_rename() {
    let src = shop("app");
    let mut tgt = shop("prod");
    table(&mut tgt, "users").columns[2].name = "full_name".into();
    table(&mut tgt, "users").indexes[0].columns[0].name = "full_name".into();
    let mut opts = CompareOptions::default();
    opts.column_renames.push(ColumnRename { table: "users".into(), from: "full_name".into(), to: "name".into() });
    let d = schema_diff(&src, &tgt, &opts);
    let users = item(&d, ObjectKind::Table, "users");
    let c = &users.children[0];
    assert_eq!((c.name.as_str(), c.target_name.as_deref(), c.status), ("name", Some("full_name"), DiffStatus::Modified));
    let plan = self_check(&src, &tgt, &opts, &ScriptOptions::full_sync(), &GenericDialect::new());
    let st = plan.statements();
    assert!(st.iter().any(|s| s.contains("RENAME COLUMN full_name TO name")), "{}", st.join(";\n"));
    assert!(!st.iter().any(|s| s.contains("DROP COLUMN")));
}

#[test]
fn cross_dialect_compares_logical_types() {
    let mut mysql = SchemaSnapshot { kind: Some(DbKind::MySql), schema: Some("shop".into()), ..Default::default() };
    let mut t = TableDef::new(Some("shop".into()), "Users");
    t.columns = vec![
        col("ID", "int(11)").not_null().auto_increment(),
        col("Email", "varchar(255)").not_null(),
        col("Created", "datetime").default_value("CURRENT_TIMESTAMP"),
        col("Active", "tinyint(1)"),
    ];
    t.primary_key = Some(KeyDef { name: Some("PRIMARY".into()), columns: vec!["ID".into()] });
    mysql.tables.push(t);
    let mut pg = SchemaSnapshot { kind: Some(DbKind::Postgres), schema: Some("public".into()), ..Default::default() };
    let mut u = TableDef::new(Some("public".into()), "users");
    u.columns = vec![
        col("id", "integer").not_null().default_value("nextval('users_id_seq'::regclass)"),
        col("email", "character varying(255)").not_null(),
        col("created", "timestamp without time zone").default_value("now()"),
        col("active", "boolean"),
    ];
    u.primary_key = Some(key("users_pkey", &["id"]));
    pg.tables.push(u);
    let d = schema_diff(&mysql, &pg, &CompareOptions::cross_dialect());
    assert!(!d.has_differences(), "{}", describe(&d));
    let d = schema_diff(&mysql, &pg, &CompareOptions::default());
    assert!(d.has_differences());
}

#[test]
fn view_chains_are_rebuilt_in_dependency_order() {
    let mut src = shop("app");
    let mut extra = src.views[1].clone();
    extra.name = "v_top".into();
    extra.definition = "SELECT id FROM app.v_big_orders".into();
    src.views.push(extra);
    let mut tgt = shop("prod");
    let mut extra = tgt.views[1].clone();
    extra.name = "v_top".into();
    extra.definition = "SELECT id FROM prod.v_big_orders".into();
    tgt.views.push(extra);
    // dropping a column used by the base view forces the whole chain to be rebuilt
    table(&mut tgt, "users").columns.push(col("nickname", "text"));
    let plan = self_check(&src, &tgt, &CompareOptions::default(), &ScriptOptions::full_sync(), &GenericDialect::new());
    let st = plan.statements();
    assert!(pos(&st, "DROP VIEW prod.v_top") < pos(&st, "DROP VIEW prod.v_big_orders"));
    assert!(pos(&st, "DROP VIEW prod.v_big_orders") < pos(&st, "DROP VIEW prod.v_user_orders"));
    assert!(pos(&st, "DROP COLUMN nickname") < pos(&st, "CREATE VIEW prod.v_user_orders"));
    assert!(pos(&st, "CREATE VIEW prod.v_user_orders") < pos(&st, "CREATE VIEW prod.v_big_orders"));
    assert!(pos(&st, "CREATE VIEW prod.v_big_orders") < pos(&st, "CREATE VIEW prod.v_top"));
    // adding a column does not touch dependent views
    let mut tgt2 = shop("prod");
    table(&mut tgt2, "users").columns.retain(|c| c.name != "created_at");
    let plan = self_check(&shop("app"), &tgt2, &CompareOptions::default(), &ScriptOptions::full_sync(), &GenericDialect::new());
    assert!(!plan.statements().iter().any(|s| s.contains("VIEW")), "{:?}", plan.statements());
}

#[test]
fn replaceable_view_uses_create_or_replace() {
    let mut src = shop("app");
    let mut tgt = shop("prod");
    let cols = |names: &[&str]| names.iter().map(|n| col(n, "integer")).collect::<Vec<_>>();
    src.views[1].columns = cols(&["id", "email", "extra"]);
    src.views[1].definition = "SELECT id, email, 1 AS extra FROM app.v_user_orders WHERE orders > 10".into();
    tgt.views[1].columns = cols(&["id", "email"]);
    let plan = self_check(&src, &tgt, &CompareOptions::default(), &ScriptOptions::full_sync(), &GenericDialect::new());
    let st = plan.statements();
    assert_eq!(st.len(), 1, "{st:?}");
    assert!(st[0].starts_with("CREATE OR REPLACE VIEW prod.v_big_orders"));
}

#[test]
fn routine_signature_change_drops_dependents_first() {
    let mut src = shop("app");
    let tgt = shop("prod");
    // audit() now returns a different type: CREATE OR REPLACE cannot do that
    src.routines[0].returns = Some("event_trigger".into());
    src.routines[0].definition = src.routines[0].definition.replace("RETURNS trigger", "RETURNS event_trigger");
    let plan = self_check(&src, &tgt, &CompareOptions::default(), &ScriptOptions::full_sync(), &GenericDialect::new());
    let st = plan.statements();
    assert!(pos(&st, "DROP TRIGGER trg_orders_audit") < pos(&st, "DROP FUNCTION prod.audit"));
    assert!(pos(&st, "DROP FUNCTION prod.audit") < pos(&st, "RETURNS event_trigger"));
    assert!(pos(&st, "RETURNS event_trigger") < pos(&st, "CREATE TRIGGER trg_orders_audit"));
}

#[test]
fn dropped_tables_referencing_each_other() {
    let src = shop("app");
    let mut tgt = shop("prod");
    let t = |name: &str, refs: Option<&str>| {
        let mut x = TableDef::new(Some("prod".into()), name);
        x.columns = vec![col("id", "integer").not_null(), col("ref_id", "integer")];
        x.primary_key = Some(key(&format!("{name}_pkey"), &["id"]));
        if let Some(r) = refs {
            x.foreign_keys = vec![fk(&format!("fk_{name}_{r}"), &["ref_id"], "prod", r, &["id"], FkAction::NoAction)];
        }
        x
    };
    // a <- b <- c is a chain, x <-> y is a cycle
    tgt.tables.extend([t("a", None), t("b", Some("a")), t("c", Some("b")), t("x", Some("y")), t("y", Some("x"))]);
    let plan = self_check(&src, &tgt, &CompareOptions::default(), &ScriptOptions::full_sync(), &GenericDialect::new());
    let st = plan.statements();
    assert!(pos(&st, "DROP TABLE prod.c") < pos(&st, "DROP TABLE prod.b"));
    assert!(pos(&st, "DROP TABLE prod.b") < pos(&st, "DROP TABLE prod.a"));
    assert!(pos(&st, "DROP CONSTRAINT fk_x_y") < pos(&st, "DROP TABLE prod.x"));
    assert!(pos(&st, "DROP CONSTRAINT fk_y_x") < pos(&st, "DROP TABLE prod.x"));
    assert!(!st.iter().any(|s| s.contains("fk_b_a") || s.contains("fk_c_b")), "chain needs no explicit FK drops");
    // with CASCADE the tables are simply dropped
    let cascade = ScriptOptions { cascade: true, ..ScriptOptions::full_sync() };
    let plan = self_check(&src, &tgt, &CompareOptions::default(), &cascade, &GenericDialect::new());
    assert!(plan.statements().iter().any(|s| s == "DROP TABLE prod.x CASCADE"));
    assert!(!plan.statements().iter().any(|s| s.contains("DROP CONSTRAINT")));
}

#[test]
fn exclusions_and_disabled_categories() {
    let src = shop("app");
    let tgt = drifted();
    let opts = CompareOptions { exclude: vec!["old_*".into(), "v_*".into()], ..Default::default() };
    let d = schema_diff(&src, &tgt, &opts);
    assert!(d.items.iter().all(|i| !i.name.starts_with("old_") && !i.name.starts_with("v_")));
    let opts = CompareOptions { indexes: false, foreign_keys: false, checks: false, ..Default::default() };
    let d = schema_diff(&src, &tgt, &opts);
    let orders = item(&d, ObjectKind::Table, "orders");
    assert!(orders.children.iter().all(|c| c.kind == ObjectKind::Column), "{}", describe(&d));
    // disabled categories are left alone by the script
    let plan = self_check(&src, &tgt, &opts, &ScriptOptions::full_sync(), &GenericDialect::new());
    assert!(!plan.statements().iter().any(|s| s.contains("idx_orders") || s.contains("fk_orders_user") || s.contains("CHECK")));
}

#[test]
fn deselected_items_are_not_scripted() {
    let src = shop("app");
    let tgt = drifted();
    let mut d = schema_diff(&src, &tgt, &CompareOptions::default());
    d.retain(|i| i.kind == ObjectKind::Sequence);
    assert_eq!(d.summary.total.total(), 2);
    let st = migration_script(&d, &src, &tgt, &GenericDialect::new(), &ScriptOptions { include_comments: false, ..ScriptOptions::full_sync() });
    assert_eq!(st, vec!["ALTER SEQUENCE prod.invoice_seq INCREMENT BY 1".to_string(), "DROP SEQUENCE prod.old_seq".to_string()]);
}

#[test]
fn comments_and_transaction_wrapping() {
    let src = shop("app");
    let tgt = drifted();
    let d = schema_diff(&src, &tgt, &CompareOptions::default());
    let opts = ScriptOptions { wrap_in_transaction: true, ..ScriptOptions::full_sync() };
    let st = migration_script(&d, &src, &tgt, &GenericDialect::new(), &opts);
    assert_eq!(st.first().map(String::as_str), Some("BEGIN"));
    assert_eq!(st.last().map(String::as_str), Some("COMMIT"));
    assert!(st[1].starts_with("-- ==== Drop foreign keys ====\n-- Drop foreign key fk_orders_user on prod.orders\nALTER TABLE"), "{}", st[1]);
    // the first statement of every step carries its description
    let plan = migration_plan(&d, &src, &tgt, &GenericDialect::new(), &opts);
    assert_eq!(st.iter().filter(|s| s.starts_with("-- ")).count(), plan.steps.len());
    assert_eq!(st.len(), plan.statements().len() + 2);
    let bare = migration_script(&d, &src, &tgt, &GenericDialect::new(), &ScriptOptions { include_comments: false, ..opts.clone() });
    assert!(bare[1..bare.len() - 1].iter().all(|s| !s.contains("--")));
    let none = migration_script(&schema_diff(&src, &src, &CompareOptions::default()), &src, &src, &GenericDialect::new(), &opts);
    assert!(none.is_empty(), "no transaction for an empty script");
}

#[test]
fn whole_table_mode_passes_full_definitions() {
    let src = shop("app");
    let tgt = drifted();
    let opts = ScriptOptions { alter_mode: AlterMode::WholeTable, ..ScriptOptions::full_sync() };
    let plan = self_check(&src, &tgt, &CompareOptions::default(), &opts, &GenericDialect::new());
    let st = plan.statements();
    // the generic generator turns the full definitions into constraint statements itself
    assert!(st.iter().any(|s| s.contains("ADD CONSTRAINT fk_orders_user")));
    assert!(st.iter().any(|s| s.contains("CREATE TABLE prod.order_items") && s.contains("FOREIGN KEY (order_id)")));
    assert!(plan.steps.iter().all(|s| !matches!(s.op, MigrationOp::DropIndex { .. } | MigrationOp::AddForeignKey { .. })));
}

#[test]
fn simulation_rejects_unsafe_order() {
    let src = shop("app");
    let tgt = drifted();
    let d = schema_diff(&src, &tgt, &CompareOptions::default());
    let mut plan = migration_plan(&d, &src, &tgt, &GenericDialect::new(), &ScriptOptions::full_sync());
    // move "drop table old_logs" before "drop view v_logs"
    let drop_table = plan.steps.iter().position(|s| matches!(&s.op, MigrationOp::DropTable { table, .. } if table.name == "old_logs")).unwrap();
    let step = plan.steps.remove(drop_table);
    plan.steps.insert(0, step);
    let err = apply_plan(&plan, &tgt).unwrap_err();
    assert!(err.contains("old_logs") && err.contains("v_logs"), "{err}");
}

#[test]
fn unused_modified_type_is_recreated_used_type_warns() {
    use dbiji_core::dialect::{SqlSyntax, ansi::AnsiDdl};
    struct Ddl(AnsiDdl);
    impl DdlGenerator for Ddl {
        fn create_table(&self, t: &TableDef, fks: bool) -> Vec<String> {
            self.0.create_table(t, fks)
        }
        fn drop_table(&self, s: Option<&str>, n: &str, c: bool) -> String {
            self.0.drop_table(s, n, c)
        }
        fn rename_table(&self, s: Option<&str>, a: &str, b: &str) -> String {
            self.0.rename_table(s, a, b)
        }
        fn alter_table(&self, f: &TableDef, t: &TableDef, r: &[(String, String)]) -> Vec<String> {
            self.0.alter_table(f, t, r)
        }
        fn add_column(&self, t: &TableDef, c: &ColumnDef) -> Vec<String> {
            self.0.add_column(t, c)
        }
        fn drop_column(&self, t: &TableDef, c: &str) -> String {
            self.0.drop_column(t, c)
        }
        fn create_index(&self, t: &TableDef, i: &IndexDef) -> String {
            self.0.create_index(t, i)
        }
        fn drop_index(&self, t: &TableDef, i: &IndexDef) -> String {
            self.0.drop_index(t, i)
        }
        fn add_primary_key(&self, t: &TableDef, k: &KeyDef) -> String {
            self.0.add_primary_key(t, k)
        }
        fn drop_primary_key(&self, t: &TableDef, k: &KeyDef) -> String {
            self.0.drop_primary_key(t, k)
        }
        fn add_unique_key(&self, t: &TableDef, k: &KeyDef) -> String {
            self.0.add_unique_key(t, k)
        }
        fn drop_unique_key(&self, t: &TableDef, k: &KeyDef) -> String {
            self.0.drop_unique_key(t, k)
        }
        fn add_check(&self, t: &TableDef, c: &CheckDef) -> String {
            self.0.add_check(t, c)
        }
        fn drop_check(&self, t: &TableDef, c: &CheckDef) -> String {
            self.0.drop_check(t, c)
        }
        fn add_foreign_key(&self, t: &TableDef, f: &ForeignKeyDef) -> String {
            self.0.add_foreign_key(t, f)
        }
        fn drop_foreign_key(&self, t: &TableDef, f: &ForeignKeyDef) -> String {
            self.0.drop_foreign_key(t, f)
        }
        fn create_trigger(&self, t: &TableDef, tr: &TriggerDef) -> Vec<String> {
            self.0.create_trigger(t, tr)
        }
        fn drop_trigger(&self, t: &TableDef, tr: &TriggerDef) -> String {
            self.0.drop_trigger(t, tr)
        }
        fn create_view(&self, v: &ViewDef, r: bool) -> Vec<String> {
            self.0.create_view(v, r)
        }
        fn drop_view(&self, v: &ViewDef) -> String {
            self.0.drop_view(v)
        }
        fn create_sequence(&self, s: &SequenceDef) -> String {
            self.0.create_sequence(s)
        }
        fn alter_sequence(&self, f: &SequenceDef, t: &SequenceDef) -> Vec<String> {
            self.0.alter_sequence(f, t)
        }
        fn drop_sequence(&self, s: &SequenceDef) -> String {
            self.0.drop_sequence(s)
        }
        fn create_routine(&self, r: &RoutineDef) -> Vec<String> {
            self.0.create_routine(r)
        }
        fn drop_routine(&self, r: &RoutineDef) -> String {
            self.0.drop_routine(r)
        }
        fn drop_type(&self, t: &TypeDef) -> Option<String> {
            Some(format!("DROP TYPE {}", self.0.q().qualified(t.schema.as_deref(), &t.name)))
        }
        fn create_schema(&self, n: &str) -> String {
            self.0.create_schema(n)
        }
        fn drop_schema(&self, n: &str, c: bool) -> String {
            self.0.drop_schema(n, c)
        }
        fn map_type(&self, s: DbKind, t: &str, k: dbiji_core::DataKind) -> String {
            self.0.map_type(s, t, k)
        }
        fn default_type_for(&self, k: dbiji_core::DataKind) -> String {
            self.0.default_type_for(k)
        }
    }
    struct D(Ddl, SqlSyntax);
    impl Dialect for D {
        fn kind(&self) -> DbKind {
            DbKind::Postgres
        }
        fn syntax(&self) -> &SqlSyntax {
            &self.1
        }
        fn ddl(&self) -> &dyn DdlGenerator {
            &self.0
        }
    }
    let dialect = D(Ddl(AnsiDdl::new(SqlSyntax::ansi())), SqlSyntax::ansi());
    let mut src = shop("app");
    let mut tgt = shop("prod");
    let mut mood = TypeDef {
        schema: Some("app".into()),
        name: "mood".into(),
        category: "enum".into(),
        elements: vec!["ok".into()],
        definition: "CREATE TYPE app.mood AS ENUM ('ok')".into(),
        comment: None,
    };
    src.types.push(mood.clone());
    mood.schema = Some("prod".into());
    mood.elements = vec!["ok".into(), "sad".into()];
    mood.definition = "CREATE TYPE prod.mood AS ENUM ('ok', 'sad')".into();
    tgt.types.push(mood);
    let plan = self_check(&src, &tgt, &CompareOptions::default(), &ScriptOptions::full_sync(), &dialect);
    assert_eq!(plan.statements(), vec!["DROP TYPE prod.mood".to_string(), "CREATE TYPE prod.mood AS ENUM ('ok')".to_string()]);
    // order_status is used by orders.status: it is reported, not dropped
    tgt.types[0].elements.pop();
    tgt.types[0].definition = "CREATE TYPE prod.order_status AS ENUM ('new', 'paid')".into();
    let d = schema_diff(&src, &tgt, &CompareOptions::default());
    let plan = migration_plan(&d, &src, &tgt, &dialect, &ScriptOptions::full_sync());
    assert!(plan.warnings.iter().any(|w| w.contains("order_status")), "{:?}", plan.warnings);
    assert!(!plan.statements().iter().any(|s| s.contains("order_status")));
}

#[test]
fn new_type_and_sequence_are_created_before_tables() {
    let src = shop("app");
    let mut tgt = shop("prod");
    tgt.types.clear();
    tgt.sequences.clear();
    tgt.tables.retain(|t| t.name == "users");
    tgt.views.clear();
    tgt.routines.retain(|r| r.name == "audit");
    let plan = self_check(&src, &tgt, &CompareOptions::default(), &ScriptOptions::full_sync(), &GenericDialect::new());
    let st = plan.statements();
    assert!(pos(&st, "CREATE TYPE prod.order_status") < pos(&st, "CREATE TABLE prod.orders"));
    assert!(pos(&st, "CREATE SEQUENCE prod.invoice_seq") < pos(&st, "CREATE TABLE prod.orders"));
    assert!(pos(&st, "CREATE TABLE prod.orders") < pos(&st, "CREATE TRIGGER trg_orders_audit"));
    assert!(st.iter().any(|s| s.contains("nextval('prod.invoice_seq'::regclass)")));
    assert!(st.iter().any(|s| s.contains("status prod.order_status")));
}

#[test]
fn index_definitions_are_requalified() {
    let mut src = shop("app");
    let tgt = shop("prod");
    let mut gin = idx("idx_users_email_trgm", &["email"]);
    gin.method = Some("gin".into());
    gin.definition = Some("CREATE INDEX idx_users_email_trgm ON app.users USING gin (email gin_trgm_ops)".into());
    table(&mut src, "users").indexes.push(gin);
    let plan = self_check(&src, &tgt, &CompareOptions::default(), &ScriptOptions::full_sync(), &GenericDialect::new());
    let created = plan
        .steps
        .iter()
        .find_map(|s| match &s.op {
            MigrationOp::CreateIndex { index, .. } => Some(index.clone()),
            _ => None,
        })
        .expect("index step");
    assert_eq!(created.definition.as_deref(), Some("CREATE INDEX idx_users_email_trgm ON prod.users USING gin (email gin_trgm_ops)"));
}

#[test]
fn schema_is_inferred_when_snapshots_do_not_name_it() {
    let mut src = shop("app");
    let mut tgt = shop("prod");
    src.schema = None;
    tgt.schema = None;
    tgt.views.remove(1);
    let d = schema_diff(&src, &tgt, &CompareOptions::default());
    assert_eq!(d.summary.total.changed(), 1, "{}", describe(&d));
    let opts = ScriptOptions { include_comments: false, ..ScriptOptions::full_sync() };
    let st = migration_script(&d, &src, &tgt, &GenericDialect::new(), &opts);
    assert_eq!(st, vec!["CREATE VIEW prod.v_big_orders AS\nSELECT id, email FROM prod.v_user_orders WHERE orders > 10".to_string()]);
    // an explicit rename overrides the inferred one
    let opts = ScriptOptions { schema_rename: Some(("app".into(), "reporting".into())), ..opts };
    let st = migration_script(&d, &src, &tgt, &GenericDialect::new(), &opts);
    assert!(st[0].contains("FROM reporting.v_user_orders"), "{st:?}");
}

#[test]
fn html_report_is_self_contained_and_escaped() {
    let mut src = shop("app");
    src.tables[0].comment = Some("<script>alert('x')</script>".into());
    let d = schema_diff(&src, &drifted(), &CompareOptions::default());
    let html = html_report(&d, "staging <db>", "prod & co");
    assert!(html.starts_with("<!DOCTYPE html>"));
    assert!(html.contains("<style>"));
    assert!(!html.contains("<script"), "unescaped script tag");
    assert!(html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"));
    assert!(html.contains("staging &lt;db&gt;") && html.contains("prod &amp; co"));
    assert!(!html.contains("src=") && !html.contains("<link"), "no external resources");
    assert!(html.contains("class=\"badge added\"") && html.contains("class=\"badge removed\"") && html.contains("class=\"badge modified\""));
    assert!(html.contains("<span class=\"n\">1</span>Added"), "summary card for the added table");
    assert!(html.contains("Unchanged objects"));
    let same = html_report(&schema_diff(&src, &src, &CompareOptions::default()), "a", "b");
    assert!(same.contains("The schemas are identical."));
}

#[test]
fn diagram_status_lists_tables_and_views() {
    let mut opts = CompareOptions::default();
    let mut tgt = drifted();
    table(&mut tgt, "orders").name = "purchases".into();
    opts.table_mapping.insert("orders".into(), "purchases".into());
    let d = schema_diff(&shop("app"), &tgt, &opts);
    let m = diagram_status(&d);
    assert_eq!(m["app.users"], DiffStatus::Modified);
    assert_eq!(m["prod.users"], DiffStatus::Modified);
    assert_eq!(m["app.orders"], DiffStatus::Modified);
    assert_eq!(m["prod.purchases"], DiffStatus::Modified);
    assert_eq!(m["app.order_items"], DiffStatus::Added);
    assert_eq!(m["prod.old_logs"], DiffStatus::Removed);
    assert_eq!(m["app.v_user_orders"], DiffStatus::Unchanged);
    assert!(!m.contains_key("app.invoice_seq"));
}

mod liquibase {
    use super::*;
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;
    use std::collections::BTreeMap;

    /// Parse the document, checking well-formedness; returns element counts
    /// and the attributes of the root element.
    fn parse(xml: &str) -> (BTreeMap<String, usize>, Vec<(String, String)>) {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().enable_all_checks(true);
        let mut counts = BTreeMap::new();
        let mut root_attrs = Vec::new();
        let mut depth = 0i64;
        loop {
            let (start, e) = match reader.read_event() {
                Ok(Event::Start(e)) => (true, e),
                Ok(Event::Empty(e)) => (false, e),
                Ok(Event::End(_)) => {
                    depth -= 1;
                    continue;
                }
                Ok(Event::Eof) => break,
                Ok(_) => continue,
                Err(e) => panic!("invalid XML: {e}\n{xml}"),
            };
            let name = e.name().as_ref().to_string();
            if counts.is_empty() {
                for a in e.attributes() {
                    let a = a.expect("valid attribute");
                    root_attrs.push((a.key.as_ref().to_string(), a.normalized_value(quick_xml::XmlVersion::Explicit1_0).expect("valid value").to_string()));
                }
            }
            *counts.entry(name).or_insert(0) += 1;
            if start {
                depth += 1;
            }
        }
        assert_eq!(depth, 0, "unbalanced elements");
        (counts, root_attrs)
    }

    #[test]
    fn changelog_is_well_formed_and_complete() {
        let src = shop("app");
        let mut tgt = drifted();
        table(&mut tgt, "users").columns[0].comment = Some("old \"remark\" & <tag>".into());
        let d = schema_diff(&src, &tgt, &CompareOptions::default());
        let opts = LiquibaseOptions { author: "tester".into(), id_prefix: "cmp".into(), script: ScriptOptions::full_sync(), dialect: None };
        let xml = liquibase_changelog(&d, &src, &tgt, &opts);
        let (counts, root) = parse(&xml);
        let has = |k: &str| counts.get(k).copied().unwrap_or(0);
        assert_eq!(has("databaseChangeLog"), 1);
        assert!(root.iter().any(|(k, v)| k == "xmlns" && v == "http://www.liquibase.org/xml/ns/dbchangelog"));
        assert!(root.iter().any(|(k, v)| k == "xsi:schemaLocation" && v.contains("dbchangelog-latest.xsd")));
        for tag in [
            "createTable",
            "dropTable",
            "addColumn",
            "dropColumn",
            "modifyDataType",
            "dropNotNullConstraint",
            "addDefaultValue",
            "createIndex",
            "dropIndex",
            "addForeignKeyConstraint",
            "dropForeignKeyConstraint",
            "createView",
            "dropView",
            "dropSequence",
            "alterSequence",
            "setColumnRemarks",
            "sql",
            "addPrimaryKey",
        ] {
            assert!(has(tag) > 0, "missing {tag} in\n{xml}");
        }
        assert!(has("changeSet") >= 20);
        assert!(xml.contains("id=\"cmp-001\" author=\"tester\""));
        assert!(xml.contains("replaceIfExists=\"true\""));
        assert!(xml.contains("<![CDATA[SELECT id, email FROM prod.v_user_orders WHERE orders > 10]]>"));
        assert!(xml.contains("onDelete=\"CASCADE\""));
        assert!(!xml.contains("app."), "objects are qualified with the target schema");
    }

    #[test]
    fn rename_and_not_null_changes() {
        let src = shop("app");
        let mut tgt = shop("prod");
        table(&mut tgt, "users").columns[2].name = "full_name".into();
        table(&mut tgt, "users").indexes[0].columns[0].name = "full_name".into();
        table(&mut tgt, "users").columns[1].nullable = true;
        let mut opts = CompareOptions::default();
        opts.column_renames.push(ColumnRename { table: "users".into(), from: "full_name".into(), to: "name".into() });
        let d = schema_diff(&src, &tgt, &opts);
        let xml = liquibase_changelog(&d, &src, &tgt, &LiquibaseOptions::default());
        let (counts, _) = parse(&xml);
        assert_eq!(counts.get("renameColumn"), Some(&1), "{xml}");
        assert_eq!(counts.get("addNotNullConstraint"), Some(&1), "{xml}");
        assert!(xml.contains("oldColumnName=\"full_name\" newColumnName=\"name\""));
    }

    #[test]
    fn key_default_and_sequence_changes() {
        let src = shop("app");
        let mut tgt = shop("prod");
        {
            let u = table(&mut tgt, "users");
            u.primary_key = Some(key("users_pk_old", &["id"]));
            u.unique_keys = vec![key("users_email_old", &["email"])];
            u.columns[2].default = Some("'anonymous'".into());
        }
        tgt.sequences.clear();
        let d = schema_diff(&src, &tgt, &CompareOptions::default());
        let opts = LiquibaseOptions { script: ScriptOptions::full_sync(), ..Default::default() };
        let xml = liquibase_changelog(&d, &src, &tgt, &opts);
        let (counts, _) = parse(&xml);
        for tag in [
            "dropPrimaryKey",
            "addPrimaryKey",
            "dropUniqueConstraint",
            "addUniqueConstraint",
            "dropDefaultValue",
            "createSequence",
            "dropForeignKeyConstraint",
            "addForeignKeyConstraint",
        ] {
            assert!(counts.get(tag).copied().unwrap_or(0) > 0, "missing {tag} in\n{xml}");
        }
        assert!(xml.contains("constraintName=\"users_pk_old\""));
        assert!(xml.contains("sequenceName=\"invoice_seq\""));
        assert!(xml.contains("startValue=\"1000\""));
        // the referencing foreign key is dropped before the primary key and re-added after it
        let pos = |needle: &str| xml.find(needle).unwrap_or_else(|| panic!("{needle}"));
        assert!(pos("<dropForeignKeyConstraint") < pos("<dropPrimaryKey"));
        assert!(pos("<addPrimaryKey") < pos("<addForeignKeyConstraint"));
    }

    #[test]
    fn invalid_characters_are_replaced() {
        let mut src = shop("app");
        src.views[0].definition.push_str(" -- \u{1} ]]> end");
        let d = schema_diff(&src, &shop("prod"), &CompareOptions::default());
        let xml = liquibase_changelog(&d, &src, &shop("prod"), &LiquibaseOptions::default());
        parse(&xml);
        assert!(!xml.contains('\u{1}'));
    }
}

/// `DBIJI_SAMPLE_DIR=/some/dir cargo test -p dbiji-compare write_samples -- --ignored`
/// writes a sample report, changelog and script for manual inspection.
#[test]
#[ignore = "writes sample outputs for manual inspection"]
fn write_samples() {
    let src = shop("app");
    let tgt = drifted();
    let d = schema_diff(&src, &tgt, &CompareOptions::default());
    let dir = std::env::var("DBIJI_SAMPLE_DIR").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string());
    std::fs::write(format!("{dir}/report.html"), html_report(&d, "staging (app)", "production (prod)")).unwrap();
    let opts = LiquibaseOptions { script: ScriptOptions::full_sync(), ..Default::default() };
    std::fs::write(format!("{dir}/changelog.xml"), liquibase_changelog(&d, &src, &tgt, &opts)).unwrap();
    let st = migration_script(&d, &src, &tgt, &GenericDialect::new(), &ScriptOptions { wrap_in_transaction: true, ..ScriptOptions::full_sync() });
    std::fs::write(format!("{dir}/migration.sql"), st.join(";\n\n") + ";\n").unwrap();
}
