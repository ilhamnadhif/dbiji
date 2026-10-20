//! Database navigator: connections → databases → schemas → object folders →
//! objects → child folders → children. Objects load lazily and are cached per
//! connection (FR-N1..N3). The visible tree is flattened every frame and drawn
//! with row virtualisation, so schemas with thousands of tables stay fast.

use crate::backend::Loadable;
use crate::command::{AppCommand, ObjectPage};
use crate::conn::{ConnStatus, LiveConnection};
use crate::cx::AppContext;
use dbiji_core::catalog::{CatalogContext, CatalogFolder, CatalogGroup, CatalogIcon, CatalogScope};
use dbiji_core::config::{ConnectionConfig, NavigatorView, wildcard_match};
use dbiji_core::meta::{ObjectInfo, ObjectKind, ObjectRef, SchemaRef};
use dbiji_core::{DbKind, tr};
use egui::{Color32, Key, RichText, Sense, Ui};
use egui_phosphor::regular as icon;
use std::collections::HashSet;

/// Identity of a tree node (used for expansion and selection state).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NavNode {
    Folder(String),
    Connection(String),
    Database {
        conn: String,
        db: String,
    },
    Schema {
        conn: String,
        schema: SchemaRef,
    },
    KindFolder {
        conn: String,
        schema: SchemaRef,
        kind: ObjectKind,
    },
    Object {
        conn: String,
        obj: ObjectRef,
    },
    ChildFolder {
        conn: String,
        parent: ObjectRef,
        kind: ObjectKind,
    },
    Child {
        conn: String,
        obj: ObjectRef,
    },
    Extensions {
        conn: String,
        db: Option<String>,
    },
    /// Group of server-level catalog folders (Security, Storage, Administer, System Info).
    CatalogGroup {
        conn: String,
        group: CatalogGroup,
    },
    /// A catalog folder (roles, settings …); `ctx` holds its database, table or parent object.
    CatalogFolder {
        conn: String,
        folder: &'static str,
        ctx: CatalogContext,
    },
    /// An object of a catalog folder (`ctx.name`).
    CatalogItem {
        conn: String,
        folder: &'static str,
        ctx: CatalogContext,
    },
}

impl NavNode {
    pub fn connection(&self) -> Option<&str> {
        match self {
            NavNode::Folder(_) => None,
            NavNode::Connection(c)
            | NavNode::Database { conn: c, .. }
            | NavNode::Schema { conn: c, .. }
            | NavNode::KindFolder { conn: c, .. }
            | NavNode::Object { conn: c, .. }
            | NavNode::ChildFolder { conn: c, .. }
            | NavNode::Child { conn: c, .. }
            | NavNode::Extensions { conn: c, .. }
            | NavNode::CatalogGroup { conn: c, .. }
            | NavNode::CatalogFolder { conn: c, .. }
            | NavNode::CatalogItem { conn: c, .. } => Some(c),
        }
    }
}

#[derive(Clone)]
struct Row {
    node: NavNode,
    depth: usize,
    label: String,
    detail: Option<String>,
    icon: &'static str,
    icon_color: Option<Color32>,
    /// A dot on the icon (connection status).
    badge: Option<Color32>,
    expandable: bool,
    expanded: bool,
    /// Placeholder rows: "Loading…", errors, empty markers.
    kind: RowKind,
}

#[derive(Clone, PartialEq)]
enum RowKind {
    Node,
    Loading,
    Error(String),
    Empty,
}

#[derive(Default)]
pub struct Navigator {
    expanded: HashSet<NavNode>,
    pub selected: Option<NavNode>,
    /// Object to select once its row exists (Link with editor), with a deadline.
    pending_reveal: Option<(String, ObjectRef, std::time::Instant)>,
    pub filter: String,
    rows: Vec<Row>,
    scroll_to_selected: bool,
    dark: bool,
    /// Navigator preferences (copied from the settings every frame).
    prefs: dbiji_config::settings::NavigatorSettings,
}

/// Text next to a connection: `user@host:port/db`, or for a database file
/// its folder (home as `~`, long paths shortened to the last folders).
pub(crate) fn connection_detail(cfg: &ConnectionConfig) -> String {
    if !cfg.kind.is_file_based() {
        return cfg.summary();
    }
    let Some(path) = cfg.file_path.as_deref().filter(|p| !p.is_empty()) else { return String::new() };
    let path = std::path::Path::new(path);
    let file = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = path.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    let home = std::env::var("HOME").ok().filter(|h| h.len() > 1);
    let dir = match home {
        Some(h) if dir.starts_with(&h) => format!("~{}", &dir[h.len()..]),
        _ => dir,
    };
    let dir = if dir.chars().count() > 32 {
        let parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
        format!("…/{}", parts[parts.len().saturating_sub(2)..].join("/"))
    } else {
        dir
    };
    // The name is usually the file name: then the folder is enough.
    if cfg.name == file { dir } else { format!("{dir}/{file}") }
}

pub fn kind_icon(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Database => icon::DATABASE,
        ObjectKind::Schema => icon::STACK,
        ObjectKind::Table | ObjectKind::Partition => icon::TABLE,
        ObjectKind::View => icon::EYE,
        ObjectKind::MaterializedView => icon::EYE_CLOSED,
        ObjectKind::ForeignTable => icon::GLOBE,
        ObjectKind::Column => icon::COLUMNS,
        ObjectKind::Index => icon::LIGHTNING,
        ObjectKind::PrimaryKey => icon::KEY,
        ObjectKind::UniqueKey => icon::FINGERPRINT,
        ObjectKind::CheckConstraint => icon::CHECK_SQUARE,
        ObjectKind::ForeignKey => icon::LINK,
        ObjectKind::Trigger => icon::LIGHTNING_A,
        ObjectKind::Sequence => icon::LIST_NUMBERS,
        ObjectKind::Function | ObjectKind::Aggregate => icon::FUNCTION,
        ObjectKind::Procedure => icon::GEAR_SIX,
        ObjectKind::Type | ObjectKind::Domain => icon::SHAPES,
        ObjectKind::Extension => icon::PUZZLE_PIECE,
        ObjectKind::Event => icon::CALENDAR,
        ObjectKind::Synonym => icon::ARROW_BEND_UP_RIGHT,
        ObjectKind::Role => icon::USER,
        ObjectKind::Tablespace => icon::HARD_DRIVES,
    }
}

pub fn db_icon(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Sqlite | DbKind::DuckDb => icon::FILE_SQL,
        DbKind::ClickHouse => icon::CHART_BAR,
        _ => icon::DATABASE,
    }
}

/// The connection filter that applies to objects of `kind`.
pub fn name_filter(f: &dbiji_core::config::ObjectFilters, kind: ObjectKind) -> &dbiji_core::config::NameFilter {
    match kind {
        ObjectKind::Database => &f.databases,
        ObjectKind::Schema => &f.schemas,
        ObjectKind::Table | ObjectKind::Partition | ObjectKind::ForeignTable => &f.tables,
        ObjectKind::View | ObjectKind::MaterializedView => &f.views,
        k if k.is_routine() => &f.routines,
        _ => &f.other,
    }
}

/// Mutable [`name_filter`].
pub fn name_filter_mut(f: &mut dbiji_core::config::ObjectFilters, kind: ObjectKind) -> &mut dbiji_core::config::NameFilter {
    match kind {
        ObjectKind::Database => &mut f.databases,
        ObjectKind::Schema => &mut f.schemas,
        ObjectKind::Table | ObjectKind::Partition | ObjectKind::ForeignTable => &mut f.tables,
        ObjectKind::View | ObjectKind::MaterializedView => &mut f.views,
        k if k.is_routine() => &mut f.routines,
        _ => &mut f.other,
    }
}

fn filter_ok(cfg: &ConnectionConfig, kind: ObjectKind, name: &str) -> bool {
    let f = &cfg.filters;
    match kind {
        ObjectKind::Database => f.databases.matches(name),
        ObjectKind::Schema => f.schemas.matches(name),
        ObjectKind::Table | ObjectKind::Partition | ObjectKind::ForeignTable => f.tables.matches(name),
        ObjectKind::View | ObjectKind::MaterializedView => f.views.matches(name),
        k if k.is_routine() => f.routines.matches(name),
        _ => f.other.matches(name),
    }
}

impl Navigator {
    pub fn select(&mut self, node: NavNode) {
        self.selected = Some(node);
        self.scroll_to_selected = true;
    }

    /// Expand the path to `obj` and select it (once loaded).
    pub fn reveal(&mut self, conn: &str, obj: &ObjectRef) {
        let c = conn.to_string();
        self.expanded.insert(NavNode::Connection(c.clone()));
        if let Some(db) = &obj.database {
            self.expanded.insert(NavNode::Database { conn: c.clone(), db: db.clone() });
        }
        for database in [obj.database.clone(), None] {
            let schema = SchemaRef { database, schema: obj.schema.clone() };
            self.expanded.insert(NavNode::Schema { conn: c.clone(), schema: schema.clone() });
            self.expanded.insert(NavNode::KindFolder { conn: c.clone(), schema, kind: obj.kind });
        }
        self.pending_reveal = Some((c, obj.clone(), std::time::Instant::now() + std::time::Duration::from_secs(5)));
    }

    /// Select the pending revealed object when its row is there.
    fn finish_reveal(&mut self) {
        let Some((conn, obj, until)) = self.pending_reveal.clone() else { return };
        let found = self.rows.iter().find(|r| match &r.node {
            NavNode::Object { conn: c, obj: o } => *c == conn && o.kind == obj.kind && o.name == obj.name && o.schema == obj.schema,
            _ => false,
        });
        if let Some(r) = found {
            self.selected = Some(r.node.clone());
            self.scroll_to_selected = true;
            self.pending_reveal = None;
        } else if std::time::Instant::now() > until {
            self.pending_reveal = None;
        }
    }

    pub fn expand(&mut self, node: NavNode) {
        self.expanded.insert(node);
    }

    pub fn collapse_all(&mut self) {
        self.expanded.clear();
    }

    fn filter_matches(&self, text: &str) -> bool {
        if self.filter.trim().is_empty() {
            return true;
        }
        let f = self.filter.trim();
        if f.contains('*') || f.contains('?') { wildcard_match(f, text) } else { text.to_lowercase().contains(&f.to_lowercase()) }
    }

    /// Rebuild the flattened list of visible rows.
    fn build(&mut self, cx: &mut AppContext) {
        let mut rows = Vec::new();
        // Folders: explicit folders plus folders used by connections.
        let mut folders: Vec<String> = cx.conns.folders().to_vec();
        for c in cx.conns.configs() {
            if let Some(f) = &c.folder {
                let mut acc = String::new();
                for part in f.split('/').filter(|p| !p.is_empty()) {
                    if !acc.is_empty() {
                        acc.push('/');
                    }
                    acc.push_str(part);
                    if !folders.contains(&acc) {
                        folders.push(acc.clone());
                    }
                }
            }
        }
        folders.sort_by_key(|f| f.to_lowercase());
        let mut configs: Vec<ConnectionConfig> = cx.conns.configs().to_vec();
        configs.sort_by_key(|c| (c.sort_index.unwrap_or(u32::MAX), c.name.to_lowercase()));

        // Top level folders, then connections without folder.
        self.build_folder_level(cx, &mut rows, &folders, &configs, None, 0);
        self.rows = rows;
    }

    fn build_folder_level(&self, cx: &mut AppContext, rows: &mut Vec<Row>, folders: &[String], configs: &[ConnectionConfig], parent: Option<&str>, depth: usize) {
        for f in folders {
            let is_child = match parent {
                None => !f.contains('/'),
                Some(p) => f.strip_prefix(p).and_then(|r| r.strip_prefix('/')).is_some_and(|r| !r.contains('/')),
            };
            if !is_child {
                continue;
            }
            let node = NavNode::Folder(f.clone());
            let expanded = self.expanded.contains(&node) || !self.filter.trim().is_empty();
            let label = f.rsplit('/').next().unwrap_or(f).to_string();
            rows.push(Row {
                node: node.clone(),
                depth,
                label,
                detail: None,
                icon: if expanded { icon::FOLDER_OPEN } else { icon::FOLDER },
                icon_color: None,
                badge: None,
                expandable: true,
                expanded,
                kind: RowKind::Node,
            });
            if expanded {
                self.build_folder_level(cx, rows, folders, configs, Some(f), depth + 1);
            }
        }
        for c in configs.iter().filter(|c| c.folder.as_deref().filter(|f| !f.is_empty()) == parent) {
            self.build_connection(cx, rows, c, depth);
        }
    }

    fn build_connection(&self, cx: &mut AppContext, rows: &mut Vec<Row>, cfg: &ConnectionConfig, depth: usize) {
        let node = NavNode::Connection(cfg.id.clone());
        let status = cx.conns.status(&cfg.id);
        let expanded = self.expanded.contains(&node) && matches!(status, ConnStatus::Connected | ConnStatus::Lost);
        let filter_hit = self.filter_matches(&cfg.name);
        let dark = self.dark;
        let color = match &status {
            ConnStatus::Connected => Some(crate::theme::success_color(dark)),
            ConnStatus::Connecting => Some(crate::theme::warning_color(dark)),
            ConnStatus::Failed(_) | ConnStatus::Lost => Some(crate::theme::error_color(dark)),
            ConnStatus::Disconnected => None,
        };
        let marker = cx.conns.color_of(&cfg.id).and_then(crate::theme::connection_marker);
        let start = rows.len();
        rows.push(Row {
            node: node.clone(),
            depth,
            label: cfg.name.clone(),
            detail: self.prefs.show_connection_details.then(|| connection_detail(cfg)),
            icon: db_icon(cfg.kind),
            icon_color: Some(marker.unwrap_or_else(|| crate::theme::brand_color(cfg.kind))),
            badge: color,
            expandable: true,
            expanded,
            kind: RowKind::Node,
        });
        if !expanded {
            if !filter_hit {
                rows.truncate(start);
            }
            return;
        }
        let backend = cx.backend;
        let Some(live) = cx.conns.live_mut(&cfg.id) else { return };
        let caps = live.driver.info().capabilities.clone();
        let show_system = live.config.init.show_system_objects || self.prefs.show_system_objects;
        let mut child_rows = Vec::new();
        if caps.multiple_databases {
            live.meta.request_databases(backend, &live.main.clone());
            match &live.meta.databases {
                Loadable::Loaded(dbs) => {
                    let current = live.current_database.clone();
                    let show_all = live.config.init.show_all_databases;
                    let mut dbs = dbs.clone();
                    dbs.retain(|d| (show_all || Some(&d.name) == current.as_ref() || d.is_current) && (show_system || !d.is_system));
                    dbs.retain(|d| filter_ok(&live.config, ObjectKind::Database, &d.name));
                    if dbs.is_empty()
                        && let Some(cur) = current.clone()
                    {
                        dbs.push(dbiji_core::meta::DatabaseInfo { name: cur, is_current: true, ..Default::default() });
                    }
                    for d in dbs {
                        self.build_database(backend, live, &mut child_rows, &cfg.id, &d.name, depth + 1, &caps, show_system);
                    }
                }
                Loadable::Failed(e) => child_rows.push(placeholder(depth + 1, RowKind::Error(e.to_string()))),
                _ => child_rows.push(placeholder(depth + 1, RowKind::Loading)),
            }
        } else {
            self.build_schemas(backend, live, &mut child_rows, &cfg.id, None, depth + 1, &caps, show_system);
        }
        self.push_server_catalog(live, &mut child_rows, &cfg.id, depth + 1, !caps.multiple_databases);
        if !filter_hit && !child_rows.iter().any(|r| r.kind == RowKind::Node) {
            rows.truncate(start);
            return;
        }
        rows.extend(child_rows);
    }

    #[allow(clippy::too_many_arguments)]
    fn build_database(
        &self,
        backend: &crate::backend::Backend,
        live: &mut LiveConnection,
        rows: &mut Vec<Row>,
        conn: &str,
        db: &str,
        depth: usize,
        caps: &dbiji_core::driver::Capabilities,
        show_system: bool,
    ) {
        let node = NavNode::Database { conn: conn.to_string(), db: db.to_string() };
        let expanded = self.expanded.contains(&node);
        let is_current = Some(db) == live.current_database.as_deref();
        rows.push(Row {
            node: node.clone(),
            depth,
            label: db.to_string(),
            detail: if is_current { Some(tr("current").to_string()) } else { None },
            icon: icon::DATABASE,
            icon_color: None,
            badge: None,
            expandable: true,
            expanded,
            kind: RowKind::Node,
        });
        if expanded {
            let db_opt = if is_current { None } else { Some(db) };
            self.build_schemas(backend, live, rows, conn, db_opt.map(|s| s.to_string()).as_deref(), depth + 1, caps, show_system);
            if live.kind().is_postgres_family() {
                let ext = NavNode::Extensions { conn: conn.to_string(), db: db_opt.map(str::to_string) };
                let ext_expanded = self.expanded.contains(&ext);
                rows.push(Row {
                    node: ext.clone(),
                    depth: depth + 1,
                    label: tr(ObjectKind::Extension.plural_label()).to_string(),
                    detail: None,
                    icon: icon::PUZZLE_PIECE,
                    icon_color: None,
                    badge: None,
                    expandable: true,
                    expanded: ext_expanded,
                    kind: RowKind::Node,
                });
                if ext_expanded {
                    let schema = SchemaRef { database: db_opt.map(str::to_string), schema: None };
                    if let Some(session) = live.ensure_session(backend, db_opt) {
                        live.meta.request_objects(backend, &session, &schema, ObjectKind::Extension);
                    }
                    self.push_objects(live, rows, conn, &schema, ObjectKind::Extension, depth + 2, false);
                }
            }
            let folders: Vec<CatalogFolder> = live.catalog_folders.iter().filter(|f| f.scope == CatalogScope::Database).cloned().collect();
            let ctx = CatalogContext { database: db_opt.map(str::to_string), ..Default::default() };
            self.push_catalog_folders(live, rows, conn, folders, ctx, depth + 1);
        }
    }

    /// Server-level catalog groups (and the database-level folders of
    /// connections without a database level).
    fn push_server_catalog(&self, live: &mut LiveConnection, rows: &mut Vec<Row>, conn: &str, depth: usize, with_database_scope: bool) {
        let all = live.catalog_folders.clone();
        if with_database_scope {
            let folders: Vec<CatalogFolder> = all.iter().filter(|f| f.scope == CatalogScope::Database).cloned().collect();
            self.push_catalog_folders(live, rows, conn, folders, CatalogContext::default(), depth);
        }
        for group in [CatalogGroup::Security, CatalogGroup::Storage, CatalogGroup::Administer, CatalogGroup::SystemInfo] {
            let folders: Vec<CatalogFolder> = all.iter().filter(|f| f.scope == CatalogScope::Server && f.group == group).cloned().collect();
            if folders.is_empty() {
                continue;
            }
            let node = NavNode::CatalogGroup { conn: conn.to_string(), group };
            let expanded = self.expanded.contains(&node);
            let group_icon = match group {
                CatalogGroup::Security => icon::SHIELD_CHECK,
                CatalogGroup::Storage => icon::HARD_DRIVES,
                CatalogGroup::Administer => icon::WRENCH,
                _ => icon::INFO,
            };
            rows.push(Row {
                node,
                depth,
                label: tr(group.label()).to_string(),
                detail: None,
                icon: group_icon,
                icon_color: None,
                badge: None,
                expandable: true,
                expanded,
                kind: RowKind::Node,
            });
            if expanded {
                self.push_catalog_folders(live, rows, conn, folders, CatalogContext::default(), depth + 1);
            }
        }
    }

    /// Catalog folders and, when expanded, their objects and sub-folders.
    fn push_catalog_folders(&self, live: &mut LiveConnection, rows: &mut Vec<Row>, conn: &str, folders: Vec<CatalogFolder>, base: CatalogContext, depth: usize) {
        for f in folders {
            let node = NavNode::CatalogFolder { conn: conn.to_string(), folder: f.id, ctx: base.clone() };
            let expanded = !f.table_only && self.expanded.contains(&node);
            rows.push(Row {
                node,
                depth,
                label: tr(f.label).to_string(),
                detail: None,
                icon: if f.table_only { icon::TABLE } else { catalog_icon(f.icon) },
                icon_color: None,
                badge: None,
                expandable: !f.table_only,
                expanded,
                kind: RowKind::Node,
            });
            if !expanded {
                continue;
            }
            let items = match live.meta.catalog.get(&(f.id, base.clone())) {
                Some(Loadable::Loaded(v)) => v.clone(),
                Some(Loadable::Failed(e)) => {
                    rows.push(placeholder(depth + 1, RowKind::Error(e.to_string())));
                    continue;
                }
                _ => {
                    rows.push(placeholder(depth + 1, RowKind::Loading));
                    continue;
                }
            };
            let mut shown = 0;
            for it in items.iter().filter(|it| self.filter_matches(&it.name)) {
                shown += 1;
                let ctx = CatalogContext { name: Some(it.name.clone()), ..base.clone() };
                let item = NavNode::CatalogItem { conn: conn.to_string(), folder: f.id, ctx };
                let has_children = !f.children.is_empty();
                let item_expanded = has_children && self.expanded.contains(&item);
                rows.push(Row {
                    node: item,
                    depth: depth + 1,
                    label: it.name.clone(),
                    detail: it.detail.clone(),
                    icon: catalog_icon(f.icon),
                    icon_color: None,
                    badge: None,
                    expandable: has_children,
                    expanded: item_expanded,
                    kind: RowKind::Node,
                });
                if item_expanded {
                    let children: Vec<CatalogFolder> = f.children.iter().filter_map(|id| live.catalog_folder(id).cloned()).collect();
                    let child_ctx = CatalogContext { parent: Some(it.name.clone()), name: None, ..base.clone() };
                    self.push_catalog_folders(live, rows, conn, children, child_ctx, depth + 2);
                }
            }
            if shown == 0 && self.filter.trim().is_empty() {
                rows.push(placeholder(depth + 1, RowKind::Empty));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_schemas(
        &self,
        backend: &crate::backend::Backend,
        live: &mut LiveConnection,
        rows: &mut Vec<Row>,
        conn: &str,
        db: Option<&str>,
        depth: usize,
        caps: &dbiji_core::driver::Capabilities,
        show_system: bool,
    ) {
        let Some(session) = live.ensure_session(backend, db) else {
            rows.push(placeholder(depth, RowKind::Loading));
            return;
        };
        live.meta.request_schemas(backend, &session, db);
        let schemas = match live.meta.schemas_of(db) {
            Loadable::Loaded(s) => s.clone(),
            Loadable::Failed(e) => {
                rows.push(placeholder(depth, RowKind::Error(e.to_string())));
                return;
            }
            _ => {
                rows.push(placeholder(depth, RowKind::Loading));
                return;
            }
        };
        let mut schemas: Vec<_> = schemas.into_iter().filter(|s| (show_system || !s.is_system) && filter_ok(&live.config, ObjectKind::Schema, &s.name)).collect();
        schemas.sort_by(|a, b| b.is_current.cmp(&a.is_current).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        // SQLite with only "main": show folders directly.
        let flatten = live.kind() == DbKind::Sqlite && schemas.len() == 1;
        for s in schemas {
            let schema_ref = SchemaRef { database: db.map(str::to_string), schema: Some(s.name.clone()) };
            if flatten {
                self.build_kind_folders(backend, live, rows, conn, &schema_ref, depth, caps);
                continue;
            }
            let node = NavNode::Schema { conn: conn.to_string(), schema: schema_ref.clone() };
            let expanded = self.expanded.contains(&node);
            let start = rows.len();
            rows.push(Row {
                node,
                depth,
                label: s.name.clone(),
                detail: if s.is_current { Some(tr("default").to_string()) } else { None },
                icon: if caps.schema_label == "Databases" { icon::DATABASE } else { icon::STACK },
                icon_color: None,
                badge: None,
                expandable: true,
                expanded,
                kind: RowKind::Node,
            });
            if expanded {
                self.build_kind_folders(backend, live, rows, conn, &schema_ref, depth + 1, caps);
            } else if !self.filter_matches(&s.name) {
                rows.truncate(start);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_kind_folders(
        &self,
        backend: &crate::backend::Backend,
        live: &mut LiveConnection,
        rows: &mut Vec<Row>,
        conn: &str,
        schema: &SchemaRef,
        depth: usize,
        caps: &dbiji_core::driver::Capabilities,
    ) {
        let simple = live.config.navigator_view == NavigatorView::Simple;
        for kind in caps.schema_object_kinds.iter().copied() {
            if simple && !matches!(kind, ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView) {
                continue;
            }
            let node = NavNode::KindFolder { conn: conn.to_string(), schema: schema.clone(), kind };
            let expanded = self.expanded.contains(&node) || (!self.filter.trim().is_empty() && matches!(kind, ObjectKind::Table | ObjectKind::View));
            let count = live.meta.objects.get(&(schema.clone(), kind)).and_then(|l| l.get()).map(|v| v.len());
            rows.push(Row {
                node,
                depth,
                label: tr(kind.plural_label()).to_string(),
                detail: count.map(|c| c.to_string()),
                icon: icon::FOLDER_SIMPLE,
                icon_color: None,
                badge: None,
                expandable: true,
                expanded,
                kind: RowKind::Node,
            });
            if expanded {
                if let Some(session) = live.ensure_session(backend, schema.database.as_deref()) {
                    live.meta.request_objects(backend, &session, schema, kind);
                }
                let has_children = kind.is_table_like() || matches!(kind, ObjectKind::View | ObjectKind::MaterializedView);
                self.push_objects(live, rows, conn, schema, kind, depth + 1, has_children);
                // expanded objects get their child folders
                let objs: Vec<ObjectInfo> = live.meta.objects.get(&(schema.clone(), kind)).and_then(|l| l.get()).cloned().unwrap_or_default();
                let _ = objs;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_objects(&self, live: &mut LiveConnection, rows: &mut Vec<Row>, conn: &str, schema: &SchemaRef, kind: ObjectKind, depth: usize, has_children: bool) {
        let show_system = live.config.init.show_system_objects || self.prefs.show_system_objects;
        let cfg = live.config.clone();
        let list = match live.meta.objects.get(&(schema.clone(), kind)) {
            Some(Loadable::Loaded(v)) => v.clone(),
            Some(Loadable::Failed(e)) => {
                rows.push(placeholder(depth, RowKind::Error(e.to_string())));
                return;
            }
            _ => {
                rows.push(placeholder(depth, RowKind::Loading));
                return;
            }
        };
        let mut shown = 0;
        let mut list = list;
        if self.prefs.sort == dbiji_config::settings::NavigatorSort::Alphabetical {
            list.sort_by_key(|o| o.obj.name.to_lowercase());
        }
        for o in list.iter().filter(|o| (show_system || !o.is_system) && filter_ok(&cfg, kind, &o.obj.name)) {
            if !self.filter_matches(&o.obj.name) {
                continue;
            }
            shown += 1;
            let node = NavNode::Object { conn: conn.to_string(), obj: o.obj.clone() };
            let expanded = has_children && self.expanded.contains(&node);
            let mut label = o.obj.name.clone();
            if let Some(sig) = &o.obj.signature
                && sig.starts_with('(')
            {
                label.push_str(sig);
            }
            rows.push(Row {
                node,
                depth,
                label,
                detail: o.detail.clone().or_else(|| o.row_estimate.filter(|r| *r >= 0).map(|r| format!("~{r}"))),
                icon: kind_icon(kind),
                icon_color: None,
                badge: None,
                expandable: has_children,
                expanded,
                kind: RowKind::Node,
            });
            if expanded {
                self.push_child_folders(live, rows, conn, &o.obj, depth + 1);
            }
        }
        if shown == 0 && self.filter.trim().is_empty() {
            rows.push(placeholder(depth, RowKind::Empty));
        }
    }

    fn push_child_folders(&self, live: &mut LiveConnection, rows: &mut Vec<Row>, conn: &str, obj: &ObjectRef, depth: usize) {
        let simple = live.config.navigator_view == NavigatorView::Simple;
        let caps = live.driver.info().capabilities.clone();
        let kinds: Vec<ObjectKind> = if obj.kind.is_table_like() { caps.table_child_kinds.clone() } else { vec![ObjectKind::Column] };
        for kind in kinds {
            if simple && kind != ObjectKind::Column {
                continue;
            }
            let node = NavNode::ChildFolder { conn: conn.to_string(), parent: obj.clone(), kind };
            let expanded = self.expanded.contains(&node);
            rows.push(Row {
                node,
                depth,
                label: tr(kind.plural_label()).to_string(),
                detail: None,
                icon: icon::FOLDER_SIMPLE,
                icon_color: None,
                badge: None,
                expandable: true,
                expanded,
                kind: RowKind::Node,
            });
            if expanded {
                match live.meta.children.get(&(obj.clone(), kind)) {
                    Some(Loadable::Loaded(v)) => {
                        if v.is_empty() {
                            rows.push(placeholder(depth + 1, RowKind::Empty));
                        }
                        for c in v {
                            rows.push(Row {
                                node: NavNode::Child { conn: conn.to_string(), obj: c.obj.clone() },
                                depth: depth + 1,
                                label: c.obj.name.clone(),
                                detail: c.detail.clone(),
                                icon: kind_icon(c.obj.kind),
                                icon_color: None,
                                badge: None,
                                expandable: false,
                                expanded: false,
                                kind: RowKind::Node,
                            });
                        }
                    }
                    Some(Loadable::Failed(e)) => rows.push(placeholder(depth + 1, RowKind::Error(e.to_string()))),
                    _ => rows.push(placeholder(depth + 1, RowKind::Loading)),
                }
            }
        }
        if obj.kind.is_table_like() && !simple {
            let folders: Vec<CatalogFolder> = live.catalog_folders.iter().filter(|f| f.scope == CatalogScope::Table).cloned().collect();
            let ctx = CatalogContext { database: obj.database.clone(), schema: obj.schema.clone(), table: Some(obj.name.clone()), ..Default::default() };
            self.push_catalog_folders(live, rows, conn, folders, ctx, depth);
        }
    }

    /// Request loads for newly expanded child folders (needs a session).
    fn request_child_loads(&self, cx: &mut AppContext) {
        let backend = cx.backend;
        for node in &self.expanded {
            if let NavNode::ChildFolder { conn, parent, kind } = node
                && let Some(live) = cx.conns.live_mut(conn)
                && let Some(session) = live.ensure_session(backend, parent.database.as_deref())
            {
                live.meta.request_children(backend, &session, parent, *kind);
            }
            if let NavNode::CatalogFolder { conn, folder, ctx } = node
                && let Some(live) = cx.conns.live_mut(conn)
                && let Some(f) = live.catalog_folder(folder).cloned()
                && !f.table_only
                && let Some(session) = live.ensure_session(backend, ctx.database.as_deref())
            {
                live.meta.request_catalog(backend, &session, &f, ctx);
            }
        }
    }

    /// Properties of the selected node (Properties panel).
    pub fn selected_properties(&self, cx: &AppContext) -> Vec<(String, String)> {
        let Some(node) = &self.selected else { return Vec::new() };
        let mut out: Vec<(String, String)> = Vec::new();
        let mut add = |k: &str, v: Option<String>| {
            if let Some(v) = v.filter(|v| !v.is_empty()) {
                out.push((tr(k).to_string(), v));
            }
        };
        let yes_no = |b: bool| Some(if b { tr("Yes") } else { tr("No") }.to_string());
        match node {
            NavNode::Folder(f) => add("Folder", Some(f.clone())),
            NavNode::Connection(c) => {
                let Some(cfg) = cx.conns.config(c) else { return Vec::new() };
                add("Name", Some(cfg.name.clone()));
                add("Database type", Some(cfg.kind.display_name().to_string()));
                add("Host", Some(cfg.host.clone()));
                add("Port", cfg.port.map(|p| p.to_string()));
                add("File", cfg.file_path.clone());
                add("Database", cfg.database.clone());
                add("User", cfg.user.clone());
                add("Connection type", Some(cx.conns.type_of(c).name));
                add("Read-only", yes_no(cfg.read_only));
                add("Description", cfg.description.clone());
                match cx.conns.live(c) {
                    Some(live) => {
                        add("Server", Some(format!("{} {}", live.server.product, live.server.version)));
                        add("Session", live.server.session_id.clone());
                    }
                    None => add("Status", Some(tr("not connected").to_string())),
                }
            }
            NavNode::Database { conn, db } => {
                add("Database", Some(db.clone()));
                add("Connection", cx.conns.config(conn).map(|c| c.name.clone()));
            }
            NavNode::Schema { conn, schema } => {
                add("Schema", schema.schema.clone());
                add("Database", schema.database.clone());
                add("Connection", cx.conns.config(conn).map(|c| c.name.clone()));
            }
            NavNode::KindFolder { conn, schema, kind } => {
                add("Folder", Some(tr(kind.plural_label()).to_string()));
                add("Schema", schema.schema.clone());
                let n = cx.conns.live(conn).and_then(|l| l.meta.objects.get(&(schema.clone(), *kind))).and_then(|l| l.get()).map(|v| v.len());
                add("Objects", n.map(|n| n.to_string()));
            }
            NavNode::ChildFolder { conn, parent, kind } => {
                add("Folder", Some(tr(kind.plural_label()).to_string()));
                add("Table", Some(parent.name.clone()));
                let n = cx.conns.live(conn).and_then(|l| l.meta.children.get(&(parent.clone(), *kind))).and_then(|l| l.get()).map(|v| v.len());
                add("Objects", n.map(|n| n.to_string()));
            }
            NavNode::Object { conn, obj } | NavNode::Child { conn, obj } => {
                add("Name", Some(obj.name.clone()));
                add("Type", Some(tr(obj.kind.label()).to_string()));
                add("Table", obj.table.clone());
                add("Schema", obj.schema.clone());
                add("Database", obj.database.clone());
                add("Signature", obj.signature.clone());
                if let Some(live) = cx.conns.live(conn) {
                    let info = live.meta.objects.values().chain(live.meta.children.values()).filter_map(|l| l.get()).flatten().find(|i| i.obj == *obj);
                    if let Some(info) = info {
                        add("Details", info.detail.clone());
                        add("Comment", info.comment.clone());
                        add("Estimated rows", info.row_estimate.filter(|n| *n >= 0).map(|n| n.to_string()));
                        if info.is_system {
                            add("System object", yes_no(true));
                        }
                    }
                    if let Some(def) = live.meta.table_def(obj) {
                        add("Columns", Some(def.columns.len().to_string()));
                    }
                }
            }
            NavNode::Extensions { db, .. } => {
                add("Folder", Some(tr("Extensions").to_string()));
                add("Database", db.clone());
            }
            NavNode::CatalogGroup { group, .. } => add("Folder", Some(tr(group.label()).to_string())),
            NavNode::CatalogFolder { conn, folder, ctx } => {
                let label = cx.conns.live(conn).and_then(|l| l.catalog_folder(folder)).map(|f| tr(f.label).to_string());
                add("Folder", label);
                let n = cx.conns.live(conn).and_then(|l| l.meta.catalog.get(&(*folder, ctx.clone()))).and_then(|l| l.get()).map(|v| v.len());
                add("Objects", n.map(|n| n.to_string()));
            }
            NavNode::CatalogItem { conn, folder, ctx } => {
                add("Name", ctx.name.clone());
                let list_ctx = CatalogContext { name: None, ..ctx.clone() };
                let row = cx
                    .conns
                    .live(conn)
                    .and_then(|l| l.meta.catalog.get(&(*folder, list_ctx)))
                    .and_then(|l| l.get())
                    .and_then(|rows| rows.iter().find(|r| Some(&r.name) == ctx.name.as_ref()));
                if let Some(row) = row {
                    for (k, v) in row.values.iter().skip(1) {
                        if !v.is_null() && !k.eq_ignore_ascii_case("detail") {
                            out.push((k.replace('_', " "), v.to_text().into_owned()));
                        }
                    }
                }
            }
        }
        out
    }

    fn properties_ui(&self, ui: &mut Ui, cx: &AppContext) {
        ui.horizontal(|ui| {
            ui.strong(format!("{} {}", icon::INFO, tr("Properties")));
        });
        // only a node of the tree (not one removed since it was selected)
        let in_tree = self.selected.as_ref().is_some_and(|sel| self.rows.iter().any(|r| &r.node == sel));
        let props = if in_tree { self.selected_properties(cx) } else { Vec::new() };
        if props.is_empty() {
            ui.weak(tr("Select an object in the navigator."));
            return;
        }
        egui::ScrollArea::vertical().id_salt("nav_properties_scroll").auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("nav_properties_grid").num_columns(2).striped(true).spacing([10.0, 3.0]).show(ui, |ui| {
                for (k, v) in &props {
                    ui.label(RichText::new(k).weak());
                    let first = v.lines().next().unwrap_or("");
                    let resp = ui.add(egui::Label::new(first).truncate());
                    resp.context_menu(|ui| {
                        if ui.button((dbiji_icons::ic(icon::COPY), tr("Copy"))).clicked() {
                            ui.ctx().copy_text(v.clone());
                            ui.close();
                        }
                    });
                    ui.end_row();
                }
            });
        });
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        self.dark = ui.visuals().dark_mode;
        if self.prefs != cx.settings.navigator {
            self.prefs = cx.settings.navigator.clone();
        }
        // Toolbar
        ui.horizontal(|ui| {
            if ui.button(dbiji_icons::ic(icon::PLUS)).on_hover_text(tr("New connection")).clicked() {
                cx.push(AppCommand::NewConnection { kind: None, folder: None });
            }
            let sel_conn = self.selected.as_ref().and_then(|n| n.connection()).map(str::to_string);
            let connected = sel_conn.as_deref().is_some_and(|c| cx.conns.is_connected(c));
            if ui
                .add_enabled(sel_conn.is_some(), egui::Button::new(if connected { icon::PLUGS } else { icon::PLUGS_CONNECTED }))
                .on_hover_text(if connected { tr("Disconnect") } else { tr("Connect") })
                .clicked()
                && let Some(c) = sel_conn.clone()
            {
                cx.push(if connected { AppCommand::Disconnect(c) } else { AppCommand::Connect(c) });
            }
            if ui.add_enabled(sel_conn.is_some(), egui::Button::new(dbiji_icons::ic(icon::FILE_SQL))).on_hover_text(tr("New SQL editor")).clicked() {
                cx.push(AppCommand::NewSqlEditor { connection: sel_conn.clone(), schema: self.selected_schema(), text: None });
            }
            if ui.add_enabled(connected, egui::Button::new(dbiji_icons::ic(icon::ARROW_CLOCKWISE))).on_hover_text(tr("Refresh")).clicked() {
                self.refresh_selected(cx);
            }
            if ui.button(dbiji_icons::ic(icon::ARROWS_IN_SIMPLE)).on_hover_text(tr("Collapse all")).clicked() {
                self.collapse_all();
            }
            let mut props = cx.settings.navigator.show_properties;
            if ui.toggle_value(&mut props, dbiji_icons::ic(icon::INFO)).on_hover_text(tr("Properties panel")).changed() {
                cx.settings.navigator.show_properties = props;
                cx.push(AppCommand::SettingsChanged);
            }
            let mut link = cx.settings.navigator.link_with_editor;
            if ui.toggle_value(&mut link, dbiji_icons::ic(icon::LINK)).on_hover_text(tr("Link with editor: select the object of the active tab")).changed() {
                cx.settings.navigator.link_with_editor = link;
                cx.push(AppCommand::SettingsChanged);
            }
        });
        let search = ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(format!("{} {}", icon::MAGNIFYING_GLASS, tr("Filter objects"))).desired_width(f32::INFINITY));
        if search.changed() {
            // keep selection visible
            self.scroll_to_selected = true;
        }
        ui.separator();
        if cx.settings.navigator.show_properties && !cx.conns.configs().is_empty() {
            egui::Panel::bottom("nav_properties").resizable(true).default_size(170.0).size_range(60.0..=600.0).show(ui, |ui| self.properties_ui(ui, cx));
        }

        self.request_child_loads(cx);
        self.build(cx);
        self.finish_reveal();
        if self.pending_reveal.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }

        if cx.conns.configs().is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(20.0);
                ui.label(RichText::new(icon::DATABASE).size(32.0).weak());
                ui.label(tr("No connections yet."));
                if ui.button(tr("Create a connection")).clicked() {
                    cx.push(AppCommand::NewConnection { kind: None, folder: None });
                }
                if ui.button(tr("Import from DBeaver")).clicked() {
                    cx.push(AppCommand::ImportConnections);
                }
            });
            return;
        }

        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        let rows = std::mem::take(&mut self.rows);
        let mut clicked: Option<(usize, bool)> = None; // (row, double)
        let mut toggled: Option<usize> = None;
        let mut context: Option<(usize, egui::Response)> = None;
        let mut scroll_target = None;
        if self.scroll_to_selected {
            scroll_target = rows.iter().position(|r| Some(&r.node) == self.selected.as_ref());
            self.scroll_to_selected = false;
        }
        let mut sa = egui::ScrollArea::both().auto_shrink([false, false]).id_salt("nav_scroll");
        if let Some(i) = scroll_target {
            sa = sa.vertical_scroll_offset((i as f32 * row_h - 80.0).max(0.0));
        }
        let dark = ui.visuals().dark_mode;
        sa.show_rows(ui, row_h, rows.len(), |ui, range| {
            for i in range {
                let r = &rows[i];
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width().max(200.0), row_h), Sense::click());
                let selected = r.kind == RowKind::Node && Some(&r.node) == self.selected.as_ref();
                resp.widget_info(|| match &r.kind {
                    RowKind::Node => egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &r.label),
                    RowKind::Loading => egui::WidgetInfo::labeled(egui::WidgetType::Label, true, tr("Loading…")),
                    RowKind::Error(e) => egui::WidgetInfo::labeled(egui::WidgetType::Label, true, e),
                    RowKind::Empty => egui::WidgetInfo::labeled(egui::WidgetType::Label, true, tr("(empty)")),
                });
                if selected {
                    ui.painter().rect_filled(rect, 3.0, ui.visuals().selection.bg_fill);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
                }
                let mut x = rect.left() + 4.0 + r.depth as f32 * 14.0;
                let y = rect.center().y;
                let text_color = if selected { ui.visuals().selection.stroke.color } else { ui.visuals().text_color() };
                let font = egui::TextStyle::Body.resolve(ui.style());
                if r.expandable {
                    let arrow = if r.expanded { icon::CARET_DOWN } else { icon::CARET_RIGHT };
                    let arrow_rect = egui::Rect::from_center_size(egui::pos2(x + 6.0, y), egui::vec2(14.0, row_h));
                    ui.painter().text(arrow_rect.center(), egui::Align2::CENTER_CENTER, arrow, font.clone(), ui.visuals().weak_text_color());
                    if resp.clicked() && resp.interact_pointer_pos().is_some_and(|p| arrow_rect.contains(p)) {
                        toggled = Some(i);
                    }
                }
                x += 16.0;
                match &r.kind {
                    RowKind::Loading => {
                        ui.painter().text(
                            egui::pos2(x, y),
                            egui::Align2::LEFT_CENTER,
                            format!("{} {}", icon::HOURGLASS, tr("Loading…")),
                            font.clone(),
                            ui.visuals().weak_text_color(),
                        );
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                        continue;
                    }
                    RowKind::Error(e) => {
                        let resp = resp.on_hover_text(e.clone());
                        ui.painter().text(
                            egui::pos2(x, y),
                            egui::Align2::LEFT_CENTER,
                            format!("{} {}", icon::WARNING, e.lines().next().unwrap_or("")),
                            font.clone(),
                            crate::theme::error_color(dark),
                        );
                        let _ = resp;
                        continue;
                    }
                    RowKind::Empty => {
                        ui.painter().text(egui::pos2(x, y), egui::Align2::LEFT_CENTER, tr("(empty)"), font.clone(), ui.visuals().weak_text_color());
                        continue;
                    }
                    RowKind::Node => {}
                }
                let icol = r.icon_color.or_else(|| dbiji_icons::color(r.icon)).unwrap_or_else(|| ui.visuals().weak_text_color());
                let icon_rect = ui.painter().text(egui::pos2(x, y), egui::Align2::LEFT_CENTER, r.icon, dbiji_icons::fill_font(ui.ctx(), font.size), icol);
                if let Some(badge) = r.badge {
                    let at = icon_rect.right_bottom() + egui::vec2(-1.0, -2.0);
                    let bg = if selected { ui.visuals().selection.bg_fill } else { ui.visuals().panel_fill };
                    ui.painter().circle(at, 3.5, badge, egui::Stroke::new(1.5, bg));
                }
                x += 20.0;
                let galley = ui.painter().layout_no_wrap(r.label.clone(), font.clone(), text_color);
                let w = galley.size().x;
                ui.painter().galley(egui::pos2(x, y - galley.size().y / 2.0), galley, text_color);
                if let Some(d) = &r.detail {
                    let small = egui::TextStyle::Small.resolve(ui.style());
                    ui.painter().text(egui::pos2(x + w + 8.0, y), egui::Align2::LEFT_CENTER, d, small, ui.visuals().weak_text_color());
                }
                if resp.double_clicked() {
                    clicked = Some((i, true));
                } else if resp.clicked() && toggled != Some(i) {
                    clicked = Some((i, false));
                }
                if resp.secondary_clicked() {
                    self.selected = Some(r.node.clone());
                }
                let node = r.node.clone();
                if resp.context_menu_opened() || resp.secondary_clicked() {
                    context = Some((i, resp.clone()));
                }
                resp.context_menu(|ui| self.context_menu(ui, cx, &node));
            }
        });
        let _ = context;
        if let Some(i) = toggled {
            self.toggle(&rows[i].node, cx);
            self.selected = Some(rows[i].node.clone());
        }
        if let Some((i, double)) = clicked {
            let node = rows[i].node.clone();
            self.selected = Some(node.clone());
            if double {
                self.activate(&node, cx);
            }
        }
        self.rows = rows;
        self.keyboard(ui, cx);
    }

    fn toggle(&mut self, node: &NavNode, cx: &mut AppContext) {
        if self.expanded.contains(node) {
            self.expanded.remove(node);
        } else {
            if let NavNode::Connection(id) = node
                && !cx.conns.is_connected(id)
            {
                cx.push(AppCommand::Connect(id.clone()));
            }
            self.expanded.insert(node.clone());
        }
    }

    /// Double click / Enter.
    fn activate(&mut self, node: &NavNode, cx: &mut AppContext) {
        match node {
            NavNode::Object { conn, obj } | NavNode::Child { conn, obj } => {
                let page = if obj.kind.has_data() {
                    ObjectPage::Data
                } else if obj.kind.is_routine() || obj.kind == ObjectKind::Trigger {
                    ObjectPage::Ddl
                } else {
                    ObjectPage::Properties
                };
                cx.push(AppCommand::OpenObject { connection: conn.clone(), object: obj.clone(), page });
            }
            NavNode::CatalogItem { conn, folder, ctx } => {
                cx.push(AppCommand::OpenCatalogObject { connection: conn.clone(), folder, ctx: ctx.clone() });
            }
            NavNode::CatalogFolder { conn, folder, ctx } if cx.conns.live(conn).and_then(|l| l.catalog_folder(folder)).is_some_and(|f| f.table_only) => {
                cx.push(AppCommand::OpenCatalogTable { connection: conn.clone(), folder, ctx: ctx.clone() });
            }
            other => self.toggle(&other.clone(), cx),
        }
    }

    pub fn selected_schema(&self) -> Option<SchemaRef> {
        match self.selected.as_ref()? {
            NavNode::Schema { schema, .. } | NavNode::KindFolder { schema, .. } => Some(schema.clone()),
            NavNode::Object { obj, .. } | NavNode::Child { obj, .. } => Some(obj.schema_ref()),
            NavNode::ChildFolder { parent, .. } => Some(parent.schema_ref()),
            _ => None,
        }
    }

    pub fn refresh_selected(&mut self, cx: &mut AppContext) {
        let Some(node) = self.selected.clone() else { return };
        self.refresh_node(&node, cx.commands);
    }

    /// Ask to reload the metadata below `node`.
    pub fn refresh_node(&self, node: &NavNode, commands: &mut Vec<AppCommand>) {
        let Some(conn) = node.connection().map(str::to_string) else { return };
        let (object, schema) = match &node {
            NavNode::Object { obj, .. } | NavNode::Child { obj, .. } => (Some(obj.clone()), None),
            NavNode::ChildFolder { parent, .. } => (Some(parent.clone()), None),
            NavNode::Schema { schema, .. } | NavNode::KindFolder { schema, .. } => (None, Some(schema.clone())),
            _ => (None, None),
        };
        commands.push(AppCommand::RefreshMetadata { connection: conn, object, schema });
    }

    fn keyboard(&mut self, ui: &mut Ui, cx: &mut AppContext) {
        if !ui.ui_contains_pointer() && !ui.memory(|m| m.focused().is_none()) {
            return;
        }
        let nodes: Vec<usize> = self.rows.iter().enumerate().filter(|(_, r)| r.kind == RowKind::Node).map(|(i, _)| i).collect();
        let cur = self.selected.as_ref().and_then(|s| self.rows.iter().position(|r| &r.node == s));
        let (down, up, left, right, enter, f5) = ui.input(|i| {
            (
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::F5),
            )
        });
        if ui.memory(|m| m.focused().is_some()) {
            return; // a text field (filter) has focus
        }
        if down || up {
            let pos = cur.and_then(|c| nodes.iter().position(|&n| n == c));
            let next = match (pos, down) {
                (None, _) => nodes.first().copied(),
                (Some(p), true) => nodes.get(p + 1).copied().or(Some(nodes[p])),
                (Some(p), false) => nodes.get(p.saturating_sub(1)).copied(),
            };
            if let Some(n) = next {
                self.select(self.rows[n].node.clone());
            }
        }
        if let Some(c) = cur {
            let node = self.rows[c].node.clone();
            if right && self.rows[c].expandable && !self.rows[c].expanded {
                self.toggle(&node, cx);
            }
            if left && self.rows[c].expanded {
                self.expanded.remove(&node);
            }
            if enter {
                self.activate(&node, cx);
            }
            if f5 {
                self.refresh_selected(cx);
            }
        }
    }

    fn context_menu(&mut self, ui: &mut Ui, cx: &mut AppContext, node: &NavNode) {
        let conn = node.connection().map(str::to_string);
        match node {
            NavNode::Folder(f) => {
                if ui.button((dbiji_icons::ic(icon::PLUS), tr("New connection here"))).clicked() {
                    cx.push(AppCommand::NewConnection { kind: None, folder: Some(f.clone()) });
                    ui.close();
                }
            }
            NavNode::Connection(id) => {
                let connected = cx.conns.is_connected(id);
                if connected {
                    if ui.button((dbiji_icons::ic(icon::PLUGS), tr("Disconnect"))).clicked() {
                        cx.push(AppCommand::Disconnect(id.clone()));
                        ui.close();
                    }
                    if ui.button((dbiji_icons::ic(icon::ARROWS_CLOCKWISE), tr("Reconnect"))).clicked() {
                        cx.push(AppCommand::Reconnect(id.clone()));
                        ui.close();
                    }
                } else if ui.button((dbiji_icons::ic(icon::PLUGS_CONNECTED), tr("Connect"))).clicked() {
                    cx.push(AppCommand::Connect(id.clone()));
                    ui.close();
                }
                ui.separator();
                if ui.button((dbiji_icons::ic(icon::FILE_SQL), tr("New SQL editor"))).clicked() {
                    cx.push(AppCommand::NewSqlEditor { connection: Some(id.clone()), schema: None, text: None });
                    ui.close();
                }
                if ui.button((dbiji_icons::ic(icon::TERMINAL), tr("Open SQL console"))).clicked() {
                    cx.push(AppCommand::NewSqlConsole { connection: id.clone() });
                    ui.close();
                }
                create_menu_items(ui, cx, id, None);
                ui.separator();
                if ui.button((dbiji_icons::ic(icon::PENCIL), tr("Edit connection"))).clicked() {
                    cx.push(AppCommand::EditConnection(id.clone()));
                    ui.close();
                }
                if ui.button((dbiji_icons::ic(icon::COPY), tr("Duplicate"))).clicked() {
                    cx.push(AppCommand::DuplicateConnection(id.clone()));
                    ui.close();
                }
                if ui.button((dbiji_icons::ic(icon::TRASH), tr("Delete"))).clicked() {
                    cx.push(AppCommand::DeleteConnection(id.clone()));
                    ui.close();
                }
                let others: Vec<String> = cx.conns.configs().iter().map(|c| c.id.clone()).filter(|c| c != id && cx.conns.is_connected(c)).collect();
                if !others.is_empty() && ui.button((dbiji_icons::ic(icon::PLUGS), tr("Disconnect others"))).clicked() {
                    for o in others {
                        cx.push(AppCommand::Disconnect(o));
                    }
                    ui.close();
                }
                ui.menu_button((dbiji_icons::ic(icon::ARROWS_DOWN_UP), tr("Move")), |ui| {
                    use crate::command::MovePos as M;
                    for (pos, label, i) in [
                        (M::Top, tr("To the top"), icon::ARROW_LINE_UP),
                        (M::Up, tr("Up"), icon::ARROW_UP),
                        (M::Down, tr("Down"), icon::ARROW_DOWN),
                        (M::Bottom, tr("To the bottom"), icon::ARROW_LINE_DOWN),
                    ] {
                        if ui.button(format!("{i} {label}")).clicked() {
                            cx.push(AppCommand::MoveConnection { id: id.clone(), to: pos });
                            ui.close();
                        }
                    }
                });
                if connected {
                    ui.separator();
                    ui.menu_button((dbiji_icons::ic(icon::WRENCH), tr("Tools")), |ui| {
                        tools_menu(ui, cx, id, None);
                    });
                }
            }
            NavNode::Database { conn, db } => {
                let schema = SchemaRef { database: Some(db.clone()), schema: None };
                if ui.button((dbiji_icons::ic(icon::FILE_SQL), tr("New SQL editor"))).clicked() {
                    cx.push(AppCommand::NewSqlEditor { connection: Some(conn.clone()), schema: Some(schema), text: None });
                    ui.close();
                }
                create_menu_items(ui, cx, conn, Some(db.clone()));
                ui.menu_button((dbiji_icons::ic(icon::WRENCH), tr("Tools")), |ui| tools_menu(ui, cx, conn, None));
            }
            NavNode::Schema { conn, schema } | NavNode::KindFolder { conn, schema, .. } => {
                if ui.button((dbiji_icons::ic(icon::FILE_SQL), tr("New SQL editor"))).clicked() {
                    cx.push(AppCommand::NewSqlEditor { connection: Some(conn.clone()), schema: Some(schema.clone()), text: None });
                    ui.close();
                }
                // Another schema in the same database.
                if matches!(node, NavNode::Schema { .. })
                    && let Some(live) = cx.conns.live(conn)
                    && live.dialect.create_schema_sql("x", None).is_some()
                {
                    let read_only = live.config.read_only;
                    let database = schema.database.clone().or_else(|| live.current_database.clone());
                    let b =
                        ui.add_enabled(!read_only, egui::Button::new((dbiji_icons::ic(icon::STACK), tr("New schema…")))).on_disabled_hover_text(tr("The connection is read-only."));
                    if b.clicked() {
                        cx.push(AppCommand::NewSchema { connection: conn.clone(), database });
                        ui.close();
                    }
                }
                if ui.button((dbiji_icons::ic(icon::TABLE), tr("Create table"))).clicked() {
                    cx.push(AppCommand::CreateTable { connection: conn.clone(), schema: schema.clone() });
                    ui.close();
                }
                if ui.button((dbiji_icons::ic(icon::GRAPH), tr("ER diagram"))).clicked() {
                    cx.push(AppCommand::OpenErDiagram { connection: conn.clone(), schema: schema.clone(), tables: Vec::new() });
                    ui.close();
                }
                if ui.button((dbiji_icons::ic(icon::DOWNLOAD), tr("Import data"))).clicked() {
                    cx.push(AppCommand::OpenImport { connection: conn.clone(), table: None, schema: Some(schema.clone()) });
                    ui.close();
                }
                if ui.button((dbiji_icons::ic(icon::GIT_DIFF), tr("Compare schema"))).clicked() {
                    cx.push(AppCommand::OpenSchemaCompare { connection: Some(conn.clone()), schema: Some(schema.clone()) });
                    ui.close();
                }
                ui.menu_button((dbiji_icons::ic(icon::WRENCH), tr("Tools")), |ui| tools_menu(ui, cx, conn, Some(schema.clone())));
                if let NavNode::KindFolder { kind, .. } = node
                    && cx.conns.config(conn).is_some_and(|c| name_filter(&c.filters, *kind).is_active())
                    && ui.button((dbiji_icons::ic(icon::FUNNEL_X), tr("Clear navigator filter"))).on_hover_text(tr("Show the hidden objects of this type again")).clicked()
                {
                    cx.push(AppCommand::ClearNavigatorFilter { connection: conn.clone(), kind: *kind });
                    ui.close();
                }
                ui.separator();
                if ui.button((dbiji_icons::ic(icon::ARROW_CLOCKWISE), tr("Refresh"))).clicked() {
                    cx.push(AppCommand::RefreshMetadata { connection: conn.clone(), object: None, schema: Some(schema.clone()) });
                    ui.close();
                }
            }
            NavNode::Object { conn, obj } | NavNode::Child { conn, obj } => {
                object_menu(ui, cx, conn, obj);
            }
            NavNode::ChildFolder { conn, parent, .. } => {
                if ui.button((dbiji_icons::ic(icon::ARROW_CLOCKWISE), tr("Refresh"))).clicked() {
                    cx.push(AppCommand::RefreshMetadata { connection: conn.clone(), object: Some(parent.clone()), schema: None });
                    ui.close();
                }
            }
            NavNode::Extensions { .. } => {
                if let Some(c) = conn
                    && ui.button((dbiji_icons::ic(icon::ARROW_CLOCKWISE), tr("Refresh"))).clicked()
                {
                    cx.push(AppCommand::RefreshMetadata { connection: c, object: None, schema: None });
                    ui.close();
                }
            }
            NavNode::CatalogGroup { conn, .. } => {
                if ui.button((dbiji_icons::ic(icon::ARROW_CLOCKWISE), tr("Refresh"))).clicked() {
                    cx.push(AppCommand::RefreshCatalog { connection: conn.clone(), folder: None });
                    ui.close();
                }
            }
            NavNode::CatalogFolder { conn, folder, ctx } => {
                if ui.button((dbiji_icons::ic(icon::TABLE), tr("View as table"))).clicked() {
                    cx.push(AppCommand::OpenCatalogTable { connection: conn.clone(), folder, ctx: ctx.clone() });
                    ui.close();
                }
                if ui.button((dbiji_icons::ic(icon::ARROW_CLOCKWISE), tr("Refresh"))).clicked() {
                    cx.push(AppCommand::RefreshCatalog { connection: conn.clone(), folder: Some(folder) });
                    ui.close();
                }
            }
            NavNode::CatalogItem { conn, folder, ctx } => {
                if ui.button((dbiji_icons::ic(icon::ARROW_SQUARE_OUT), tr("Open"))).clicked() {
                    cx.push(AppCommand::OpenCatalogObject { connection: conn.clone(), folder, ctx: ctx.clone() });
                    ui.close();
                }
                if ui.button((dbiji_icons::ic(icon::COPY), tr("Copy name"))).clicked() {
                    ui.ctx().copy_text(ctx.name.clone().unwrap_or_default());
                    ui.close();
                }
                if ui.button((dbiji_icons::ic(icon::ARROW_CLOCKWISE), tr("Refresh"))).clicked() {
                    cx.push(AppCommand::RefreshCatalog { connection: conn.clone(), folder: Some(folder) });
                    ui.close();
                }
            }
        }
    }
}

/// `CREATE INDEX` / unique constraint DDL for one column (edit before running).
pub fn column_constraint_sql(d: &dyn dbiji_core::Dialect, column: &ObjectRef, unique: bool) -> String {
    let table = column.table.clone().unwrap_or_default();
    let qualified = d.qualified(column.schema.as_deref(), &table);
    let col = d.quote_ident(&column.name);
    let base = format!("{table}_{}", column.name).to_lowercase().replace(|c: char| !c.is_ascii_alphanumeric() && c != '_', "_");
    if unique && d.kind() != dbiji_core::DbKind::Sqlite {
        let name = d.quote_ident(&format!("uq_{base}"));
        format!("ALTER TABLE {qualified} ADD CONSTRAINT {name} UNIQUE ({col});")
    } else {
        let name = d.quote_ident(&format!("{}_{base}", if unique { "uq" } else { "idx" }));
        let schema_prefix = match (d.kind(), column.schema.as_deref()) {
            // SQLite and PostgreSQL put the index in the table's schema.
            (dbiji_core::DbKind::Sqlite, Some(s)) => format!("{}.", d.quote_ident(s)),
            _ => String::new(),
        };
        let table_ref = if d.kind() == dbiji_core::DbKind::Sqlite { d.quote_ident(&table) } else { qualified };
        format!("CREATE {}INDEX {schema_prefix}{name} ON {table_ref} ({col});", if unique { "UNIQUE " } else { "" })
    }
}

/// Icon of a catalog folder and its objects.
pub fn catalog_icon(i: CatalogIcon) -> &'static str {
    match i {
        CatalogIcon::User => icon::USER,
        CatalogIcon::Users => icon::USERS,
        CatalogIcon::Storage => icon::HARD_DRIVES,
        CatalogIcon::Settings => icon::SLIDERS,
        CatalogIcon::Plug => icon::PLUG,
        CatalogIcon::Server => icon::HARD_DRIVE,
        CatalogIcon::Trigger => icon::LIGHTNING,
        CatalogIcon::Language => icon::CODE,
        CatalogIcon::Text => icon::TEXT_AA,
        CatalogIcon::Shield => icon::SHIELD_CHECK,
        CatalogIcon::Rule => icon::ARROW_BEND_DOWN_RIGHT,
        CatalogIcon::Gauge => icon::GAUGE,
        CatalogIcon::Engine => icon::ENGINE,
        CatalogIcon::Key => icon::KEY,
        CatalogIcon::Puzzle => icon::PUZZLE_PIECE,
    }
}

fn placeholder(depth: usize, kind: RowKind) -> Row {
    Row { node: NavNode::Folder(String::new()), depth, label: String::new(), detail: None, icon: "", icon_color: None, badge: None, expandable: false, expanded: false, kind }
}

/// "New database…" / "New schema…" of a connected server (what its
/// database type supports; `database`: schemas go into that database).
/// `database`: the database node the menu belongs to, `None` on the
/// connection node.
pub fn create_menu_items(ui: &mut Ui, cx: &mut AppContext, conn: &str, database: Option<String>) {
    let Some(live) = cx.conns.live(conn) else { return };
    let read_only = live.config.read_only;
    let probe = dbiji_core::NewDatabase { name: "x".into(), ..Default::default() };
    let databases = live.dialect.create_database_sql(&probe).is_some();
    let schemas = live.dialect.create_schema_sql("x", None).is_some();
    let multiple_databases = live.driver.info().capabilities.multiple_databases;
    let why = tr("The connection is read-only.");
    if databases {
        let b = ui.add_enabled(!read_only, egui::Button::new((dbiji_icons::ic(icon::DATABASE), tr("New database…")))).on_disabled_hover_text(why);
        if b.clicked() {
            cx.push(AppCommand::NewDatabase { connection: conn.to_string() });
            ui.close();
        }
    }
    // A schema belongs to a database: on servers with several databases it
    // is created from a database (or schema) node, not the connection.
    if schemas && (database.is_some() || !multiple_databases) {
        let b = ui.add_enabled(!read_only, egui::Button::new((dbiji_icons::ic(icon::STACK), tr("New schema…")))).on_disabled_hover_text(why);
        if b.clicked() {
            cx.push(AppCommand::NewSchema { connection: conn.to_string(), database });
            ui.close();
        }
    }
}

/// Administration tools for a connection.
pub fn tools_menu(ui: &mut Ui, cx: &mut AppContext, conn: &str, schema: Option<SchemaRef>) {
    let caps = cx.conns.live(conn).map(|l| l.driver.info().capabilities.clone()).unwrap_or_default();
    if caps.session_manager && ui.button((dbiji_icons::ic(icon::USERS), tr("Session manager"))).clicked() {
        cx.push(AppCommand::OpenSessionManager { connection: conn.to_string() });
        ui.close();
    }
    if caps.lock_manager && ui.button((dbiji_icons::ic(icon::LOCK), tr("Lock manager"))).clicked() {
        cx.push(AppCommand::OpenLockManager { connection: conn.to_string() });
        ui.close();
    }
    if caps.server_metrics && ui.button((dbiji_icons::ic(icon::CHART_LINE), tr("Server dashboard"))).clicked() {
        cx.push(AppCommand::OpenDashboard { connection: conn.to_string() });
        ui.close();
    }
    if caps.backup && ui.button((dbiji_icons::ic(icon::ARCHIVE), tr("Backup"))).clicked() {
        cx.push(AppCommand::OpenBackup { connection: conn.to_string(), schema: schema.clone(), restore: false });
        ui.close();
    }
    if caps.restore && ui.button((dbiji_icons::ic(icon::ARROW_COUNTER_CLOCKWISE), tr("Restore"))).clicked() {
        cx.push(AppCommand::OpenBackup { connection: conn.to_string(), schema: schema.clone(), restore: true });
        ui.close();
    }
    if ui.button((dbiji_icons::ic(icon::BLUEPRINT), tr("Query builder"))).clicked() {
        cx.push(AppCommand::OpenQueryBuilder { connection: conn.to_string(), schema: schema.clone() });
        ui.close();
    }
    if ui.button((dbiji_icons::ic(icon::MAGNIFYING_GLASS), tr("Search metadata"))).clicked() {
        cx.push(AppCommand::OpenSearch { connection: Some(conn.to_string()), data: false });
        ui.close();
    }
    if ui.button((dbiji_icons::ic(icon::TEXT_AA), tr("Search data"))).clicked() {
        cx.push(AppCommand::OpenSearch { connection: Some(conn.to_string()), data: true });
        ui.close();
    }
    if cx.conns.live(conn).is_some_and(|l| l.config.kind == DbKind::Postgres) && ui.button((dbiji_icons::ic(icon::PLUGS_CONNECTED), tr("Foreign data wrapper…"))).clicked() {
        cx.push(AppCommand::OpenFdwConfigurator { connection: conn.to_string() });
        ui.close();
    }
    let can_change_password = cx.conns.live(conn).is_some_and(|l| l.dialect.change_password_sql(None, "x").is_some());
    if can_change_password && ui.button((dbiji_icons::ic(icon::PASSWORD), tr("Change password"))).clicked() {
        cx.push(AppCommand::ChangePassword { connection: conn.to_string() });
        ui.close();
    }
}

/// Context menu for a database object (navigator and object editors).
pub fn object_menu(ui: &mut Ui, cx: &mut AppContext, conn: &str, obj: &ObjectRef) {
    let c = conn.to_string();
    if ui.button((dbiji_icons::ic(icon::ARROW_SQUARE_OUT), tr("Open"))).clicked() {
        cx.push(AppCommand::OpenObject { connection: c.clone(), object: obj.clone(), page: ObjectPage::Properties });
        ui.close();
    }
    if obj.kind.has_data() && ui.button((dbiji_icons::ic(icon::TABLE), tr("View data"))).clicked() {
        cx.push(AppCommand::OpenObject { connection: c.clone(), object: obj.clone(), page: ObjectPage::Data });
        ui.close();
    }
    if ui.button((dbiji_icons::ic(icon::CODE), tr("View DDL"))).clicked() {
        cx.push(AppCommand::OpenObject { connection: c.clone(), object: obj.clone(), page: ObjectPage::Ddl });
        ui.close();
    }
    if obj.kind.is_table_like() {
        if ui.button((dbiji_icons::ic(icon::PENCIL), tr("Edit table"))).clicked() {
            cx.push(AppCommand::EditTable { connection: c.clone(), table: obj.clone() });
            ui.close();
        }
        if ui.button((dbiji_icons::ic(icon::GRAPH), tr("ER diagram"))).clicked() {
            cx.push(AppCommand::OpenErDiagram { connection: c.clone(), schema: obj.schema_ref(), tables: vec![obj.name.clone()] });
            ui.close();
        }
    }
    ui.menu_button((dbiji_icons::ic(icon::FILE_SQL), tr("Generate SQL")), |ui| {
        for (label, kind) in [("SELECT", "select"), ("INSERT", "insert"), ("UPDATE", "update"), ("DELETE", "delete"), ("UPSERT", "upsert"), ("DDL", "ddl")] {
            if (obj.kind.is_table_like() || kind == "ddl" || kind == "select") && ui.button(label).clicked() {
                cx.push(AppCommand::GenerateSql { connection: c.clone(), object: obj.clone(), kind: kind.to_string() });
                ui.close();
            }
        }
    });
    if obj.kind.has_data() {
        ui.separator();
        if ui.button((dbiji_icons::ic(icon::EXPORT), tr("Export data"))).clicked() {
            cx.push(AppCommand::OpenExport { connection: c.clone(), objects: vec![obj.clone()], sql: None });
            ui.close();
        }
        if obj.kind.is_table_like() {
            if ui.button((dbiji_icons::ic(icon::DOWNLOAD), tr("Import data"))).clicked() {
                cx.push(AppCommand::OpenImport { connection: c.clone(), table: Some(obj.clone()), schema: Some(obj.schema_ref()) });
                ui.close();
            }
            if ui.button((dbiji_icons::ic(icon::SHUFFLE), tr("Generate mock data"))).clicked() {
                cx.push(AppCommand::OpenMockData { connection: c.clone(), table: obj.clone() });
                ui.close();
            }
            if ui.button((dbiji_icons::ic(icon::ARROWS_LEFT_RIGHT), tr("Migrate to another database"))).clicked() {
                cx.push(AppCommand::OpenMigration { connection: c.clone(), objects: vec![obj.clone()] });
                ui.close();
            }
            if ui.button((dbiji_icons::ic(icon::GIT_DIFF), tr("Compare data"))).clicked() {
                cx.push(AppCommand::OpenDataCompare { connection: Some(c.clone()), table: Some(obj.clone()) });
                ui.close();
            }
            if ui.button((dbiji_icons::ic(icon::BROOM), tr("Maintenance"))).clicked() {
                cx.push(AppCommand::OpenMaintenance { connection: c.clone(), objects: vec![obj.clone()] });
                ui.close();
            }
        }
    }
    if obj.kind.is_routine() && ui.button((dbiji_icons::ic(icon::PLAY), tr("Execute…"))).on_hover_text(tr("Open a script that calls it with parameters")).clicked() {
        cx.push(AppCommand::ExecuteRoutine { connection: c.clone(), routine: obj.clone() });
        ui.close();
    }
    if obj.kind.is_routine() && ui.button((dbiji_icons::ic(icon::BUG), tr("Debug"))).clicked() {
        cx.push(AppCommand::OpenDebugger { connection: c.clone(), routine: obj.clone() });
        ui.close();
    }
    ui.separator();
    if obj.kind == ObjectKind::Column && obj.table.is_some() {
        for (label, unique) in [(tr("Create index…"), false), (tr("Add unique constraint…"), true)] {
            if ui.button(format!("{} {label}", if unique { icon::KEY } else { icon::LIST_NUMBERS })).clicked() {
                if let Some(live) = cx.conns.live(conn) {
                    let text = column_constraint_sql(live.dialect.as_ref(), obj, unique);
                    cx.push(AppCommand::NewSqlEditor { connection: Some(conn.to_string()), schema: Some(obj.schema_ref()), text: Some(text) });
                }
                ui.close();
            }
        }
    }
    if obj.kind == ObjectKind::Trigger
        && cx.conns.live(conn).is_some_and(|l| l.dialect.maintenance_ops().iter().any(|o| o.applies_to.contains(&ObjectKind::Trigger)))
        && ui.button((dbiji_icons::ic(icon::BROOM), tr("Maintenance"))).on_hover_text(tr("Enable or disable the trigger")).clicked()
    {
        cx.push(AppCommand::OpenMaintenance { connection: conn.to_string(), objects: vec![obj.clone()] });
        ui.close();
    }
    if obj.table.is_none()
        && ui.button((dbiji_icons::ic(icon::EYE_SLASH), tr("Hide in navigator"))).on_hover_text(tr("Adds the name to the connection's navigator filter")).clicked()
    {
        cx.push(AppCommand::HideInNavigator { connection: conn.to_string(), object: obj.clone() });
        ui.close();
    }
    if ui.button((dbiji_icons::ic(icon::BOOKMARK_SIMPLE), tr("Add bookmark"))).clicked() {
        cx.push(AppCommand::AddBookmark { connection: c.clone(), object: obj.clone() });
        ui.close();
    }
    if ui.button((dbiji_icons::ic(icon::COPY), tr("Copy name"))).clicked() {
        let name = cx.conns.live(conn).map(|l| l.dialect.qualified_ref(obj)).unwrap_or_else(|| obj.display_path());
        ui.ctx().copy_text(name);
        ui.close();
    }
    if ui.button((dbiji_icons::ic(icon::ARROW_CLOCKWISE), tr("Refresh"))).clicked() {
        cx.push(AppCommand::RefreshMetadata { connection: c.clone(), object: Some(obj.clone()), schema: None });
        ui.close();
    }
    ui.separator();
    if ui.button(RichText::new(format!("{} {}", icon::TRASH, tr("Drop…"))).color(crate::theme::error_color(ui.visuals().dark_mode))).clicked() {
        cx.push(AppCommand::DropObjects { connection: c, objects: vec![obj.clone()] });
        ui.close();
    }
}
