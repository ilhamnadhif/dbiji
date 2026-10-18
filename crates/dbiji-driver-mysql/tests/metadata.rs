//! Metadata (information_schema) and DDL round trips against real servers.

mod common;

use common::*;
use dbiji_core::ddl::*;
use dbiji_core::meta::*;
use dbiji_core::{DataKind, Session};
use dbiji_driver_mysql::MySqlSession;

async fn create_objects(srv: &Server, s: &mut MySqlSession) {
    exec(
        s,
        "CREATE TABLE parent (
            id INT UNSIGNED NOT NULL AUTO_INCREMENT COMMENT 'surrogate key',
            code VARCHAR(20) NOT NULL,
            name VARCHAR(100) NOT NULL DEFAULT 'it''s',
            PRIMARY KEY (id),
            UNIQUE KEY uk_parent_code (code)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='Parent rows'",
    )
    .await;
    let check = "CONSTRAINT chk_qty CHECK (qty >= 0)";
    exec(
        s,
        &format!(
            "CREATE TABLE child (
                id BIGINT NOT NULL AUTO_INCREMENT,
                parent_id INT UNSIGNED NOT NULL,
                qty INT NOT NULL DEFAULT 1,
                price DECIMAL(10,2) NULL DEFAULT 9.99,
                total DECIMAL(12,2) AS (qty * price) STORED,
                half DECIMAL(12,2) AS (price / 2) VIRTUAL,
                note TEXT,
                body VARCHAR(200) NOT NULL DEFAULT '',
                flags BIT(3) DEFAULT b'101',
                kind ENUM('a','b') NOT NULL DEFAULT 'a',
                created TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated DATETIME(3) NULL DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3),
                PRIMARY KEY (id),
                KEY idx_parent (parent_id),
                KEY idx_body_prefix (body(10), created DESC),
                FULLTEXT KEY ft_note (note),
                {check},
                CONSTRAINT fk_child_parent FOREIGN KEY (parent_id) REFERENCES parent (id) ON DELETE CASCADE ON UPDATE RESTRICT
            ) ENGINE=InnoDB COMMENT='Child rows'"
        ),
    )
    .await;
    exec(
        s,
        "CREATE TABLE parted (id INT NOT NULL, y INT NOT NULL, PRIMARY KEY (id, y))
         PARTITION BY RANGE (y) (PARTITION p2023 VALUES LESS THAN (2024), PARTITION pmax VALUES LESS THAN MAXVALUE)",
    )
    .await;
    exec(s, "CREATE VIEW v_child AS SELECT c.id, p.name, c.qty FROM child c JOIN parent p ON p.id = c.parent_id").await;
    exec(
        s,
        "CREATE FUNCTION f_double(x INT) RETURNS INT DETERMINISTIC COMMENT 'doubles' RETURN x * 2",
    )
    .await;
    exec(s, "CREATE PROCEDURE p_touch(IN pid INT, OUT n INT) BEGIN SELECT COUNT(*) INTO n FROM child WHERE parent_id = pid; END").await;
    exec(s, "CREATE TRIGGER trg_child_bi BEFORE INSERT ON child FOR EACH ROW SET NEW.qty = GREATEST(NEW.qty, 0)").await;
    exec(s, "CREATE EVENT ev_cleanup ON SCHEDULE EVERY 1 DAY DISABLE DO DELETE FROM child WHERE qty = 0").await;
    if srv.is_mariadb() {
        exec(
            s,
            "CREATE SEQUENCE seq_orders START WITH 100 INCREMENT BY 5 MAXVALUE 100000 CACHE 10",
        )
        .await;
    }
    exec(
        s,
        "INSERT INTO parent (code, name) VALUES ('A', 'alpha'), ('B', 'beta')",
    )
    .await;
    exec(s, "INSERT INTO child (parent_id, qty, price, note, body) VALUES (1, 2, 3.50, 'hello world', 'body text')").await;
}

fn names(v: &[ObjectInfo]) -> Vec<String> {
    v.iter().map(|o| o.obj.name.clone()).collect()
}

#[tokio::test]
async fn navigator_metadata() {
    for srv in servers() {
        let db = TestDb::create(&srv).await;
        let mut s = db.session().await;
        create_objects(&srv, &mut s).await;
        let sch = SchemaRef::schema(db.name.clone());

        let schemas = s.list_schemas().await.unwrap();
        let mine = schemas
            .iter()
            .find(|x| x.name == db.name)
            .expect("test schema listed");
        assert!(mine.is_current && !mine.is_system);
        assert!(schemas.iter().any(|x| x.name == "mysql" && x.is_system));
        assert!(
            schemas
                .iter()
                .any(|x| x.name == "information_schema" && x.is_system)
        );

        let tables = s.list_objects(&sch, ObjectKind::Table).await.unwrap();
        assert_eq!(
            names(&tables),
            vec!["child", "parent", "parted"],
            "[{}]",
            srv.name
        );
        let parent = tables.iter().find(|t| t.obj.name == "parent").unwrap();
        assert_eq!(parent.comment.as_deref(), Some("Parent rows"));
        assert_eq!(parent.obj.kind, ObjectKind::Table);
        assert!(
            tables
                .iter()
                .find(|t| t.obj.name == "parted")
                .unwrap()
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("partitioned")
        );
        assert_eq!(
            names(&s.list_objects(&sch, ObjectKind::View).await.unwrap()),
            vec!["v_child"]
        );
        let funcs = s.list_objects(&sch, ObjectKind::Function).await.unwrap();
        assert_eq!(names(&funcs), vec!["f_double"]);
        assert!(
            funcs[0]
                .detail
                .as_deref()
                .unwrap_or("")
                .replace("int(11)", "int")
                .contains("RETURNS int"),
            "{:?}",
            funcs[0].detail
        );
        let procs = s.list_objects(&sch, ObjectKind::Procedure).await.unwrap();
        assert_eq!(names(&procs), vec!["p_touch"]);
        assert!(
            procs[0]
                .detail
                .as_deref()
                .unwrap_or("")
                .replace("int(11)", "int")
                .contains("OUT n int"),
            "{:?}",
            procs[0].detail
        );
        let trg = s.list_objects(&sch, ObjectKind::Trigger).await.unwrap();
        assert_eq!(names(&trg), vec!["trg_child_bi"]);
        assert_eq!(trg[0].obj.table.as_deref(), Some("child"));
        let ev = s.list_objects(&sch, ObjectKind::Event).await.unwrap();
        assert_eq!(names(&ev), vec!["ev_cleanup"]);
        assert!(
            ev[0]
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("EVERY 1 DAY")
        );
        let seqs = s.list_objects(&sch, ObjectKind::Sequence).await.unwrap();
        assert_eq!(
            names(&seqs),
            if srv.is_mariadb() {
                vec!["seq_orders".to_string()]
            } else {
                Vec::<String>::new()
            }
        );

        // children
        let child = ObjectRef::table(Some(db.name.clone()), "child");
        let cols = s.list_children(&child, ObjectKind::Column).await.unwrap();
        assert_eq!(cols.len(), 12);
        assert_eq!(cols[0].obj.table.as_deref(), Some("child"));
        let d0 = cols[0]
            .detail
            .as_deref()
            .unwrap()
            .replace("bigint(20)", "bigint");
        assert!(d0.contains("bigint NOT NULL auto_increment PK"), "{d0}");
        let idx = s.list_children(&child, ObjectKind::Index).await.unwrap();
        assert_eq!(
            names(&idx),
            vec![
                "PRIMARY",
                "fk_child_parent",
                "ft_note",
                "idx_body_prefix",
                "idx_parent"
            ]
            .into_iter()
            .filter(|n| idx.iter().any(|i| i.obj.name == *n))
            .map(String::from)
            .collect::<Vec<_>>()
        );
        assert!(idx.iter().any(|i| {
            i.obj.name == "idx_body_prefix"
                && i.detail
                    .as_deref()
                    .unwrap()
                    .contains("body(10), created DESC")
        }));
        assert!(
            idx.iter()
                .any(|i| i.obj.name == "ft_note"
                    && i.detail.as_deref().unwrap().contains("FULLTEXT"))
        );
        let pk = s
            .list_children(&child, ObjectKind::PrimaryKey)
            .await
            .unwrap();
        assert_eq!(pk.len(), 1);
        assert_eq!(pk[0].detail.as_deref(), Some("(id)"));
        let uk = s
            .list_children(
                &ObjectRef::table(Some(db.name.clone()), "parent"),
                ObjectKind::UniqueKey,
            )
            .await
            .unwrap();
        assert_eq!(names(&uk), vec!["uk_parent_code"]);
        let fks = s
            .list_children(&child, ObjectKind::ForeignKey)
            .await
            .unwrap();
        assert_eq!(names(&fks), vec!["fk_child_parent"]);
        assert!(
            fks[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("parent(id) ON DELETE CASCADE ON UPDATE RESTRICT"),
            "{:?}",
            fks[0].detail
        );
        let checks = s
            .list_children(&child, ObjectKind::CheckConstraint)
            .await
            .unwrap();
        assert!(
            names(&checks).contains(&"chk_qty".to_string()),
            "[{}] {:?}",
            srv.name,
            names(&checks)
        );
        let trg = s.list_children(&child, ObjectKind::Trigger).await.unwrap();
        assert_eq!(names(&trg), vec!["trg_child_bi"]);
        let parts = s
            .list_children(
                &ObjectRef::table(Some(db.name.clone()), "parted"),
                ObjectKind::Partition,
            )
            .await
            .unwrap();
        assert_eq!(names(&parts), vec!["p2023", "pmax"]);
        assert_eq!(parts[0].detail.as_deref(), Some("VALUES LESS THAN (2024)"));
        let vcols = s
            .list_children(
                &ObjectRef::new(ObjectKind::View, Some(db.name.clone()), "v_child"),
                ObjectKind::Column,
            )
            .await
            .unwrap();
        assert_eq!(names(&vcols), vec!["id", "name", "qty"]);

        s.close().await.unwrap();
        db.drop_now().await;
    }
}

#[tokio::test]
async fn definitions_and_ddl() {
    for srv in servers() {
        let db = TestDb::create(&srv).await;
        let mut s = db.session().await;
        create_objects(&srv, &mut s).await;
        let child_ref = ObjectRef::table(Some(db.name.clone()), "child");
        let t = s.table_definition(&child_ref).await.unwrap();
        assert_eq!(t.schema.as_deref(), Some(db.name.as_str()));
        assert_eq!(t.comment.as_deref(), Some("Child rows"));
        assert_eq!(t.options.get("engine").map(String::as_str), Some("InnoDB"));
        assert_eq!(
            t.options.get("charset").map(String::as_str),
            Some("utf8mb4")
        );
        assert!(
            t.options.contains_key("auto_increment"),
            "[{}] {:?}",
            srv.name,
            t.options
        );
        let col = |n: &str| t.column(n).unwrap_or_else(|| panic!("column {n}"));
        assert!(col("id").auto_increment && !col("id").nullable);
        assert_eq!(col("qty").default.as_deref(), Some("1"));
        assert_eq!(col("price").default.as_deref(), Some("9.99"));
        assert_eq!(col("body").default.as_deref(), Some("''"));
        assert_eq!(col("flags").default.as_deref(), Some("b'101'"));
        assert_eq!(col("kind").default.as_deref(), Some("'a'"));
        assert_eq!(col("kind").kind, DataKind::Enum);
        assert!(
            col("created")
                .default
                .as_deref()
                .unwrap()
                .eq_ignore_ascii_case(if srv.is_mariadb() {
                    "current_timestamp()"
                } else {
                    "CURRENT_TIMESTAMP"
                })
        );
        assert!(col("note").default.is_none());
        assert_eq!(
            col("total")
                .generated
                .as_deref()
                .map(|g| g.replace('`', "")),
            Some("qty * price".to_string()),
            "[{}]",
            srv.name
        );
        assert_eq!(
            t.options.get("generated:total").map(String::as_str),
            Some("STORED")
        );
        assert_eq!(
            t.options.get("generated:half").map(String::as_str),
            Some("VIRTUAL")
        );
        assert!(
            t.options
                .get("on_update:updated")
                .unwrap()
                .eq_ignore_ascii_case("CURRENT_TIMESTAMP(3)")
        );
        assert_eq!(
            t.primary_key.as_ref().map(|k| k.columns.clone()),
            Some(vec!["id".to_string()])
        );
        let fk = &t.foreign_keys[0];
        assert_eq!(
            (
                fk.name.as_deref(),
                fk.ref_table.as_str(),
                fk.ref_schema.as_deref()
            ),
            (Some("fk_child_parent"), "parent", None)
        );
        assert_eq!(
            (fk.on_delete, fk.on_update),
            (FkAction::Cascade, FkAction::Restrict)
        );
        assert!(
            t.checks.iter().any(|c| c.name.as_deref() == Some("chk_qty")
                && c.expression.replace('`', "") == "qty >= 0"),
            "{:?}",
            t.checks
        );
        let ix = t
            .indexes
            .iter()
            .find(|i| i.name == "idx_body_prefix")
            .unwrap();
        assert_eq!(ix.columns[0].name, "body(10)");
        assert!(ix.columns[1].descending);
        assert_eq!(
            t.indexes
                .iter()
                .find(|i| i.name == "ft_note")
                .unwrap()
                .method
                .as_deref(),
            Some("FULLTEXT")
        );
        assert!(
            t.indexes
                .iter()
                .find(|i| i.name == "PRIMARY")
                .unwrap()
                .primary
        );
        assert_eq!(t.triggers.len(), 1);
        assert_eq!(
            (t.triggers[0].timing.as_str(), t.triggers[0].events.clone()),
            ("BEFORE", vec!["INSERT".to_string()])
        );
        assert_eq!(t.row_estimate.map(|r| r >= 0), Some(true));
        let parent = s
            .table_definition(&ObjectRef::table(Some(db.name.clone()), "parent"))
            .await
            .unwrap();
        assert_eq!(parent.referenced_by.len(), 1);
        assert_eq!(parent.referenced_by[0].table, "child");
        assert_eq!(
            parent.unique_keys[0].name.as_deref(),
            Some("uk_parent_code")
        );
        assert_eq!(
            parent.column("name").unwrap().default.as_deref(),
            Some("'it''s'")
        );
        assert_eq!(
            parent.column("id").unwrap().comment.as_deref(),
            Some("surrogate key")
        );
        let parted = s
            .table_definition(&ObjectRef::table(Some(db.name.clone()), "parted"))
            .await
            .unwrap();
        assert_eq!(parted.table_kind, TableKind::PartitionedTable);
        assert_eq!(
            parted.partitioning.as_ref().map(|p| p.strategy.as_str()),
            Some("RANGE")
        );
        assert_eq!(
            parted
                .partitions
                .iter()
                .map(|p| p.bound.clone())
                .collect::<Vec<_>>(),
            vec!["VALUES LESS THAN (2024)", "VALUES LESS THAN MAXVALUE"]
        );
        assert!(
            s.table_definition(&ObjectRef::table(Some(db.name.clone()), "nope"))
                .await
                .is_err()
        );

        let v = s
            .view_definition(&ObjectRef::new(
                ObjectKind::View,
                Some(db.name.clone()),
                "v_child",
            ))
            .await
            .unwrap();
        assert!(
            v.definition.to_lowercase().starts_with("select"),
            "{}",
            v.definition
        );
        assert!(
            !v.definition.contains(&format!("`{}`.", db.name)),
            "{}",
            v.definition
        );
        assert_eq!(v.columns.len(), 3);
        assert_eq!(
            v.options.get("algorithm").map(String::as_str),
            Some("UNDEFINED")
        );

        let f = s
            .routine_definition(&ObjectRef::new(
                ObjectKind::Function,
                Some(db.name.clone()),
                "f_double",
            ))
            .await
            .unwrap();
        assert_eq!(f.kind, Some(ObjectKind::Function));
        assert_eq!(f.arguments.replace("int(11)", "int"), "`x` int");
        assert_eq!(
            f.returns
                .as_deref()
                .map(|r| r.replace("int(11)", "int"))
                .as_deref(),
            Some("int")
        );
        assert!(
            f.definition.contains("FUNCTION") && f.definition.contains("RETURN"),
            "{}",
            f.definition
        );
        let p = s
            .routine_definition(&ObjectRef::new(
                ObjectKind::Procedure,
                Some(db.name.clone()),
                "p_touch",
            ))
            .await
            .unwrap();
        assert_eq!(
            p.arguments.replace("int(11)", "int"),
            "IN `pid` int, OUT `n` int"
        );
        if srv.is_mariadb() {
            let seq = s
                .sequence_definition(&ObjectRef::new(
                    ObjectKind::Sequence,
                    Some(db.name.clone()),
                    "seq_orders",
                ))
                .await
                .unwrap();
            assert_eq!(
                (seq.start, seq.increment, seq.max_value, seq.cache),
                (Some(100), Some(5), Some(100000), Some(10))
            );
        } else {
            assert!(
                s.sequence_definition(&ObjectRef::new(
                    ObjectKind::Sequence,
                    Some(db.name.clone()),
                    "x"
                ))
                .await
                .is_err()
            );
        }

        // object DDL
        let ddl = |kind, name: &str, table: Option<&str>| {
            let mut o = ObjectRef::new(kind, Some(db.name.clone()), name);
            o.table = table.map(str::to_string);
            o
        };
        for (o, keyword, name) in [
            (
                ddl(ObjectKind::Table, "child", None),
                "CREATE TABLE",
                "child",
            ),
            (ddl(ObjectKind::View, "v_child", None), "VIEW", "v_child"),
            (
                ddl(ObjectKind::Function, "f_double", None),
                "FUNCTION",
                "f_double",
            ),
            (
                ddl(ObjectKind::Procedure, "p_touch", None),
                "PROCEDURE",
                "p_touch",
            ),
            (
                ddl(ObjectKind::Trigger, "trg_child_bi", Some("child")),
                "TRIGGER",
                "trg_child_bi",
            ),
            (
                ddl(ObjectKind::Event, "ev_cleanup", None),
                "EVENT",
                "ev_cleanup",
            ),
            (
                ddl(ObjectKind::Index, "idx_body_prefix", Some("child")),
                "CREATE INDEX",
                "`body`(10)",
            ),
            (
                ddl(ObjectKind::ForeignKey, "fk_child_parent", Some("child")),
                "FOREIGN KEY",
                "fk_child_parent",
            ),
            (
                ddl(ObjectKind::CheckConstraint, "chk_qty", Some("child")),
                "CHECK",
                "chk_qty",
            ),
            (
                ddl(ObjectKind::Column, "qty", Some("child")),
                "ADD COLUMN",
                "`qty`",
            ),
            (
                ddl(ObjectKind::PrimaryKey, "PRIMARY", Some("child")),
                "PRIMARY KEY",
                "`id`",
            ),
            (
                ObjectRef::new(ObjectKind::Schema, None, db.name.clone()),
                "CREATE DATABASE",
                db.name.as_str(),
            ),
        ] {
            let text = s
                .object_ddl(&o)
                .await
                .unwrap_or_else(|e| panic!("[{}] ddl of {o}: {}", srv.name, e.full_text()));
            assert!(
                text.to_uppercase().contains(keyword) && text.contains(name),
                "[{}] {o}: {text}",
                srv.name
            );
        }
        if srv.is_mariadb() {
            let text = s
                .object_ddl(&ddl(ObjectKind::Sequence, "seq_orders", None))
                .await
                .unwrap();
            assert!(text.to_uppercase().contains("CREATE SEQUENCE"), "{text}");
        }

        // properties, statistics, dependencies
        let props = s.object_properties(&child_ref).await.unwrap();
        assert!(
            props
                .iter()
                .any(|p| p.name == "Engine" && p.value == "InnoDB")
        );
        let stats = s.object_statistics(&child_ref).await.unwrap();
        assert!(stats.iter().any(|p| p.name == "Engine"), "{stats:?}");
        assert!(stats.iter().any(|p| p.name == "Total size"));
        let sp = s
            .object_properties(&ObjectRef::new(ObjectKind::Schema, None, db.name.clone()))
            .await
            .unwrap();
        assert!(
            sp.iter()
                .any(|p| p.name == "Default character set" && p.value == "utf8mb4")
        );
        assert!(
            !s.object_properties(&ddl(ObjectKind::Procedure, "p_touch", None))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            !s.object_properties(&ddl(ObjectKind::Event, "ev_cleanup", None))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            !s.object_properties(&ddl(ObjectKind::Column, "qty", Some("child")))
                .await
                .unwrap()
                .is_empty()
        );
        let deps = s
            .object_dependencies(&ObjectRef::table(Some(db.name.clone()), "parent"))
            .await
            .unwrap();
        assert!(
            deps.iter()
                .any(|d| d.direction == DependencyDirection::UsedBy && d.target.name == "child"),
            "{deps:?}"
        );
        assert!(
            deps.iter()
                .any(|d| d.direction == DependencyDirection::UsedBy && d.target.name == "v_child"),
            "[{}] {deps:?}",
            srv.name
        );
        let deps = s.object_dependencies(&child_ref).await.unwrap();
        assert!(deps.iter().any(|d| d.direction == DependencyDirection::DependsOn && d.target.name == "parent"));
        assert!(deps.iter().any(|d| d.target.name == "trg_child_bi"));
        let deps = s
            .object_dependencies(&ObjectRef::new(
                ObjectKind::View,
                Some(db.name.clone()),
                "v_child",
            ))
            .await
            .unwrap();
        let mut used: Vec<String> = deps
            .iter()
            .filter(|d| d.direction == DependencyDirection::DependsOn)
            .map(|d| d.target.name.clone())
            .collect();
        used.sort();
        assert_eq!(used, vec!["child", "parent"], "[{}]", srv.name);

        s.close().await.unwrap();
        db.drop_now().await;
    }
}

#[tokio::test]
async fn search_and_snapshot() {
    for srv in servers() {
        let db = TestDb::create(&srv).await;
        let mut s = db.session().await;
        create_objects(&srv, &mut s).await;
        let search = MetadataSearch {
            pattern: "chil".into(),
            kinds: vec![
                ObjectKind::Table,
                ObjectKind::View,
                ObjectKind::Column,
                ObjectKind::Trigger,
            ],
            schemas: vec![db.name.clone()],
            ..Default::default()
        };
        let found = s.search_metadata(&search).await.unwrap();
        assert!(
            found
                .iter()
                .any(|o| o.obj.kind == ObjectKind::Table && o.obj.name == "child")
        );
        assert!(
            found
                .iter()
                .any(|o| o.obj.kind == ObjectKind::View && o.obj.name == "v_child")
        );
        assert!(
            found
                .iter()
                .any(|o| o.obj.kind == ObjectKind::Trigger && o.obj.name == "trg_child_bi")
        );
        let wild = MetadataSearch {
            pattern: "P?R*".into(),
            kinds: vec![ObjectKind::Table],
            schemas: vec![db.name.clone()],
            ..Default::default()
        };
        let mut n = names(&s.search_metadata(&wild).await.unwrap());
        n.sort();
        assert_eq!(n, vec!["parent", "parted"]);
        let cs = MetadataSearch {
            pattern: "P?R*".into(),
            case_sensitive: true,
            kinds: vec![ObjectKind::Table],
            schemas: vec![db.name.clone()],
            ..Default::default()
        };
        assert!(s.search_metadata(&cs).await.unwrap().is_empty());
        let by_comment = MetadataSearch {
            pattern: "Parent rows".into(),
            kinds: vec![ObjectKind::Table],
            search_comments: true,
            schemas: vec![db.name.clone()],
            ..Default::default()
        };
        assert_eq!(
            names(&s.search_metadata(&by_comment).await.unwrap()),
            vec!["parent"]
        );
        let cols = MetadataSearch {
            pattern: "parent_id".into(),
            kinds: vec![ObjectKind::Column],
            ..Default::default()
        };
        let found = s.search_metadata(&cols).await.unwrap();
        assert!(found.iter().any(|o| o.obj.table.as_deref() == Some("child")
            && o.obj.schema.as_deref() == Some(db.name.as_str())));

        let snap = s
            .schema_snapshot(
                &SchemaRef::schema(db.name.clone()),
                &SnapshotOptions::full(),
            )
            .await
            .unwrap();
        assert_eq!(snap.kind, Some(srv.kind));
        assert_eq!(
            snap.tables
                .iter()
                .map(|t| t.name.clone())
                .collect::<Vec<_>>(),
            vec!["child", "parent", "parted"]
        );
        assert_eq!(snap.views.len(), 1);
        assert!(!snap.views[0].definition.is_empty());
        assert_eq!(snap.routines.len(), 2);
        assert!(
            snap.routines
                .iter()
                .all(|r| r.definition.starts_with("CREATE ")),
            "{:?}",
            snap.routines
        );
        assert_eq!(snap.sequences.len(), if srv.is_mariadb() { 1 } else { 0 });
        // the snapshot agrees with the single table definition
        let single = s
            .table_definition(&ObjectRef::table(Some(db.name.clone()), "child"))
            .await
            .unwrap();
        let mut from_snap = snap.table("child").unwrap().clone();
        let mut single_cmp = single.clone();
        for t in [&mut from_snap, &mut single_cmp] {
            t.row_estimate = None;
            t.size_bytes = None;
            t.options.remove("auto_increment");
        }
        assert_eq!(
            from_snap, single_cmp,
            "[{}] snapshot vs table_definition",
            srv.name
        );
        let light = s
            .schema_snapshot(
                &SchemaRef::schema(db.name.clone()),
                &SnapshotOptions::light(),
            )
            .await
            .unwrap();
        assert!(
            light.tables[0].indexes.is_empty()
                && light.tables[0].triggers.is_empty()
                && !light.tables[0].columns.is_empty()
        );
        assert!(light.routines.is_empty());

        // 300 tables: batched snapshot stays fast
        let mut ddl = String::new();
        for i in 0..300 {
            ddl.push_str(&format!(
                "CREATE TABLE bulk_{i:03} (id INT PRIMARY KEY, a VARCHAR(10), b INT, KEY (b));"
            ));
        }
        exec(&mut s, &ddl).await;
        let started = std::time::Instant::now();
        let snap = s
            .schema_snapshot(
                &SchemaRef::schema(db.name.clone()),
                &SnapshotOptions::full(),
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(snap.tables.len(), 303);
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "[{}] snapshot took {elapsed:?}",
            srv.name
        );
        println!("[{}] snapshot of 303 tables: {elapsed:?}", srv.name);
        s.close().await.unwrap();
        db.drop_now().await;
    }
}

/// Create a rich table, read it back, regenerate the DDL into a second
/// database, read that back and compare.
#[tokio::test]
async fn ddl_round_trip() {
    for srv in servers() {
        let db1 = TestDb::create(&srv).await;
        let db2 = TestDb::create(&srv).await;
        let mut s = db1.session().await;
        create_objects(&srv, &mut s).await;
        let functional = if srv.is_mariadb() {
            ""
        } else {
            ", KEY idx_lower ((lower(label)), id DESC)"
        };
        exec(
            &mut s,
            &format!(
                "CREATE TABLE geo (id INT PRIMARY KEY, pt POINT NOT NULL, label VARCHAR(50) COLLATE utf8mb4_bin, \
                 hidden INT DEFAULT 7 INVISIBLE, SPATIAL KEY sp_pt (pt){functional}) ROW_FORMAT=DYNAMIC"
            ),
        )
        .await;
        let dialect = s.mysql_dialect();
        let ddl = dialect.mysql_ddl();
        let geo = s
            .table_definition(&ObjectRef::table(Some(db1.name.clone()), "geo"))
            .await
            .unwrap();
        assert_eq!(
            geo.options
                .get("row_format")
                .map(|v| v.to_ascii_uppercase())
                .as_deref(),
            Some("DYNAMIC")
        );
        assert_eq!(
            geo.indexes
                .iter()
                .find(|i| i.name == "sp_pt")
                .and_then(|i| i.method.clone())
                .as_deref(),
            Some("SPATIAL")
        );
        assert_eq!(
            geo.column("label")
                .and_then(|c| c.collation.clone())
                .as_deref(),
            Some("utf8mb4_bin")
        );
        assert_eq!(
            geo.options.get("invisible:hidden").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            geo.column("hidden")
                .and_then(|c| c.default.clone())
                .as_deref(),
            Some("7")
        );
        if !srv.is_mariadb() {
            let idx = geo
                .indexes
                .iter()
                .find(|i| i.name == "idx_lower")
                .expect("functional index");
            assert!(idx.columns[0].is_expression, "{idx:?}");
            assert_eq!(idx.columns[0].name.replace('`', ""), "lower(label)");
            assert!(idx.columns[1].descending);
        }
        for table in ["parent", "child", "parted", "geo"] {
            let src = s
                .table_definition(&ObjectRef::table(Some(db1.name.clone()), table))
                .await
                .unwrap();
            let mut target = src.clone();
            target.schema = Some(db2.name.clone());
            for stmt in ddl.create_table(&target, true) {
                if let Err(e) = dbiji_core::driver::execute_update(&mut s, &stmt, &[]).await {
                    panic!("[{}] {stmt}\n{}", srv.name, e.full_text());
                }
            }
            let copy = s
                .table_definition(&ObjectRef::table(Some(db2.name.clone()), table))
                .await
                .unwrap();
            let normalize = |t: &TableDef| {
                let mut t = t.clone();
                t.schema = None;
                t.row_estimate = None;
                t.size_bytes = None;
                t.referenced_by.clear();
                t.options.remove("auto_increment");
                for fk in t.foreign_keys.iter_mut() {
                    fk.schema = None;
                }
                for tr in t.triggers.iter_mut() {
                    tr.definition.clear();
                }
                for p in t.partitions.iter_mut() {
                    p.row_estimate = None;
                }
                t
            };
            assert_eq!(
                normalize(&copy),
                normalize(&src),
                "[{}] round trip of {table}",
                srv.name
            );
            // nothing to alter between identical definitions in two schemas
            let diff = ddl.alter_table(&src, &copy, &[]);
            assert!(diff.is_empty(), "[{}] {table}: {diff:#?}", srv.name);
        }
        // ALTER: modify the copy and apply the generated statements
        let child2 = ObjectRef::table(Some(db2.name.clone()), "child");
        let from = s.table_definition(&child2).await.unwrap();
        let mut to = from.clone();
        to.columns.retain(|c| c.name != "half");
        to.options.remove("generated:half");
        to.columns
            .push(ColumnDef::new("extra", "varchar(30)").default_value("'x'"));
        if let Some(c) = to.columns.iter_mut().find(|c| c.name == "note") {
            c.name = "remark".into();
            c.comment = Some("renamed".into());
        }
        if let Some(c) = to.columns.iter_mut().find(|c| c.name == "qty") {
            c.default = Some("5".into());
        }
        to.comment = Some("Changed".into());
        to.indexes
            .retain(|i| i.name != "idx_parent" && i.name != "ft_note");
        to.indexes.push(IndexDef {
            name: "idx_qty".into(),
            columns: vec![IndexColumn::column("qty")],
            ..Default::default()
        });
        to.indexes.push(IndexDef {
            name: "ft_remark".into(),
            columns: vec![IndexColumn::column("remark")],
            method: Some("FULLTEXT".into()),
            ..Default::default()
        });
        to.foreign_keys[0].on_delete = FkAction::Restrict;
        to.indexes.push(IndexDef {
            name: "idx_parent".into(),
            columns: vec![IndexColumn::column("parent_id")],
            ..Default::default()
        });
        let stmts = ddl.alter_table(&from, &to, &[("note".into(), "remark".into())]);
        for stmt in &stmts {
            if let Err(e) = dbiji_core::driver::execute_update(&mut s, stmt, &[]).await {
                panic!("[{}] {stmt}\n{}\nall: {stmts:#?}", srv.name, e.full_text());
            }
        }
        let after = s.table_definition(&child2).await.unwrap();
        assert!(after.column("half").is_none());
        assert_eq!(
            after
                .column("extra")
                .and_then(|c| c.default.clone())
                .as_deref(),
            Some("'x'")
        );
        assert_eq!(
            after
                .column("remark")
                .and_then(|c| c.comment.clone())
                .as_deref(),
            Some("renamed")
        );
        assert_eq!(
            after
                .column("qty")
                .and_then(|c| c.default.clone())
                .as_deref(),
            Some("5")
        );
        assert_eq!(after.comment.as_deref(), Some("Changed"));
        assert!(after.indexes.iter().any(|i| i.name == "idx_qty"));
        assert!(after.indexes.iter().any(|i| i.name == "ft_remark"));
        assert_eq!(after.foreign_keys[0].on_delete, FkAction::Restrict);
        // the data survived
        assert_eq!(
            query(
                &mut s,
                &format!("SELECT COUNT(*) FROM `{}`.parent", db2.name)
            )
            .await[0][0],
            dbiji_core::Value::Int(0)
        );

        // views, routines, schemas through the generator
        let v = s
            .view_definition(&ObjectRef::new(
                ObjectKind::View,
                Some(db1.name.clone()),
                "v_child",
            ))
            .await
            .unwrap();
        let mut v2 = v.clone();
        v2.schema = Some(db2.name.clone());
        exec(&mut s, &format!("USE `{}`", db2.name)).await;
        for stmt in ddl.create_view(&v2, true) {
            exec(&mut s, &stmt).await;
        }
        let r = s
            .routine_definition(&ObjectRef::new(
                ObjectKind::Function,
                Some(db1.name.clone()),
                "f_double",
            ))
            .await
            .unwrap();
        for stmt in ddl.create_routine(&r) {
            exec(&mut s, &stmt).await;
        }
        assert_eq!(
            query(&mut s, "SELECT f_double(21)").await[0][0],
            dbiji_core::Value::Int(42)
        );
        if srv.is_mariadb() {
            let seq = s
                .sequence_definition(&ObjectRef::new(
                    ObjectKind::Sequence,
                    Some(db1.name.clone()),
                    "seq_orders",
                ))
                .await
                .unwrap();
            let mut seq2 = seq.clone();
            seq2.schema = Some(db2.name.clone());
            exec(&mut s, &ddl.create_sequence(&seq2)).await;
            let back = s
                .sequence_definition(&ObjectRef::new(
                    ObjectKind::Sequence,
                    Some(db2.name.clone()),
                    "seq_orders",
                ))
                .await
                .unwrap();
            assert_eq!(
                (
                    back.start,
                    back.increment,
                    back.max_value,
                    back.cache,
                    back.cycle
                ),
                (
                    seq.start,
                    seq.increment,
                    seq.max_value,
                    seq.cache,
                    seq.cycle
                )
            );
            let mut seq3 = back.clone();
            seq3.increment = Some(7);
            seq3.cycle = true;
            for stmt in ddl.alter_sequence(&back, &seq3) {
                exec(&mut s, &stmt).await;
            }
            let back = s
                .sequence_definition(&ObjectRef::new(
                    ObjectKind::Sequence,
                    Some(db2.name.clone()),
                    "seq_orders",
                ))
                .await
                .unwrap();
            assert_eq!((back.increment, back.cycle), (Some(7), true));
            exec(&mut s, &ddl.drop_sequence(&back)).await;
        }
        let sch = format!("dbiji_t_{}", suffix());
        exec(&mut s, &ddl.create_schema(&sch)).await;
        exec(&mut s, &ddl.drop_schema(&sch, true)).await;
        s.close().await.unwrap();
        db2.drop_now().await;
        db1.drop_now().await;
    }
}
