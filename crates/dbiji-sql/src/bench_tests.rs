//! Performance checks. Run in release mode:
//! `cargo test -p dbiji-sql --release -- --ignored --nocapture bench_`

use crate::catalog::SnapshotCatalog;
use crate::complete::{CompletionOptions, complete};
use crate::format::{FormatOptions, format_sql};
use crate::highlight::highlight;
use crate::spell::{SpellChecker, check_spelling};
use crate::split::{SplitOptions, split_statements};
use crate::testutil::*;
use crate::token::tokenize;
use dbiji_core::ddl::{ColumnDef, SchemaSnapshot, TableDef};
use std::collections::HashSet;
use std::time::Instant;

fn script(lines: usize) -> String {
    let parts = [
        "SELECT u.id, u.name, count(o.id) AS n FROM users u",
        "  LEFT JOIN orders o ON o.user_id = u.id AND o.status <> 'cancelled' -- only live orders",
        "WHERE u.created_at > now() - interval '30 days' AND u.email LIKE '%@example.com'",
        "GROUP BY u.id, u.name HAVING count(o.id) > 3 ORDER BY n DESC LIMIT 100;",
        "/* a block comment with ; inside */ INSERT INTO log (msg, at) VALUES ('it''s done', $1);",
    ];
    let mut s = String::with_capacity(lines * 80);
    for i in 0..lines {
        s.push_str(parts[i % parts.len()]);
        s.push('\n');
    }
    s
}

fn catalog(n: usize) -> SnapshotCatalog {
    let mut snap = SchemaSnapshot {
        schema: Some("public".into()),
        ..Default::default()
    };
    for i in 0..n {
        let mut t = TableDef::new(Some("public".into()), format!("table_{i:04}"));
        for c in 0..15 {
            t.columns
                .push(ColumnDef::new(format!("column_{c}"), "integer"));
        }
        snap.tables.push(t);
    }
    SnapshotCatalog::new(vec![snap], Some("public".into()))
}

fn time<T>(label: &str, runs: u32, mut f: impl FnMut() -> T) -> f64 {
    let start = Instant::now();
    for _ in 0..runs {
        std::hint::black_box(f());
    }
    let ms = start.elapsed().as_secs_f64() * 1000.0 / runs as f64;
    println!("{label:<45} {ms:>9.3} ms");
    ms
}

#[test]
#[ignore]
fn bench_all() {
    let t = Instant::now();
    let en = SpellChecker::english();
    println!(
        "{:<45} {:>9.3} ms ({} words)",
        "spell dictionary init",
        t.elapsed().as_secs_f64() * 1000.0,
        en.len()
    );

    let big = script(100_000);
    let syn = pg();
    let tok = time("tokenize 100k lines", 5, || tokenize(&big, &syn).len());
    time("split 100k lines", 3, || {
        split_statements(&big, &syn, &SplitOptions::default()).len()
    });
    time("highlight 100k lines", 3, || highlight(&big, &syn).len());

    let mid = script(1000);
    let cat = catalog(1000);
    let d = TestDialect::pg();
    let opts = CompletionOptions::default();
    let mut doc = mid.clone();
    doc.push_str("SELECT t.");
    let cur = doc.len();
    doc.push_str(" FROM table_0500 t JOIN ");
    let c1 = time("complete alias.column (1k tables, 1k lines)", 50, || {
        complete(&doc, cur, &d, &cat, &opts).items.len()
    });
    let end = doc.len();
    let c2 = time("complete after JOIN (1k tables, 1k lines)", 50, || {
        complete(&doc, end, &d, &cat, &opts).items.len()
    });
    let mut doc2 = mid.clone();
    doc2.push_str("SELECT * FROM tab");
    let end2 = doc2.len();
    let c3 = time("complete table prefix (1k tables, 1k lines)", 50, || {
        complete(&doc2, end2, &d, &cat, &opts).items.len()
    });

    time("format 1k lines", 5, || {
        format_sql(&mid, &syn, &FormatOptions::default()).len()
    });
    time("spell check 1k lines", 5, || {
        check_spelling(&mid, &syn, en, &HashSet::new()).len()
    });
    time("suggestions (5 words)", 5, || {
        ["recieve", "seperate", "tabel", "qeury", "definately"]
            .iter()
            .map(|w| en.suggestions(w, 5).len())
            .sum::<usize>()
    });

    if cfg!(not(debug_assertions)) {
        assert!(tok < 100.0, "tokenize too slow: {tok} ms");
        assert!(
            c1 < 10.0 && c2 < 10.0 && c3 < 10.0,
            "completion too slow: {c1} {c2} {c3}"
        );
    }
}
