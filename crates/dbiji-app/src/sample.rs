//! Sample database (DBeaver's "create sample database"): a small SQLite
//! store with customers, products, orders and order lines, plus a view,
//! indexes, a trigger and foreign keys, so every feature can be tried
//! without a server.

use dbiji_core::DbResult;
use std::path::Path;

const SCHEMA: &[&str] = &[
    "CREATE TABLE customer (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT UNIQUE, city TEXT, created_at TEXT NOT NULL DEFAULT (datetime('now')))",
    "CREATE TABLE category (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE)",
    "CREATE TABLE product (id INTEGER PRIMARY KEY, category_id INTEGER NOT NULL REFERENCES category(id), name TEXT NOT NULL, price NUMERIC NOT NULL CHECK (price >= 0), stock INTEGER NOT NULL DEFAULT 0)",
    "CREATE TABLE \"order\" (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES customer(id), ordered_at TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'new' CHECK (status IN ('new', 'paid', 'shipped', 'cancelled')))",
    "CREATE TABLE order_line (order_id INTEGER NOT NULL REFERENCES \"order\"(id) ON DELETE CASCADE, product_id INTEGER NOT NULL REFERENCES product(id), quantity INTEGER NOT NULL CHECK (quantity > 0), unit_price NUMERIC NOT NULL, PRIMARY KEY (order_id, product_id))",
    "CREATE INDEX ix_order_customer ON \"order\" (customer_id)",
    "CREATE INDEX ix_product_category ON product (category_id)",
    "CREATE VIEW order_total AS SELECT o.id AS order_id, c.name AS customer, o.ordered_at, o.status, SUM(l.quantity * l.unit_price) AS total FROM \"order\" o JOIN customer c ON c.id = o.customer_id JOIN order_line l ON l.order_id = o.id GROUP BY o.id",
    "CREATE TRIGGER trg_stock AFTER INSERT ON order_line BEGIN UPDATE product SET stock = stock - NEW.quantity WHERE id = NEW.product_id; END",
];

const CITIES: &[&str] = &["Jakarta", "Bandung", "Surabaya", "Yogyakarta", "Medan", "Makassar", "Denpasar", "Semarang"];
const FIRST: &[&str] = &["Ani", "Budi", "Citra", "Dewi", "Eko", "Fajar", "Gita", "Hadi", "Indah", "Joko", "Kartika", "Lukman"];
const LAST: &[&str] = &["Santoso", "Wijaya", "Pratama", "Saputra", "Lestari", "Hidayat", "Kurniawan", "Nugroho"];
const CATEGORIES: &[&str] = &["Books", "Coffee", "Electronics", "Garden", "Kitchen", "Toys"];

/// Create the sample database at `path` (fails when the file exists).
pub fn create(path: &Path) -> DbResult<()> {
    if path.exists() {
        return Err(dbiji_core::DbError::config(format!("{} already exists", path.display())));
    }
    let mut sql = String::from("BEGIN;\n");
    for s in SCHEMA {
        sql.push_str(s);
        sql.push_str(";\n");
    }
    for (i, c) in CATEGORIES.iter().enumerate() {
        sql.push_str(&format!("INSERT INTO category VALUES ({}, '{c}');\n", i + 1));
    }
    // Deterministic pseudo random data.
    let mut seed: u64 = 0x5eed;
    let mut next = |m: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % m
    };
    for id in 1..=200u64 {
        let f = FIRST[next(FIRST.len() as u64) as usize];
        let l = LAST[next(LAST.len() as u64) as usize];
        let city = CITIES[next(CITIES.len() as u64) as usize];
        let email = if next(10) == 0 { "NULL".to_string() } else { format!("'{}.{}{}@example.com'", f.to_lowercase(), l.to_lowercase(), id) };
        sql.push_str(&format!("INSERT INTO customer (id, name, email, city, created_at) VALUES ({id}, '{f} {l}', {email}, '{city}', '2026-{:02}-{:02} 10:00:00');\n", 1 + next(9), 1 + next(28)));
    }
    for id in 1..=60u64 {
        let cat = 1 + next(CATEGORIES.len() as u64);
        let price = 5 + next(495);
        sql.push_str(&format!("INSERT INTO product VALUES ({id}, {cat}, '{} item {id}', {price}.{:02}, {});\n", CATEGORIES[(cat - 1) as usize], next(100), 50 + next(500)));
    }
    let statuses = ["new", "paid", "shipped", "cancelled"];
    for id in 1..=800u64 {
        sql.push_str(&format!(
            "INSERT INTO \"order\" VALUES ({id}, {}, '2026-{:02}-{:02} {:02}:{:02}:00', '{}');\n",
            1 + next(200),
            1 + next(9),
            1 + next(28),
            8 + next(12),
            next(60),
            statuses[next(4) as usize]
        ));
        let lines = 1 + next(4);
        let mut used = Vec::new();
        for _ in 0..lines {
            let p = 1 + next(60);
            if used.contains(&p) {
                continue;
            }
            used.push(p);
            sql.push_str(&format!("INSERT INTO order_line VALUES ({id}, {p}, {}, (SELECT price FROM product WHERE id = {p}));\n", 1 + next(5)));
        }
    }
    sql.push_str("COMMIT;\n");
    let conn = rusqlite_like::open(path)?;
    conn.exec(&sql)
}

/// Minimal blocking SQLite access through the SQLite driver's session API.
mod rusqlite_like {
    use dbiji_core::{ConnectParams, DbResult, Driver};
    use std::path::Path;

    pub struct Conn {
        params: ConnectParams,
    }

    pub fn open(path: &Path) -> DbResult<Conn> {
        let mut cfg = dbiji_core::config::ConnectionConfig::new(dbiji_core::DbKind::Sqlite, "sample");
        cfg.file_path = Some(path.display().to_string());
        Ok(Conn { params: ConnectParams::from_config(&cfg, None) })
    }

    impl Conn {
        pub fn exec(&self, script: &str) -> DbResult<()> {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(dbiji_core::DbError::from)?;
            rt.block_on(async {
                let driver = dbiji_driver_sqlite::SqliteDriver::new();
                let syntax = driver.dialect().syntax().clone();
                let mut s = driver.connect(&self.params).await?;
                for span in dbiji_sql::split_statements(script, &syntax, &dbiji_sql::SplitOptions::default()) {
                    let stmt = span.text(script);
                    dbiji_core::driver::execute_update(s.as_mut(), stmt, &[]).await.map_err(|e| e.with_context(stmt.chars().take(120).collect::<String>()))?;
                }
                s.close().await
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sample_database_is_created() {
        let dir = std::env::temp_dir().join(format!("dbiji-sample-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("sample.db");
        let _ = std::fs::remove_file(&path);
        super::create(&path).unwrap();
        assert!(path.metadata().unwrap().len() > 10_000);
        assert!(super::create(&path).is_err(), "never overwrites");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
