//! Query builder model: tables with aliases, joins, select list,
//! conditions, grouping, ordering and limit.

use crate::common::serde_helpers;
use crate::common::text::text_width;
use dbiji_core::ddl::{ForeignKeyDef, TableDef};
use dbiji_core::meta::ObjectRef;
use egui::{Pos2, Rect, Vec2, pos2, vec2};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Join type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum JoinKind {
    /// `INNER JOIN`
    #[default]
    Inner,
    /// `LEFT JOIN` (all rows of the left table)
    Left,
    /// `RIGHT JOIN` (all rows of the right table)
    Right,
    /// `FULL JOIN`
    Full,
}

impl JoinKind {
    /// All kinds in menu order.
    pub const ALL: [JoinKind; 4] = [
        JoinKind::Inner,
        JoinKind::Left,
        JoinKind::Right,
        JoinKind::Full,
    ];

    /// SQL keywords.
    pub fn sql(self) -> &'static str {
        match self {
            JoinKind::Inner => "INNER JOIN",
            JoinKind::Left => "LEFT JOIN",
            JoinKind::Right => "RIGHT JOIN",
            JoinKind::Full => "FULL JOIN",
        }
    }

    /// English label (route through `tr`).
    pub fn label(self) -> &'static str {
        match self {
            JoinKind::Inner => "Inner join",
            JoinKind::Left => "Left outer join",
            JoinKind::Right => "Right outer join",
            JoinKind::Full => "Full outer join",
        }
    }

    /// Same join seen from the other side.
    pub fn flipped(self) -> JoinKind {
        match self {
            JoinKind::Left => JoinKind::Right,
            JoinKind::Right => JoinKind::Left,
            k => k,
        }
    }
}

/// Aggregate function applied to a select item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Aggregate {
    /// `COUNT(x)` (`COUNT(*)` for an empty expression)
    Count,
    /// `COUNT(DISTINCT x)`
    CountDistinct,
    /// `SUM(x)`
    Sum,
    /// `AVG(x)`
    Avg,
    /// `MIN(x)`
    Min,
    /// `MAX(x)`
    Max,
}

impl Aggregate {
    /// All aggregates in menu order.
    pub const ALL: [Aggregate; 6] = [
        Aggregate::Count,
        Aggregate::CountDistinct,
        Aggregate::Sum,
        Aggregate::Avg,
        Aggregate::Min,
        Aggregate::Max,
    ];

    /// Display label (SQL function name, not translated).
    pub fn label(self) -> &'static str {
        match self {
            Aggregate::Count => "COUNT",
            Aggregate::CountDistinct => "COUNT DISTINCT",
            Aggregate::Sum => "SUM",
            Aggregate::Avg => "AVG",
            Aggregate::Min => "MIN",
            Aggregate::Max => "MAX",
        }
    }

    /// Wraps an already rendered expression.
    pub fn apply(self, expr: &str) -> String {
        let e = if expr.trim().is_empty() {
            "*"
        } else {
            expr.trim()
        };
        match self {
            Aggregate::Count => format!("COUNT({e})"),
            Aggregate::CountDistinct => format!("COUNT(DISTINCT {e})"),
            Aggregate::Sum => format!("SUM({e})"),
            Aggregate::Avg => format!("AVG({e})"),
            Aggregate::Min => format!("MIN({e})"),
            Aggregate::Max => format!("MAX({e})"),
        }
    }
}

/// How a condition is combined with the previous one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Connector {
    /// `AND`
    #[default]
    And,
    /// `OR`
    Or,
}

impl Connector {
    /// SQL keyword.
    pub fn sql(self) -> &'static str {
        match self {
            Connector::And => "AND",
            Connector::Or => "OR",
        }
    }
}

/// Comparison operators offered by the condition editor.
pub const OPERATORS: [&str; 14] = [
    "=",
    "<>",
    "<",
    "<=",
    ">",
    ">=",
    "LIKE",
    "NOT LIKE",
    "IN",
    "NOT IN",
    "BETWEEN",
    "IS NULL",
    "IS NOT NULL",
    "",
];

/// A column of a builder table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct QbColumn {
    /// Column name.
    pub name: String,
    /// Type text.
    pub data_type: String,
    /// Ticked in the table box (part of the select list).
    pub selected: bool,
}

/// A table placed on the builder canvas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QbTable {
    /// The table (or view).
    pub obj: ObjectRef,
    /// Alias used in the query (unique within the model).
    pub alias: String,
    /// Columns in table order.
    pub columns: Vec<QbColumn>,
    /// Top-left corner on the canvas.
    #[serde(with = "serde_helpers::pos2")]
    pub pos: Pos2,
    /// Foreign keys of the table (used for automatic joins).
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKeyDef>,
}

/// One join condition (`left.column = right.column`); several conditions
/// between the same pair of tables are combined with AND.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct QbJoin {
    /// Alias of the left table.
    pub left_alias: String,
    /// Column of the left table.
    pub left_column: String,
    /// Alias of the right table.
    pub right_alias: String,
    /// Column of the right table.
    pub right_column: String,
    /// Join type (`Left` keeps all rows of the left table).
    pub kind: JoinKind,
}

/// An item of the select list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QbSelectItem {
    /// Column reference `alias.column` (quoted automatically) or any SQL expression.
    pub expr: String,
    /// Output name (`AS ...`), empty for none.
    pub alias: String,
    /// Aggregate applied to the expression.
    pub aggregate: Option<Aggregate>,
    /// Part of the output (hidden items can still be used for ordering).
    pub visible: bool,
}

impl Default for QbSelectItem {
    fn default() -> Self {
        QbSelectItem {
            expr: String::new(),
            alias: String::new(),
            aggregate: None,
            visible: true,
        }
    }
}

impl QbSelectItem {
    /// Visible item for an expression.
    pub fn new(expr: impl Into<String>) -> Self {
        QbSelectItem {
            expr: expr.into(),
            ..Default::default()
        }
    }
}

/// A WHERE / HAVING condition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct QbCondition {
    /// Left side: column reference or expression.
    pub expr: String,
    /// Operator from [`OPERATORS`] (empty: `expr` is a complete condition).
    pub operator: String,
    /// Right side as SQL text (literal, list, column reference ...).
    pub value: String,
    /// Combination with the previous condition (ignored for the first).
    pub connector: Connector,
}

impl QbCondition {
    /// Condition `expr operator value`.
    pub fn new(
        expr: impl Into<String>,
        operator: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        QbCondition {
            expr: expr.into(),
            operator: operator.into(),
            value: value.into(),
            connector: Connector::And,
        }
    }
}

/// The whole query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct QueryModel {
    /// Tables in FROM order.
    pub tables: Vec<QbTable>,
    /// Join conditions.
    pub joins: Vec<QbJoin>,
    /// Select list (empty selects `*`).
    pub select: Vec<QbSelectItem>,
    /// WHERE conditions.
    pub conditions: Vec<QbCondition>,
    /// Explicit GROUP BY expressions; non aggregated select items are added
    /// automatically when the select list contains aggregates.
    pub group_by: Vec<String>,
    /// HAVING conditions.
    pub having: Vec<QbCondition>,
    /// ORDER BY items: (expression, descending).
    pub order_by: Vec<(String, bool)>,
    /// `SELECT DISTINCT`.
    pub distinct: bool,
    /// Maximum number of rows.
    pub limit: Option<u64>,
}

// geometry of the boxes on the canvas
pub(crate) const QB_HEADER: f32 = 26.0;
pub(crate) const QB_ROW: f32 = 20.0;
pub(crate) const QB_FONT: f32 = 12.5;
const QB_MIN_W: f32 = 170.0;
const QB_MAX_W: f32 = 340.0;

impl QbTable {
    /// Size of the table box on the canvas.
    pub fn box_size(&self) -> Vec2 {
        let mut w = 40.0
            + text_width(&self.obj.name, QB_FONT + 0.5, true) * 1.08
            + text_width(&self.alias, QB_FONT, false)
            + 40.0;
        for c in &self.columns {
            w = w.max(
                34.0 + text_width(&c.name, QB_FONT, false) * 1.08
                    + 16.0
                    + text_width(&c.data_type, QB_FONT - 1.0, false) * 1.08
                    + 10.0,
            );
        }
        vec2(
            w.clamp(QB_MIN_W, QB_MAX_W).round(),
            QB_HEADER + self.columns.len() as f32 * QB_ROW + 6.0,
        )
    }

    /// Rectangle of the box on the canvas.
    pub fn rect(&self) -> Rect {
        Rect::from_min_size(self.pos, self.box_size())
    }

    /// Y (canvas) of the middle of a column row.
    pub fn column_y(&self, column: &str) -> Option<f32> {
        let i = self.columns.iter().position(|c| c.name == column)?;
        Some(self.pos.y + QB_HEADER + (i as f32 + 0.5) * QB_ROW)
    }

    fn matches(&self, schema: Option<&str>, name: &str) -> bool {
        self.obj.name == name
            && (schema.is_none()
                || self.obj.schema.is_none()
                || self.obj.schema.as_deref() == schema)
    }
}

fn base_alias(name: &str) -> String {
    let parts: Vec<&str> = name
        .split(['_', ' ', '.'])
        .filter(|p| !p.is_empty())
        .collect();
    let mut alias: String = if parts.len() > 1 {
        parts.iter().filter_map(|p| p.chars().next()).collect()
    } else {
        name.chars()
            .next()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "t".into())
    };
    alias = alias.to_lowercase();
    if !alias
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
    {
        alias.insert(0, 't');
    }
    alias.retain(|c| c.is_ascii_alphanumeric() || c == '_');
    if alias.is_empty() { "t".into() } else { alias }
}

impl QueryModel {
    /// Table by alias.
    pub fn table(&self, alias: &str) -> Option<&QbTable> {
        self.tables.iter().find(|t| t.alias == alias)
    }

    /// Mutable table by alias.
    pub fn table_mut(&mut self, alias: &str) -> Option<&mut QbTable> {
        self.tables.iter_mut().find(|t| t.alias == alias)
    }

    /// An alias not used yet, derived from the table name (`order_items` -> `oi`).
    pub fn unique_alias(&self, name: &str) -> String {
        let base = base_alias(name);
        let used: BTreeSet<&str> = self.tables.iter().map(|t| t.alias.as_str()).collect();
        if !used.contains(base.as_str()) && !dbiji_core::keywords::is_reserved(&base) {
            return base;
        }
        (2..)
            .map(|i| format!("{base}{i}"))
            .find(|a| !used.contains(a.as_str()))
            .unwrap_or(base)
    }

    /// Adds a table: assigns an alias, places it right of the existing boxes
    /// and creates joins from its foreign keys to tables already present
    /// (and from their foreign keys to it). Returns the alias.
    pub fn add_table(&mut self, def: &TableDef) -> String {
        let alias = self.unique_alias(&def.name);
        let x = self
            .tables
            .iter()
            .map(|t| t.rect().right())
            .fold(20.0f32, |a, b| a.max(b + 60.0));
        let mut obj = ObjectRef::table(def.schema.clone(), def.name.clone());
        obj.kind = dbiji_core::meta::ObjectKind::Table;
        let table = QbTable {
            obj,
            alias: alias.clone(),
            columns: def
                .columns
                .iter()
                .map(|c| QbColumn {
                    name: c.name.clone(),
                    data_type: c.data_type.clone(),
                    selected: false,
                })
                .collect(),
            pos: pos2(x, 20.0 + (self.tables.len() % 3) as f32 * 30.0),
            foreign_keys: def.foreign_keys.clone(),
        };
        let mut joins = Vec::new();
        // new table -> existing parents
        for fk in &def.foreign_keys {
            let ref_schema = fk.ref_schema.as_deref().or(def.schema.as_deref());
            if fk.ref_table == def.name && ref_schema == def.schema.as_deref() {
                continue; // self reference: needs an explicit second instance
            }
            if let Some(parent) = self
                .tables
                .iter()
                .find(|t| t.matches(ref_schema, &fk.ref_table))
            {
                for (c, rc) in fk.columns.iter().zip(&fk.ref_columns) {
                    joins.push(QbJoin {
                        left_alias: alias.clone(),
                        left_column: c.clone(),
                        right_alias: parent.alias.clone(),
                        right_column: rc.clone(),
                        kind: JoinKind::Inner,
                    });
                }
            }
        }
        // existing children -> new table
        let mut incoming: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
        for t in &self.tables {
            for fk in &t.foreign_keys {
                let ref_schema = fk.ref_schema.as_deref().or(t.obj.schema.as_deref());
                if fk.ref_table == def.name
                    && (ref_schema.is_none()
                        || def.schema.is_none()
                        || ref_schema == def.schema.as_deref())
                {
                    incoming.push((t.alias.clone(), fk.columns.clone(), fk.ref_columns.clone()));
                }
            }
        }
        for fk in &def.referenced_by {
            let schema = fk.schema.as_deref().or(def.schema.as_deref());
            if let Some(child) = self.tables.iter().find(|t| t.matches(schema, &fk.table)) {
                incoming.push((
                    child.alias.clone(),
                    fk.columns.clone(),
                    fk.ref_columns.clone(),
                ));
            }
        }
        for (child_alias, cols, ref_cols) in incoming {
            for (c, rc) in cols.iter().zip(&ref_cols) {
                joins.push(QbJoin {
                    left_alias: child_alias.clone(),
                    left_column: c.clone(),
                    right_alias: alias.clone(),
                    right_column: rc.clone(),
                    kind: JoinKind::Inner,
                });
            }
        }
        self.tables.push(table);
        for j in joins {
            if !self.joins.iter().any(|x| same_join(x, &j)) {
                self.joins.push(j);
            }
        }
        alias
    }

    /// Removes a table and everything referring to its alias.
    pub fn remove_table(&mut self, alias: &str) {
        self.tables.retain(|t| t.alias != alias);
        self.joins
            .retain(|j| j.left_alias != alias && j.right_alias != alias);
        let prefix = format!("{alias}.");
        let refers = |e: &str| e.trim_start().starts_with(&prefix);
        self.select.retain(|s| !refers(&s.expr));
        self.conditions.retain(|c| !refers(&c.expr));
        self.having.retain(|c| !refers(&c.expr));
        self.group_by.retain(|g| !refers(g));
        self.order_by.retain(|(e, _)| !refers(e));
    }

    /// Renames an alias everywhere. Fails (returns false) when the new
    /// alias is empty or already used.
    pub fn rename_alias(&mut self, old: &str, new: &str) -> bool {
        let new = new.trim();
        if new.is_empty() || new == old || self.table(new).is_some() || self.table(old).is_none() {
            return new == old;
        }
        if let Some(t) = self.table_mut(old) {
            t.alias = new.to_string();
        }
        for j in &mut self.joins {
            if j.left_alias == old {
                j.left_alias = new.to_string();
            }
            if j.right_alias == old {
                j.right_alias = new.to_string();
            }
        }
        let (from, to) = (format!("{old}."), format!("{new}."));
        let fix = |e: &mut String| {
            if let Some(rest) = e.strip_prefix(&from) {
                *e = format!("{to}{rest}");
            }
        };
        self.select.iter_mut().for_each(|s| fix(&mut s.expr));
        self.conditions.iter_mut().for_each(|c| fix(&mut c.expr));
        self.having.iter_mut().for_each(|c| fix(&mut c.expr));
        self.group_by.iter_mut().for_each(fix);
        self.order_by.iter_mut().for_each(|(e, _)| fix(e));
        true
    }

    /// Ticks / unticks a column: adds or removes the `alias.column` select item.
    pub fn set_column_selected(&mut self, alias: &str, column: &str, selected: bool) {
        let expr = format!("{alias}.{column}");
        if let Some(c) = self
            .table_mut(alias)
            .and_then(|t| t.columns.iter_mut().find(|c| c.name == column))
        {
            c.selected = selected;
        }
        if selected {
            if !self.select.iter().any(|s| s.expr == expr) {
                self.select.push(QbSelectItem::new(expr));
            }
        } else {
            self.select.retain(|s| s.expr != expr);
        }
    }

    /// Recomputes the column check marks from the select list (after the
    /// select list was edited directly).
    pub fn sync_selected_flags(&mut self) {
        let exprs: BTreeSet<String> = self
            .select
            .iter()
            .map(|s| s.expr.trim().to_string())
            .collect();
        for t in &mut self.tables {
            let alias = t.alias.clone();
            for c in &mut t.columns {
                c.selected = exprs.contains(&format!("{alias}.{}", c.name));
            }
        }
    }

    /// Adds a join unless the same condition exists; returns its index.
    pub fn add_join(&mut self, join: QbJoin) -> Option<usize> {
        if join.left_alias == join.right_alias {
            return None;
        }
        if let Some(i) = self.joins.iter().position(|j| same_join(j, &join)) {
            return Some(i);
        }
        self.joins.push(join);
        Some(self.joins.len() - 1)
    }

    /// Every `alias.column` reference of the model (for pickers).
    pub fn column_refs(&self) -> Vec<String> {
        self.tables
            .iter()
            .flat_map(|t| {
                t.columns
                    .iter()
                    .map(move |c| format!("{}.{}", t.alias, c.name))
            })
            .collect()
    }
}

fn same_join(a: &QbJoin, b: &QbJoin) -> bool {
    let direct = a.left_alias == b.left_alias
        && a.left_column == b.left_column
        && a.right_alias == b.right_alias
        && a.right_column == b.right_column;
    let reversed = a.left_alias == b.right_alias
        && a.left_column == b.right_column
        && a.right_alias == b.left_alias
        && a.right_column == b.left_column;
    direct || reversed
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use dbiji_core::ddl::{ColumnDef, KeyDef};

    pub(crate) fn customers() -> TableDef {
        let mut t = TableDef::new(Some("shop".into()), "customers");
        t.columns = vec![
            ColumnDef::new("id", "int"),
            ColumnDef::new("name", "text"),
            ColumnDef::new("region", "text"),
        ];
        t.primary_key = Some(KeyDef {
            name: None,
            columns: vec!["id".into()],
        });
        t
    }

    pub(crate) fn orders() -> TableDef {
        let mut t = TableDef::new(Some("shop".into()), "orders");
        t.columns = vec![
            ColumnDef::new("id", "int"),
            ColumnDef::new("customer_id", "int"),
            ColumnDef::new("total", "numeric"),
            ColumnDef::new("order", "text"),
        ];
        t.foreign_keys.push(ForeignKeyDef {
            name: Some("fk".into()),
            columns: vec!["customer_id".into()],
            ref_table: "customers".into(),
            ref_columns: vec!["id".into()],
            ..Default::default()
        });
        t
    }

    pub(crate) fn order_items() -> TableDef {
        let mut t = TableDef::new(Some("shop".into()), "order_items");
        t.columns = vec![
            ColumnDef::new("order_id", "int"),
            ColumnDef::new("qty", "int"),
        ];
        t.foreign_keys.push(ForeignKeyDef {
            name: None,
            columns: vec!["order_id".into()],
            ref_schema: Some("shop".into()),
            ref_table: "orders".into(),
            ref_columns: vec!["id".into()],
            ..Default::default()
        });
        t
    }

    #[test]
    fn aliases_and_auto_joins() {
        let mut m = QueryModel::default();
        assert_eq!(m.add_table(&customers()), "c");
        // parent added first, child second: join from the child's FK
        assert_eq!(m.add_table(&orders()), "o");
        assert_eq!(m.joins.len(), 1);
        assert_eq!(m.joins[0].left_alias, "o");
        assert_eq!(m.joins[0].right_alias, "c");
        assert_eq!(m.add_table(&order_items()), "oi");
        assert_eq!(m.joins.len(), 2);
        // second instance of a table gets a numbered alias
        assert_eq!(m.add_table(&customers()), "c2");
        assert!(m.tables[1].pos.x > m.tables[0].pos.x);

        // child added first, parent second: join from the stored FK
        let mut m = QueryModel::default();
        m.add_table(&orders());
        m.add_table(&customers());
        assert_eq!(m.joins.len(), 1);
        assert_eq!(
            (
                m.joins[0].left_alias.as_str(),
                m.joins[0].right_alias.as_str()
            ),
            ("o", "c")
        );
    }

    #[test]
    fn select_flags_remove_and_rename() {
        let mut m = QueryModel::default();
        m.add_table(&customers());
        m.add_table(&orders());
        m.set_column_selected("c", "name", true);
        m.set_column_selected("o", "total", true);
        m.set_column_selected("o", "total", true);
        assert_eq!(m.select.len(), 2);
        m.conditions.push(QbCondition::new("o.total", ">", "100"));
        m.order_by.push(("o.total".into(), true));
        assert!(m.rename_alias("o", "ord"));
        assert!(!m.rename_alias("ord", "c"), "alias in use");
        assert_eq!(m.select[1].expr, "ord.total");
        assert_eq!(m.joins[0].left_alias, "ord");
        m.select[0].expr = "c.region".into();
        m.sync_selected_flags();
        assert!(m.table("c").unwrap().columns[2].selected);
        assert!(!m.table("c").unwrap().columns[1].selected);
        m.remove_table("ord");
        assert!(m.joins.is_empty());
        assert_eq!(m.select.len(), 1);
        assert!(m.conditions.is_empty() && m.order_by.is_empty());
        assert_eq!(
            m.add_join(QbJoin {
                left_alias: "c".into(),
                right_alias: "c".into(),
                ..Default::default()
            }),
            None
        );
    }

    #[test]
    fn alias_generation() {
        let m = QueryModel::default();
        assert_eq!(m.unique_alias("order_items"), "oi");
        assert_eq!(m.unique_alias("Users"), "u");
        assert_eq!(m.unique_alias("1table"), "t1");
        // "as" would be a reserved alias
        assert_eq!(m.unique_alias("account_sessions"), "as2");
    }
}
