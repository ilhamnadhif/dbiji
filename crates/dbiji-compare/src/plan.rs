//! Migration planning: the ordered operations (and their SQL) that make the
//! TARGET schema identical to the SOURCE schema.
//!
//! The plan is made of [`MigrationStep`]s. Each step carries a logical
//! [`MigrationOp`] (used by [`crate::apply_plan`] to simulate the migration
//! and by the Liquibase generator) and the SQL produced for it by the target
//! dialect's [`DdlGenerator`]. Steps are grouped in [`Phase`]s that run in a
//! dependency-safe order:
//!
//! 1. drop foreign keys that are removed, changed or block other changes,
//! 2. drop changed / removed triggers,
//! 3. drop changed / removed views and views depending on changed objects,
//! 4. drop routines that cannot be replaced in place,
//! 5. drop changed / removed indexes and 6. constraints of modified tables,
//! 7. drop removed tables (referencing tables first),
//! 8. recreate changed user types, 9. create new types,
//! 10. create / alter sequences,
//! 11. create routines needed by tables or views,
//! 12. create new tables (without foreign keys and triggers),
//! 13. alter modified tables with [`DdlGenerator::alter_table`],
//! 14. drop removed sequences, types and routines,
//! 15. add constraints, 16. create indexes, 17. add foreign keys,
//! 18. create / replace views in dependency order,
//! 19. create the remaining routines, 20. create triggers.

use crate::analysis::{Analysis, Ctx, Side, SubMatch, TablePair, is_secondary_index, map_col, pk_vec, snapshot_schema};
use crate::model::{DiffStatus, SchemaDiff};
use crate::normalize::{SqlNorm, normalize_sql, references, rename_schema};
use crate::util::topo_sort;
use dbiji_core::ddl::*;
use dbiji_core::{DbKind, Dialect, ObjectKind, trf};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// How modified tables are altered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlterMode {
    /// Indexes, constraints, foreign keys and triggers are dropped / created
    /// with their own statements around [`DdlGenerator::alter_table`], which
    /// only sees column and table level changes. Best for PostgreSQL, MySQL,
    /// SQL Server.
    #[default]
    Granular,
    /// The full desired definition is passed to `alter_table` (and foreign keys
    /// are created inline), for dialects that rebuild tables to alter them
    /// (SQLite). Only triggers are handled separately.
    WholeTable,
}

/// Options for [`crate::migration_script`] / [`migration_plan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScriptOptions {
    /// Drop objects (tables, columns, constraints, views ...) that exist only
    /// in the target. Without it the migration is additive and target-only
    /// objects are kept.
    pub drop_removed: bool,
    /// Use `CASCADE` when dropping tables.
    pub cascade: bool,
    /// Prefix statements with `--` comments describing each step.
    pub include_comments: bool,
    /// Wrap the script in a transaction (where the dialect supports it).
    pub wrap_in_transaction: bool,
    /// Rewrite schema qualifiers inside definitions (views, routines, triggers,
    /// defaults, checks). Defaults to (source schema, target schema) when both
    /// snapshots name different schemas.
    pub schema_rename: Option<(String, String)>,
    /// How modified tables are altered.
    pub alter_mode: AlterMode,
}

impl Default for ScriptOptions {
    fn default() -> Self {
        ScriptOptions { drop_removed: false, cascade: false, include_comments: true, wrap_in_transaction: false, schema_rename: None, alter_mode: AlterMode::Granular }
    }
}

impl ScriptOptions {
    /// Options making the target fully identical to the source (target-only
    /// objects are dropped). This is what the "schemas become identical" gate uses.
    pub fn full_sync() -> Self {
        ScriptOptions { drop_removed: true, ..Default::default() }
    }
}

/// Group of steps executed together, in declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Foreign keys that are removed, changed or block other changes.
    DropForeignKeys,
    /// Removed / changed triggers and triggers using recreated routines.
    DropTriggers,
    /// Removed / changed views and views depending on changed objects.
    DropViews,
    /// Routines that must be dropped before being recreated.
    DropRoutines,
    /// Removed / changed indexes of modified tables.
    DropIndexes,
    /// Removed / changed checks, unique and primary keys of modified tables.
    DropConstraints,
    /// Removed tables.
    DropTables,
    /// Changed user types (drop and create).
    RecreateTypes,
    /// New user types.
    CreateTypes,
    /// New and changed sequences.
    Sequences,
    /// Routines used by new or changed tables and views.
    EarlyRoutines,
    /// New tables (foreign keys and triggers come later).
    CreateTables,
    /// Column and table level changes.
    AlterTables,
    /// Removed sequences, types and routines.
    DropUnused,
    /// New / changed primary keys, unique keys and checks.
    AddConstraints,
    /// New / changed indexes.
    CreateIndexes,
    /// New, changed and recreated foreign keys.
    AddForeignKeys,
    /// New, changed and rebuilt views.
    CreateViews,
    /// The other new and changed routines.
    Routines,
    /// New, changed and recreated triggers.
    CreateTriggers,
}

impl Phase {
    /// English label (used in script comments, routed through i18n).
    pub fn label(self) -> &'static str {
        match self {
            Phase::DropForeignKeys => "Drop foreign keys",
            Phase::DropTriggers => "Drop triggers",
            Phase::DropViews => "Drop views",
            Phase::DropRoutines => "Drop routines",
            Phase::DropIndexes => "Drop indexes",
            Phase::DropConstraints => "Drop constraints",
            Phase::DropTables => "Drop tables",
            Phase::RecreateTypes => "Recreate types",
            Phase::CreateTypes => "Create types",
            Phase::Sequences => "Sequences",
            Phase::EarlyRoutines => "Routines used by tables and views",
            Phase::CreateTables => "Create tables",
            Phase::AlterTables => "Alter tables",
            Phase::DropUnused => "Drop removed sequences, types and routines",
            Phase::AddConstraints => "Add constraints",
            Phase::CreateIndexes => "Create indexes",
            Phase::AddForeignKeys => "Add foreign keys",
            Phase::CreateViews => "Create views",
            Phase::Routines => "Routines",
            Phase::CreateTriggers => "Create triggers",
        }
    }
}

/// Logical effect of a migration step. Table names are target names; all
/// objects live in the target schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // one value per step; boxing would only complicate matching
pub enum MigrationOp {
    /// Drop a foreign key of `table`.
    DropForeignKey {
        /// Owning table.
        table: String,
        /// The foreign key as it exists in the target.
        fk: ForeignKeyDef,
    },
    /// Drop a trigger of `table`.
    DropTrigger {
        /// Owning table.
        table: String,
        /// The trigger.
        trigger: TriggerDef,
    },
    /// Drop a view.
    DropView {
        /// The view as it exists in the target.
        view: ViewDef,
    },
    /// Drop a routine.
    DropRoutine {
        /// The routine as it exists in the target.
        routine: RoutineDef,
    },
    /// Drop a secondary index of `table`.
    DropIndex {
        /// Owning table.
        table: String,
        /// The index.
        index: IndexDef,
    },
    /// Drop the primary key of `table`.
    DropPrimaryKey {
        /// Owning table.
        table: String,
        /// The key.
        key: KeyDef,
    },
    /// Drop a unique key of `table`.
    DropUniqueKey {
        /// Owning table.
        table: String,
        /// The key.
        key: KeyDef,
    },
    /// Drop a check constraint of `table`.
    DropCheck {
        /// Owning table.
        table: String,
        /// The constraint.
        check: CheckDef,
    },
    /// Drop a table.
    DropTable {
        /// The table as it exists in the target.
        table: TableDef,
        /// `DROP TABLE ... CASCADE`.
        cascade: bool,
    },
    /// Drop a sequence.
    DropSequence {
        /// The sequence as it exists in the target.
        sequence: SequenceDef,
    },
    /// Drop a user type.
    DropType {
        /// The type as it exists in the target.
        ty: TypeDef,
    },
    /// Create a user type.
    CreateType {
        /// The type to create.
        ty: TypeDef,
    },
    /// Create a sequence.
    CreateSequence {
        /// The sequence to create.
        sequence: SequenceDef,
    },
    /// Alter a sequence.
    AlterSequence {
        /// Current definition.
        from: SequenceDef,
        /// Desired definition.
        to: SequenceDef,
    },
    /// Create (or replace) a routine.
    CreateRoutine {
        /// The routine to create.
        routine: RoutineDef,
        /// The routine exists and is replaced in place.
        replace: bool,
    },
    /// Create a table.
    CreateTable {
        /// The table, without triggers; with foreign keys only when `inline_foreign_keys`.
        table: TableDef,
        /// Foreign keys are part of `CREATE TABLE` ([`AlterMode::WholeTable`]).
        inline_foreign_keys: bool,
    },
    /// Column and table level changes of a table.
    AlterTable {
        /// Definition before the step.
        from: TableDef,
        /// Definition after the step.
        to: TableDef,
        /// `(old name, new name)` of renamed columns.
        renamed_columns: Vec<(String, String)>,
    },
    /// Add a primary key to `table`.
    AddPrimaryKey {
        /// Owning table.
        table: String,
        /// The key.
        key: KeyDef,
    },
    /// Add a unique key to `table`.
    AddUniqueKey {
        /// Owning table.
        table: String,
        /// The key.
        key: KeyDef,
    },
    /// Add a check constraint to `table`.
    AddCheck {
        /// Owning table.
        table: String,
        /// The constraint.
        check: CheckDef,
    },
    /// Create an index on `table`.
    CreateIndex {
        /// Owning table.
        table: String,
        /// The index.
        index: IndexDef,
    },
    /// Add a foreign key to `table`.
    AddForeignKey {
        /// Owning table.
        table: String,
        /// The foreign key.
        fk: ForeignKeyDef,
    },
    /// Create (or replace) a view.
    CreateView {
        /// The view.
        view: ViewDef,
        /// `CREATE OR REPLACE` of an existing view.
        replace: bool,
    },
    /// Create a trigger on `table`.
    CreateTrigger {
        /// Owning table.
        table: String,
        /// The trigger.
        trigger: TriggerDef,
    },
}

/// One step of a migration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationStep {
    /// Group the step belongs to.
    pub phase: Phase,
    /// Qualified name of the object the step works on.
    pub object: String,
    /// Human readable description (used for script comments).
    pub description: String,
    /// Logical effect of the step.
    pub op: MigrationOp,
    /// SQL statements (without trailing `;`) produced by the target dialect.
    pub statements: Vec<String>,
}

/// Ordered migration steps plus warnings about things that could not be
/// scripted automatically.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MigrationPlan {
    /// Steps in execution order.
    pub steps: Vec<MigrationStep>,
    /// Differences that could not be scripted automatically.
    pub warnings: Vec<String>,
    /// Statements opening / closing a transaction, when requested and supported.
    pub transaction: Option<(String, String)>,
}

impl MigrationPlan {
    /// All statements of all steps, in order, without comments or transaction.
    pub fn statements(&self) -> Vec<String> {
        self.steps.iter().flat_map(|s| s.statements.iter().cloned()).collect()
    }

    /// Statements ready to execute: optional comments and transaction wrapping.
    pub fn script(&self, include_comments: bool) -> Vec<String> {
        let mut out = Vec::new();
        if self.steps.is_empty() {
            return out;
        }
        if let Some((begin, _)) = &self.transaction {
            out.push(begin.clone());
        }
        let mut last_phase = None;
        let mut first = true;
        for step in &self.steps {
            for (i, stmt) in step.statements.iter().enumerate() {
                let mut s = String::new();
                if include_comments && i == 0 {
                    if first && !self.warnings.is_empty() {
                        for w in &self.warnings {
                            s.push_str("-- WARNING: ");
                            s.push_str(&w.replace('\n', " "));
                            s.push('\n');
                        }
                    }
                    if last_phase != Some(step.phase) {
                        s.push_str(&format!("-- ==== {} ====\n", dbiji_core::tr(step.phase.label())));
                    }
                    s.push_str("-- ");
                    s.push_str(&step.description.replace('\n', " "));
                    s.push('\n');
                }
                s.push_str(stmt);
                out.push(s);
                first = false;
            }
            last_phase = Some(step.phase);
        }
        if let Some((_, commit)) = &self.transaction {
            out.push(commit.clone());
        }
        out
    }
}

/// Statements opening and committing a transaction in `kind`.
pub(crate) fn transaction_statements(kind: DbKind) -> Option<(&'static str, &'static str)> {
    match kind {
        DbKind::MySql | DbKind::MariaDb => Some(("START TRANSACTION", "COMMIT")),
        DbKind::SqlServer => Some(("BEGIN TRANSACTION", "COMMIT TRANSACTION")),
        DbKind::ClickHouse => None,
        _ => Some(("BEGIN", "COMMIT")),
    }
}

fn is_create_or_replace(stmt: &str) -> bool {
    let n = normalize_sql(stmt, &SqlNorm::default());
    n.starts_with("create or replace") || n.starts_with("create or alter")
}

/// Objects selected for scripting, from the diff items.
#[derive(Default)]
struct Selection {
    tables_mod: HashSet<String>,
    tables_add: HashSet<String>,
    tables_rem: HashSet<String>,
    views_mod: HashSet<String>,
    views_add: HashSet<String>,
    views_rem: HashSet<String>,
    seq_mod: HashSet<String>,
    seq_add: HashSet<String>,
    seq_rem: HashSet<String>,
    types_mod: HashSet<String>,
    types_add: HashSet<String>,
    types_rem: HashSet<String>,
    routines_mod: HashSet<(String, String)>,
    routines_add: HashSet<(String, String)>,
    routines_rem: HashSet<(String, String)>,
}

fn args_key(args: &str) -> String {
    normalize_sql(args, &SqlNorm::default())
}

impl Selection {
    fn from_diff(diff: &SchemaDiff, ctx: &Ctx<'_>) -> Self {
        let mut s = Selection::default();
        for item in &diff.items {
            let src_name = ctx.key(&item.name);
            let tgt_name = ctx.key(item.target_name.as_deref().unwrap_or(&item.name));
            let rkey = (src_name.clone(), args_key(item.signature.as_deref().unwrap_or("")));
            let rkey_t = (tgt_name.clone(), rkey.1.clone());
            let (m, a, r): (&mut HashSet<String>, &mut HashSet<String>, &mut HashSet<String>) = match item.kind {
                ObjectKind::Table | ObjectKind::ForeignTable | ObjectKind::Partition => (&mut s.tables_mod, &mut s.tables_add, &mut s.tables_rem),
                ObjectKind::View | ObjectKind::MaterializedView => (&mut s.views_mod, &mut s.views_add, &mut s.views_rem),
                ObjectKind::Sequence => (&mut s.seq_mod, &mut s.seq_add, &mut s.seq_rem),
                ObjectKind::Type | ObjectKind::Domain => (&mut s.types_mod, &mut s.types_add, &mut s.types_rem),
                ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Aggregate => {
                    match item.status {
                        DiffStatus::Modified => {
                            s.routines_mod.insert(rkey);
                        }
                        DiffStatus::Added => {
                            s.routines_add.insert(rkey);
                        }
                        DiffStatus::Removed => {
                            s.routines_rem.insert(rkey_t);
                        }
                        DiffStatus::Unchanged => {}
                    }
                    continue;
                }
                _ => continue,
            };
            match item.status {
                DiffStatus::Modified => {
                    m.insert(src_name);
                }
                DiffStatus::Added => {
                    a.insert(src_name);
                }
                DiffStatus::Removed => {
                    r.insert(tgt_name);
                }
                DiffStatus::Unchanged => {}
            }
        }
        s
    }
}

/// Planned changes of one modified table.
struct ModTable<'a> {
    target: &'a TableDef,
    desired: TableDef,
    /// Keys of target columns that are dropped.
    dropped_cols: HashSet<String>,
    /// Keys of target columns whose type changes.
    retyped_cols: HashSet<String>,
    /// (old target name, new name)
    renames: Vec<(String, String)>,
    idx_drop: Vec<IndexDef>,
    idx_add: Vec<IndexDef>,
    pk_drop: Option<KeyDef>,
    pk_add: Option<KeyDef>,
    uq_drop: Vec<KeyDef>,
    uq_add: Vec<KeyDef>,
    ck_drop: Vec<CheckDef>,
    ck_add: Vec<CheckDef>,
    fk_drop: Vec<ForeignKeyDef>,
    fk_add: Vec<ForeignKeyDef>,
    trg_drop: Vec<TriggerDef>,
    trg_add: Vec<TriggerDef>,
}

impl ModTable<'_> {
    fn breaking(&self) -> bool {
        !self.dropped_cols.is_empty() || !self.retyped_cols.is_empty() || !self.renames.is_empty()
    }
    fn renamed(&self, ctx: &Ctx<'_>, col: &str) -> String {
        self.renames.iter().find(|(old, _)| ctx.same(old, col)).map(|(_, new)| new.clone()).unwrap_or_else(|| col.to_string())
    }
}

fn col_set(ctx: &Ctx<'_>, cols: &[String]) -> BTreeSet<String> {
    cols.iter().map(|c| ctx.key(c)).collect()
}

/// Reconcile one kind of sub-object: returns the final list, the target items
/// to drop and the (mapped) source items to add.
fn reconcile<T: Clone>(m: &SubMatch, src: &[T], tgt: &[T], map: impl Fn(&T) -> T, drop_removed: bool, keep_uncompared: bool) -> (Vec<T>, Vec<T>, Vec<T>) {
    let (mut fin, mut drops, mut adds) = (Vec::new(), Vec::new(), Vec::new());
    for (ti, item) in tgt.iter().enumerate() {
        if let Some(si) = m.source_of(ti) {
            if m.changes_of_target(ti).is_some_and(|c| c.is_empty()) {
                fin.push(item.clone());
            } else {
                drops.push(item.clone());
                let n = map(&src[si]);
                fin.push(n.clone());
                adds.push(n);
            }
        } else if m.removed.contains(&ti) {
            if drop_removed {
                drops.push(item.clone());
            } else {
                fin.push(item.clone());
            }
        } else if keep_uncompared {
            fin.push(item.clone());
        }
    }
    for &si in &m.added {
        let n = map(&src[si]);
        fin.push(n.clone());
        adds.push(n);
    }
    (fin, drops, adds)
}

fn same_fk(a: &ForeignKeyDef, b: &ForeignKeyDef) -> bool {
    match (&a.name, &b.name) {
        (Some(x), Some(y)) => x == y,
        _ => a.columns == b.columns && a.ref_table == b.ref_table && a.ref_columns == b.ref_columns,
    }
}

struct Planner<'a> {
    a: Analysis<'a>,
    dialect: &'a dyn Dialect,
    opts: &'a ScriptOptions,
    sel: Selection,
    rename: Option<(String, String)>,
    tgt_schema: Option<String>,
    steps: Vec<MigrationStep>,
    warnings: Vec<String>,
}

/// Plan the migration of `target` to `source` for the objects listed in `diff`.
pub fn migration_plan(diff: &SchemaDiff, source: &SchemaSnapshot, target: &SchemaSnapshot, target_dialect: &dyn Dialect, opts: &ScriptOptions) -> MigrationPlan {
    let a = Analysis::new(source, target, &diff.options);
    let sel = Selection::from_diff(diff, &a.ctx);
    let rename = opts.schema_rename.clone().or_else(|| match (snapshot_schema(source), snapshot_schema(target)) {
        (Some(s), Some(t)) if s != t => Some((s.to_string(), t.to_string())),
        _ => None,
    });
    let mut p = Planner { a, dialect: target_dialect, opts, sel, rename, tgt_schema: snapshot_schema(target).map(str::to_string), steps: Vec::new(), warnings: Vec::new() };
    p.plan();
    let mut transaction = None;
    if opts.wrap_in_transaction && !p.steps.is_empty() {
        match transaction_statements(target_dialect.kind()) {
            Some((b, c)) => transaction = Some((b.to_string(), c.to_string())),
            None => p.warnings.push(dbiji_core::tr("The target database does not support transactions; the script is not wrapped").to_string()),
        }
        if target_dialect.kind().is_mysql_family() {
            p.warnings.push(dbiji_core::tr("MySQL commits DDL statements implicitly; the transaction cannot roll back schema changes").to_string());
        }
    }
    MigrationPlan { steps: p.steps, warnings: p.warnings, transaction }
}

impl<'a> Planner<'a> {
    fn ctx(&self) -> &Ctx<'a> {
        &self.a.ctx
    }

    fn rq(&self, text: &str) -> String {
        match &self.rename {
            Some((from, to)) => rename_schema(text, from, to),
            None => text.to_string(),
        }
    }

    fn rq_opt(&self, text: &Option<String>) -> Option<String> {
        text.as_ref().map(|t| self.rq(t))
    }

    fn qual(&self, name: &str) -> String {
        match &self.tgt_schema {
            Some(s) if !s.is_empty() => format!("{s}.{name}"),
            _ => name.to_string(),
        }
    }

    fn mapped_type(&self, c: &ColumnDef) -> String {
        if self.ctx().opts.cross_dialect {
            let kind = self.ctx().source.kind.unwrap_or(DbKind::Generic);
            self.dialect.ddl().map_type(kind, &c.data_type, Ctx::logical_kind(c))
        } else {
            // user types may be qualified with the source schema
            self.rq(&c.data_type)
        }
    }

    /// Source column prepared for the target (types mapped, schema renamed).
    fn new_column(&self, c: &ColumnDef) -> ColumnDef {
        let mut n = c.clone();
        n.data_type = self.mapped_type(c);
        if self.ctx().opts.cross_dialect {
            n.auto_increment = self.ctx().effective_auto_increment(c);
            if self.ctx().effective_default(c, Side::Source).is_none() {
                n.default = None;
            }
        }
        n.default = self.rq_opt(&n.default);
        n.generated = self.rq_opt(&n.generated);
        n
    }

    fn reconcile_column(&self, sc: &ColumnDef, tc: &ColumnDef, changes: &[crate::model::PropertyChange]) -> ColumnDef {
        let mut c = tc.clone();
        let ctx = self.ctx();
        for ch in changes {
            match ch.property.as_str() {
                "name" => c.name = sc.name.clone(),
                "type" => {
                    c.data_type = self.mapped_type(sc);
                    c.kind = sc.kind;
                }
                "collation" => c.collation = sc.collation.clone(),
                "nullable" => c.nullable = sc.nullable,
                "default" => c.default = if ctx.opts.cross_dialect && ctx.effective_default(sc, Side::Source).is_none() { None } else { self.rq_opt(&sc.default) },
                "auto_increment" => c.auto_increment = ctx.effective_auto_increment(sc),
                "generated" => c.generated = self.rq_opt(&sc.generated),
                "comment" => c.comment = sc.comment.clone(),
                _ => {}
            }
        }
        c
    }

    fn map_index(&self, p: &TablePair<'_>, i: &IndexDef) -> IndexDef {
        let ctx = self.ctx();
        let mut x = i.clone();
        for c in &mut x.columns {
            c.name = if c.is_expression { self.rq(&c.name) } else { map_col(&p.col_map, ctx, &c.name) };
        }
        x.include = x.include.iter().map(|c| map_col(&p.col_map, ctx, c)).collect();
        x.predicate = self.rq_opt(&x.predicate);
        x.definition = self.rq_opt(&x.definition);
        x
    }

    fn map_trigger(&self, t: &TriggerDef) -> TriggerDef {
        let mut x = t.clone();
        x.definition = self.rq(&t.definition);
        x.body = self.rq_opt(&t.body);
        x
    }

    /// Source table prepared as a new target table.
    fn new_table_def(&self, s: &TableDef) -> TableDef {
        let mut d = s.clone();
        d.schema = self.tgt_schema.clone();
        d.name = self.ctx().mapped_table_name(&s.name);
        d.referenced_by.clear();
        d.columns = s.columns.iter().map(|c| self.new_column(c)).collect();
        for ch in &mut d.checks {
            ch.expression = self.rq(&ch.expression);
        }
        for i in &mut d.indexes {
            for c in &mut i.columns {
                if c.is_expression {
                    c.name = self.rq(&c.name);
                }
            }
            i.predicate = self.rq_opt(&i.predicate);
            i.definition = self.rq_opt(&i.definition);
        }
        d.triggers = s.triggers.iter().map(|t| self.map_trigger(t)).collect();
        let empty = HashMap::new();
        let maps = self.a.maps_by_source();
        d.foreign_keys = s.foreign_keys.iter().map(|fk| self.a.map_source_fk(fk, &d.name, &empty, &maps)).collect();
        if let Some(p) = &mut d.partitioning {
            p.key = self.rq(&p.key);
        }
        for part in &mut d.partitions {
            part.bound = self.rq(&part.bound);
        }
        d
    }

    fn plan_table(&self, p: &TablePair<'a>) -> ModTable<'a> {
        let ctx = self.ctx();
        let o = ctx.opts;
        let drop = self.opts.drop_removed;
        let (s, t) = (p.source, p.target);
        let mut d = t.clone();
        d.referenced_by.clear();
        let mut m = ModTable {
            target: t,
            desired: TableDef::default(),
            dropped_cols: HashSet::new(),
            retyped_cols: HashSet::new(),
            renames: p.renames.clone(),
            idx_drop: Vec::new(),
            idx_add: Vec::new(),
            pk_drop: None,
            pk_add: None,
            uq_drop: Vec::new(),
            uq_add: Vec::new(),
            ck_drop: Vec::new(),
            ck_add: Vec::new(),
            fk_drop: Vec::new(),
            fk_add: Vec::new(),
            trg_drop: Vec::new(),
            trg_add: Vec::new(),
        };

        if o.columns {
            let mut cols: Vec<(u64, ColumnDef)> = Vec::new();
            for (ti, tc) in t.columns.iter().enumerate() {
                if let Some(si) = p.columns.source_of(ti) {
                    let changes = p.columns.changes_of_target(ti).cloned().unwrap_or_default();
                    if changes.iter().any(|c| c.property == "type") {
                        m.retyped_cols.insert(ctx.key(&tc.name));
                    }
                    let c = self.reconcile_column(&s.columns[si], tc, &changes);
                    let order = if o.ignore_column_order { ti as u64 } else { si as u64 };
                    cols.push((order, c));
                } else if p.columns.removed.contains(&ti) && drop {
                    m.dropped_cols.insert(ctx.key(&tc.name));
                } else {
                    let order = if o.ignore_column_order { ti as u64 } else { 1_000_000 + ti as u64 };
                    cols.push((order, tc.clone()));
                }
            }
            for &si in &p.columns.added {
                let order = if o.ignore_column_order { 1_000_000 + si as u64 } else { si as u64 };
                cols.push((order, self.new_column(&s.columns[si])));
            }
            cols.sort_by_key(|c| c.0);
            d.columns = cols.into_iter().map(|c| c.1).collect();
            for (i, c) in d.columns.iter_mut().enumerate() {
                c.ordinal = (i + 1) as u32;
            }
        }

        for ch in &p.changes {
            match ch.property.as_str() {
                "comment" => d.comment = s.comment.clone(),
                "table_kind" => d.table_kind = s.table_kind,
                "partitioning" => {
                    d.partitioning = s.partitioning.clone().map(|mut x| {
                        x.key = self.rq(&x.key);
                        x
                    })
                }
                prop if prop.starts_with("option ") => {
                    let key = &prop["option ".len()..];
                    d.options.retain(|k, _| !k.eq_ignore_ascii_case(key));
                    if let Some((k, v)) = s.options.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
                        d.options.insert(k.clone(), v.clone());
                    }
                }
                _ => {}
            }
        }
        if o.partitions {
            let (list, _, _) = reconcile(&p.partitions, &s.partitions, &t.partitions, |x: &PartitionDef| PartitionDef { bound: self.rq(&x.bound), ..x.clone() }, drop, true);
            d.partitions = list;
        }

        let map_key = |k: &KeyDef| KeyDef { name: k.name.clone(), columns: k.columns.iter().map(|c| map_col(&p.col_map, ctx, c)).collect() };
        if o.primary_keys {
            let (spk, tpk) = (pk_vec(s), pk_vec(t));
            if let Some((si, ti, ch)) = p.primary_key.pairs.first() {
                if !ch.is_empty() {
                    m.pk_drop = Some(tpk[*ti].clone());
                    m.pk_add = Some(map_key(&spk[*si]));
                    d.primary_key = m.pk_add.clone();
                }
            } else if let Some(&si) = p.primary_key.added.first() {
                m.pk_add = Some(map_key(&spk[si]));
                d.primary_key = m.pk_add.clone();
            } else if let Some(&ti) = p.primary_key.removed.first()
                && drop
            {
                m.pk_drop = Some(tpk[ti].clone());
                d.primary_key = None;
            }
        }
        if o.unique_keys {
            let (list, drops, adds) = reconcile(&p.unique_keys, &s.unique_keys, &t.unique_keys, map_key, drop, true);
            d.unique_keys = list;
            m.uq_drop = drops;
            m.uq_add = adds;
        }
        if o.checks {
            let (list, drops, adds) = reconcile(&p.checks, &s.checks, &t.checks, |c: &CheckDef| CheckDef { name: c.name.clone(), expression: self.rq(&c.expression) }, drop, true);
            d.checks = list;
            m.ck_drop = drops;
            m.ck_add = adds;
        }
        if o.foreign_keys {
            let (list, drops, adds) = reconcile(&p.foreign_keys, &p.src_fks_mapped, &t.foreign_keys, |f| f.clone(), drop, true);
            d.foreign_keys = list;
            m.fk_drop = drops;
            m.fk_add = adds;
        }
        if o.indexes {
            let (mut list, drops, adds) = reconcile(&p.indexes, &s.indexes, &t.indexes, |i| self.map_index(p, i), drop, false);
            // keep indexes backing keys that still exist in the desired definition
            let mut keys_only = d.clone();
            keys_only.indexes.clear();
            for idx in &t.indexes {
                if !is_secondary_index(ctx, t, idx) && !is_secondary_index(ctx, &keys_only, idx) {
                    list.insert(0, idx.clone());
                }
            }
            d.indexes = list;
            m.idx_drop = drops;
            m.idx_add = adds;
        }
        if o.triggers {
            let (list, drops, adds) = reconcile(&p.triggers, &s.triggers, &t.triggers, |x| self.map_trigger(x), drop, true);
            d.triggers = list;
            m.trg_drop = drops;
            m.trg_add = adds;
        }
        m.desired = d;
        m
    }

    fn push(&mut self, phase: Phase, object: String, description: String, op: MigrationOp, statements: Vec<String>) {
        let statements: Vec<String> = statements.into_iter().filter(|s| !s.trim().is_empty()).collect();
        if statements.is_empty() {
            if let MigrationOp::AlterTable { from, to, .. } = &op
                && from == to
            {
                return;
            }
            self.warnings.push(trf("No SQL generated for: {}", &[&description]));
            return;
        }
        self.steps.push(MigrationStep { phase, object, description, op, statements });
    }

    fn table_def_for(&self, name: &str, mods: &[ModTable<'_>], new_tables: &[TableDef], desired: bool) -> TableDef {
        let ctx = self.ctx();
        if desired {
            if let Some(m) = mods.iter().find(|m| ctx.same(&m.target.name, name)) {
                return m.desired.clone();
            }
            if let Some(t) = new_tables.iter().find(|t| ctx.same(&t.name, name)) {
                return t.clone();
            }
        }
        ctx.target.tables.iter().find(|t| ctx.same(&t.name, name)).cloned().unwrap_or_else(|| TableDef::new(self.tgt_schema.clone(), name))
    }

    #[allow(clippy::type_complexity)]
    fn plan_foreign_keys(&mut self, mods: &[ModTable<'_>], dropped: &HashSet<String>) -> (Vec<(String, ForeignKeyDef)>, Vec<(String, ForeignKeyDef)>) {
        let ctx = &self.a.ctx;
        let mut drops: Vec<(String, ForeignKeyDef)> = Vec::new();
        let mut adds: Vec<(String, ForeignKeyDef)> = Vec::new();
        for m in mods {
            for f in &m.fk_drop {
                drops.push((m.target.name.clone(), f.clone()));
            }
            for f in &m.fk_add {
                adds.push((m.target.name.clone(), f.clone()));
            }
        }
        let find_mod = |name: &str| mods.iter().find(|m| ctx.same(&m.target.name, name));
        let mut warnings = Vec::new();
        for t in &ctx.target.tables {
            if dropped.contains(&ctx.key(&t.name)) {
                continue;
            }
            let tmod = find_mod(&t.name);
            for f in &t.foreign_keys {
                if drops.iter().any(|(tn, d)| ctx.same(tn, &t.name) && same_fk(d, f)) {
                    continue;
                }
                let mut must_drop = false;
                let mut recreate = true;
                let mut nf = f.clone();
                if let Some(tm) = tmod {
                    if f.columns.iter().any(|c| tm.dropped_cols.contains(&ctx.key(c))) {
                        must_drop = true;
                        recreate = false;
                    }
                    if f.columns.iter().any(|c| tm.retyped_cols.contains(&ctx.key(c))) {
                        must_drop = true;
                    }
                    nf.columns = f.columns.iter().map(|c| tm.renamed(ctx, c)).collect();
                }
                if ctx.is_own_schema(f.ref_schema.as_deref(), Side::Target) {
                    if dropped.contains(&ctx.key(&f.ref_table)) {
                        must_drop = true;
                        recreate = false;
                    } else if let Some(rm) = find_mod(&f.ref_table) {
                        if f.ref_columns.iter().any(|c| rm.dropped_cols.contains(&ctx.key(c))) {
                            must_drop = true;
                            recreate = false;
                        }
                        if f.ref_columns.iter().any(|c| rm.retyped_cols.contains(&ctx.key(c))) {
                            must_drop = true;
                        }
                        let set = col_set(ctx, &f.ref_columns);
                        let mut dropped_sets: Vec<BTreeSet<String>> = Vec::new();
                        if let Some(pk) = &rm.pk_drop {
                            dropped_sets.push(col_set(ctx, &pk.columns));
                        }
                        dropped_sets.extend(rm.uq_drop.iter().map(|u| col_set(ctx, &u.columns)));
                        dropped_sets.extend(rm.idx_drop.iter().filter(|i| i.unique).map(|i| i.columns.iter().map(|c| ctx.key(&c.name)).collect()));
                        nf.ref_columns = f.ref_columns.iter().map(|c| rm.renamed(ctx, c)).collect();
                        if dropped_sets.contains(&set) {
                            must_drop = true;
                            let new_set = col_set(ctx, &nf.ref_columns);
                            let d = &rm.desired;
                            let mut final_sets: Vec<BTreeSet<String>> = Vec::new();
                            if let Some(pk) = &d.primary_key {
                                final_sets.push(col_set(ctx, &pk.columns));
                            }
                            final_sets.extend(d.unique_keys.iter().map(|u| col_set(ctx, &u.columns)));
                            final_sets.extend(d.indexes.iter().filter(|i| i.unique).map(|i| i.columns.iter().map(|c| ctx.key(&c.name)).collect()));
                            if !final_sets.contains(&new_set) {
                                recreate = false;
                            }
                        }
                    }
                }
                if must_drop {
                    drops.push((t.name.clone(), f.clone()));
                    if recreate {
                        adds.push((t.name.clone(), nf));
                    } else if tmod.is_none() || !self.opts.drop_removed {
                        warnings.push(trf("Foreign key {} on {} is dropped because the referenced key or table changes", &[&f.name.clone().unwrap_or_default(), &t.name]));
                    }
                }
            }
        }
        self.warnings.extend(warnings);
        (drops, adds)
    }

    fn plan(&mut self) {
        let granular = self.opts.alter_mode == AlterMode::Granular;
        let drop = self.opts.drop_removed;
        let source = self.a.ctx.source;
        let target = self.a.ctx.target;

        // ------------------------------------------------------------ tables
        let mut mods: Vec<ModTable<'a>> = Vec::new();
        for p in &self.a.tables {
            if self.sel.tables_mod.contains(&self.a.ctx.key(&p.source.name)) && !p.is_unchanged() {
                mods.push(self.plan_table(p));
            }
        }
        let new_tables: Vec<TableDef> = self
            .a
            .added_tables
            .iter()
            .filter(|&&i| self.sel.tables_add.contains(&self.a.ctx.key(&source.tables[i].name)))
            .map(|&i| self.new_table_def(&source.tables[i]))
            .collect();
        let dropped_tables: Vec<&TableDef> =
            if drop { self.a.removed_tables.iter().map(|&i| &target.tables[i]).filter(|t| self.sel.tables_rem.contains(&self.a.ctx.key(&t.name))).collect() } else { Vec::new() };
        let dropped_keys: HashSet<String> = dropped_tables.iter().map(|t| self.a.ctx.key(&t.name)).collect();

        let (mut fk_drops, fk_adds) = if granular { self.plan_foreign_keys(&mods, &dropped_keys) } else { (Vec::new(), Vec::new()) };

        // ----------------------------------------------------------- routines
        let mut create_routines: Vec<(RoutineDef, bool)> = Vec::new();
        let mut drop_first: Vec<RoutineDef> = Vec::new();
        let mut drop_late: Vec<RoutineDef> = Vec::new();
        {
            let ctx = &self.a.ctx;
            let rq_routine = |r: &RoutineDef| RoutineDef {
                schema: self.tgt_schema.clone(),
                arguments: self.rq(&r.arguments),
                returns: self.rq_opt(&r.returns),
                definition: self.rq(&r.definition),
                ..r.clone()
            };
            for (si, ti, ch) in &self.a.routines.pairs {
                let r = &source.routines[*si];
                if ch.is_empty() || !self.sel.routines_mod.contains(&(ctx.key(&r.name), args_key(&r.arguments))) {
                    continue;
                }
                let nr = rq_routine(r);
                let stmts = self.dialect.ddl().create_routine(&nr);
                let signature_changed = ch.iter().any(|c| c.property == "arguments" || c.property == "returns");
                let replace = !signature_changed && stmts.first().is_some_and(|s| is_create_or_replace(s));
                if !replace {
                    drop_first.push(target.routines[*ti].clone());
                }
                create_routines.push((nr, replace));
            }
            for &si in &self.a.routines.added {
                let r = &source.routines[si];
                if self.sel.routines_add.contains(&(ctx.key(&r.name), args_key(&r.arguments))) {
                    create_routines.push((rq_routine(r), false));
                }
            }
            if drop {
                for &ti in &self.a.routines.removed {
                    let r = &target.routines[ti];
                    if self.sel.routines_rem.contains(&(ctx.key(&r.name), args_key(&r.arguments))) {
                        drop_late.push(r.clone());
                    }
                }
            }
        }
        let dropped_routine_names: Vec<String> = drop_first.iter().chain(drop_late.iter()).map(|r| r.name.clone()).collect();

        // ------------------------------------------- triggers using dropped routines
        let mut extra_trg_drop: Vec<(String, TriggerDef)> = Vec::new();
        let mut extra_trg_add: Vec<(String, TriggerDef)> = Vec::new();
        {
            let ctx = &self.a.ctx;
            let own = [None, snapshot_schema(ctx.target)];
            let recreated: Vec<String> = drop_first.iter().map(|r| r.name.clone()).collect();
            for t in &target.tables {
                if dropped_keys.contains(&ctx.key(&t.name)) {
                    continue;
                }
                let m = mods.iter().find(|m| ctx.same(&m.target.name, &t.name));
                for tr in &t.triggers {
                    let refs = references(&tr.definition);
                    let uses = |names: &[String]| names.iter().any(|n| refs.calls(n, &own) || refs.mentions(n, &own));
                    if !uses(&dropped_routine_names) {
                        continue;
                    }
                    if m.is_some_and(|m| m.trg_drop.iter().any(|x| ctx.same(&x.name, &tr.name))) {
                        continue;
                    }
                    extra_trg_drop.push((t.name.clone(), tr.clone()));
                    if uses(&recreated) {
                        let final_trg = match m {
                            Some(m) => m.desired.triggers.iter().find(|x| ctx.same(&x.name, &tr.name)).cloned(),
                            None => Some(tr.clone()),
                        };
                        if let Some(ft) = final_trg
                            && !m.is_some_and(|m| m.trg_add.iter().any(|x| ctx.same(&x.name, &ft.name)))
                        {
                            extra_trg_add.push((t.name.clone(), ft));
                        }
                    }
                }
            }
        }

        // ------------------------------------------------------------- views
        let (view_drops, view_creates) = self.plan_views(&mods, &dropped_tables, &dropped_routine_names, granular);

        // early routines: referenced by tables or views being created
        let mut early_names: HashSet<String> = HashSet::new();
        {
            let mut texts: Vec<String> = Vec::new();
            let collect = |t: &TableDef, texts: &mut Vec<String>| {
                for c in &t.columns {
                    texts.extend(c.default.clone());
                    texts.extend(c.generated.clone());
                }
                texts.extend(t.checks.iter().map(|c| c.expression.clone()));
                for i in &t.indexes {
                    texts.extend(i.columns.iter().filter(|c| c.is_expression).map(|c| c.name.clone()));
                    texts.extend(i.predicate.clone());
                }
            };
            for t in &new_tables {
                collect(t, &mut texts);
            }
            for m in &mods {
                collect(&m.desired, &mut texts);
            }
            texts.extend(view_creates.iter().map(|(v, _)| v.definition.clone()));
            let own = [None, self.tgt_schema.as_deref()];
            for (r, _) in &create_routines {
                if texts.iter().any(|t| references(t).calls(&r.name, &own)) {
                    early_names.insert(self.a.ctx.key(&r.name));
                }
            }
        }

        // -------------------------------------------------- dropped table order
        let (drop_order, cyclic) = {
            let ctx = &self.a.ctx;
            let names: Vec<String> = dropped_tables.iter().map(|t| ctx.key(&t.name)).collect();
            let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for t in &dropped_tables {
                let d = t
                    .foreign_keys
                    .iter()
                    .filter(|f| ctx.is_own_schema(f.ref_schema.as_deref(), Side::Target))
                    .map(|f| ctx.key(&f.ref_table))
                    .filter(|r| dropped_keys.contains(r))
                    .collect();
                deps.insert(ctx.key(&t.name), d);
            }
            let (order, cyclic) = topo_sort(&names, &deps);
            // referencing tables must be dropped first
            let mut order: Vec<String> = order.into_iter().rev().collect();
            order.extend(cyclic.iter().cloned());
            (order, cyclic)
        };
        if granular && !self.opts.cascade {
            let ctx = &self.a.ctx;
            for t in &dropped_tables {
                if !cyclic.contains(&ctx.key(&t.name)) {
                    continue;
                }
                for f in &t.foreign_keys {
                    if cyclic.contains(&ctx.key(&f.ref_table)) && !ctx.same(&f.ref_table, &t.name) {
                        fk_drops.push((t.name.clone(), f.clone()));
                    }
                }
            }
        }

        // ================================================================ emit
        let dialect = self.dialect;
        let ddl = dialect.ddl();
        // 1. foreign keys
        for (table, fk) in &fk_drops {
            let ctx_def = self.table_def_for(table, &mods, &new_tables, false);
            let name = fk.name.clone().unwrap_or_else(|| fk.columns.join(", "));
            self.push(
                Phase::DropForeignKeys,
                self.qual(table),
                trf("Drop foreign key {} on {}", &[&name, &self.qual(table)]),
                MigrationOp::DropForeignKey { table: table.clone(), fk: fk.clone() },
                vec![ddl.drop_foreign_key(&ctx_def, fk)],
            );
        }
        // 2. triggers
        let mut trg_drops: Vec<(String, TriggerDef)> = Vec::new();
        for m in &mods {
            trg_drops.extend(m.trg_drop.iter().map(|t| (m.target.name.clone(), t.clone())));
        }
        trg_drops.extend(extra_trg_drop.iter().cloned());
        for (table, trg) in &trg_drops {
            let ctx_def = self.table_def_for(table, &mods, &new_tables, false);
            self.push(
                Phase::DropTriggers,
                self.qual(table),
                trf("Drop trigger {} on {}", &[&trg.name, &self.qual(table)]),
                MigrationOp::DropTrigger { table: table.clone(), trigger: trg.clone() },
                vec![ddl.drop_trigger(&ctx_def, trg)],
            );
        }
        // 3. views
        for v in &view_drops {
            self.push(Phase::DropViews, self.qual(&v.name), trf("Drop view {}", &[&self.qual(&v.name)]), MigrationOp::DropView { view: v.clone() }, vec![ddl.drop_view(v)]);
        }
        // 4. routines that must be dropped before being recreated
        for r in &drop_first {
            self.push(
                Phase::DropRoutines,
                self.qual(&r.name),
                trf("Drop routine {}({})", &[&self.qual(&r.name), &r.arguments]),
                MigrationOp::DropRoutine { routine: r.clone() },
                vec![ddl.drop_routine(r)],
            );
        }
        // 5./6. indexes and constraints of modified tables
        if granular {
            for m in &mods {
                for i in &m.idx_drop {
                    self.push(
                        Phase::DropIndexes,
                        self.qual(&m.target.name),
                        trf("Drop index {} on {}", &[&i.name, &self.qual(&m.target.name)]),
                        MigrationOp::DropIndex { table: m.target.name.clone(), index: i.clone() },
                        vec![ddl.drop_index(m.target, i)],
                    );
                }
            }
            for m in &mods {
                let table = m.target.name.clone();
                for c in &m.ck_drop {
                    self.push(
                        Phase::DropConstraints,
                        self.qual(&table),
                        trf("Drop check {} on {}", &[&c.name.clone().unwrap_or_default(), &self.qual(&table)]),
                        MigrationOp::DropCheck { table: table.clone(), check: c.clone() },
                        vec![ddl.drop_check(m.target, c)],
                    );
                }
                for u in &m.uq_drop {
                    self.push(
                        Phase::DropConstraints,
                        self.qual(&table),
                        trf("Drop unique key {} on {}", &[&u.name.clone().unwrap_or_default(), &self.qual(&table)]),
                        MigrationOp::DropUniqueKey { table: table.clone(), key: u.clone() },
                        vec![ddl.drop_unique_key(m.target, u)],
                    );
                }
                if let Some(pk) = &m.pk_drop {
                    self.push(
                        Phase::DropConstraints,
                        self.qual(&table),
                        trf("Drop primary key on {}", &[&self.qual(&table)]),
                        MigrationOp::DropPrimaryKey { table: table.clone(), key: pk.clone() },
                        vec![ddl.drop_primary_key(m.target, pk)],
                    );
                }
            }
        }
        // 7. tables
        for key in &drop_order {
            if let Some(t) = dropped_tables.iter().find(|t| self.a.ctx.key(&t.name) == *key) {
                let t = (*t).clone();
                self.push(
                    Phase::DropTables,
                    self.qual(&t.name),
                    trf("Drop table {}", &[&self.qual(&t.name)]),
                    MigrationOp::DropTable { table: t.clone(), cascade: self.opts.cascade },
                    vec![ddl.drop_table(t.schema.as_deref().or(self.tgt_schema.as_deref()), &t.name, self.opts.cascade)],
                );
            }
        }
        // 8./9. types
        let rq_type = |t: &TypeDef| TypeDef { schema: self.tgt_schema.clone(), definition: self.rq(&t.definition), ..t.clone() };
        let mut type_steps: Vec<(Phase, String, String, MigrationOp, Vec<String>)> = Vec::new();
        let mut type_warnings: Vec<String> = Vec::new();
        for (si, ti, ch) in &self.a.types.pairs {
            let s = &source.types[*si];
            if ch.is_empty() || !self.sel.types_mod.contains(&self.a.ctx.key(&s.name)) {
                continue;
            }
            let old = &target.types[*ti];
            let used = target.tables.iter().chain(source.tables.iter()).flat_map(|t| t.columns.iter()).any(|c| {
                let base = self.a.ctx.norm_type(&c.data_type);
                let base = base.trim_end_matches("[]");
                base.rsplit('.').next().is_some_and(|b| b.trim_matches('"') == old.name.to_lowercase())
            });
            if used {
                type_warnings.push(trf("Type {} differs but is used by table columns; alter it manually", &[&self.qual(&old.name)]));
                continue;
            }
            match ddl.drop_type(old) {
                Some(drop_sql) => {
                    type_steps.push((
                        Phase::RecreateTypes,
                        self.qual(&old.name),
                        trf("Drop type {} (recreated)", &[&self.qual(&old.name)]),
                        MigrationOp::DropType { ty: old.clone() },
                        vec![drop_sql],
                    ));
                    let n = rq_type(s);
                    type_steps.push((
                        Phase::RecreateTypes,
                        self.qual(&n.name),
                        trf("Create type {}", &[&self.qual(&n.name)]),
                        MigrationOp::CreateType { ty: n.clone() },
                        ddl.create_type(&n),
                    ));
                }
                None => type_warnings.push(trf("Type {} differs but cannot be altered automatically", &[&self.qual(&old.name)])),
            }
        }
        for &si in &self.a.types.added {
            let s = &source.types[si];
            if self.sel.types_add.contains(&self.a.ctx.key(&s.name)) {
                let n = rq_type(s);
                type_steps.push((
                    Phase::CreateTypes,
                    self.qual(&n.name),
                    trf("Create type {}", &[&self.qual(&n.name)]),
                    MigrationOp::CreateType { ty: n.clone() },
                    ddl.create_type(&n),
                ));
            }
        }
        self.warnings.extend(type_warnings);
        for (phase, obj, desc, op, sql) in type_steps {
            self.push(phase, obj, desc, op, sql);
        }
        // 10. sequences
        let rq_seq = |s: &SequenceDef| SequenceDef { schema: self.tgt_schema.clone(), owned_by: None, last_value: None, ..s.clone() };
        let mut seq_steps: Vec<(String, String, MigrationOp, Vec<String>)> = Vec::new();
        for &si in &self.a.sequences.added {
            let s = &source.sequences[si];
            if self.sel.seq_add.contains(&self.a.ctx.key(&s.name)) {
                let n = rq_seq(s);
                seq_steps.push((
                    self.qual(&n.name),
                    trf("Create sequence {}", &[&self.qual(&n.name)]),
                    MigrationOp::CreateSequence { sequence: n.clone() },
                    vec![ddl.create_sequence(&n)],
                ));
            }
        }
        for (si, ti, ch) in &self.a.sequences.pairs {
            let s = &source.sequences[*si];
            if ch.is_empty() || !self.sel.seq_mod.contains(&self.a.ctx.key(&s.name)) {
                continue;
            }
            let from = target.sequences[*ti].clone();
            let mut to = rq_seq(s);
            to.name = from.name.clone();
            to.owned_by = from.owned_by.clone();
            to.last_value = from.last_value;
            if !self.a.ctx.opts.sequence_start_values {
                to.start = from.start;
            }
            if to.data_type.is_none() || self.a.ctx.opts.cross_dialect {
                to.data_type = from.data_type.clone();
            }
            seq_steps.push((
                self.qual(&from.name),
                trf("Alter sequence {}", &[&self.qual(&from.name)]),
                MigrationOp::AlterSequence { from: from.clone(), to: to.clone() },
                ddl.alter_sequence(&from, &to),
            ));
        }
        for (obj, desc, op, sql) in seq_steps {
            self.push(Phase::Sequences, obj, desc, op, sql);
        }
        // 11. early routines
        let (early, late): (Vec<_>, Vec<_>) = create_routines.into_iter().partition(|(r, _)| early_names.contains(&self.a.ctx.key(&r.name)));
        for (r, replace) in &early {
            self.push_routine(Phase::EarlyRoutines, r, *replace);
        }
        // 12. new tables
        let new_order: Vec<TableDef> = if granular {
            new_tables.clone()
        } else {
            let ctx = &self.a.ctx;
            let names: Vec<String> = new_tables.iter().map(|t| ctx.key(&t.name)).collect();
            let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for t in &new_tables {
                deps.insert(ctx.key(&t.name), t.foreign_keys.iter().map(|f| ctx.key(&f.ref_table)).collect());
            }
            let (order, cyclic) = topo_sort(&names, &deps);
            if !cyclic.is_empty() {
                self.warnings.push(trf("Circular foreign keys between new tables: {}", &[&cyclic.join(", ")]));
            }
            order.iter().chain(cyclic.iter()).filter_map(|k| new_tables.iter().find(|t| ctx.key(&t.name) == *k).cloned()).collect()
        };
        for t in &new_order {
            let mut def = t.clone();
            def.triggers.clear();
            if granular {
                def.foreign_keys.clear();
            }
            self.push(
                Phase::CreateTables,
                self.qual(&def.name),
                trf("Create table {}", &[&self.qual(&def.name)]),
                MigrationOp::CreateTable { table: def.clone(), inline_foreign_keys: !granular },
                ddl.create_table(&def, !granular),
            );
        }
        // 13. alter tables
        for m in &mods {
            let ctx = &self.a.ctx;
            let mut from = m.target.clone();
            from.referenced_by.clear();
            let mut to = m.desired.clone();
            let kept_trg: Vec<TriggerDef> =
                m.target.triggers.iter().filter(|t| !trg_drops.iter().any(|(tn, d)| ctx.same(tn, &m.target.name) && ctx.same(&d.name, &t.name))).cloned().collect();
            if granular {
                let kept_fks: Vec<ForeignKeyDef> =
                    m.target.foreign_keys.iter().filter(|f| !fk_drops.iter().any(|(tn, d)| ctx.same(tn, &m.target.name) && same_fk(d, f))).cloned().collect();
                let dropped_key_cols: Vec<BTreeSet<String>> = m.pk_drop.iter().chain(m.uq_drop.iter()).map(|k| col_set(ctx, &k.columns)).collect();
                let kept_idx: Vec<IndexDef> = m
                    .target
                    .indexes
                    .iter()
                    .filter(|i| !m.idx_drop.contains(i))
                    .filter(|i| is_secondary_index(ctx, m.target, i) || !dropped_key_cols.contains(&i.columns.iter().map(|c| ctx.key(&c.name)).collect()))
                    .cloned()
                    .collect();
                from.primary_key = if m.pk_drop.is_some() { None } else { m.target.primary_key.clone() };
                from.unique_keys = m.target.unique_keys.iter().filter(|u| !m.uq_drop.contains(u)).cloned().collect();
                from.checks = m.target.checks.iter().filter(|c| !m.ck_drop.contains(c)).cloned().collect();
                from.indexes = kept_idx;
                from.foreign_keys = kept_fks;
                from.triggers = kept_trg.clone();
                // the same lists, with renamed columns
                let rn = |c: &String| m.renamed(ctx, c);
                to.primary_key = from.primary_key.clone().map(|k| KeyDef { columns: k.columns.iter().map(rn).collect(), ..k });
                to.unique_keys = from.unique_keys.iter().map(|k| KeyDef { columns: k.columns.iter().map(rn).collect(), ..k.clone() }).collect();
                to.checks = from.checks.clone();
                to.indexes = from
                    .indexes
                    .iter()
                    .map(|i| {
                        let mut i = i.clone();
                        for c in &mut i.columns {
                            if !c.is_expression {
                                c.name = rn(&c.name);
                            }
                        }
                        i.include = i.include.iter().map(rn).collect();
                        i
                    })
                    .collect();
                to.foreign_keys = from
                    .foreign_keys
                    .iter()
                    .map(|f| {
                        let mut f = f.clone();
                        f.columns = f.columns.iter().map(rn).collect();
                        if ctx.same(&f.ref_table, &m.target.name) {
                            f.ref_columns = f.ref_columns.iter().map(rn).collect();
                        }
                        f
                    })
                    .collect();
                to.triggers = kept_trg;
            } else {
                from.triggers = kept_trg.clone();
                to.triggers = kept_trg;
            }
            let renames = m.renames.clone();
            let sql = ddl.alter_table(&from, &to, &renames);
            let name = m.target.name.clone();
            self.push(Phase::AlterTables, self.qual(&name), trf("Alter table {}", &[&self.qual(&name)]), MigrationOp::AlterTable { from, to, renamed_columns: renames }, sql);
        }
        // 14. removed sequences, types, routines
        for r in &drop_late {
            self.push(
                Phase::DropUnused,
                self.qual(&r.name),
                trf("Drop routine {}({})", &[&self.qual(&r.name), &r.arguments]),
                MigrationOp::DropRoutine { routine: r.clone() },
                vec![ddl.drop_routine(r)],
            );
        }
        if drop {
            let ctx = &self.a.ctx;
            let mut seq_drops = Vec::new();
            for &ti in &self.a.sequences.removed {
                let s = &target.sequences[ti];
                if !self.sel.seq_rem.contains(&ctx.key(&s.name)) {
                    continue;
                }
                // sequences owned by dropped tables / columns disappear with them
                let owned_gone = s.owned_by.as_deref().and_then(|o| o.rsplit_once('.')).is_some_and(|(table, col)| {
                    let table = table.rsplit('.').next().unwrap_or(table);
                    dropped_keys.contains(&ctx.key(table)) || mods.iter().any(|m| ctx.same(&m.target.name, table) && m.dropped_cols.contains(&ctx.key(col)))
                });
                if !owned_gone {
                    seq_drops.push(s.clone());
                }
            }
            for s in seq_drops {
                self.push(
                    Phase::DropUnused,
                    self.qual(&s.name),
                    trf("Drop sequence {}", &[&self.qual(&s.name)]),
                    MigrationOp::DropSequence { sequence: s.clone() },
                    vec![ddl.drop_sequence(&s)],
                );
            }
            let mut type_drops = Vec::new();
            for &ti in &self.a.types.removed {
                let t = &target.types[ti];
                if self.sel.types_rem.contains(&self.a.ctx.key(&t.name)) {
                    type_drops.push(t.clone());
                }
            }
            for t in type_drops {
                match ddl.drop_type(&t) {
                    Some(sql) => self.push(Phase::DropUnused, self.qual(&t.name), trf("Drop type {}", &[&self.qual(&t.name)]), MigrationOp::DropType { ty: t.clone() }, vec![sql]),
                    None => self.warnings.push(trf("Type {} cannot be dropped automatically", &[&self.qual(&t.name)])),
                }
            }
        }
        // 15./16. constraints and indexes
        if granular {
            for m in &mods {
                let table = m.desired.name.clone();
                if let Some(pk) = &m.pk_add {
                    self.push(
                        Phase::AddConstraints,
                        self.qual(&table),
                        trf("Add primary key on {}", &[&self.qual(&table)]),
                        MigrationOp::AddPrimaryKey { table: table.clone(), key: pk.clone() },
                        vec![ddl.add_primary_key(&m.desired, pk)],
                    );
                }
                for u in &m.uq_add {
                    self.push(
                        Phase::AddConstraints,
                        self.qual(&table),
                        trf("Add unique key {} on {}", &[&u.name.clone().unwrap_or_default(), &self.qual(&table)]),
                        MigrationOp::AddUniqueKey { table: table.clone(), key: u.clone() },
                        vec![ddl.add_unique_key(&m.desired, u)],
                    );
                }
                for c in &m.ck_add {
                    self.push(
                        Phase::AddConstraints,
                        self.qual(&table),
                        trf("Add check {} on {}", &[&c.name.clone().unwrap_or_default(), &self.qual(&table)]),
                        MigrationOp::AddCheck { table: table.clone(), check: c.clone() },
                        vec![ddl.add_check(&m.desired, c)],
                    );
                }
            }
            for m in &mods {
                let table = m.desired.name.clone();
                for i in &m.idx_add {
                    self.push(
                        Phase::CreateIndexes,
                        self.qual(&table),
                        trf("Create index {} on {}", &[&i.name, &self.qual(&table)]),
                        MigrationOp::CreateIndex { table: table.clone(), index: i.clone() },
                        vec![ddl.create_index(&m.desired, i)],
                    );
                }
            }
            // 17. foreign keys
            let mut all_adds: Vec<(String, ForeignKeyDef)> = Vec::new();
            for t in &new_tables {
                all_adds.extend(t.foreign_keys.iter().map(|f| (t.name.clone(), f.clone())));
            }
            all_adds.extend(fk_adds);
            for (table, fk) in &all_adds {
                let ctx_def = self.table_def_for(table, &mods, &new_tables, true);
                let name = fk.name.clone().unwrap_or_else(|| fk.columns.join(", "));
                self.push(
                    Phase::AddForeignKeys,
                    self.qual(table),
                    trf("Add foreign key {} on {}", &[&name, &self.qual(table)]),
                    MigrationOp::AddForeignKey { table: table.clone(), fk: fk.clone() },
                    vec![ddl.add_foreign_key(&ctx_def, fk)],
                );
            }
        }
        // 18. views
        for (v, replace) in &view_creates {
            let desc = if *replace { trf("Replace view {}", &[&self.qual(&v.name)]) } else { trf("Create view {}", &[&self.qual(&v.name)]) };
            self.push(Phase::CreateViews, self.qual(&v.name), desc, MigrationOp::CreateView { view: v.clone(), replace: *replace }, ddl.create_view(v, *replace));
        }
        // 19. other routines
        for (r, replace) in &late {
            self.push_routine(Phase::Routines, r, *replace);
        }
        // 20. triggers
        let mut trg_adds: Vec<(String, TriggerDef)> = Vec::new();
        for t in &new_tables {
            trg_adds.extend(t.triggers.iter().map(|x| (t.name.clone(), x.clone())));
        }
        for m in &mods {
            trg_adds.extend(m.trg_add.iter().map(|x| (m.desired.name.clone(), x.clone())));
        }
        trg_adds.extend(extra_trg_add);
        for (table, trg) in &trg_adds {
            let ctx_def = self.table_def_for(table, &mods, &new_tables, true);
            self.push(
                Phase::CreateTriggers,
                self.qual(table),
                trf("Create trigger {} on {}", &[&trg.name, &self.qual(table)]),
                MigrationOp::CreateTrigger { table: table.clone(), trigger: trg.clone() },
                ddl.create_trigger(&ctx_def, trg),
            );
        }
    }

    fn push_routine(&mut self, phase: Phase, r: &RoutineDef, replace: bool) {
        let sql = self.dialect.ddl().create_routine(r);
        let desc = if replace { trf("Replace routine {}({})", &[&self.qual(&r.name), &r.arguments]) } else { trf("Create routine {}({})", &[&self.qual(&r.name), &r.arguments]) };
        self.push(phase, self.qual(&r.name), desc, MigrationOp::CreateRoutine { routine: r.clone(), replace }, sql);
    }

    /// Views to drop (dependents first) and to create (dependencies first).
    fn plan_views(&mut self, mods: &[ModTable<'_>], dropped_tables: &[&TableDef], dropped_routines: &[String], granular: bool) -> (Vec<ViewDef>, Vec<(ViewDef, bool)>) {
        let ctx = &self.a.ctx;
        let source = ctx.source;
        let target = ctx.target;
        let drop = self.opts.drop_removed;
        let rq_view = |v: &ViewDef| ViewDef { schema: self.tgt_schema.clone(), definition: self.rq(&v.definition), ..v.clone() };
        let own_t = [None, snapshot_schema(target)];

        let mut drop_set: BTreeSet<String> = BTreeSet::new();
        let mut replace_in_place: Vec<ViewDef> = Vec::new();
        for (si, ti, ch) in &self.a.views.pairs {
            let s = &source.views[*si];
            let t = &target.views[*ti];
            if ch.is_empty() || !self.sel.views_mod.contains(&ctx.key(&s.name)) {
                continue;
            }
            if view_replaceable(ctx, s, t) {
                replace_in_place.push(ViewDef { name: t.name.clone(), ..rq_view(s) });
            } else {
                drop_set.insert(ctx.key(&t.name));
            }
        }
        let mut intentionally_dropped: BTreeSet<String> = BTreeSet::new();
        if drop {
            for &ti in &self.a.views.removed {
                let t = &target.views[ti];
                if self.sel.views_rem.contains(&ctx.key(&t.name)) {
                    drop_set.insert(ctx.key(&t.name));
                    intentionally_dropped.insert(ctx.key(&t.name));
                }
            }
        }
        // dependency closure over target views
        let breaking: Vec<String> = mods.iter().filter(|m| !granular || m.breaking()).map(|m| m.target.name.clone()).chain(dropped_tables.iter().map(|t| t.name.clone())).collect();
        let target_refs: Vec<_> = target.views.iter().map(|v| references(&v.definition)).collect();
        loop {
            let mut changed = false;
            for (i, v) in target.views.iter().enumerate() {
                let k = ctx.key(&v.name);
                if drop_set.contains(&k) || ctx.excluded(&v.name) {
                    continue;
                }
                let r = &target_refs[i];
                let hit = breaking.iter().any(|t| r.mentions(t, &own_t))
                    || dropped_routines.iter().any(|n| r.calls(n, &own_t))
                    || target.views.iter().any(|w| drop_set.contains(&ctx.key(&w.name)) && !ctx.same(&w.name, &v.name) && r.mentions(&w.name, &own_t));
                if hit {
                    drop_set.insert(k);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        replace_in_place.retain(|v| !drop_set.contains(&ctx.key(&v.name)));

        // drop order: dependents first
        let drop_names: Vec<String> = drop_set.iter().cloned().collect();
        let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (i, v) in target.views.iter().enumerate() {
            let k = ctx.key(&v.name);
            if !drop_set.contains(&k) {
                continue;
            }
            let d = target.views.iter().filter(|w| !ctx.same(&w.name, &v.name) && target_refs[i].mentions(&w.name, &own_t)).map(|w| ctx.key(&w.name)).collect();
            deps.insert(k, d);
        }
        let (order, cyclic) = topo_sort(&drop_names, &deps);
        let drops: Vec<ViewDef> = order.iter().rev().chain(cyclic.iter()).filter_map(|k| target.views.iter().find(|v| ctx.key(&v.name) == *k).cloned()).collect();

        // views to create
        let mut creates: Vec<(ViewDef, bool)> = Vec::new();
        for k in &drop_set {
            if intentionally_dropped.contains(k) {
                continue;
            }
            let Some(ti) = target.views.iter().position(|v| ctx.key(&v.name) == *k) else { continue };
            match self.a.views.pairs.iter().find(|(_, t, _)| *t == ti) {
                Some((si, _, ch)) => {
                    let s = &source.views[*si];
                    if !ch.is_empty() && self.sel.views_mod.contains(&ctx.key(&s.name)) {
                        creates.push((ViewDef { name: target.views[ti].name.clone(), ..rq_view(s) }, false));
                    } else {
                        creates.push((target.views[ti].clone(), false));
                    }
                }
                None => creates.push((target.views[ti].clone(), false)),
            }
        }
        for &si in &self.a.views.added {
            let s = &source.views[si];
            if self.sel.views_add.contains(&ctx.key(&s.name)) {
                creates.push((rq_view(s), false));
            }
        }
        for v in replace_in_place {
            creates.push((v, true));
        }
        let own_c = [None, self.tgt_schema.as_deref()];
        let names: Vec<String> = creates.iter().map(|(v, _)| ctx.key(&v.name)).collect();
        let mut cdeps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (v, _) in &creates {
            let r = references(&v.definition);
            let d = creates.iter().filter(|(w, _)| !ctx.same(&w.name, &v.name) && r.mentions(&w.name, &own_c)).map(|(w, _)| ctx.key(&w.name)).collect();
            cdeps.insert(ctx.key(&v.name), d);
        }
        let (order, cyclic) = topo_sort(&names, &cdeps);
        if !cyclic.is_empty() {
            self.warnings.push(trf("Circular view dependencies: {}", &[&cyclic.join(", ")]));
        }
        let creates_sorted: Vec<(ViewDef, bool)> = order.iter().chain(cyclic.iter()).filter_map(|k| creates.iter().find(|(v, _)| ctx.key(&v.name) == *k).cloned()).collect();
        (drops, creates_sorted)
    }
}

/// A modified view can be replaced in place (`CREATE OR REPLACE`) when the
/// new column list extends the old one unchanged.
fn view_replaceable(ctx: &Ctx<'_>, s: &ViewDef, t: &ViewDef) -> bool {
    if s.materialized || t.materialized || s.columns.is_empty() || t.columns.is_empty() || s.columns.len() < t.columns.len() {
        return false;
    }
    t.columns.iter().zip(&s.columns).all(|(tc, sc)| ctx.same(&tc.name, &sc.name) && ctx.types_equal(sc, tc))
}
