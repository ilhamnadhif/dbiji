//! Generated sample data shared by the examples (not part of the library).
#![allow(dead_code)]

use dbiji_core::ddl::{ColumnDef, ForeignKeyDef, KeyDef, SchemaSnapshot, TableDef, ViewDef};
use dbiji_core::plan::{PlanNode, QueryPlan};
use dbiji_core::value::{ColumnMeta, DataKind, Row, Value};

/// Small deterministic random generator.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    pub fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    pub fn f64(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

const DOMAINS: [&str; 15] = [
    "auth",
    "crm",
    "sales",
    "billing",
    "catalog",
    "inventory",
    "shipping",
    "hr",
    "finance",
    "support",
    "marketing",
    "analytics",
    "project",
    "content",
    "audit",
];
const ENTITIES: [&str; 16] = [
    "accounts",
    "items",
    "events",
    "documents",
    "settings",
    "notes",
    "logs",
    "tags",
    "attachments",
    "requests",
    "batches",
    "periods",
    "rules",
    "assignments",
    "snapshots",
    "links",
];
const ATTRS: [(&str, &str); 22] = [
    ("name", "varchar(200)"),
    ("code", "varchar(32)"),
    ("status", "varchar(20)"),
    ("created_at", "timestamp with time zone"),
    ("updated_at", "timestamp with time zone"),
    ("amount", "numeric(12,2)"),
    ("quantity", "integer"),
    ("description", "text"),
    ("email", "varchar(320)"),
    ("phone", "varchar(40)"),
    ("is_active", "boolean"),
    ("valid_from", "date"),
    ("valid_to", "date"),
    ("priority", "smallint"),
    ("payload", "jsonb"),
    ("currency", "char(3)"),
    ("weight_kg", "numeric(8,3)"),
    ("external_ref", "uuid"),
    ("notes", "text"),
    ("score", "double precision"),
    ("sort_order", "integer"),
    ("version", "bigint"),
];

/// A schema snapshot with `tables` tables (~10 columns each, so 200 tables
/// give ~2,000 columns) in domains of related tables with foreign keys,
/// a central users table, some views, cycles and self references.
pub fn schema(tables: usize, seed: u64) -> SchemaSnapshot {
    let mut rng = Rng::new(seed);
    let mut defs: Vec<TableDef> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let per_domain = tables.div_ceil(DOMAINS.len()).max(1);
    'outer: for domain in DOMAINS {
        for e in 0..per_domain {
            if defs.len() >= tables {
                break 'outer;
            }
            let entity = ENTITIES[e % ENTITIES.len()];
            let name = if e < ENTITIES.len() {
                format!("{domain}_{entity}")
            } else {
                format!("{domain}_{entity}_{e}")
            };
            let mut t = TableDef::new(Some("public".into()), name.clone());
            t.columns
                .push(ColumnDef::new("id", "bigint").not_null().auto_increment());
            t.primary_key = Some(KeyDef {
                name: Some(format!("{name}_pkey")),
                columns: vec!["id".into()],
            });
            let attrs = 6 + rng.below(5) as usize;
            let mut used = Vec::new();
            while used.len() < attrs {
                let (n, ty) = *rng.pick(&ATTRS);
                if !used.contains(&n) {
                    used.push(n);
                    let mut c = ColumnDef::new(n, ty);
                    if n == "name" || n == "code" || n == "created_at" {
                        c = c.not_null();
                    }
                    t.columns.push(c);
                }
            }
            if rng.below(4) == 0 {
                t.comment = Some(format!(
                    "{} of the {domain} module",
                    entity.replace('_', " ")
                ));
            }
            defs.push(t);
            names.push(name);
        }
    }
    // central table referenced from many domains
    let users_idx = 0;
    let len = defs.len();
    for i in 1..len {
        let domain_start = (i / per_domain) * per_domain;
        let mut refs: Vec<usize> = Vec::new();
        // reference an earlier table of the same domain (master-detail chains)
        if i > domain_start && rng.below(10) < 8 {
            refs.push(domain_start + rng.below((i - domain_start) as u64) as usize);
        }
        // cross-domain references to the users table or a domain master
        if rng.below(10) < 3 {
            refs.push(users_idx);
        } else if rng.below(10) < 2 && domain_start > 0 {
            refs.push(
                (rng.below(domain_start as u64 / per_domain as u64 + 1) as usize) * per_domain,
            );
        }
        refs.sort_unstable();
        refs.dedup();
        for (k, r) in refs.into_iter().enumerate() {
            if r == i || r >= len {
                continue;
            }
            let ref_name = names[r].clone();
            let col = format!(
                "{}_id",
                ref_name
                    .split('_')
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join("_")
                    .trim_end_matches('s')
            );
            let col = if defs[i].column(&col).is_some() {
                format!("{col}_{k}")
            } else {
                col
            };
            let nullable = rng.below(3) == 0;
            let mut c = ColumnDef::new(col.clone(), "bigint");
            if !nullable {
                c = c.not_null();
            }
            defs[i].columns.insert(1, c);
            defs[i].foreign_keys.push(ForeignKeyDef {
                name: Some(format!("{}_{}_fkey", names[i], col)),
                columns: vec![col],
                ref_table: ref_name,
                ref_columns: vec!["id".into()],
                ..Default::default()
            });
        }
    }
    // self references and a cycle
    if len > 20 {
        for idx in [7usize, 19] {
            let n = names[idx].clone();
            defs[idx]
                .columns
                .push(ColumnDef::new("parent_id", "bigint"));
            defs[idx].foreign_keys.push(ForeignKeyDef {
                name: Some(format!("{n}_parent_fkey")),
                columns: vec!["parent_id".into()],
                ref_table: n,
                ref_columns: vec!["id".into()],
                ..Default::default()
            });
        }
        let (a, b) = (names[3].clone(), names[len - 1].clone());
        defs[3]
            .columns
            .push(ColumnDef::new("last_link_id", "bigint"));
        defs[3].foreign_keys.push(ForeignKeyDef {
            name: Some(format!("{a}_last_link_fkey")),
            columns: vec!["last_link_id".into()],
            ref_table: b,
            ref_columns: vec!["id".into()],
            ..Default::default()
        });
    }
    let views = vec![
        ViewDef {
            schema: Some("public".into()),
            name: "v_active_accounts".into(),
            columns: vec![
                ColumnDef::new("id", "bigint"),
                ColumnDef::new("name", "varchar(200)"),
            ],
            comment: Some("Accounts with recent activity".into()),
            ..Default::default()
        },
        ViewDef {
            schema: Some("public".into()),
            name: "v_monthly_revenue".into(),
            columns: vec![
                ColumnDef::new("month", "date"),
                ColumnDef::new("revenue", "numeric"),
            ],
            ..Default::default()
        },
    ];
    SchemaSnapshot {
        schema: Some("public".into()),
        tables: defs,
        views,
        ..Default::default()
    }
}

/// A PostgreSQL like EXPLAIN ANALYZE plan.
pub fn sample_plan() -> QueryPlan {
    fn node(
        t: &str,
        rel: Option<&str>,
        cost: (f64, f64),
        rows: f64,
        actual: Option<(f64, f64, f64)>,
    ) -> PlanNode {
        let mut n = PlanNode::new(t);
        n.relation = rel.map(str::to_string);
        n.alias = rel.map(|r| r.chars().next().unwrap_or('t').to_string());
        n.startup_cost = Some(cost.0);
        n.total_cost = Some(cost.1);
        n.plan_rows = Some(rows);
        n.plan_width = Some(48.0);
        if let Some((ms, rows, loops)) = actual {
            n.actual_total_ms = Some(ms);
            n.actual_startup_ms = Some(ms * 0.1);
            n.actual_rows = Some(rows);
            n.loops = Some(loops);
        }
        n
    }
    let mut seq = node(
        "Seq Scan",
        Some("orders"),
        (0.0, 18334.0),
        120_000.0,
        Some((210.4, 1_000_000.0, 1.0)),
    );
    seq.condition = Some("Filter: (status = 'shipped'::text)".into());
    seq.properties = vec![
        ("Rows Removed by Filter".into(), "880000".into()),
        ("Shared Hit Blocks".into(), "1203".into()),
        ("Shared Read Blocks".into(), "7012".into()),
    ];
    let mut idx = node(
        "Index Scan",
        Some("customers"),
        (0.42, 8.44),
        1.0,
        Some((0.004, 1.0, 120_000.0)),
    );
    idx.index = Some("customers_pkey".into());
    idx.condition = Some("Index Cond: (id = o.customer_id)".into());
    let mut nl = node(
        "Nested Loop",
        None,
        (0.42, 1_030_000.0),
        120_000.0,
        Some((820.0, 120_000.0, 1.0)),
    );
    nl.children = vec![seq, idx];
    let mut hash_inner = node(
        "Seq Scan",
        Some("regions"),
        (0.0, 1.2),
        12.0,
        Some((0.02, 12.0, 1.0)),
    );
    hash_inner.properties = vec![("Shared Hit Blocks".into(), "1".into())];
    let mut hash = node("Hash", None, (1.2, 1.2), 12.0, Some((0.03, 12.0, 1.0)));
    hash.properties = vec![
        ("Hash Buckets".into(), "1024".into()),
        ("Hash Batches".into(), "1".into()),
        ("Peak Memory Usage".into(), "9kB".into()),
    ];
    hash.children = vec![hash_inner];
    let mut hj = node(
        "Hash Join",
        None,
        (1.5, 1_031_200.0),
        120_000.0,
        Some((905.0, 120_000.0, 1.0)),
    );
    hj.condition = Some("Hash Cond: (c.region_id = r.id)".into());
    hj.children = vec![nl, hash];
    let mut sort = node(
        "Sort",
        None,
        (1_045_000.0, 1_045_300.0),
        120_000.0,
        Some((1012.0, 120_000.0, 1.0)),
    );
    sort.properties = vec![
        ("Sort Key".into(), "r.name, (sum(o.total)) DESC".into()),
        ("Sort Method".into(), "external merge  Disk: 5480kB".into()),
    ];
    sort.children = vec![hj];
    let mut agg = node(
        "HashAggregate",
        None,
        (1_046_000.0, 1_046_100.0),
        12.0,
        Some((1033.0, 12.0, 1.0)),
    );
    agg.properties = vec![
        ("Group Key".into(), "r.name".into()),
        ("Batches".into(), "1".into()),
    ];
    agg.children = vec![sort];
    let mut limit = node(
        "Limit",
        None,
        (1_046_100.0, 1_046_100.1),
        10.0,
        Some((1034.0, 10.0, 1.0)),
    );
    limit.children = vec![agg];
    let mut plan = QueryPlan {
        roots: vec![limit],
        planning_ms: Some(0.9),
        execution_ms: Some(1035.2),
        analyzed: true,
        ..Default::default()
    };
    plan.text = plan.to_text_tree();
    plan
}

/// Result set for charts: daily revenue per region and channel.
pub fn chart_rows(days: usize, seed: u64) -> (Vec<ColumnMeta>, Vec<Row>) {
    let mut rng = Rng::new(seed);
    let columns = vec![
        ColumnMeta::new("day", "date", DataKind::Date),
        ColumnMeta::new("region", "text", DataKind::Text),
        ColumnMeta::new("channel", "text", DataKind::Text),
        ColumnMeta::new("revenue", "numeric", DataKind::Decimal),
        ColumnMeta::new("orders", "int8", DataKind::Int),
        ColumnMeta::new("avg_basket", "float8", DataKind::Float),
    ];
    let start = chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap_or_default();
    let regions = ["North", "South", "East", "West"];
    let channels = ["web", "store", "partner"];
    let mut rows = Vec::new();
    for d in 0..days {
        let day = start + chrono::Days::new(d as u64);
        for (ri, region) in regions.iter().enumerate() {
            for channel in channels {
                let base = 1000.0 + ri as f64 * 400.0 + (d as f64 / 7.0).sin() * 250.0;
                let revenue = base * (0.6 + rng.f64()) * if channel == "web" { 1.4 } else { 1.0 };
                let orders = (revenue / (40.0 + rng.f64() * 20.0)).round() as i64;
                rows.push(vec![
                    Value::Date(day),
                    Value::text(*region),
                    Value::text(channel),
                    Value::decimal(format!("{revenue:.2}")),
                    Value::Int(orders),
                    Value::Float(revenue / orders.max(1) as f64),
                ]);
            }
        }
    }
    (columns, rows)
}

/// Rows with WKT geometries: city points, roads and districts with holes.
pub fn spatial_rows() -> (Vec<ColumnMeta>, Vec<Row>) {
    let columns = vec![
        ColumnMeta::new("id", "int4", DataKind::Int),
        ColumnMeta::new("name", "text", DataKind::Text),
        ColumnMeta::new("geom", "geometry", DataKind::Geometry),
    ];
    let mut rows = Vec::new();
    let mut rng = Rng::new(7);
    let mut id = 1;
    let mut push = |rows: &mut Vec<Row>, name: String, wkt: String| {
        rows.push(vec![Value::Int(id), Value::text(name), Value::text(wkt)]);
        id += 1;
    };
    let cities = [
        ("Jakarta", 106.85, -6.21),
        ("Bandung", 107.61, -6.91),
        ("Surabaya", 112.75, -7.25),
        ("Semarang", 110.42, -6.97),
        ("Yogyakarta", 110.37, -7.80),
        ("Denpasar", 115.22, -8.65),
        ("Medan", 98.67, 3.59),
        ("Makassar", 119.43, -5.15),
    ];
    for (name, x, y) in cities {
        push(
            &mut rows,
            name.to_string(),
            format!("SRID=4326;POINT({x} {y})"),
        );
    }
    push(
        &mut rows,
        "Java road".into(),
        "SRID=4326;LINESTRING(106.85 -6.21, 107.61 -6.91, 110.42 -6.97, 112.75 -7.25)".into(),
    );
    push(
        &mut rows,
        "Bali ferry".into(),
        "SRID=4326;LINESTRING(114.38 -8.12, 114.44 -8.20, 115.22 -8.65)".into(),
    );
    push(
        &mut rows,
        "Central district".into(),
        "SRID=4326;POLYGON((109.5 -6.5, 111.5 -6.6, 111.8 -7.6, 110.3 -8.2, 109.4 -7.5, 109.5 -6.5),(110.2 -7.0, 110.8 -7.0, 110.6 -7.4, 110.2 -7.0))"
            .into(),
    );
    push(
        &mut rows,
        "Islands".into(),
        "SRID=4326;MULTIPOLYGON(((114.4 -8.0, 115.7 -8.05, 115.6 -8.8, 114.5 -8.6, 114.4 -8.0)),((115.9 -8.3, 116.7 -8.2, 116.5 -8.9, 116.0 -8.8, 115.9 -8.3)))"
            .into(),
    );
    for i in 0..40 {
        let x = 105.0 + rng.f64() * 12.0;
        let y = -9.0 + rng.f64() * 4.0;
        push(
            &mut rows,
            format!("Station {i}"),
            format!("SRID=4326;POINT({x:.4} {y:.4})"),
        );
    }
    (columns, rows)
}
