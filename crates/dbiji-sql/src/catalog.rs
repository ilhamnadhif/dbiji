//! Metadata access for autocomplete and diagnostics.
//!
//! The application implements [`CompletionCatalog`] over its metadata cache
//! (or uses [`SnapshotCatalog`] built from [`SchemaSnapshot`]s). All name
//! lookups are case-insensitive; `schema: None` means the default schema.

use crate::lexicon::ident_eq;
use dbiji_core::ddl::{ForeignKeyDef, SchemaSnapshot, TableKind};
use dbiji_core::meta::ColumnBrief;
use dbiji_core::{DbKind, ObjectKind, trf};
use std::collections::HashMap;

/// A table-like object offered by autocomplete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogTable {
    /// Schema of the object (`None` for unqualified catalogs).
    pub schema: Option<String>,
    /// Object name.
    pub name: String,
    /// Table, View, MaterializedView, ForeignTable ...
    pub kind: ObjectKind,
    /// Object comment, shown as completion detail.
    pub comment: Option<String>,
}

/// Read access to database metadata.
pub trait CompletionCatalog {
    /// Current schema of the editor (MySQL: current database).
    fn default_schema(&self) -> Option<String>;
    /// Names of all schemas (loaded or not).
    fn schemas(&self) -> Vec<String>;
    /// Tables and views of `schema` (`None` = default schema).
    fn tables(&self, schema: Option<&str>) -> Vec<CatalogTable>;
    /// Columns of a table; `None` when they are not loaded (yet).
    fn columns(&self, schema: Option<&str>, table: &str) -> Option<Vec<ColumnBrief>>;
    /// Foreign keys of a table, outgoing and incoming. `table`/`schema` of
    /// each key name the referencing table.
    fn foreign_keys(&self, schema: Option<&str>, table: &str) -> Vec<ForeignKeyDef>;
    /// Function names of `schema` (in addition to the dialect built-ins).
    fn functions(&self, schema: Option<&str>) -> Vec<String> {
        let _ = schema;
        Vec::new()
    }
    /// The table list of `schema` is loaded, so a missing name is really
    /// unknown (used by diagnostics). Default: it has at least one table.
    fn is_schema_loaded(&self, schema: Option<&str>) -> bool {
        !self.tables(schema).is_empty()
    }
    /// Find one table or view by name (case-insensitive, exact case preferred).
    fn find_table(&self, schema: Option<&str>, name: &str) -> Option<CatalogTable> {
        let tables = self.tables(schema);
        let exact = tables.iter().position(|t| t.name == name);
        let idx = exact.or_else(|| tables.iter().position(|t| ident_eq(&t.name, name)))?;
        tables.into_iter().nth(idx)
    }
}

#[derive(Debug, Clone, Default)]
struct SchemaEntry {
    name: Option<String>,
    tables: Vec<CatalogTable>,
    columns: Vec<Vec<ColumnBrief>>,
    index: HashMap<String, usize>,
    functions: Vec<String>,
}

impl SchemaEntry {
    fn find(&self, name: &str) -> Option<usize> {
        if let Some(i) = self.tables.iter().position(|t| t.name == name) {
            return Some(i);
        }
        self.index.get(&name.to_lowercase()).copied()
    }
}

/// [`CompletionCatalog`] over schema snapshots, with incremental updates.
#[derive(Debug, Clone, Default)]
pub struct SnapshotCatalog {
    default_schema: Option<String>,
    extra_schemas: Vec<String>,
    entries: Vec<SchemaEntry>,
    fks: Vec<ForeignKeyDef>,
}

impl SnapshotCatalog {
    /// Catalog from snapshots (one per schema) and the default schema.
    pub fn new(snapshots: Vec<SchemaSnapshot>, default_schema: Option<String>) -> SnapshotCatalog {
        let mut c = SnapshotCatalog {
            default_schema,
            ..Default::default()
        };
        for s in snapshots {
            c.add_snapshot(s);
        }
        c
    }

    /// Also list these schema names (schemas whose objects are not loaded).
    pub fn with_extra_schemas<I, S>(mut self, names: I) -> SnapshotCatalog
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.set_extra_schemas(names);
        self
    }

    /// Replace the list of additional schema names.
    pub fn set_extra_schemas<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.extra_schemas = names.into_iter().map(Into::into).collect();
    }

    /// Change the default schema.
    pub fn set_default_schema(&mut self, schema: Option<String>) {
        self.default_schema = schema;
    }

    /// Add a snapshot, replacing a loaded snapshot of the same schema.
    pub fn add_snapshot(&mut self, snap: SchemaSnapshot) {
        let schema = snap.schema.clone();
        self.remove_entry(schema.as_deref());
        let mut e = SchemaEntry {
            name: schema.clone(),
            ..Default::default()
        };
        for t in &snap.tables {
            let kind = if t.table_kind == TableKind::ForeignTable {
                ObjectKind::ForeignTable
            } else {
                ObjectKind::Table
            };
            e.tables.push(CatalogTable {
                schema: schema.clone(),
                name: t.name.clone(),
                kind,
                comment: t.comment.clone(),
            });
            let columns: Vec<ColumnBrief> = t
                .columns
                .iter()
                .map(|c| ColumnBrief {
                    name: c.name.clone(),
                    data_type: c.data_type.clone(),
                    nullable: c.nullable,
                    is_primary_key: t.is_pk_column(&c.name),
                    comment: c.comment.clone(),
                })
                .collect();
            e.columns.push(columns.clone());
            // PostgreSQL partitions are tables of their own (read by name) with their
            // table's columns; a MySQL partition is no table.
            let partitions = if snap.kind == Some(DbKind::Postgres) {
                t.partitions.as_slice()
            } else {
                &[]
            };
            for p in partitions {
                e.tables.push(CatalogTable {
                    schema: schema.clone(),
                    name: p.name.clone(),
                    kind: ObjectKind::Partition,
                    comment: Some(trf("partition of {}", &[&t.name])),
                });
                e.columns.push(columns.clone());
            }
            let table_schema = t.schema.clone().or_else(|| schema.clone());
            for fk in &t.foreign_keys {
                let mut fk = fk.clone();
                if fk.table.is_empty() {
                    fk.table = t.name.clone();
                }
                if fk.schema.is_none() {
                    fk.schema = table_schema.clone();
                }
                if fk.ref_schema.is_none() {
                    fk.ref_schema = table_schema.clone();
                }
                self.push_fk(fk);
            }
            for fk in &t.referenced_by {
                let mut fk = fk.clone();
                if fk.ref_table.is_empty() {
                    fk.ref_table = t.name.clone();
                }
                if fk.ref_schema.is_none() {
                    fk.ref_schema = table_schema.clone();
                }
                if !fk.table.is_empty() {
                    self.push_fk(fk);
                }
            }
        }
        for v in &snap.views {
            let kind = if v.materialized {
                ObjectKind::MaterializedView
            } else {
                ObjectKind::View
            };
            e.tables.push(CatalogTable {
                schema: schema.clone(),
                name: v.name.clone(),
                kind,
                comment: v.comment.clone(),
            });
            e.columns.push(
                v.columns
                    .iter()
                    .map(|c| ColumnBrief {
                        name: c.name.clone(),
                        data_type: c.data_type.clone(),
                        nullable: c.nullable,
                        is_primary_key: false,
                        comment: c.comment.clone(),
                    })
                    .collect(),
            );
        }
        for (i, t) in e.tables.iter().enumerate() {
            e.index.entry(t.name.to_lowercase()).or_insert(i);
        }
        e.functions = snap.routines.iter().map(|r| r.name.clone()).collect();
        e.functions.sort();
        e.functions.dedup();
        self.entries.push(e);
    }

    /// Forget a loaded schema. Returns whether it was loaded.
    pub fn remove_schema(&mut self, schema: &str) -> bool {
        self.remove_entry(Some(schema))
    }

    /// Schemas with loaded snapshots (`None` for unnamed ones such as SQLite `main`).
    pub fn loaded_schemas(&self) -> Vec<Option<String>> {
        self.entries.iter().map(|e| e.name.clone()).collect()
    }

    fn remove_entry(&mut self, schema: Option<&str>) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| match (&e.name, schema) {
            (Some(a), Some(b)) => !ident_eq(a, b),
            (None, None) => false,
            _ => true,
        });
        if self.entries.len() != before {
            self.fks.retain(|fk| match (&fk.schema, schema) {
                (Some(a), Some(b)) => !ident_eq(a, b),
                (None, None) => false,
                _ => true,
            });
            true
        } else {
            false
        }
    }

    fn push_fk(&mut self, fk: ForeignKeyDef) {
        let same = |a: &ForeignKeyDef, b: &ForeignKeyDef| {
            ident_eq(&a.table, &b.table)
                && ident_eq(&a.ref_table, &b.ref_table)
                && a.columns == b.columns
                && a.ref_columns == b.ref_columns
                && opt_eq(a.schema.as_deref(), b.schema.as_deref())
        };
        if !self.fks.iter().any(|x| same(x, &fk)) {
            self.fks.push(fk);
        }
    }

    fn effective<'a>(&'a self, schema: Option<&'a str>) -> Option<&'a str> {
        schema.or(self.default_schema.as_deref())
    }

    fn entry(&self, schema: Option<&str>) -> Option<&SchemaEntry> {
        match self.effective(schema) {
            Some(s) => self
                .entries
                .iter()
                .find(|e| e.name.as_deref().is_some_and(|n| ident_eq(n, s)))
                .or_else(|| {
                    if schema.is_none() {
                        self.entries.iter().find(|e| e.name.is_none())
                    } else {
                        None
                    }
                }),
            None => self
                .entries
                .iter()
                .find(|e| e.name.is_none())
                .or(if self.entries.len() == 1 {
                    self.entries.first()
                } else {
                    None
                }),
        }
    }
}

fn opt_eq(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => ident_eq(a, b),
        _ => true,
    }
}

impl CompletionCatalog for SnapshotCatalog {
    fn default_schema(&self) -> Option<String> {
        self.default_schema.clone()
    }

    fn schemas(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for n in self
            .entries
            .iter()
            .filter_map(|e| e.name.clone())
            .chain(self.extra_schemas.iter().cloned())
        {
            if !out.iter().any(|x| ident_eq(x, &n)) {
                out.push(n);
            }
        }
        out
    }

    fn tables(&self, schema: Option<&str>) -> Vec<CatalogTable> {
        self.entry(schema)
            .map(|e| e.tables.clone())
            .unwrap_or_default()
    }

    fn columns(&self, schema: Option<&str>, table: &str) -> Option<Vec<ColumnBrief>> {
        let e = self.entry(schema)?;
        let i = e.find(table)?;
        e.columns.get(i).cloned()
    }

    fn foreign_keys(&self, schema: Option<&str>, table: &str) -> Vec<ForeignKeyDef> {
        let schema = self.effective(schema);
        let default = self.default_schema.as_deref();
        self.fks
            .iter()
            .filter(|fk| {
                let out =
                    ident_eq(&fk.table, table) && opt_eq(fk.schema.as_deref().or(default), schema);
                let inc = ident_eq(&fk.ref_table, table)
                    && opt_eq(fk.ref_schema.as_deref().or(default), schema);
                out || inc
            })
            .cloned()
            .collect()
    }

    fn functions(&self, schema: Option<&str>) -> Vec<String> {
        self.entry(schema)
            .map(|e| e.functions.clone())
            .unwrap_or_default()
    }

    fn is_schema_loaded(&self, schema: Option<&str>) -> bool {
        self.entry(schema).is_some()
    }

    fn find_table(&self, schema: Option<&str>, name: &str) -> Option<CatalogTable> {
        let e = self.entry(schema)?;
        e.find(name).and_then(|i| e.tables.get(i).cloned())
    }
}

#[cfg(test)]
pub(crate) mod test_catalog {
    //! The standard test schema used by autocomplete and diagnostics tests.
    use super::*;
    use dbiji_core::ddl::{ColumnDef, ForeignKeyDef, KeyDef, RoutineDef, TableDef, ViewDef};

    fn table(
        schema: &str,
        name: &str,
        cols: &[(&str, &str)],
        pk: &[&str],
        fks: &[(&[&str], &str, &str, &[&str])],
    ) -> TableDef {
        let mut t = TableDef::new(Some(schema.into()), name);
        for (c, ty) in cols {
            t.columns.push(ColumnDef::new(*c, *ty));
        }
        t.primary_key = Some(KeyDef {
            name: None,
            columns: pk.iter().map(|s| s.to_string()).collect(),
        });
        for (cols, ref_schema, ref_table, ref_cols) in fks {
            t.foreign_keys.push(ForeignKeyDef {
                columns: cols.iter().map(|s| s.to_string()).collect(),
                ref_schema: Some(ref_schema.to_string()),
                ref_table: ref_table.to_string(),
                ref_columns: ref_cols.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            });
        }
        t
    }

    /// Schemas `public` (default) and `sales`.
    pub(crate) fn snapshots() -> Vec<SchemaSnapshot> {
        let users = table(
            "public",
            "users",
            &[
                ("id", "integer"),
                ("name", "text"),
                ("email", "varchar(200)"),
                ("created_at", "timestamptz"),
            ],
            &["id"],
            &[],
        );
        let categories = table(
            "public",
            "categories",
            &[
                ("id", "integer"),
                ("name", "text"),
                ("parent_id", "integer"),
            ],
            &["id"],
            &[(&["parent_id"], "public", "categories", &["id"])],
        );
        let products = table(
            "public",
            "products",
            &[
                ("id", "integer"),
                ("name", "text"),
                ("price", "numeric(10,2)"),
                ("category_id", "integer"),
            ],
            &["id"],
            &[(&["category_id"], "public", "categories", &["id"])],
        );
        let orders = table(
            "public",
            "orders",
            &[
                ("id", "integer"),
                ("user_id", "integer"),
                ("total", "numeric(12,2)"),
                ("status", "text"),
                ("created_at", "timestamptz"),
            ],
            &["id"],
            &[(&["user_id"], "public", "users", &["id"])],
        );
        let items = table(
            "public",
            "order_items",
            &[
                ("order_id", "integer"),
                ("product_id", "integer"),
                ("quantity", "integer"),
                ("unit_price", "numeric(10,2)"),
            ],
            &["order_id", "product_id"],
            &[
                (&["order_id"], "public", "orders", &["id"]),
                (&["product_id"], "public", "products", &["id"]),
            ],
        );
        let mixed = table(
            "public",
            "MixedCase",
            &[("Id", "integer"), ("Some Value", "text")],
            &["Id"],
            &[],
        );
        let mut view = ViewDef {
            schema: Some("public".into()),
            name: "active_users".into(),
            ..Default::default()
        };
        view.columns = vec![
            ColumnDef::new("id", "integer"),
            ColumnDef::new("name", "text"),
        ];
        let public = SchemaSnapshot {
            schema: Some("public".into()),
            tables: vec![users, categories, products, orders, items, mixed],
            views: vec![view],
            routines: vec![RoutineDef {
                name: "calc_discount".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let customers = table(
            "sales",
            "customers",
            &[
                ("id", "integer"),
                ("user_id", "integer"),
                ("region", "text"),
            ],
            &["id"],
            &[(&["user_id"], "public", "users", &["id"])],
        );
        let invoices = table(
            "sales",
            "invoices",
            &[
                ("id", "integer"),
                ("customer_id", "integer"),
                ("amount", "numeric"),
            ],
            &["id"],
            &[(&["customer_id"], "sales", "customers", &["id"])],
        );
        let sales = SchemaSnapshot {
            schema: Some("sales".into()),
            tables: vec![customers, invoices],
            ..Default::default()
        };
        vec![public, sales]
    }

    pub(crate) fn catalog() -> SnapshotCatalog {
        SnapshotCatalog::new(snapshots(), Some("public".into())).with_extra_schemas(["archive"])
    }
}

#[cfg(test)]
mod tests {
    use super::test_catalog::catalog;
    use super::*;
    use dbiji_core::ddl::TableDef;

    #[test]
    fn lookups() {
        let c = catalog();
        assert_eq!(c.default_schema().as_deref(), Some("public"));
        assert_eq!(c.schemas(), vec!["public", "sales", "archive"]);
        let names: Vec<_> = c.tables(None).into_iter().map(|t| t.name).collect();
        assert!(
            names.contains(&"users".to_string()) && names.contains(&"active_users".to_string())
        );
        assert_eq!(c.tables(Some("SALES")).len(), 2);
        assert!(c.tables(Some("archive")).is_empty());
        assert!(!c.is_schema_loaded(Some("archive")));
        assert!(c.is_schema_loaded(None));
        let cols = c.columns(None, "USERS").unwrap();
        assert_eq!(cols[0].name, "id");
        assert!(cols[0].is_primary_key);
        assert!(c.columns(None, "nope").is_none());
        assert_eq!(c.columns(None, "active_users").map(|c| c.len()), Some(2));
        assert_eq!(
            c.find_table(None, "mixedcase").map(|t| t.name).as_deref(),
            Some("MixedCase")
        );
        assert_eq!(
            c.find_table(Some("sales"), "invoices").map(|t| t.kind),
            Some(ObjectKind::Table)
        );
        assert_eq!(
            c.find_table(None, "active_users").map(|t| t.kind),
            Some(ObjectKind::View)
        );
        assert_eq!(c.functions(None), vec!["calc_discount"]);
    }

    #[test]
    fn foreign_keys_both_directions() {
        let c = catalog();
        let fks = c.foreign_keys(None, "users");
        // incoming from public.orders and sales.customers
        let mut referencing: Vec<_> = fks.iter().map(|f| f.table.clone()).collect();
        referencing.sort();
        assert_eq!(referencing, vec!["customers", "orders"]);
        let fks = c.foreign_keys(None, "order_items");
        assert_eq!(fks.len(), 2);
        assert!(fks.iter().all(|f| f.table == "order_items"));
        let fks = c.foreign_keys(Some("sales"), "customers");
        assert_eq!(fks.len(), 2);
        // self reference
        assert_eq!(c.foreign_keys(None, "categories").len(), 2);
    }

    #[test]
    fn incremental_updates() {
        let mut c = catalog();
        let mut snap = SchemaSnapshot {
            schema: Some("sales".into()),
            ..Default::default()
        };
        snap.tables
            .push(TableDef::new(Some("sales".into()), "only_one"));
        c.add_snapshot(snap);
        assert_eq!(c.tables(Some("sales")).len(), 1);
        assert!(c.foreign_keys(Some("sales"), "invoices").is_empty());
        assert!(c.remove_schema("sales"));
        assert!(!c.remove_schema("sales"));
        assert!(c.tables(Some("sales")).is_empty());
        c.set_default_schema(Some("sales".into()));
        assert!(c.tables(None).is_empty());
        // Unnamed schema (SQLite main) acts as default
        let mut snap = SchemaSnapshot::default();
        snap.tables.push(TableDef::new(None, "t"));
        let c = SnapshotCatalog::new(vec![snap], None);
        assert_eq!(c.tables(None).len(), 1);
        assert!(c.schemas().is_empty());
    }
}
