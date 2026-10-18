use super::*;
use crate::testutil::*;

fn fmt(src: &str) -> String {
    format_sql(src, &pg(), &FormatOptions::default())
}

fn fmt_with(src: &str, opts: &FormatOptions) -> String {
    format_sql(src, &pg(), opts)
}

#[test]
fn select_basic() {
    assert_eq!(
        fmt("select a, b from t where x = 1 and y = 2"),
        "SELECT a, b\nFROM t\nWHERE x = 1 AND y = 2"
    );
    assert_eq!(fmt("SELECT   *\n\n  FROM t"), "SELECT *\nFROM t");
    assert_eq!(
        fmt(
            "select count(*),max(x) from t group by y having count(*)>1 order by 1 desc limit 10 offset 5"
        ),
        "SELECT count(*), max(x)\nFROM t\nGROUP BY y\nHAVING count(*) > 1\nORDER BY 1 DESC\nLIMIT 10\nOFFSET 5"
    );
}

#[test]
fn long_lists_break() {
    let src = "select customer_identifier, customer_first_name, customer_last_name, customer_email_address, customer_created_at from customers";
    assert_eq!(
        fmt(src),
        "SELECT\n    customer_identifier,\n    customer_first_name,\n    customer_last_name,\n    customer_email_address,\n    customer_created_at\nFROM customers"
    );
    let src = "select * from t where first_condition_column = 1 and second_condition_column = 2 and third_condition_column = 3 or fourth = 4";
    assert_eq!(
        fmt(src),
        "SELECT *\nFROM t\nWHERE\n    first_condition_column = 1\n    AND second_condition_column = 2\n    AND third_condition_column = 3\n    OR fourth = 4"
    );
}

#[test]
fn joins_and_subqueries() {
    assert_eq!(
        fmt(
            "select * from users u left join orders o on o.user_id = u.id inner join items i on i.order_id = o.id"
        ),
        "SELECT *\nFROM users u\nLEFT JOIN orders o ON o.user_id = u.id\nINNER JOIN items i ON i.order_id = o.id"
    );
    assert_eq!(
        fmt("select * from (select a from t) s"),
        "SELECT *\nFROM (SELECT a FROM t) s"
    );
    let long = "select * from users where id in (select user_id from orders where total > 100 and status = 'paid' and created_at > now())";
    assert_eq!(
        fmt(long),
        "SELECT *\nFROM users\nWHERE id IN (\n    SELECT user_id FROM orders WHERE total > 100 AND status = 'paid' AND created_at > now()\n)"
    );
}

#[test]
fn ctes_and_unions() {
    assert_eq!(
        fmt("with x as (select 1) select * from x"),
        "WITH x AS (SELECT 1)\nSELECT *\nFROM x"
    );
    assert_eq!(
        fmt("with a as (select 1), b as (select 2) select * from a, b"),
        "WITH a AS (SELECT 1),\nb AS (SELECT 2)\nSELECT *\nFROM a, b"
    );
    assert_eq!(
        fmt("select a from t union all select b from u"),
        "SELECT a\nFROM t\nUNION ALL\nSELECT b\nFROM u"
    );
}

#[test]
fn dml() {
    assert_eq!(
        fmt("insert into t (a,b) values (1,2),(3,4)"),
        "INSERT INTO t (a, b)\nVALUES (1, 2), (3, 4)"
    );
    assert_eq!(
        fmt("update t set a=1,b=2 where id=3"),
        "UPDATE t\nSET a = 1, b = 2\nWHERE id = 3"
    );
    assert_eq!(
        fmt("delete from t where id=1"),
        "DELETE FROM t\nWHERE id = 1"
    );
    assert_eq!(
        fmt(
            "insert into t (a) values (1) on conflict (a) do update set a = excluded.a returning *"
        ),
        "INSERT INTO t (a)\nVALUES (1)\nON CONFLICT (a)\nDO UPDATE SET a = excluded.a\nRETURNING *"
    );
    assert_eq!(
        fmt("insert into t select * from u"),
        "INSERT INTO t\nSELECT *\nFROM u"
    );
}

#[test]
fn create_table_and_view() {
    assert_eq!(
        fmt(
            "create table t (id int primary key, name varchar(100) not null, constraint u unique (name))"
        ),
        "CREATE TABLE t (\n    id int PRIMARY KEY,\n    name varchar(100) NOT NULL,\n    CONSTRAINT u UNIQUE (name)\n)"
    );
    assert_eq!(
        fmt("create view v as select a from t"),
        "CREATE VIEW v AS\nSELECT a\nFROM t"
    );
    assert_eq!(
        fmt("create table t2 as select * from t"),
        "CREATE TABLE t2 AS\nSELECT *\nFROM t"
    );
}

#[test]
fn case_expressions() {
    assert_eq!(
        fmt("select case when a=1 then 'x' else 'y' end from t"),
        "SELECT CASE WHEN a = 1 THEN 'x' ELSE 'y' END\nFROM t"
    );
    let long = "select case when status = 'pending_payment' then 'awaiting payment' when status = 'shipped' then 'on its way' else 'unknown status' end as label from orders";
    assert_eq!(
        fmt(long),
        "SELECT CASE\n    WHEN status = 'pending_payment' THEN 'awaiting payment'\n    WHEN status = 'shipped' THEN 'on its way'\n    ELSE 'unknown status'\nEND AS label\nFROM orders"
    );
    // With several items, the CASE is indented with the list.
    let long2 = format!(
        "{}, id from orders",
        &long[..long.len() - " from orders".len()]
    );
    assert_eq!(
        fmt(&long2),
        "SELECT\n    CASE\n        WHEN status = 'pending_payment' THEN 'awaiting payment'\n        WHEN status = 'shipped' THEN 'on its way'\n        ELSE 'unknown status'\n    END AS label,\n    id\nFROM orders"
    );
}

#[test]
fn comments_are_kept() {
    assert_eq!(
        fmt("select a, -- first\n b from t"),
        "SELECT\n    a, -- first\n    b\nFROM t"
    );
    assert_eq!(
        fmt("-- header\nselect 1; -- trailing\n\n/* standalone */\n\nselect 2"),
        "-- header\nSELECT 1; -- trailing\n\n/* standalone */\n\nSELECT 2"
    );
    assert_eq!(
        fmt("select 1 /* inline */ from t"),
        "SELECT 1 /* inline */\nFROM t"
    );
    assert_eq!(fmt("select 1 -- c\n;"), "SELECT 1 -- c\n;");
    assert_eq!(fmt("select 1\n-- own line\n;"), "SELECT 1\n-- own line\n;");
}

#[test]
fn multiple_statements_and_options() {
    assert_eq!(fmt("select 1;select 2;"), "SELECT 1;\n\nSELECT 2;");
    let o = FormatOptions {
        lines_between_statements: 0,
        ..Default::default()
    };
    assert_eq!(fmt_with("select 1;select 2;", &o), "SELECT 1;\nSELECT 2;");
    // statements separated only by a blank line stay separate
    assert_eq!(fmt_with("select 1\n\nselect 2", &o), "SELECT 1\n\nSELECT 2");
    let o = FormatOptions {
        keyword_case: KeywordCase::Lower,
        ..Default::default()
    };
    assert_eq!(
        fmt_with("SELECT A FROM T WHERE X IS NULL", &o),
        "select A\nfrom T\nwhere X is null"
    );
    let o = FormatOptions {
        keyword_case: KeywordCase::Preserve,
        ..Default::default()
    };
    assert_eq!(fmt_with("Select a From t", &o), "Select a\nFrom t");
    let o = FormatOptions {
        compact_short_lists: false,
        ..Default::default()
    };
    assert_eq!(
        fmt_with("select a, b from t where x = 1", &o),
        "SELECT\n    a,\n    b\nFROM\n    t\nWHERE\n    x = 1"
    );
    let o = FormatOptions {
        compact_short_lists: false,
        comma_first: true,
        indent: IndentStyle::Spaces(2),
        ..Default::default()
    };
    assert_eq!(
        fmt_with("select a, b from t", &o),
        "SELECT\n  a\n  , b\nFROM\n  t"
    );
    let o = FormatOptions {
        compact_short_lists: false,
        newline_before_and_or: false,
        indent: IndentStyle::Tab,
        ..Default::default()
    };
    assert_eq!(
        fmt_with("select * from t where a = 1 and b = 2", &o),
        "SELECT\n\t*\nFROM\n\tt\nWHERE\n\ta = 1 AND\n\tb = 2"
    );
    assert_eq!(fmt("select 1\n"), "SELECT 1\n");
}

#[test]
fn keyword_case_respects_names() {
    // `user` after FROM is a table name: case kept
    assert_eq!(
        format_sql("select * from user", &mysql(), &FormatOptions::default()),
        "SELECT *\nFROM user"
    );
    // non-reserved keywords used as columns keep their case
    assert_eq!(fmt("select name, type from t"), "SELECT name, type\nFROM t");
    assert_eq!(fmt("select t.select from t"), "SELECT t.select\nFROM t");
}

#[test]
fn spacing_rules() {
    assert_eq!(
        fmt("select a::int, -1, x-1, f(a,b), arr[1], t.*, count( * ) from t"),
        "SELECT a::int, -1, x - 1, f(a, b), arr[1], t.*, count(*)\nFROM t"
    );
    assert_eq!(
        fmt("select * from t where a=-1 and b in(1,2)"),
        "SELECT *\nFROM t\nWHERE a = -1 AND b IN (1, 2)"
    );
    assert_eq!(
        format_sql("select 1--1", &mysql(), &FormatOptions::default()),
        "SELECT 1 --1"
    );
    assert_eq!(
        format_sql(
            "select _utf8mb4'x', @a:=1, @@version",
            &mysql(),
            &FormatOptions::default()
        ),
        "SELECT _utf8mb4'x', @a := 1, @@version"
    );
    // MySQL function calls keep the parenthesis glued
    assert_eq!(
        format_sql(
            "select count (*), ifnull(a, 1) from t",
            &mysql(),
            &FormatOptions::default()
        ),
        "SELECT count (*), ifnull(a, 1)\nFROM t"
    );
}

#[test]
fn procedural_code_keeps_lines() {
    let src = "DELIMITER //\nCREATE PROCEDURE p()\nBEGIN\n    select 1;\n  select 2;   \nEND//\nDELIMITER ;\nselect 3;";
    let out = format_sql(src, &mysql(), &FormatOptions::default());
    assert_eq!(
        out,
        "DELIMITER //\n\nCREATE PROCEDURE p()\nBEGIN\n    SELECT 1;\n  SELECT 2;\nEND//\n\nDELIMITER ;\n\nSELECT 3;"
    );
    let src = "create function f() returns int as $$ select 1 $$ language sql;";
    assert_eq!(
        fmt(src),
        "CREATE FUNCTION f() RETURNS int AS $$ select 1 $$ LANGUAGE sql;"
    );
    let src = "SELECT 1\nGO\nselect 2\ngo";
    assert_eq!(
        format_sql(src, &tsql(), &FormatOptions::default()),
        "SELECT 1\nGO\n\nSELECT 2\ngo"
    );
    let src = "@set x = 1\nselect ${x}";
    assert_eq!(fmt(src), "@set x = 1\n\nSELECT ${x}");
}

/// Statements of several dialects for corpus tests.
const CORPUS_PG: &[&str] = &[
    "SELECT 1",
    "select a, b, c from t",
    "SELECT DISTINCT ON (a) a, b FROM t ORDER BY a, b DESC NULLS LAST",
    "select * from t1 natural join t2 cross join t3 full outer join t4 using (id)",
    "SELECT u.id, count(o.id) AS n FROM users u LEFT JOIN orders o ON o.user_id = u.id AND o.status <> 'x' WHERE u.created_at >= now() - interval '7 days' GROUP BY u.id HAVING count(o.id) > 3 ORDER BY n DESC LIMIT 10",
    "WITH RECURSIVE t(n) AS (VALUES (1) UNION ALL SELECT n + 1 FROM t WHERE n < 100) SELECT sum(n) FROM t",
    "with x as materialized (select * from big) select * from x",
    "select a from t where b between 1 and 10 and c not between 2 and 3 or d is distinct from e",
    "SELECT * FROM t WHERE (a = 1 OR b = 2) AND NOT (c = 3)",
    "SELECT json_col->>'key', json_col #> '{a,b}', arr[1:2], data @> '{\"a\":1}' FROM t",
    "SELECT $1::text, E'it\\'s', U&'d\\0061t', $tag$ body ; $tag$ FROM t WHERE x = ANY($2)",
    "select row_number() over (partition by a order by b rows between unbounded preceding and current row) from t",
    "select count(*) filter (where x > 0) from t",
    "SELECT * FROM t FOR UPDATE SKIP LOCKED",
    "insert into t (a, b) values (1, 'x') on conflict (a) do nothing",
    "INSERT INTO t DEFAULT VALUES RETURNING id",
    "update t set (a, b) = (select 1, 2) where id = 1 returning *",
    "UPDATE t SET a = s.a FROM s WHERE s.id = t.id",
    "delete from t using u where u.id = t.id",
    "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET v = s.v WHEN NOT MATCHED THEN INSERT (id, v) VALUES (s.id, s.v)",
    "create table if not exists s.t (id bigserial primary key, data jsonb not null default '{}', created_at timestamptz default now(), check (id > 0))",
    "create index concurrently if not exists idx on t using gin (data jsonb_path_ops) where deleted_at is null",
    "ALTER TABLE t ADD COLUMN x int, DROP COLUMN y, ALTER COLUMN z SET NOT NULL",
    "drop table if exists a, b cascade",
    "TRUNCATE TABLE t RESTART IDENTITY",
    "explain (analyze, buffers) select * from t",
    "EXPLAIN ANALYZE SELECT 1",
    "COPY t (a, b) FROM STDIN WITH (FORMAT csv)",
    "grant select, insert on all tables in schema public to app",
    "COMMENT ON TABLE t IS 'a table'",
    "SET search_path TO app, public",
    "BEGIN; COMMIT;",
    "VACUUM (VERBOSE, ANALYZE) t",
    "do $$ begin perform 1; end $$",
    "CREATE OR REPLACE FUNCTION f(a int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT a + 1; END",
    "select (select max(x) from u where u.id = t.id) as m, exists (select 1 from v) from t",
    "SELECT * FROM generate_series(1, 10) AS g(n) CROSS JOIN LATERAL (SELECT n * 2 AS d) l",
    "SELECT 'a' || 'b', 10 % 3, 2 ^ 3, |/ 25, @ -5, ~ 1, 1 << 2",
    "SELECT * FROM \"Mixed Case\".\"Table\" \"T\" WHERE \"T\".\"Col\" = 'v'",
    "select /* hint */ a -- trailing\n, b /* mid */ from t -- end",
    "SELECT ARRAY[1, 2, 3], ROW(1, 'a'), CAST(x AS numeric(10, 2)) FROM t",
    "select * from t where a in (select b from u union select c from v)",
    "SELECT * FROM (SELECT * FROM (SELECT 1) a) b",
    "values (1, 'a'), (2, 'b')",
    "TABLE users",
    "select a from t order by a using >",
    "PREPARE q(int) AS SELECT $1",
    "LISTEN chan",
    "select interval '1 day', timestamp '2024-01-01', date '2024-01-01'",
    "SELECT x FROM t WHERE y ILIKE '%z%' ESCAPE '\\'",
];

const CORPUS_MYSQL: &[&str] = &[
    "SELECT `a`, `b` FROM `db`.`t` WHERE `c` = \"str\" AND d = 'it\\'s'",
    "select * from t limit 10, 20",
    "SELECT SQL_CALC_FOUND_ROWS * FROM t USE INDEX (idx) WHERE a = ?",
    "insert into t (a, b) values (1, 2) on duplicate key update b = values(b)",
    "INSERT IGNORE INTO t SET a = 1, b = 2",
    "REPLACE INTO t (id, v) VALUES (1, 'x')",
    "update t1 join t2 on t1.id = t2.id set t1.a = t2.a where t2.b > 0",
    "delete t1 from t1 left join t2 on t1.id = t2.id where t2.id is null",
    "CREATE TABLE `t` (`id` int NOT NULL AUTO_INCREMENT, `name` varchar(50) DEFAULT NULL, PRIMARY KEY (`id`), KEY `idx_name` (`name`)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
    "SELECT @rownum := @rownum + 1, @@session.sql_mode FROM t",
    "select 1 # hash comment\n from dual",
    "SELECT * FROM t WHERE a <=> b AND c REGEXP '^x'",
    "select group_concat(distinct a order by a separator ',') from t",
    "SHOW CREATE TABLE t",
    "LOCK TABLES t READ, u WRITE",
    "select * from t partition (p1) as x straight_join u on u.id = x.id",
    "SELECT CAST(a AS CHAR(10)), CONVERT(b, SIGNED) FROM t",
    "/*!40101 SET NAMES utf8mb4 */",
    "select * from t where x = 1--1",
];

const CORPUS_SQLITE: &[&str] = &[
    "select * from [table name] where \"col\" = 1",
    "INSERT OR REPLACE INTO t (a) VALUES (?1)",
    "PRAGMA table_info(users)",
    "CREATE TRIGGER t AFTER INSERT ON a BEGIN INSERT INTO b VALUES (new.x); END",
    "SELECT * FROM t WHERE a GLOB 'x*' LIMIT 5",
    "WITH x AS (SELECT 1) SELECT * FROM x",
];

const CORPUS_TSQL: &[&str] = &[
    "SELECT TOP (10) PERCENT [a], [b c] FROM [dbo].[t] WITH (NOLOCK) WHERE @x = 1",
    "select top 5 * from #tmp order by 1",
    "DECLARE @x INT = 1",
    "IF @x = 1 BEGIN PRINT 'a'; END",
    "SELECT N'unicode', 0xFF FROM t",
    "UPDATE t SET a = 1 OUTPUT inserted.a WHERE b = 2",
];

fn check_corpus(stmts: &[&str], syn: &SqlSyntax) {
    let options = [
        FormatOptions::default(),
        FormatOptions {
            compact_short_lists: false,
            ..Default::default()
        },
        FormatOptions {
            comma_first: true,
            newline_before_and_or: false,
            keyword_case: KeywordCase::Lower,
            ..Default::default()
        },
        FormatOptions {
            max_line_length: 30,
            indent: IndentStyle::Tab,
            lines_between_statements: 0,
            ..Default::default()
        },
        FormatOptions {
            keyword_case: KeywordCase::Preserve,
            max_line_length: 200,
            ..Default::default()
        },
    ];
    for src in stmts {
        for opts in &options {
            let (once, fallbacks) = format_inner(src, syn, opts);
            assert!(
                equivalent(src, &once, syn),
                "tokens changed for {src:?}\n--- got ---\n{once}"
            );
            assert_eq!(fallbacks, 0, "full layout fell back for {src:?}");
            let twice = format_sql(&once, syn, opts);
            assert_eq!(once, twice, "not idempotent for {src:?} with {opts:?}");
        }
    }
    // The whole corpus as one script.
    let script = stmts.join(";\n");
    for opts in &options {
        let (once, fallbacks) = format_inner(&script, syn, opts);
        assert!(
            equivalent(&script, &once, syn),
            "script tokens changed:\n{once}"
        );
        assert_eq!(fallbacks, 0);
        assert_eq!(format_sql(&once, syn, opts), once);
    }
}

#[test]
fn corpus_postgres() {
    check_corpus(CORPUS_PG, &pg());
}

#[test]
fn corpus_mysql() {
    check_corpus(CORPUS_MYSQL, &mysql());
}

#[test]
fn corpus_sqlite() {
    check_corpus(CORPUS_SQLITE, &sqlite());
}

#[test]
fn corpus_tsql() {
    check_corpus(CORPUS_TSQL, &tsql());
}

#[test]
fn random_inputs_preserve_tokens_and_are_idempotent() {
    let mut rng = Rng::new(77);
    let opts = FormatOptions::default();
    for _ in 0..300 {
        let src = random_sql(&mut rng, 25);
        for syn in [pg(), mysql(), tsql()] {
            let once = format_sql(&src, &syn, &opts);
            assert!(equivalent(&src, &once, &syn));
            let twice = format_sql(&once, &syn, &opts);
            assert_eq!(once, twice, "not idempotent for {src:?}");
        }
    }
}

#[test]
fn empty_and_whitespace() {
    assert_eq!(fmt(""), "");
    assert_eq!(fmt("  \n "), "  \n ");
    assert_eq!(fmt("-- only a comment"), "-- only a comment");
    assert_eq!(fmt(";"), ";");
}
