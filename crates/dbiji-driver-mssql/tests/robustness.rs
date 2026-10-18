//! Arbitrary-input robustness: the hand written T-SQL scanners, the plan
//! parser and the dialect helpers must never panic.

use dbiji_core::dialect::Dialect;
use dbiji_driver_mssql::MsSqlDialect;
use dbiji_driver_mssql::sqltext::*;

/// Deterministic pseudo random strings over a nasty alphabet.
fn samples() -> Vec<String> {
    let alphabet: Vec<char> = "abcSELECTGOgo \n\t'\"[]()/*-;,.@#$0123456789éü😀N\\".chars().collect();
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut out = vec![
        String::new(),
        "'".into(),
        "[".into(),
        "/*".into(),
        "/* /* */".into(),
        "N'".into(),
        "GO".into(),
        "\nGO 0\n".into(),
        "SELECT '😀".into(),
        "((((".into(),
        "))))".into(),
        "CREATE VIEW".into(),
        "WITH".into(),
        "SELECT TOP".into(),
        "1e".into(),
        "0x".into(),
        "@".into(),
        "é".repeat(10),
    ];
    for _ in 0..20_000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let len = (seed % 40) as usize;
        let mut s = String::new();
        let mut x = seed;
        for _ in 0..len {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            s.push(alphabet[(x >> 33) as usize % alphabet.len()]);
        }
        out.push(s);
    }
    out
}

#[test]
fn scanners_never_panic() {
    let d = MsSqlDialect::new();
    for s in samples() {
        let _ = tokenize(&s);
        let _ = first_keyword(&s);
        let _ = classify(&s);
        let _ = is_scope_sensitive(&s);
        let _ = touches_session_state(&s);
        let _ = split_batches(&s);
        let _ = line_start_position(&s, 3);
        let _ = strip_outer_parens(&s);
        let _ = view_select(&s);
        let _ = select_shape(&s);
        let _ = unquote_ident(&s);
        let _ = d.limit_query(&s, 10, 5);
        let _ = d.count_query(&s);
        let _ = d.quote_ident(&s);
        let _ = d.literal(&dbiji_core::Value::text(s.clone()), Some(&s));
        let _ = dbiji_driver_mssql::plan::build_plan(std::slice::from_ref(&s), false);
        let _ = dbiji_driver_mssql::types::parse_numeric(&s);
        let _ = dbiji_driver_mssql::types::kind_of_type_name(&s);
        let _ = d.ddl().map_type(dbiji_core::DbKind::Postgres, &s, dbiji_core::value::DataKind::Text);
        let _ = d.ddl().map_type(dbiji_core::DbKind::ClickHouse, &s, dbiji_core::value::DataKind::Text);
    }
}
