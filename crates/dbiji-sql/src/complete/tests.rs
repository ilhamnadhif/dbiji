//! Autocomplete scenarios against the standard test schema
//! (`public`: users, orders, order_items, products, categories, MixedCase,
//! view active_users; `sales`: customers, invoices).

use super::*;
use crate::catalog::test_catalog::catalog;
use crate::catalog::{CatalogTable, SnapshotCatalog};
use crate::testutil::*;
use dbiji_core::ddl::{ColumnDef, SchemaSnapshot, TableDef};

const CURSOR: char = '‸';

fn run_with(sql: &str, dialect: &dyn Dialect, opts: &CompletionOptions) -> CompletionResult {
    let cursor = sql.find(CURSOR).expect("cursor marker");
    let src = sql.replacen(CURSOR, "", 1);
    complete(&src, cursor, dialect, &catalog(), opts)
}

fn run(sql: &str) -> CompletionResult {
    run_with(sql, &TestDialect::pg(), &CompletionOptions::default())
}

fn labels(r: &CompletionResult) -> Vec<&str> {
    r.items.iter().map(|i| i.label.as_str()).collect()
}

fn find<'a>(r: &'a CompletionResult, label: &str) -> Option<&'a CompletionItem> {
    r.items.iter().find(|i| i.label == label)
}

fn pos(r: &CompletionResult, label: &str) -> usize {
    r.items
        .iter()
        .position(|i| i.label == label)
        .unwrap_or_else(|| panic!("{label} not proposed: {:?}", labels(r)))
}

fn has(r: &CompletionResult, label: &str) -> bool {
    find(r, label).is_some()
}

fn columns(r: &CompletionResult) -> Vec<&str> {
    r.items
        .iter()
        .filter(|i| i.kind == CompletionKind::Column)
        .map(|i| i.label.as_str())
        .collect()
}

#[test]
fn s01_statement_start() {
    let r = run("‸");
    assert!(
        has(&r, "SELECT") && has(&r, "INSERT INTO") && has(&r, "WITH") && has(&r, "CREATE TABLE")
    );
    assert!(
        r.items
            .iter()
            .all(|i| matches!(i.kind, CompletionKind::Keyword | CompletionKind::Snippet))
    );
    let r = run("SEL‸");
    assert_eq!(r.items[0].label, "SELECT");
    assert_eq!((r.replace_start, r.replace_end), (0, 3));
    let r = run_with(
        "sel‸",
        &TestDialect::pg(),
        &CompletionOptions {
            keyword_case: KeywordCase::Lower,
            ..Default::default()
        },
    );
    assert_eq!(r.items[0].insert, "select");
}

#[test]
fn s02_tables_after_from() {
    let r = run("SELECT * FROM ‸");
    for t in [
        "users",
        "orders",
        "order_items",
        "products",
        "categories",
        "MixedCase",
    ] {
        assert_eq!(
            find(&r, t).map(|i| i.kind),
            Some(CompletionKind::Table),
            "{t}"
        );
    }
    assert_eq!(
        find(&r, "active_users").map(|i| i.kind),
        Some(CompletionKind::View)
    );
    assert_eq!(
        find(&r, "sales").map(|i| i.kind),
        Some(CompletionKind::Schema)
    );
    assert!(has(&r, "archive"));
    // tables of other schemas are not offered unqualified
    assert!(!has(&r, "customers"));
    assert!(pos(&r, "users") < pos(&r, "sales"));
}

#[test]
fn s03_table_prefix_and_alias() {
    let r = run("SELECT * FROM us‸");
    assert_eq!(r.items[0].label, "users");
    assert_eq!(r.items[0].insert, "users u");
    assert_eq!(r.replace_start, 14);
    let r = run_with(
        "SELECT * FROM us‸",
        &TestDialect::pg(),
        &CompletionOptions {
            insert_aliases: false,
            ..Default::default()
        },
    );
    assert_eq!(r.items[0].insert, "users");
    // alias collisions get a number
    let r = run("SELECT * FROM users u JOIN us‸");
    assert_eq!(
        find(&r, "users").map(|i| i.insert.as_str()),
        Some("users u2")
    );
}

#[test]
fn s04_schema_qualified_tables() {
    let r = run("SELECT * FROM sales.‸");
    let mut l = labels(&r);
    l.sort();
    assert_eq!(l, vec!["customers", "invoices"]);
    let r = run("SELECT * FROM sales.inv‸ WHERE 1 = 1");
    assert_eq!(r.items[0].label, "invoices");
    assert_eq!(
        &"SELECT * FROM sales.invoices WHERE 1 = 1"[r.replace_start..r.replace_start + 8],
        "invoices"
    );
}

#[test]
fn s05_alias_columns_before_from() {
    let r = run("SELECT u.‸ FROM users u");
    let mut c = columns(&r);
    c.sort();
    assert_eq!(c, vec!["created_at", "email", "id", "name"]);
    assert!(r.items.iter().all(|i| i.kind == CompletionKind::Column));
    let r = run("SELECT u.na‸ FROM users u");
    assert_eq!(r.items[0].label, "name");
    assert_eq!(r.items[0].insert, "name");
    assert_eq!((r.replace_start, r.replace_end), (9, 11));
}

#[test]
fn s06_alias_columns_in_where() {
    let r = run("SELECT * FROM users u WHERE u.‸");
    assert!(has(&r, "email") && has(&r, "created_at"));
    let d = find(&r, "email")
        .and_then(|i| i.detail.clone())
        .unwrap_or_default();
    assert!(d.contains("users") && d.contains("varchar(200)"), "{d}");
    // table name as qualifier
    let r = run("SELECT orders.‸ FROM orders");
    assert!(has(&r, "total") && has(&r, "user_id"));
}

#[test]
fn s07_join_tables_ranked_by_foreign_keys() {
    let r = run("SELECT * FROM users u JOIN ‸");
    let join = "orders o ON o.user_id = u.id";
    assert_eq!(
        find(&r, join).map(|i| i.kind),
        Some(CompletionKind::Join),
        "{:?}",
        labels(&r)
    );
    assert!(pos(&r, join) < pos(&r, "products"));
    assert!(pos(&r, "orders") < pos(&r, "products"));
    // related table in another schema, inserted qualified
    assert!(
        has(&r, "sales.customers c ON c.user_id = u.id"),
        "{:?}",
        labels(&r)
    );
}

#[test]
fn s08_join_conditions_after_on() {
    let r = run("SELECT * FROM users u JOIN orders o ON ‸");
    assert_eq!(r.items[0].label, "o.user_id = u.id");
    assert_eq!(r.items[0].kind, CompletionKind::Join);
    // columns are still offered
    assert!(has(&r, "total"));
    let r = run("SELECT * FROM orders o JOIN order_items oi ON ‸");
    assert_eq!(r.items[0].label, "oi.order_id = o.id");
    let r = run("SELECT * FROM orders o LEFT JOIN users ON ‸");
    assert_eq!(r.items[0].label, "users.id = o.user_id");
}

#[test]
fn s09_join_proposals_for_many_to_many() {
    let r = run("SELECT * FROM order_items oi JOIN ‸");
    assert!(
        has(&r, "orders o ON o.id = oi.order_id"),
        "{:?}",
        labels(&r)
    );
    assert!(has(&r, "products p ON p.id = oi.product_id"));
    assert!(pos(&r, "orders") < pos(&r, "users"));
}

#[test]
fn s10_on_proposal_after_join_table() {
    let r = run("SELECT * FROM users u JOIN orders o ‸");
    assert_eq!(r.items[0].label, "ON o.user_id = u.id");
    assert!(has(&r, "ON") && has(&r, "USING"));
}

#[test]
fn s11_select_list_columns() {
    let r = run("SELECT ‸ FROM users");
    let c = columns(&r);
    assert!(c.contains(&"id") && c.contains(&"email"));
    let d = find(&r, "created_at")
        .and_then(|i| i.detail.clone())
        .unwrap_or_default();
    assert!(d.contains("users") && d.contains("timestamptz"), "{d}");
    // columns first, keywords later
    assert_eq!(r.items[0].kind, CompletionKind::Column);
    assert!(has(&r, "count"));
}

#[test]
fn s12_columns_of_all_joined_tables() {
    let r = run("SELECT ‸ FROM users u JOIN orders o ON o.user_id = u.id");
    let c = columns(&r);
    for col in ["email", "total", "status", "user_id"] {
        assert!(c.contains(&col), "{col}: {c:?}");
    }
    assert_eq!(find(&r, "u").map(|i| i.kind), Some(CompletionKind::Alias));
    assert_eq!(find(&r, "o").map(|i| i.kind), Some(CompletionKind::Alias));
    let opts = CompletionOptions {
        qualify_columns: true,
        ..Default::default()
    };
    let r = run_with(
        "SELECT ‸ FROM users u JOIN orders o ON o.user_id = u.id",
        &TestDialect::pg(),
        &opts,
    );
    assert!(r.items.iter().any(|i| i.insert == "u.email"));
    assert!(r.items.iter().any(|i| i.insert == "o.total"));
}

#[test]
fn s13_cte_columns_derived_and_explicit() {
    let r = run("WITH x AS (SELECT id, name AS n FROM users) SELECT x.‸ FROM x");
    let mut c = columns(&r);
    c.sort();
    assert_eq!(c, vec!["id", "n"]);
    let r = run("WITH x(a, b) AS (SELECT 1, 2) SELECT * FROM x WHERE x.‸");
    let mut c = columns(&r);
    c.sort();
    assert_eq!(c, vec!["a", "b"]);
    // star in a CTE expands to the table's columns
    let r = run("WITH p AS (SELECT * FROM products) SELECT p.‸ FROM p");
    assert!(has(&r, "price") && has(&r, "category_id"));
}

#[test]
fn s14_cte_names_in_from() {
    let r = run("WITH recent AS (SELECT * FROM orders) SELECT * FROM ‸");
    assert_eq!(r.items[0].label, "recent");
    assert_eq!(r.items[0].kind, CompletionKind::Cte);
    // unqualified columns of a CTE source
    let r = run("WITH recent AS (SELECT id, total FROM orders) SELECT ‸ FROM recent");
    let mut c = columns(&r);
    c.sort();
    assert_eq!(c, vec!["id", "total"]);
}

#[test]
fn s15_derived_table_columns() {
    let r = run("SELECT s.‸ FROM (SELECT id, total AS amount FROM orders) s");
    let mut c = columns(&r);
    c.sort();
    assert_eq!(c, vec!["amount", "id"]);
    let r = run("SELECT s.‸ FROM (SELECT * FROM products) s");
    assert!(has(&r, "price") && has(&r, "name"));
    let r = run("SELECT q.‸ FROM (SELECT 1, 2) AS q(x, y)");
    let mut c = columns(&r);
    c.sort();
    assert_eq!(c, vec!["x", "y"]);
}

#[test]
fn s16_insert_column_list() {
    let r = run("INSERT INTO users (‸");
    let mut c = columns(&r);
    c.sort();
    assert_eq!(c, vec!["created_at", "email", "id", "name"]);
    let r = run("INSERT INTO users (id, ‸) VALUES (1, 'x')");
    assert!(has(&r, "email"));
    let r = run("INSERT INTO ‸");
    assert!(has(&r, "users") && find(&r, "users").is_some_and(|i| i.insert == "users"));
    let r = run("INSERT INTO users (id) ‸");
    assert!(has(&r, "VALUES") && has(&r, "SELECT"));
}

#[test]
fn s17_update_set_columns() {
    let r = run("UPDATE users SET ‸");
    let mut c = columns(&r);
    c.sort();
    assert_eq!(c, vec!["created_at", "email", "id", "name"]);
    let r = run("UPDATE users SET name = 'x', ‸");
    assert!(has(&r, "email"));
    let r = run("UPDATE users SET name = 'x' WHERE ‸");
    assert!(has(&r, "created_at"));
    let r = run("UPDATE users ‸");
    assert_eq!(r.items[0].label, "SET");
    let r = run("UPDATE ‸");
    assert!(has(&r, "users"));
}

#[test]
fn s18_delete() {
    let r = run("DELETE FROM ‸");
    assert!(has(&r, "orders"));
    let r = run("DELETE FROM orders WHERE ‸");
    assert!(has(&r, "status") && has(&r, "total"));
    let r = run("DELETE FROM orders ‸");
    assert_eq!(r.items[0].label, "WHERE");
}

#[test]
fn s19_quoted_identifier_prefix() {
    let r = run("SELECT * FROM \"Mix‸");
    assert_eq!(r.items[0].label, "MixedCase");
    assert_eq!(r.items[0].insert, "\"MixedCase\" mc");
    assert_eq!(r.replace_start, 14);
    let r = run("SELECT * FROM Mix‸");
    assert_eq!(r.items[0].insert, "\"MixedCase\" mc");
    let r = run("SELECT m.‸ FROM \"MixedCase\" m");
    assert_eq!(
        find(&r, "Some Value").map(|i| i.insert.as_str()),
        Some("\"Some Value\"")
    );
    assert_eq!(find(&r, "Id").map(|i| i.insert.as_str()), Some("\"Id\""));
}

#[test]
fn s20_mysql_backticks() {
    let my = TestDialect::mysql();
    let opts = CompletionOptions::default();
    let r = run_with("SELECT * FROM `us‸", &my, &opts);
    assert_eq!(r.items[0].insert, "`users` u");
    let r = run_with("SELECT m.‸ FROM MixedCase m", &my, &opts);
    assert_eq!(
        find(&r, "Some Value").map(|i| i.insert.as_str()),
        Some("`Some Value`")
    );
    assert_eq!(find(&r, "Id").map(|i| i.insert.as_str()), Some("Id"));
    let r = run_with("SELECT * FROM `sales`.‸", &my, &opts);
    assert!(has(&r, "invoices"));
}

#[test]
fn s21_cursor_in_middle_of_statement() {
    let r = run("SELECT id, ‸ FROM orders WHERE total > 10");
    assert!(has(&r, "status") && has(&r, "user_id"));
    assert!(!has(&r, "email"));
}

#[test]
fn s22_incomplete_statement() {
    let r = run("SELECT * FROM users WHERE id = 1 AND ‸");
    assert!(has(&r, "email") && has(&r, "name"));
    let r = run("SELECT * FROM users WHERE (name = 'a' OR ‸");
    assert!(has(&r, "email"));
}

#[test]
fn s23_multiple_statements() {
    let r =
        run("SELECT * FROM orders;\nSELECT * FROM users u WHERE u.‸;\nSELECT * FROM products p");
    assert!(has(&r, "email"));
    assert!(!has(&r, "total") && !has(&r, "price"));
    let r = run("SELECT 1;\n‸");
    assert!(has(&r, "SELECT") && !has(&r, "WHERE"));
    let r = run("SELECT * FROM orders\n\nSELECT * FROM users WHERE ‸");
    assert!(has(&r, "email") && !has(&r, "total"));
}

#[test]
fn s24_no_completion_in_strings_and_comments() {
    assert!(run("SELECT 'us‸'").items.is_empty());
    assert!(run("SELECT 1 -- us‸").items.is_empty());
    assert!(run("SELECT 1 /* us‸ */").items.is_empty());
    assert!(run("SELECT * FROM users LIMIT 1‸").items.is_empty());
    // right after a closed string the statement continues
    assert!(has(&run("SELECT * FROM users WHERE name = 'x' ‸"), "AND"));
}

#[test]
fn s25_keywords_after_from_item() {
    let r = run("SELECT * FROM users ‸");
    for k in [
        "WHERE",
        "JOIN",
        "LEFT JOIN",
        "GROUP BY",
        "ORDER BY",
        "LIMIT",
    ] {
        assert!(has(&r, k), "{k}");
    }
    let r = run("SELECT * FROM users WH‸");
    assert_eq!(r.items[0].label, "WHERE");
    let r = run("SELECT * FROM users u ‸");
    assert!(has(&r, "WHERE"));
}

#[test]
fn s26_keywords_after_select_item() {
    let r = run("SELECT id ‸");
    assert_eq!(r.items[0].label, "FROM");
    let r = run("SELECT id, name FR‸");
    assert_eq!(r.items[0].label, "FROM");
}

#[test]
fn s27_order_and_group_by() {
    let r = run("SELECT * FROM users ORDER BY ‸");
    assert!(has(&r, "created_at"));
    let r = run("SELECT * FROM users ORDER BY name ‸");
    assert!(pos(&r, "ASC") < 3 && pos(&r, "DESC") < 3);
    let r = run("SELECT status, count(*) FROM orders GROUP BY ‸");
    assert!(has(&r, "status"));
    let r = run("SELECT status, count(*) FROM orders GROUP BY status ‸");
    assert!(has(&r, "HAVING"));
}

#[test]
fn s28_outer_alias_in_correlated_subquery() {
    let r =
        run("SELECT * FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.‸)");
    assert!(has(&r, "email"));
    let r = run("SELECT * FROM users WHERE id IN (SELECT ‸ FROM orders)");
    assert!(pos(&r, "user_id") < pos(&r, "email"));
    let r = run("SELECT * FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.‸)");
    assert!(has(&r, "total") && !has(&r, "email"));
}

#[test]
fn s29_functions() {
    let r = run("SELECT co‸ FROM users");
    let f = find(&r, "count").expect("count");
    assert_eq!(
        (f.kind, f.insert.as_str(), f.cursor_back),
        (CompletionKind::Function, "count()", 1)
    );
    assert!(has(&r, "coalesce") && has(&r, "concat"));
    assert!(has(&run("SELECT calc‸"), "calc_discount"));
    // no extra parentheses when one follows
    let r = run("SELECT cou‸(*) FROM users");
    assert_eq!(find(&r, "count").map(|i| i.insert.as_str()), Some("count"));
}

#[test]
fn s30_variables() {
    let r = run("@set lim = 10\nSELECT * FROM users LIMIT ${li‸");
    assert_eq!(r.items[0].insert, "${lim}");
    assert_eq!(r.items[0].kind, CompletionKind::Variable);
    let r = run("SELECT * FROM users WHERE id = :uid AND name = :u‸");
    assert!(r.items.iter().any(|i| i.insert == ":uid"));
}

#[test]
fn s31_self_referencing_join() {
    let r = run("SELECT * FROM categories c JOIN categories p ON ‸");
    assert!(
        has(&r, "p.parent_id = c.id") || has(&r, "p.id = c.parent_id"),
        "{:?}",
        labels(&r)
    );
}

#[test]
fn s32_schema_qualified_columns() {
    let r = run("SELECT sales.customers.‸ FROM sales.customers");
    assert!(has(&r, "region"));
    let r = run("SELECT c.‸ FROM sales.customers c");
    assert!(has(&r, "region") && has(&r, "user_id"));
    // schema name in an expression offers its tables
    let r = run("SELECT sales.‸");
    assert!(has(&r, "customers"));
}

#[test]
fn s33_fuzzy_matches_rank_below_prefix() {
    let r = run("SELECT * FROM oi‸");
    assert!(has(&r, "order_items"));
    let r = run("SELECT * FROM ord‸");
    assert!(pos(&r, "orders") < pos(&r, "order_items") || pos(&r, "order_items") < 2);
    let r = run("SELECT * FROM ors‸");
    // "ors" is a subsequence of orders/order_items/users? only fuzzy items
    assert!(r.items.iter().all(|i| i.score < 2000));
}

#[test]
fn s34_ddl_column_positions() {
    let r = run("ALTER TABLE users ALTER COLUMN ‸");
    assert!(has(&r, "email"));
    let r = run("CREATE INDEX idx ON orders (‸");
    assert!(has(&r, "status") && has(&r, "created_at"));
    let r = run("ALTER TABLE ‸");
    assert!(has(&r, "users"));
}

#[test]
fn s35_join_using_and_subquery_start() {
    let r = run("SELECT * FROM orders o JOIN order_items oi USING (‸");
    assert!(has(&r, "order_id"));
    let r = run("SELECT * FROM (‸");
    assert!(has(&r, "SELECT") && has(&r, "users"));
    let r = run("SELECT * FROM users u, ‸");
    assert!(has(&r, "orders"));
    let r = run("SELECT * FROM users WHERE id IN (SEL‸");
    assert_eq!(r.items[0].label, "SELECT");
}

#[test]
fn s36_client_commands_and_with() {
    let r = run("@se‸");
    assert_eq!(r.items[0].label, "@set");
    let r = run("WITH x AS (SELECT 1) ‸");
    assert!(has(&r, "SELECT") && has(&r, "INSERT INTO"));
    let r = run("WITH x ‸");
    assert!(has(&r, "AS"));
}

#[test]
fn s37_tsql_brackets() {
    let ms = TestDialect::tsql();
    let opts = CompletionOptions::default();
    let r = run_with("SELECT * FROM [sales].‸", &ms, &opts);
    assert!(has(&r, "customers"));
    let r = run_with("SELECT * FROM [us‸", &ms, &opts);
    assert_eq!(r.items[0].insert, "[users] u");
}

#[test]
fn s38_unknown_tables_and_unloaded_columns() {
    let r = run("SELECT x.‸ FROM nonexistent x");
    assert!(r.items.is_empty());
    let r = run("SELECT ‸ FROM nonexistent");
    assert!(columns(&r).is_empty());
    assert!(has(&r, "nonexistent")); // as alias / qualifier
}

#[test]
fn s39_max_items_and_ordering() {
    let r = run_with(
        "SELECT * FROM ‸",
        &TestDialect::pg(),
        &CompletionOptions {
            max_items: 3,
            ..Default::default()
        },
    );
    assert_eq!(r.items.len(), 3);
    for w in r.items.windows(2) {
        assert!(w[0].score >= w[1].score);
    }
}

#[test]
fn s40_merge_and_returning() {
    let r = run("MERGE INTO orders o USING users u ON ‸");
    assert!(
        has(&r, "o.user_id = u.id") || has(&r, "u.id = o.user_id"),
        "{:?}",
        labels(&r)
    );
    let r = run("INSERT INTO orders (user_id) VALUES (1) RETURNING ‸");
    assert!(has(&r, "id"));
}

#[test]
fn s41_other_table_positions() {
    for sql in [
        "TABLE ‸",
        "DESCRIBE ‸",
        "TRUNCATE TABLE ‸",
        "DROP TABLE ‸",
        "LOCK TABLE ‸",
        "COMMENT ON TABLE ‸",
    ] {
        let r = run(sql);
        assert!(
            has(&r, "users") && has(&r, "orders"),
            "{sql}: {:?}",
            labels(&r)
        );
    }
    let r = run("SELECT * FROM users u JOIN orders o USING (id) JOIN ‸");
    assert!(
        has(&r, "order_items oi ON oi.order_id = o.id"),
        "{:?}",
        labels(&r)
    );
    let r = run("EXPLAIN ANALYZE ‸");
    assert!(has(&r, "SELECT"));
    let r = run("CREATE VIEW v AS SELECT * FROM ‸");
    assert!(has(&r, "users"));
}

#[test]
fn s42_split_options_are_respected() {
    let never = CompletionOptions {
        split: SplitOptions {
            blank_line: BlankLineMode::Never,
            ..Default::default()
        },
        ..Default::default()
    };
    // With blank lines not splitting, the FROM above the blank line is in scope.
    let r = run_with(
        "SELECT *\nFROM users\n\nWHERE ‸",
        &TestDialect::pg(),
        &never,
    );
    assert!(has(&r, "email"));
    let r = run_with(
        "SELECT * FROM orders\n\nSELECT ‸ FROM users",
        &TestDialect::pg(),
        &never,
    );
    assert!(has(&r, "email"));
}

fn big_catalog(n: usize) -> SnapshotCatalog {
    let mut snap = SchemaSnapshot {
        schema: Some("public".into()),
        ..Default::default()
    };
    for i in 0..n {
        let mut t = TableDef::new(Some("public".into()), format!("table_{i:04}"));
        for c in 0..12 {
            t.columns
                .push(ColumnDef::new(format!("col_{c}"), "integer"));
        }
        snap.tables.push(t);
    }
    SnapshotCatalog::new(vec![snap], Some("public".into()))
}

#[test]
fn performance_large_catalog_and_script() {
    let cat = big_catalog(1000);
    let mut script = String::new();
    for i in 0..1000 {
        script.push_str(&format!(
            "SELECT col_1, col_2 FROM table_{:04} t WHERE t.col_3 = {i};\n",
            i % 1000
        ));
    }
    let base = script.len();
    script.push_str("SELECT t.");
    let cur = script.len();
    script.push_str(" FROM table_0500 t JOIN ");
    let d = TestDialect::pg();
    let opts = CompletionOptions::default();
    let start = std::time::Instant::now();
    let r = complete(&script, cur, &d, &cat, &opts);
    let r2 = complete(&script, script.len(), &d, &cat, &opts);
    let r3 = complete(&script, base + 7, &d, &cat, &CompletionOptions::default());
    let elapsed = start.elapsed();
    assert_eq!(r.items.len(), 12);
    assert!(!r2.items.is_empty());
    assert!(r3.items.iter().any(|i| i.kind == CompletionKind::Column));
    // Release target is < 10 ms per call; debug builds are much slower.
    assert!(elapsed.as_millis() < 3000, "{elapsed:?}");
    let _ = CatalogTable {
        schema: None,
        name: String::new(),
        kind: ObjectKind::Table,
        comment: None,
    };
}

#[test]
fn random_inputs_never_panic() {
    let cat = catalog();
    let dialects = [
        TestDialect::pg(),
        TestDialect::mysql(),
        TestDialect::sqlite(),
        TestDialect::tsql(),
    ];
    let mut rng = Rng::new(99);
    for round in 0..300 {
        let src = random_sql(&mut rng, 30);
        let d = &dialects[round % dialects.len()];
        for _ in 0..4 {
            let off = floor_boundary(&src, rng.below(src.len() + 1));
            let r = complete(&src, off, d, &cat, &CompletionOptions::default());
            assert!(r.replace_start <= r.replace_end && r.replace_end <= src.len());
            assert!(src.is_char_boundary(r.replace_start) && src.is_char_boundary(r.replace_end));
        }
    }
}

/// A partitioned table and two monthly partitions of it.
fn partitioned(kind: dbiji_core::DbKind) -> SnapshotCatalog {
    let mut t = TableDef::new(Some("public".into()), "api_calls");
    t.columns.push(ColumnDef::new("id", "bigint"));
    t.columns.push(ColumnDef::new("called_at", "timestamptz"));
    for m in ["2029_01", "2029_02"] {
        t.partitions.push(dbiji_core::ddl::PartitionDef {
            name: format!("api_calls_{m}"),
            bound: "FOR VALUES FROM (...) TO (...)".into(),
            row_estimate: None,
        });
    }
    let snap = SchemaSnapshot {
        kind: Some(kind),
        schema: Some("public".into()),
        tables: vec![t],
        ..Default::default()
    };
    SnapshotCatalog::new(vec![snap], Some("public".into()))
}

#[test]
fn partitions_are_tables_after_their_table() {
    let cat = partitioned(dbiji_core::DbKind::Postgres);
    let d = TestDialect::pg();
    let opts = CompletionOptions::default();
    let sql = "SELECT * FROM api";
    let r = complete(sql, sql.len(), &d, &cat, &opts);
    assert_eq!(
        labels(&r)[..3],
        ["api_calls", "api_calls_2029_01", "api_calls_2029_02"]
    );
    assert_eq!(
        find(&r, "api_calls_2029_01").unwrap().detail.as_deref(),
        Some("partition of api_calls")
    );
    // Its columns are its table's.
    let sql = "SELECT p. FROM api_calls_2029_01 p";
    let r = complete(sql, "SELECT p.".len(), &d, &cat, &opts);
    assert_eq!(columns(&r), ["id", "called_at"]);
    // A MySQL partition is no table.
    let cat = partitioned(dbiji_core::DbKind::MySql);
    let sql = "SELECT * FROM api";
    let r = complete(sql, sql.len(), &TestDialect::mysql(), &cat, &opts);
    assert!(
        has(&r, "api_calls") && !has(&r, "api_calls_2029_01"),
        "{:?}",
        labels(&r)
    );
}
