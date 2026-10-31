//! SQL generation for the query builder.

use super::model::{JoinKind, QbCondition, QbJoin, QueryModel};
use dbiji_core::{DbKind, Dialect};
use std::collections::BTreeSet;

impl QueryModel {
    /// Renders an expression: `alias.column` references of tables in the
    /// model (and `alias.*`) are quoted with the dialect, anything else is
    /// passed through as written.
    pub fn render_expr(&self, expr: &str, dialect: &dyn Dialect) -> String {
        let e = expr.trim();
        if let Some(alias) = e.strip_suffix(".*")
            && self.table(alias).is_some()
        {
            return format!("{}.*", dialect.quote_ident(alias));
        }
        for t in &self.tables {
            if let Some(col) = e
                .strip_prefix(t.alias.as_str())
                .and_then(|r| r.strip_prefix('.'))
                && t.columns.iter().any(|c| c.name == col)
            {
                return format!(
                    "{}.{}",
                    dialect.quote_ident(&t.alias),
                    dialect.quote_ident(col)
                );
            }
        }
        e.to_string()
    }

    fn render_condition(&self, c: &QbCondition, dialect: &dyn Dialect) -> Option<String> {
        if c.expr.trim().is_empty() {
            return None;
        }
        let lhs = self.render_expr(&c.expr, dialect);
        let op = c.operator.trim().to_ascii_uppercase();
        let value = self.render_expr(&c.value, dialect);
        Some(match op.as_str() {
            "" => lhs,
            "IS NULL" | "IS NOT NULL" => format!("{lhs} {op}"),
            "IN" | "NOT IN" => {
                if value.starts_with('(') {
                    format!("{lhs} {op} {value}")
                } else {
                    format!("{lhs} {op} ({value})")
                }
            }
            _ if value.is_empty() => return None,
            _ => format!("{lhs} {op} {value}"),
        })
    }

    fn render_conditions(&self, list: &[QbCondition], dialect: &dyn Dialect) -> Option<String> {
        let mut out = String::new();
        for c in list {
            let Some(text) = self.render_condition(c, dialect) else {
                continue;
            };
            if out.is_empty() {
                out.push_str(&text);
            } else {
                out.push_str(&format!("\n    {} {text}", c.connector.sql()));
            }
        }
        if out.is_empty() { None } else { Some(out) }
    }

    fn table_ref(&self, idx: usize, dialect: &dyn Dialect) -> String {
        let Some(t) = self.tables.get(idx) else {
            return String::new();
        };
        let name = dialect.qualified(t.obj.schema.as_deref(), &t.obj.name);
        format!("{name} {}", dialect.quote_ident(&t.alias))
    }

    fn join_condition(&self, j: &QbJoin, dialect: &dyn Dialect) -> String {
        format!(
            "{}.{} = {}.{}",
            dialect.quote_ident(&j.left_alias),
            dialect.quote_ident(&j.left_column),
            dialect.quote_ident(&j.right_alias),
            dialect.quote_ident(&j.right_column)
        )
    }

    /// FROM clause with joins: tables are attached in model order, each one
    /// through all join conditions to tables already attached; tables
    /// without a join are cross joined.
    fn render_from(&self, dialect: &dyn Dialect) -> String {
        let n = self.tables.len();
        let mut included: Vec<usize> = vec![0];
        let mut aliases: BTreeSet<&str> = self
            .tables
            .first()
            .map(|t| t.alias.as_str())
            .into_iter()
            .collect();
        let mut used = vec![false; self.joins.len()];
        let mut out = format!("FROM {}", self.table_ref(0, dialect));
        while included.len() < n {
            let connected = (0..n).filter(|i| !included.contains(i)).find(|&i| {
                let a = self.tables[i].alias.as_str();
                self.joins.iter().any(|j| {
                    (j.left_alias == a && aliases.contains(j.right_alias.as_str()))
                        || (j.right_alias == a && aliases.contains(j.left_alias.as_str()))
                })
            });
            match connected {
                Some(i) => {
                    let a = self.tables[i].alias.clone();
                    let mut kind: Option<JoinKind> = None;
                    let mut conds = Vec::new();
                    for (ji, j) in self.joins.iter().enumerate() {
                        if used[ji] {
                            continue;
                        }
                        let new_is_right =
                            j.right_alias == a && aliases.contains(j.left_alias.as_str());
                        let new_is_left =
                            j.left_alias == a && aliases.contains(j.right_alias.as_str());
                        if new_is_right || new_is_left {
                            used[ji] = true;
                            conds.push(self.join_condition(j, dialect));
                            if kind.is_none() {
                                kind = Some(if new_is_right {
                                    j.kind
                                } else {
                                    j.kind.flipped()
                                });
                            }
                        }
                    }
                    out.push_str(&format!(
                        "\n    {} {} ON {}",
                        kind.unwrap_or_default().sql(),
                        self.table_ref(i, dialect),
                        conds.join(" AND ")
                    ));
                    included.push(i);
                    if let Some(t) = self.tables.get(i) {
                        aliases.insert(t.alias.as_str());
                    }
                }
                None => {
                    let Some(i) = (0..n).find(|i| !included.contains(i)) else {
                        break;
                    };
                    out.push_str(&format!("\n    CROSS JOIN {}", self.table_ref(i, dialect)));
                    included.push(i);
                    if let Some(t) = self.tables.get(i) {
                        aliases.insert(t.alias.as_str());
                    }
                }
            }
        }
        out
    }

    /// GROUP BY expressions actually used: the explicit list plus every
    /// non aggregated output item when the select list has aggregates.
    pub fn effective_group_by(&self, dialect: &dyn Dialect) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |e: String| {
            if !e.is_empty() && !out.contains(&e) {
                out.push(e);
            }
        };
        for g in &self.group_by {
            push(self.render_expr(g, dialect));
        }
        let has_aggregates = self
            .select
            .iter()
            .any(|s| s.visible && s.aggregate.is_some());
        if has_aggregates {
            for s in self
                .select
                .iter()
                .filter(|s| s.visible && s.aggregate.is_none())
            {
                push(self.render_expr(&s.expr, dialect));
            }
        }
        out
    }

    /// Generates the SELECT statement for `dialect` (empty when the model
    /// has no tables).
    pub fn to_sql(&self, dialect: &dyn Dialect) -> String {
        if self.tables.is_empty() {
            return String::new();
        }
        let items: Vec<String> = self
            .select
            .iter()
            .filter(|s| s.visible && (!s.expr.trim().is_empty() || s.aggregate.is_some()))
            .map(|s| {
                let e = self.render_expr(&s.expr, dialect);
                let e = match s.aggregate {
                    Some(a) => a.apply(&e),
                    None => e,
                };
                if s.alias.trim().is_empty() {
                    e
                } else {
                    format!("{e} AS {}", dialect.quote_ident(s.alias.trim()))
                }
            })
            .collect();
        let kind = dialect.kind();
        let mut sql = String::from("SELECT");
        if self.distinct {
            sql.push_str(" DISTINCT");
        }
        if let (DbKind::SqlServer, Some(n)) = (kind, self.limit) {
            sql.push_str(&format!(" TOP {n}"));
        }
        match items.as_slice() {
            [] => sql.push_str(" *"),
            [one] => {
                sql.push(' ');
                sql.push_str(one);
            }
            many => {
                sql.push_str("\n    ");
                sql.push_str(&many.join(",\n    "));
            }
        }
        sql.push('\n');
        sql.push_str(&self.render_from(dialect));
        if let Some(w) = self.render_conditions(&self.conditions, dialect) {
            sql.push_str("\nWHERE ");
            sql.push_str(&w);
        }
        let group = self.effective_group_by(dialect);
        if !group.is_empty() {
            sql.push_str("\nGROUP BY ");
            sql.push_str(&group.join(", "));
        }
        if let Some(h) = self.render_conditions(&self.having, dialect) {
            sql.push_str("\nHAVING ");
            sql.push_str(&h);
        }
        let order: Vec<String> = self
            .order_by
            .iter()
            .filter(|(e, _)| !e.trim().is_empty())
            .map(|(e, desc)| {
                format!(
                    "{}{}",
                    self.render_expr(e, dialect),
                    if *desc { " DESC" } else { "" }
                )
            })
            .collect();
        if !order.is_empty() {
            sql.push_str("\nORDER BY ");
            sql.push_str(&order.join(", "));
        }
        if let Some(n) = self.limit {
            match kind {
                DbKind::SqlServer => {}
                DbKind::Generic | DbKind::Oracle => return dialect.limit_query(&sql, n, 0),
                DbKind::Postgres
                | DbKind::CockroachDb
                | DbKind::MySql
                | DbKind::MariaDb
                | DbKind::Sqlite
                | DbKind::ClickHouse
                | DbKind::Cassandra => sql.push_str(&format!("\nLIMIT {n}")),
                // The visual builder is not offered for these.
                DbKind::Redis | DbKind::MongoDb | DbKind::Elasticsearch | DbKind::Neo4j => {}
            }
        }
        sql
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::tests::{customers, order_items, orders};
    use super::super::model::*;
    use dbiji_core::ddl::DdlGenerator;
    use dbiji_core::dialect::{GenericDialect, SqlSyntax};
    use dbiji_core::{DbKind, Dialect};

    /// Dialect wrapper reporting another product.
    struct KindDialect(DbKind, GenericDialect, SqlSyntax);
    impl Dialect for KindDialect {
        fn kind(&self) -> DbKind {
            self.0
        }
        fn syntax(&self) -> &SqlSyntax {
            &self.2
        }
        fn ddl(&self) -> &dyn DdlGenerator {
            self.1.ddl()
        }
    }
    fn dialect(kind: DbKind) -> KindDialect {
        let mut syntax = SqlSyntax::ansi();
        if kind == DbKind::MySql {
            syntax.identifier_quotes = &[('`', '`')];
        }
        KindDialect(kind, GenericDialect::new(), syntax)
    }

    fn model() -> QueryModel {
        let mut m = QueryModel::default();
        m.add_table(&orders());
        m.add_table(&customers());
        m
    }

    #[test]
    fn empty_and_star() {
        let d = dialect(DbKind::Postgres);
        assert_eq!(QueryModel::default().to_sql(&d), "");
        let m = model();
        assert_eq!(
            m.to_sql(&d),
            "SELECT *\nFROM shop.orders o\n    INNER JOIN shop.customers c ON o.customer_id = c.id"
        );
    }

    #[test]
    fn full_query_with_quoting() {
        let d = dialect(DbKind::Postgres);
        let mut m = model();
        m.set_column_selected("o", "order", true); // reserved word -> quoted
        m.set_column_selected("c", "name", true);
        m.select[1].alias = "customer name".into();
        m.conditions.push(QbCondition::new("o.total", ">", "100"));
        m.conditions.push(QbCondition {
            connector: Connector::Or,
            ..QbCondition::new("c.region", "IN", "'EU', 'US'")
        });
        m.conditions
            .push(QbCondition::new("c.name", "IS NOT NULL", "ignored"));
        m.conditions.push(QbCondition::new("o.total", "=", "")); // incomplete: skipped
        m.order_by.push(("o.total".into(), true));
        m.order_by.push(("c.name".into(), false));
        m.distinct = true;
        m.limit = Some(50);
        let sql = m.to_sql(&d);
        assert_eq!(
            sql,
            "SELECT DISTINCT\n    o.\"order\",\n    c.name AS \"customer name\"\nFROM shop.orders o\n    INNER JOIN shop.customers c ON o.customer_id = c.id\nWHERE o.total > 100\n    OR c.region IN ('EU', 'US')\n    AND c.name IS NOT NULL\nORDER BY o.total DESC, c.name\nLIMIT 50"
        );
    }

    #[test]
    fn group_by_is_derived_from_aggregates() {
        let d = dialect(DbKind::Postgres);
        let mut m = model();
        m.set_column_selected("c", "region", true);
        m.select.push(QbSelectItem {
            aggregate: Some(Aggregate::Sum),
            alias: "revenue".into(),
            ..QbSelectItem::new("o.total")
        });
        m.select.push(QbSelectItem {
            aggregate: Some(Aggregate::Count),
            ..QbSelectItem::new("")
        });
        m.select.push(QbSelectItem {
            aggregate: Some(Aggregate::CountDistinct),
            ..QbSelectItem::new("o.customer_id")
        });
        m.having.push(QbCondition::new("SUM(o.total)", ">", "1000"));
        let sql = m.to_sql(&d);
        assert!(sql.contains("SUM(o.total) AS revenue"), "{sql}");
        assert!(sql.contains("COUNT(*)"), "{sql}");
        assert!(sql.contains("COUNT(DISTINCT o.customer_id)"), "{sql}");
        assert!(
            sql.contains("\nGROUP BY c.region\nHAVING SUM(o.total) > 1000"),
            "{sql}"
        );
        // explicit group by is kept and merged without duplicates
        m.group_by.push("c.region".into());
        m.group_by.push("c.name".into());
        assert_eq!(
            m.effective_group_by(&d),
            vec!["c.region".to_string(), "c.name".to_string()]
        );
        // hidden items are not output and not grouped
        m.select[0].visible = false;
        assert!(!m.to_sql(&d).contains("    c.region,"));
    }

    #[test]
    fn join_kinds_orientation_and_cross_join() {
        let d = dialect(DbKind::Postgres);
        let mut m = QueryModel::default();
        m.add_table(&customers());
        m.add_table(&orders());
        m.joins[0].kind = JoinKind::Left; // o LEFT JOIN c
        let sql = m.to_sql(&d);
        // customers come first, so the join is written from the other side
        assert!(
            sql.contains("RIGHT JOIN shop.orders o ON o.customer_id = c.id"),
            "{sql}"
        );
        m.add_table(&order_items());
        let sql = m.to_sql(&d);
        assert!(
            sql.contains("INNER JOIN shop.order_items oi ON oi.order_id = o.id"),
            "{sql}"
        );
        let mut unrelated = dbiji_core::ddl::TableDef::new(None, "regions");
        unrelated
            .columns
            .push(dbiji_core::ddl::ColumnDef::new("id", "int"));
        m.add_table(&unrelated);
        assert!(m.to_sql(&d).contains("CROSS JOIN regions r"));
        // multi column join conditions are combined
        m.add_join(QbJoin {
            left_alias: "oi".into(),
            left_column: "qty".into(),
            right_alias: "o".into(),
            right_column: "total".into(),
            kind: JoinKind::Inner,
        });
        assert!(
            m.to_sql(&d)
                .contains("ON oi.order_id = o.id AND oi.qty = o.total")
        );
    }

    #[test]
    fn limit_per_dialect() {
        let mut m = model();
        m.limit = Some(10);
        assert!(m.to_sql(&dialect(DbKind::MySql)).ends_with("\nLIMIT 10"));
        assert!(
            m.to_sql(&dialect(DbKind::MySql))
                .contains("FROM shop.orders o")
        );
        let ss = m.to_sql(&dialect(DbKind::SqlServer));
        assert!(ss.starts_with("SELECT TOP 10 *"), "{ss}");
        assert!(!ss.contains("LIMIT"));
        let generic = m.to_sql(&dialect(DbKind::Generic));
        assert!(generic.starts_with("SELECT * FROM (SELECT *"), "{generic}");
        // identifiers needing quotes use the dialect's quote characters
        m.set_column_selected("o", "order", true);
        assert!(m.to_sql(&dialect(DbKind::MySql)).contains("o.`order`"));
    }
}
