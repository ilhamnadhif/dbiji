//! Deterministic generator of a wide mixed-type result set for the demo and
//! the benchmark (no external dependencies).

#![allow(dead_code)]

use chrono::NaiveDate;
use dbiji_core::{ColumnMeta, ColumnSource, DataKind, Row, SharedRowStore, Value};

/// Small xorshift PRNG.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

const FIRST: [&str; 16] = [
    "Alice", "Bob", "Carol", "Dewi", "Eko", "Farah", "Gita", "Hadi", "Intan", "Joko", "Kartika", "Lukas", "Maya",
    "Nina", "Oscar", "Putri",
];
const CITIES: [&str; 8] = ["Jakarta", "Bandung", "Surabaya", "Medan", "Denpasar", "Yogyakarta", "Makassar", "Semarang"];

/// Column kinds in a repeating pattern.
fn kind_for(c: usize) -> (DataKind, &'static str) {
    match c % 20 {
        0 => (DataKind::Int, "int8"),
        1 => (DataKind::Text, "varchar(40)"),
        2 => (DataKind::Decimal, "numeric(12,2)"),
        3 => (DataKind::Int, "int4"),
        4 => (DataKind::Float, "float8"),
        5 => (DataKind::Bool, "bool"),
        6 => (DataKind::Date, "date"),
        7 => (DataKind::Text, "text"),
        8 => (DataKind::DateTime, "timestamp"),
        9 => (DataKind::Int, "int4"),
        10 => (DataKind::Float, "float4"),
        11 => (DataKind::Text, "varchar(20)"),
        12 => (DataKind::Json, "jsonb"),
        13 => (DataKind::Int, "int2"),
        14 => (DataKind::Uuid, "uuid"),
        15 => (DataKind::Decimal, "numeric"),
        16 => (DataKind::Int, "int8"),
        17 => (DataKind::Text, "text"),
        18 => (DataKind::Binary, "bytea"),
        _ => (DataKind::Int, "int4"),
    }
}

/// Column metadata for `n` columns of table `demo.big` (column 0 is the key).
pub fn columns(n: usize) -> Vec<ColumnMeta> {
    (0..n)
        .map(|c| {
            let (kind, ty) = kind_for(c);
            let name = match c {
                0 => "id".to_string(),
                1 => "name".to_string(),
                2 => "amount".to_string(),
                3 => "category_id".to_string(),
                5 => "active".to_string(),
                6 => "birth_date".to_string(),
                7 => "city".to_string(),
                8 => "created_at".to_string(),
                12 => "attributes".to_string(),
                14 => "external_id".to_string(),
                17 => "notes".to_string(),
                18 => "avatar".to_string(),
                _ => format!("{}_{c}", ty.split('(').next().unwrap_or("col")),
            };
            let mut m = ColumnMeta::new(name.clone(), ty, kind).with_source(ColumnSource {
                database: None,
                schema: Some("demo".into()),
                table: "big".into(),
                column: name,
            });
            if c == 0 {
                m.nullable = Some(false);
                m.auto_increment = Some(true);
            }
            m
        })
        .collect()
}

/// One generated row.
pub fn row(i: usize, ncols: usize, rng: &mut Rng) -> Row {
    let base = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default();
    (0..ncols)
        .map(|c| {
            if c == 0 {
                return Value::Int(i as i64 + 1);
            }
            let r = rng.next();
            if r.is_multiple_of(37) && c != 1 {
                return Value::Null;
            }
            let (kind, _) = kind_for(c);
            match kind {
                DataKind::Int => match c {
                    3 => Value::Int((r % 12) as i64 + 1),
                    _ => Value::Int((r % 1_000_000) as i64 - 500_000),
                },
                DataKind::Float => Value::Float((r % 10_000_000) as f64 / 1000.0),
                DataKind::Decimal => Value::decimal(format!("{}.{:02}", r % 100_000, (r >> 20) % 100)),
                DataKind::Bool => Value::Bool(r & 1 == 0),
                DataKind::Date => Value::Date(base + chrono::Days::new(r % 20_000)),
                DataKind::DateTime => Value::DateTime(
                    (base + chrono::Days::new(18_000 + r % 2_000))
                        .and_hms_opt(((r >> 8) % 24) as u32, ((r >> 16) % 60) as u32, 0)
                        .unwrap_or_default(),
                ),
                DataKind::Text => match c % 20 {
                    1 => Value::text(format!("{} {}", FIRST[(r % 16) as usize], FIRST[((r >> 8) % 16) as usize])),
                    7 => Value::text(CITIES[(r % 8) as usize]),
                    17 if r.is_multiple_of(5) => {
                        Value::text(format!("Multi-line note for row {i}\nsecond line\nthird line"))
                    }
                    17 => Value::text(format!(
                        "Note {i}: a somewhat longer text value that will be truncated in narrow cells"
                    )),
                    _ => Value::text(format!("v{}", r % 100_000)),
                },
                DataKind::Json => {
                    Value::json(format!("{{\"k\":{},\"tags\":[\"a\",\"b\"],\"ok\":{}}}", r % 1000, r & 1 == 0))
                }
                DataKind::Uuid => Value::parse_as(
                    &format!(
                        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
                        r as u32,
                        (r >> 32) as u16,
                        (r >> 48) % 4096,
                        (r >> 4) % 4096,
                        r % 0xFFFF_FFFF_FFFF
                    ),
                    DataKind::Uuid,
                )
                .unwrap_or(Value::Null),
                // A few rows carry a real PNG so the image preview can be tried.
                DataKind::Binary if i < 3 || i.is_multiple_of(997) => Value::bytes(avatar_png(r)),
                DataKind::Binary => Value::bytes((0..(r % 24 + 4)).map(|k| (r >> (k % 8)) as u8).collect::<Vec<u8>>()),
                _ => Value::Null,
            }
        })
        .collect()
}

/// A small generated PNG (gradient with a circle).
pub fn avatar_png(seed: u64) -> Vec<u8> {
    let (a, b) = ((seed % 200) as u8 + 30, ((seed >> 8) % 200) as u8 + 30);
    let img = image::RgbaImage::from_fn(48, 48, |x, y| {
        let (dx, dy) = (x as i32 - 24, y as i32 - 24);
        if dx * dx + dy * dy < 15 * 15 {
            image::Rgba([250, 250, 250, 255])
        } else {
            image::Rgba([a, (x * 5) as u8, b.wrapping_add((y * 3) as u8), 255])
        }
    });
    let mut buf = Vec::new();
    let _ = image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png);
    buf
}

/// Fill `store` with `nrows` rows of `ncols` columns in batches (locking per batch).
pub fn fill(store: &SharedRowStore, nrows: usize, ncols: usize, seed: u64) {
    let mut rng = Rng::new(seed);
    let mut batch = Vec::with_capacity(4096);
    for i in 0..nrows {
        batch.push(row(i, ncols, &mut rng));
        if batch.len() == 4096 {
            store.lock().extend(batch.drain(..));
        }
    }
    store.lock().extend(batch.drain(..));
}
