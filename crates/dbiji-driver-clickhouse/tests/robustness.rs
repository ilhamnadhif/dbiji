//! Arbitrary-input robustness: the hand written JSON reader, type parser,
//! response body parser, placeholder binder and plan parsers must never
//! panic.

use dbiji_core::Value;
use dbiji_core::dialect::Dialect;
use dbiji_driver_clickhouse::ClickHouseDialect;
use dbiji_driver_clickhouse::stream::{BodyParser, Feed};

fn samples() -> Vec<Vec<u8>> {
    let alphabet: Vec<u8> = b"[]{},:\"\\nulrtefasd0123456789.-+eE \n\r'`?#()=NullableArrayTupleMap\xff\xc3\xa9".to_vec();
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut out: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"[".to_vec(),
        b"\"".to_vec(),
        b"\"\\u".to_vec(),
        b"\"\\ud800\\u".to_vec(),
        b"__exception__".to_vec(),
        b"Code: ".to_vec(),
        b"Code: 1.".to_vec(),
        b"Nullable(".to_vec(),
        b"Tuple(a".to_vec(),
        b"Enum8('a' =".to_vec(),
        b"DateTime64(".to_vec(),
        "[\"😀\"]".as_bytes().to_vec(),
        vec![b'['; 5000],
    ];
    for _ in 0..20_000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let len = (seed % 48) as usize;
        let mut s = Vec::new();
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
fn parsers_never_panic() {
    let d = ClickHouseDialect::new();
    for bytes in samples() {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let _ = dbiji_driver_clickhouse::json::parse(&bytes);
        let _ = dbiji_driver_clickhouse::json::parse_row(&bytes);
        let ty = dbiji_driver_clickhouse::types::parse_type(&text);
        let _ = ty.kind();
        if let Ok(j) = dbiji_driver_clickhouse::json::parse(&bytes) {
            let _ = dbiji_driver_clickhouse::types::json_to_value(&ty, j);
        }
        let _ = dbiji_driver_clickhouse::types::strip_nullable(&text);
        let _ = dbiji_driver_clickhouse::types::make_nullable(&text);
        let mut p = BodyParser::new(Some("tag".into()), None);
        for line in bytes.split(|b| *b == b'\n') {
            if let Feed::Error(_) = p.feed(line) {
                break;
            }
        }
        let _ = p.finish();
        let _ = dbiji_driver_clickhouse::error::parse_exception(&text);
        let _ = dbiji_driver_clickhouse::error::exception_to_error(&text, Some(&text));
        let _ = dbiji_driver_clickhouse::params::bind(&text, &[Value::Int(1), Value::text("x")], &d);
        let _ = dbiji_driver_clickhouse::plan::build_plan(Some(&text), Some(&text));
        let _ = d.literal(&Value::text(text.clone()), Some(&text));
        let _ = d.quote_ident(&text);
        let _ = d.ddl().map_type(dbiji_core::DbKind::Postgres, &text, dbiji_core::value::DataKind::Text);
        let _ = dbiji_driver_clickhouse::session::first_keyword(&text);
        let _ = dbiji_driver_clickhouse::session::use_target(&text);
    }
}
