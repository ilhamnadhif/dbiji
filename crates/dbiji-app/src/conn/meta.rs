//! Lazily loaded, cached metadata per connection (navigator, autocomplete, ERD).

use crate::backend::{Backend, Loadable};
use crate::conn::session::SessionHandle;
use dbiji_core::catalog::{CatalogContext, CatalogFolder, CatalogRow};
use dbiji_core::ddl::{SchemaSnapshot, SnapshotOptions, TableDef};
use dbiji_core::meta::{DatabaseInfo, ObjectInfo, ObjectKind, ObjectRef, SchemaInfo, SchemaRef};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Default)]
pub struct MetadataCache {
    pub databases: Loadable<Vec<DatabaseInfo>>,
    /// Keyed by database ("" = the session's current database).
    pub schemas: HashMap<String, Loadable<Vec<SchemaInfo>>>,
    pub objects: HashMap<(SchemaRef, ObjectKind), Loadable<Vec<ObjectInfo>>>,
    pub children: HashMap<(ObjectRef, ObjectKind), Loadable<Vec<ObjectInfo>>>,
    /// Light snapshots (tables, views, columns, keys) for autocomplete and ER diagrams.
    pub snapshots: HashMap<SchemaRef, Loadable<Arc<SchemaSnapshot>>>,
    pub table_defs: HashMap<ObjectRef, Loadable<Arc<TableDef>>>,
    /// Catalog folders (roles, settings, users …), keyed by folder id and context.
    pub catalog: HashMap<(&'static str, CatalogContext), Loadable<Vec<CatalogRow>>>,
    /// Incremented whenever any entry finishes loading; consumers rebuild derived data.
    pub generation: u64,
    /// Counts the clears (refresh, DDL): open object editors reload after one.
    pub invalidations: u64,
}

fn db_key(db: Option<&str>) -> String {
    db.unwrap_or("").to_string()
}

impl MetadataCache {
    /// Move finished loads into place. Returns true if anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = self.databases.poll();
        for v in self.schemas.values_mut() {
            changed |= v.poll();
        }
        for v in self.objects.values_mut() {
            changed |= v.poll();
        }
        for v in self.children.values_mut() {
            changed |= v.poll();
        }
        for v in self.snapshots.values_mut() {
            changed |= v.poll();
        }
        for v in self.table_defs.values_mut() {
            changed |= v.poll();
        }
        for v in self.catalog.values_mut() {
            changed |= v.poll();
        }
        if changed {
            self.generation += 1;
        }
        changed
    }

    pub fn is_loading(&self) -> bool {
        self.databases.is_loading()
            || self.schemas.values().any(Loadable::is_loading)
            || self.objects.values().any(Loadable::is_loading)
            || self.children.values().any(Loadable::is_loading)
            || self.snapshots.values().any(Loadable::is_loading)
            || self.catalog.values().any(Loadable::is_loading)
    }

    /// Forget everything (refresh).
    pub fn clear(&mut self) {
        *self = MetadataCache { generation: self.generation + 1, invalidations: self.invalidations + 1, ..Default::default() };
    }

    /// Forget one schema's objects and snapshot.
    pub fn clear_schema(&mut self, schema: &SchemaRef) {
        self.objects.retain(|(s, _), _| s != schema);
        self.children.retain(|(o, _), _| o.schema_ref() != *schema);
        self.snapshots.remove(schema);
        self.table_defs.retain(|o, _| o.schema_ref() != *schema);
        self.generation += 1;
        self.invalidations += 1;
    }

    /// Forget a whole database: its schema list, objects and snapshots (a script's
    /// DDL may change any schema, or create a database). `current` is the
    /// connection's database, which references name either way (`None` or by name).
    pub fn clear_database(&mut self, database: Option<&str>, current: Option<&str>) {
        let norm = |d: Option<&str>| d.filter(|d| !d.is_empty() && Some(*d) != current).map(str::to_string);
        let db = norm(database);
        let same = |s: &SchemaRef| norm(s.database.as_deref()) == db;
        self.objects.retain(|(s, _), _| !same(s));
        self.children.retain(|(o, _), _| !same(&o.schema_ref()));
        self.snapshots.retain(|s, _| !same(s));
        self.table_defs.retain(|o, _| !same(&o.schema_ref()));
        self.schemas.retain(|k, _| norm(Some(k)) != db);
        if !self.databases.is_loading() {
            self.databases = Loadable::NotLoaded;
        }
        self.generation += 1;
        self.invalidations += 1;
    }

    pub fn clear_object(&mut self, obj: &ObjectRef) {
        self.children.retain(|(o, _), _| o != obj);
        self.table_defs.remove(obj);
        self.snapshots.remove(&obj.schema_ref());
        self.generation += 1;
        self.invalidations += 1;
    }

    pub fn request_databases(&mut self, backend: &Backend, session: &SessionHandle) {
        if self.databases.is_not_loaded() {
            let s = session.clone();
            self.databases = Loadable::Loading(backend.spawn(async move { s.lock().await.list_databases().await }));
        }
    }

    pub fn schemas_of(&mut self, database: Option<&str>) -> &mut Loadable<Vec<SchemaInfo>> {
        self.schemas.entry(db_key(database)).or_default()
    }

    pub fn request_schemas(&mut self, backend: &Backend, session: &SessionHandle, database: Option<&str>) {
        let entry = self.schemas.entry(db_key(database)).or_default();
        if entry.is_not_loaded() {
            let s = session.clone();
            *entry = Loadable::Loading(backend.spawn(async move { s.lock().await.list_schemas().await }));
        }
    }

    pub fn request_objects(&mut self, backend: &Backend, session: &SessionHandle, schema: &SchemaRef, kind: ObjectKind) {
        let entry = self.objects.entry((schema.clone(), kind)).or_default();
        if entry.is_not_loaded() {
            let s = session.clone();
            let schema = schema.clone();
            *entry = Loadable::Loading(backend.spawn(async move { s.lock().await.list_objects(&schema, kind).await }));
        }
    }

    pub fn request_children(&mut self, backend: &Backend, session: &SessionHandle, parent: &ObjectRef, kind: ObjectKind) {
        let entry = self.children.entry((parent.clone(), kind)).or_default();
        if entry.is_not_loaded() {
            let s = session.clone();
            let parent = parent.clone();
            *entry = Loadable::Loading(backend.spawn(async move { s.lock().await.list_children(&parent, kind).await }));
        }
    }

    pub fn request_catalog(&mut self, backend: &Backend, session: &SessionHandle, folder: &CatalogFolder, ctx: &CatalogContext) {
        let entry = self.catalog.entry((folder.id, ctx.clone())).or_default();
        if entry.is_not_loaded() {
            let s = session.clone();
            let (folder, ctx) = (folder.clone(), ctx.clone());
            *entry = Loadable::Loading(backend.spawn(async move { dbiji_core::catalog::list(s.lock().await.as_mut(), &folder, &ctx).await }));
        }
    }

    /// Forget one catalog folder (refresh).
    pub fn clear_catalog(&mut self, folder: &str) {
        self.catalog.retain(|(f, _), _| *f != folder);
        self.generation += 1;
    }

    pub fn request_snapshot(&mut self, backend: &Backend, session: &SessionHandle, schema: &SchemaRef) {
        let entry = self.snapshots.entry(schema.clone()).or_default();
        if entry.is_not_loaded() {
            let s = session.clone();
            let schema = schema.clone();
            *entry = Loadable::Loading(backend.spawn(async move { s.lock().await.schema_snapshot(&schema, &SnapshotOptions::light()).await.map(Arc::new) }));
        }
    }

    pub fn request_table_def(&mut self, backend: &Backend, session: &SessionHandle, table: &ObjectRef) {
        let entry = self.table_defs.entry(table.clone()).or_default();
        if entry.is_not_loaded() {
            let s = session.clone();
            let t = table.clone();
            *entry = Loadable::Loading(backend.spawn(async move { s.lock().await.table_definition(&t).await.map(Arc::new) }));
        }
    }

    pub fn snapshot(&self, schema: &SchemaRef) -> Option<Arc<SchemaSnapshot>> {
        self.snapshots.get(schema).and_then(|l| l.get()).cloned()
    }

    pub fn table_def(&self, table: &ObjectRef) -> Option<Arc<TableDef>> {
        self.table_defs.get(table).and_then(|l| l.get()).cloned()
    }

    /// All snapshots loaded so far.
    pub fn loaded_snapshots(&self) -> Vec<Arc<SchemaSnapshot>> {
        self.snapshots.values().filter_map(|l| l.get().cloned()).collect()
    }
}
