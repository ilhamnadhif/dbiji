//! Object matching and property comparison shared by the diff, the migration
//! planner and the Liquibase generator, so that all of them agree on which
//! objects correspond and which properties differ.

use crate::model::PropertyChange;
use crate::normalize::{SqlNorm, normalize_sql, normalize_type, references, trim_sql};
use crate::options::CompareOptions;
use dbiji_core::config::wildcard_match;
use dbiji_core::ddl::*;
use dbiji_core::{DataKind, ObjectKind};
use std::collections::{BTreeMap, HashMap};

/// Which snapshot a definition comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Source,
    Target,
}

/// Storage option keys whose values change with the data and are never compared.
const VOLATILE_OPTIONS: &[&str] =
    &["auto_increment", "rows", "row_count", "avg_row_length", "data_length", "index_length", "data_free", "create_time", "update_time", "check_time", "checksum"];

pub(crate) fn is_volatile_option(key: &str) -> bool {
    VOLATILE_OPTIONS.contains(&key.to_ascii_lowercase().as_str())
}

/// Result of matching two lists of sub-objects. Indices refer to the
/// original vectors of the source / target definitions.
#[derive(Debug, Clone, Default)]
pub(crate) struct SubMatch {
    /// (source index, target index, differences)
    pub pairs: Vec<(usize, usize, Vec<PropertyChange>)>,
    pub added: Vec<usize>,
    pub removed: Vec<usize>,
}

impl SubMatch {
    pub fn is_unchanged(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.pairs.iter().all(|(_, _, c)| c.is_empty())
    }
    /// Source index matched to target index `t`.
    pub fn source_of(&self, t: usize) -> Option<usize> {
        self.pairs.iter().find(|(_, b, _)| *b == t).map(|(a, _, _)| *a)
    }
    pub fn changes_of_target(&self, t: usize) -> Option<&Vec<PropertyChange>> {
        self.pairs.iter().find(|(_, b, _)| *b == t).map(|(_, _, c)| c)
    }
}

/// Two-pass matching: first by name (when `by_name` says so), then the
/// remaining items by structure.
fn match_two_pass<S, T>(
    src: &[S],
    tgt: &[T],
    src_included: impl Fn(usize) -> bool,
    tgt_included: impl Fn(usize) -> bool,
    by_name: impl Fn(&S, &T) -> bool,
    by_struct: impl Fn(&S, &T) -> bool,
    changes: impl Fn(&S, &T) -> Vec<PropertyChange>,
) -> SubMatch {
    let mut used = vec![false; tgt.len()];
    let mut matched: Vec<Option<usize>> = vec![None; src.len()];
    for (si, s) in src.iter().enumerate() {
        if !src_included(si) {
            continue;
        }
        if let Some(ti) = (0..tgt.len()).find(|&ti| !used[ti] && tgt_included(ti) && by_name(s, &tgt[ti])) {
            used[ti] = true;
            matched[si] = Some(ti);
        }
    }
    for (si, s) in src.iter().enumerate() {
        if !src_included(si) || matched[si].is_some() {
            continue;
        }
        if let Some(ti) = (0..tgt.len()).find(|&ti| !used[ti] && tgt_included(ti) && by_struct(s, &tgt[ti])) {
            used[ti] = true;
            matched[si] = Some(ti);
        }
    }
    let mut m = SubMatch::default();
    for (si, t) in matched.iter().enumerate() {
        if !src_included(si) {
            continue;
        }
        match t {
            Some(ti) => m.pairs.push((si, *ti, changes(&src[si], &tgt[*ti]))),
            None => m.added.push(si),
        }
    }
    for (ti, u) in used.iter().enumerate() {
        if !*u && tgt_included(ti) {
            m.removed.push(ti);
        }
    }
    m
}

/// Schema of a snapshot: its `schema` field, or (when a driver left it
/// empty) the schema its objects are qualified with.
pub(crate) fn snapshot_schema(s: &SchemaSnapshot) -> Option<&str> {
    s.schema
        .as_deref()
        .filter(|x| !x.is_empty())
        .or_else(|| s.tables.iter().find_map(|t| t.schema.as_deref()))
        .or_else(|| s.views.iter().find_map(|v| v.schema.as_deref()))
        .or_else(|| s.sequences.iter().find_map(|q| q.schema.as_deref()))
        .or_else(|| s.routines.iter().find_map(|r| r.schema.as_deref()))
        .or_else(|| s.types.iter().find_map(|t| t.schema.as_deref()))
        .filter(|x| !x.is_empty())
}

/// Comparison context: options plus both snapshots.
pub(crate) struct Ctx<'a> {
    pub opts: &'a CompareOptions,
    pub source: &'a SchemaSnapshot,
    pub target: &'a SchemaSnapshot,
}

pub(crate) fn yes_no(b: bool) -> String {
    if b { "YES".into() } else { "NO".into() }
}

fn opt_string<T: ToString>(v: Option<T>) -> Option<String> {
    v.map(|x| x.to_string())
}

impl<'a> Ctx<'a> {
    pub fn key(&self, s: &str) -> String {
        if self.opts.ignore_identifier_case { s.to_lowercase() } else { s.to_string() }
    }

    pub fn same(&self, a: &str, b: &str) -> bool {
        self.key(a) == self.key(b)
    }

    fn same_opt(&self, a: Option<&str>, b: Option<&str>) -> bool {
        match (a, b) {
            (Some(a), Some(b)) => self.same(a, b),
            _ => false,
        }
    }

    pub fn excluded(&self, name: &str) -> bool {
        self.opts.exclude.iter().any(|p| wildcard_match(p, name))
    }

    pub fn schema_of(&self, side: Side) -> Option<&'a str> {
        match side {
            Side::Source => snapshot_schema(self.source),
            Side::Target => snapshot_schema(self.target),
        }
    }

    pub fn sql_norm(&self, side: Side) -> SqlNorm {
        SqlNorm { own_schema: self.schema_of(side).map(str::to_string), strip_casts: self.opts.cross_dialect, canonical_functions: self.opts.cross_dialect }
    }

    /// Canonical form of a SQL definition.
    pub fn norm_sql(&self, text: &str, side: Side) -> String {
        if self.opts.normalize_sql { normalize_sql(text, &self.sql_norm(side)) } else { trim_sql(text) }
    }

    /// Canonical default expression; `NULL` and empty mean "no default".
    pub fn norm_default(&self, d: Option<&str>, side: Side) -> Option<String> {
        let d = d?.trim();
        if d.is_empty() {
            return None;
        }
        let n = self.norm_sql(d, side);
        if n.eq_ignore_ascii_case("null") || n.is_empty() { None } else { Some(n) }
    }

    pub fn norm_comment(c: Option<&str>) -> Option<String> {
        c.map(str::trim).filter(|c| !c.is_empty()).map(str::to_string)
    }

    pub fn norm_type(&self, t: &str) -> String {
        let mut n = normalize_type(t, self.opts.normalize_type_aliases);
        if let Some(f) = &self.opts.type_normalizer {
            n = f(&n);
        }
        n
    }

    /// Logical kind of a column: the driver supplied kind, refined by the
    /// type name when the kind was left at its default.
    pub fn logical_kind(c: &ColumnDef) -> DataKind {
        // MySQL booleans
        if normalize_type(&c.data_type, true) == "tinyint(1)" {
            return DataKind::Bool;
        }
        let guessed = DataKind::guess_from_type_name(&c.data_type);
        if c.kind == DataKind::Text && guessed != DataKind::Other { guessed } else { c.kind }
    }

    fn is_sequence_default(&self, c: &ColumnDef) -> bool {
        c.default.as_deref().is_some_and(|d| {
            let r = references(d);
            !r.sequences.is_empty()
        })
    }

    /// Auto increment, including PostgreSQL `serial` style defaults in cross-dialect mode.
    pub fn effective_auto_increment(&self, c: &ColumnDef) -> bool {
        c.auto_increment || (self.opts.cross_dialect && self.is_sequence_default(c))
    }

    pub fn effective_default(&self, c: &ColumnDef, side: Side) -> Option<String> {
        if self.opts.cross_dialect && self.is_sequence_default(c) {
            return None;
        }
        self.norm_default(c.default.as_deref(), side)
    }

    /// Normalised type name without a qualifier naming the snapshot's own schema
    /// (`app.order_status` = `order_status`).
    pub fn norm_type_in(&self, t: &str, side: Side) -> String {
        let n = self.norm_type(t);
        if let Some(own) = self.schema_of(side).filter(|s| !s.is_empty()) {
            let own = own.to_lowercase();
            for prefix in [format!("{own}."), format!("\"{own}\".")] {
                if let Some(rest) = n.strip_prefix(&prefix) {
                    return rest.to_string();
                }
            }
        }
        n
    }

    pub fn types_equal(&self, s: &ColumnDef, t: &ColumnDef) -> bool {
        if self.opts.cross_dialect {
            Self::logical_kind(s) == Self::logical_kind(t)
        } else {
            self.norm_type_in(&s.data_type, Side::Source) == self.norm_type_in(&t.data_type, Side::Target)
        }
    }

    /// Target name for a source table (table mapping, else the same name).
    pub fn mapped_table_name(&self, source_name: &str) -> String {
        if let Some(t) = self.opts.table_mapping.get(source_name) {
            return t.clone();
        }
        if self.opts.ignore_identifier_case {
            let k = source_name.to_lowercase();
            if let Some((_, t)) = self.opts.table_mapping.iter().find(|(s, _)| s.to_lowercase() == k) {
                return t.clone();
            }
        }
        source_name.to_string()
    }

    /// True when a reference qualifier denotes the snapshot's own schema.
    pub fn is_own_schema(&self, qualifier: Option<&str>, side: Side) -> bool {
        match (qualifier, self.schema_of(side)) {
            (None, _) => true,
            (Some(q), Some(own)) => q.eq_ignore_ascii_case(own),
            (Some(q), None) => q.is_empty(),
        }
    }

    // ------------------------------------------------------------ columns

    pub fn compare_column(&self, s: &ColumnDef, t: &ColumnDef) -> Vec<PropertyChange> {
        let o = self.opts;
        let mut ch = Vec::new();
        if s.name != t.name && !self.same(&s.name, &t.name) {
            ch.push(PropertyChange::new("name", Some(s.name.clone()), Some(t.name.clone())));
        }
        if o.column_types {
            if !self.types_equal(s, t) {
                ch.push(PropertyChange::new("type", Some(s.data_type.clone()), Some(t.data_type.clone())));
            }
            if let (Some(a), Some(b)) = (&s.collation, &t.collation)
                && !o.cross_dialect
                && !a.eq_ignore_ascii_case(b)
            {
                ch.push(PropertyChange::new("collation", Some(a.clone()), Some(b.clone())));
            }
        }
        if o.nullability && s.nullable != t.nullable {
            ch.push(PropertyChange::new("nullable", Some(yes_no(s.nullable)), Some(yes_no(t.nullable))));
        }
        if o.defaults {
            if self.effective_default(s, Side::Source) != self.effective_default(t, Side::Target) {
                ch.push(PropertyChange::new("default", s.default.clone(), t.default.clone()));
            }
            if self.effective_auto_increment(s) != self.effective_auto_increment(t) {
                ch.push(PropertyChange::new("auto_increment", Some(yes_no(self.effective_auto_increment(s))), Some(yes_no(self.effective_auto_increment(t)))));
            }
            let gs = s.generated.as_deref().map(|g| self.norm_sql(g, Side::Source));
            let gt = t.generated.as_deref().map(|g| self.norm_sql(g, Side::Target));
            if gs != gt {
                ch.push(PropertyChange::new("generated", s.generated.clone(), t.generated.clone()));
            }
        }
        if o.comments && Self::norm_comment(s.comment.as_deref()) != Self::norm_comment(t.comment.as_deref()) {
            ch.push(PropertyChange::new("comment", s.comment.clone(), t.comment.clone()));
        }
        ch
    }

    pub fn column_summary(c: &ColumnDef) -> String {
        let mut s = format!("{} {}", c.name, c.data_type);
        if !c.nullable {
            s.push_str(" NOT NULL");
        }
        if c.auto_increment {
            s.push_str(" AUTO_INCREMENT");
        }
        if let Some(d) = &c.default {
            s.push_str(" DEFAULT ");
            s.push_str(d);
        }
        s
    }

    /// Match columns of a table pair, honouring explicit renames.
    pub fn match_columns(&self, s: &TableDef, t: &TableDef) -> (SubMatch, Vec<(String, String)>) {
        let renames: Vec<(String, String)> = self
            .opts
            .column_renames
            .iter()
            .filter(|r| self.same(&r.table, &s.name))
            .filter(|r| t.columns.iter().any(|c| self.same(&c.name, &r.from)) && s.columns.iter().any(|c| self.same(&c.name, &r.to)))
            .map(|r| (r.from.clone(), r.to.clone()))
            .collect();
        let rename_target = |source_col: &str| -> Option<&str> { renames.iter().find(|(_, to)| self.same(to, source_col)).map(|(from, _)| from.as_str()) };
        let m = match_two_pass(
            &s.columns,
            &t.columns,
            |_| true,
            |_| true,
            |sc: &ColumnDef, tc: &ColumnDef| match rename_target(&sc.name) {
                Some(from) => self.same(from, &tc.name),
                None => self.same(&sc.name, &tc.name) && !renames.iter().any(|(from, _)| self.same(from, &tc.name)),
            },
            |_, _| false,
            |sc, tc| self.compare_column(sc, tc),
        );
        // report renames with real names
        let renames = m
            .pairs
            .iter()
            .filter(|(si, ti, _)| !self.same(&s.columns[*si].name, &t.columns[*ti].name))
            .map(|(si, ti, _)| (t.columns[*ti].name.clone(), s.columns[*si].name.clone()))
            .collect();
        (m, renames)
    }
}

/// Column matching of a table pair: matches, renames, source key -> target name.
type ColumnMatch = (SubMatch, Vec<(String, String)>, HashMap<String, String>);

/// Map a source column name to the target column name.
pub(crate) fn map_col(map: &HashMap<String, String>, ctx: &Ctx<'_>, name: &str) -> String {
    map.get(&ctx.key(name)).cloned().unwrap_or_else(|| name.to_string())
}

/// Analysis of one matched table pair.
#[derive(Debug, Clone)]
pub(crate) struct TablePair<'a> {
    pub source: &'a TableDef,
    pub target: &'a TableDef,
    pub columns: SubMatch,
    /// (target name, source name) of renamed columns.
    pub renames: Vec<(String, String)>,
    /// key(source column) -> target column name.
    pub col_map: HashMap<String, String>,
    pub primary_key: SubMatch,
    pub unique_keys: SubMatch,
    pub checks: SubMatch,
    pub foreign_keys: SubMatch,
    /// Source foreign keys rewritten to target names (parallel to `source.foreign_keys`).
    pub src_fks_mapped: Vec<ForeignKeyDef>,
    pub indexes: SubMatch,
    pub triggers: SubMatch,
    pub partitions: SubMatch,
    pub changes: Vec<PropertyChange>,
}

impl TablePair<'_> {
    pub fn is_unchanged(&self) -> bool {
        self.changes.is_empty()
            && self.columns.is_unchanged()
            && self.primary_key.is_unchanged()
            && self.unique_keys.is_unchanged()
            && self.checks.is_unchanged()
            && self.foreign_keys.is_unchanged()
            && self.indexes.is_unchanged()
            && self.triggers.is_unchanged()
            && self.partitions.is_unchanged()
    }
}

/// Top level objects matched by name.
#[derive(Debug, Clone, Default)]
pub(crate) struct ObjMatch {
    pub pairs: Vec<(usize, usize, Vec<PropertyChange>)>,
    pub added: Vec<usize>,
    pub removed: Vec<usize>,
}

/// Complete analysis of two snapshots.
pub(crate) struct Analysis<'a> {
    pub ctx: Ctx<'a>,
    pub tables: Vec<TablePair<'a>>,
    pub added_tables: Vec<usize>,
    pub removed_tables: Vec<usize>,
    pub views: ObjMatch,
    pub sequences: ObjMatch,
    pub routines: ObjMatch,
    pub types: ObjMatch,
}

pub(crate) fn pk_vec(t: &TableDef) -> Vec<KeyDef> {
    t.primary_key.iter().filter(|k| !k.columns.is_empty()).cloned().collect()
}

/// True when `idx` is a secondary index (not backing the primary key or a unique key).
pub(crate) fn is_secondary_index(ctx: &Ctx<'_>, t: &TableDef, idx: &IndexDef) -> bool {
    if idx.primary {
        return false;
    }
    if !idx.unique {
        return true;
    }
    let cols: Vec<String> = idx.columns.iter().map(|c| ctx.key(&c.name)).collect();
    let same_cols = |k: &KeyDef| k.columns.iter().map(|c| ctx.key(c)).collect::<Vec<_>>() == cols;
    let backs_unique = t.unique_keys.iter().any(|u| u.name.as_deref().is_some_and(|n| ctx.same(n, &idx.name)) || same_cols(u));
    let backs_pk = t.primary_key.as_ref().is_some_and(|pk| pk.name.as_deref().is_some_and(|n| ctx.same(n, &idx.name)) || same_cols(pk));
    !(backs_unique || backs_pk)
}

/// True when a sequence is the implicit sequence of an identity / auto
/// increment column of a table in the same snapshot.
pub(crate) fn is_implicit_sequence(ctx: &Ctx<'_>, snap: &SchemaSnapshot, seq: &SequenceDef) -> bool {
    let Some(owned) = seq.owned_by.as_deref() else { return false };
    let Some((table, column)) = owned.rsplit_once('.') else { return false };
    let table = table.rsplit('.').next().unwrap_or(table);
    snap.tables.iter().filter(|t| ctx.same(&t.name, table)).any(|t| t.columns.iter().any(|c| ctx.same(&c.name, column) && c.auto_increment))
}

fn routine_kind(r: &RoutineDef) -> ObjectKind {
    match r.kind {
        Some(k @ (ObjectKind::Procedure | ObjectKind::Aggregate)) => k,
        _ => ObjectKind::Function,
    }
}

impl<'a> Analysis<'a> {
    pub fn new(source: &'a SchemaSnapshot, target: &'a SchemaSnapshot, opts: &'a CompareOptions) -> Self {
        let ctx = Ctx { opts, source, target };
        let mut a = Analysis {
            ctx,
            tables: Vec::new(),
            added_tables: Vec::new(),
            removed_tables: Vec::new(),
            views: ObjMatch::default(),
            sequences: ObjMatch::default(),
            routines: ObjMatch::default(),
            types: ObjMatch::default(),
        };
        if opts.tables {
            a.match_tables();
        }
        if opts.views {
            a.views = a.match_views();
        }
        if opts.sequences {
            a.sequences = a.match_sequences();
        }
        if opts.routines {
            a.routines = a.match_routines();
        }
        if opts.types {
            a.types = a.match_types();
        }
        a
    }

    // ------------------------------------------------------------------ tables

    fn match_tables(&mut self) {
        let ctx = &self.ctx;
        let src = &ctx.source.tables;
        let tgt = &ctx.target.tables;
        let mut used = vec![false; tgt.len()];
        let mut pairs: Vec<(usize, usize)> = Vec::new();
        for (si, s) in src.iter().enumerate() {
            if ctx.excluded(&s.name) {
                continue;
            }
            let want = ctx.mapped_table_name(&s.name);
            match (0..tgt.len()).find(|&ti| !used[ti] && !ctx.excluded(&tgt[ti].name) && ctx.same(&tgt[ti].name, &want)) {
                Some(ti) => {
                    used[ti] = true;
                    pairs.push((si, ti));
                }
                None => self.added_tables.push(si),
            }
        }
        for (ti, t) in tgt.iter().enumerate() {
            if !used[ti] && !ctx.excluded(&t.name) {
                self.removed_tables.push(ti);
            }
        }
        // phase 1: columns (needed by foreign keys of other tables)
        let mut col_matches: Vec<ColumnMatch> = Vec::new();
        for &(si, ti) in &pairs {
            let (m, renames) = ctx.match_columns(&src[si], &tgt[ti]);
            let mut map = HashMap::new();
            for (a, b, _) in &m.pairs {
                map.insert(ctx.key(&src[si].columns[*a].name), tgt[ti].columns[*b].name.clone());
            }
            col_matches.push((m, renames, map));
        }
        let maps_by_source: HashMap<String, (String, HashMap<String, String>)> =
            pairs.iter().zip(col_matches.iter()).map(|(&(si, ti), (_, _, map))| (ctx.key(&src[si].name), (tgt[ti].name.clone(), map.clone()))).collect();
        for (&(si, ti), (columns, renames, col_map)) in pairs.iter().zip(col_matches) {
            let s = &src[si];
            let t = &tgt[ti];
            let pair = self.analyse_table(s, t, columns, renames, col_map, &maps_by_source);
            self.tables.push(pair);
        }
    }

    /// Source foreign key rewritten to target table / column names.
    pub fn map_source_fk(
        &self,
        fk: &ForeignKeyDef,
        own_target_name: &str,
        own_map: &HashMap<String, String>,
        maps_by_source: &HashMap<String, (String, HashMap<String, String>)>,
    ) -> ForeignKeyDef {
        let ctx = &self.ctx;
        let mut m = fk.clone();
        m.columns = fk.columns.iter().map(|c| map_col(own_map, ctx, c)).collect();
        m.schema = snapshot_schema(ctx.target).map(str::to_string);
        m.table = own_target_name.to_string();
        if ctx.is_own_schema(fk.ref_schema.as_deref(), Side::Source) {
            m.ref_schema = snapshot_schema(ctx.target).map(str::to_string);
            match maps_by_source.get(&ctx.key(&fk.ref_table)) {
                Some((tname, rmap)) => {
                    m.ref_table = tname.clone();
                    m.ref_columns = fk.ref_columns.iter().map(|c| map_col(rmap, ctx, c)).collect();
                }
                None => m.ref_table = ctx.mapped_table_name(&fk.ref_table),
            }
        }
        m
    }

    fn analyse_table(
        &self,
        s: &'a TableDef,
        t: &'a TableDef,
        columns: SubMatch,
        renames: Vec<(String, String)>,
        col_map: HashMap<String, String>,
        maps_by_source: &HashMap<String, (String, HashMap<String, String>)>,
    ) -> TablePair<'a> {
        let ctx = &self.ctx;
        let o = ctx.opts;
        let names = !o.ignore_constraint_names;
        let mcol = |c: &str| ctx.key(&map_col(&col_map, ctx, c));
        let cols_eq = |a: &[String], b: &[String]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| mcol(x) == ctx.key(y));
        let list = |v: &[String]| v.join(", ");
        let name_change = |a: Option<&str>, b: Option<&str>| -> Option<PropertyChange> {
            if names && a.is_some() && b.is_some() && !ctx.same_opt(a, b) { Some(PropertyChange::new("name", a.map(str::to_string), b.map(str::to_string))) } else { None }
        };
        let columns = if o.columns { columns } else { SubMatch::default() };

        let primary_key = if o.primary_keys {
            let (sp, tp) = (pk_vec(s), pk_vec(t));
            match_two_pass(
                &sp,
                &tp,
                |_| true,
                |_| true,
                |_, _| true,
                |_, _| true,
                |a: &KeyDef, b: &KeyDef| {
                    let mut ch = Vec::new();
                    if !cols_eq(&a.columns, &b.columns) {
                        ch.push(PropertyChange::new("columns", Some(list(&a.columns)), Some(list(&b.columns))));
                    }
                    ch.extend(name_change(a.name.as_deref(), b.name.as_deref()));
                    ch
                },
            )
        } else {
            SubMatch::default()
        };

        let unique_keys = if o.unique_keys {
            match_two_pass(
                &s.unique_keys,
                &t.unique_keys,
                |_| true,
                |_| true,
                |a: &KeyDef, b: &KeyDef| names && ctx.same_opt(a.name.as_deref(), b.name.as_deref()),
                |a, b| cols_eq(&a.columns, &b.columns),
                |a, b| {
                    let mut ch = Vec::new();
                    if !cols_eq(&a.columns, &b.columns) {
                        ch.push(PropertyChange::new("columns", Some(list(&a.columns)), Some(list(&b.columns))));
                    }
                    ch.extend(name_change(a.name.as_deref(), b.name.as_deref()));
                    ch
                },
            )
        } else {
            SubMatch::default()
        };

        let checks = if o.checks {
            let ne = |c: &CheckDef, side| ctx.norm_sql(&c.expression, side);
            match_two_pass(
                &s.checks,
                &t.checks,
                |_| true,
                |_| true,
                |a: &CheckDef, b: &CheckDef| names && ctx.same_opt(a.name.as_deref(), b.name.as_deref()),
                |a, b| ne(a, Side::Source) == ne(b, Side::Target),
                |a, b| {
                    let mut ch = Vec::new();
                    if ne(a, Side::Source) != ne(b, Side::Target) {
                        ch.push(PropertyChange::new("expression", Some(a.expression.clone()), Some(b.expression.clone())));
                    }
                    ch.extend(name_change(a.name.as_deref(), b.name.as_deref()));
                    ch
                },
            )
        } else {
            SubMatch::default()
        };

        let src_fks_mapped: Vec<ForeignKeyDef> = s.foreign_keys.iter().map(|fk| self.map_source_fk(fk, &t.name, &col_map, maps_by_source)).collect();
        let foreign_keys = if o.foreign_keys {
            let same_ref_schema = |a: &ForeignKeyDef, b: &ForeignKeyDef| {
                let own_a = ctx.is_own_schema(a.ref_schema.as_deref(), Side::Target);
                let own_b = ctx.is_own_schema(b.ref_schema.as_deref(), Side::Target);
                (own_a && own_b) || ctx.same_opt(a.ref_schema.as_deref(), b.ref_schema.as_deref())
            };
            let keys = |v: &[String]| v.iter().map(|c| ctx.key(c)).collect::<Vec<_>>();
            let structure = |a: &ForeignKeyDef, b: &ForeignKeyDef| {
                keys(&a.columns) == keys(&b.columns) && ctx.same(&a.ref_table, &b.ref_table) && keys(&a.ref_columns) == keys(&b.ref_columns) && same_ref_schema(a, b)
            };
            let fk_ref = |f: &ForeignKeyDef| format!("{}({})", f.ref_table, list(&f.ref_columns));
            let mut m = match_two_pass(
                &src_fks_mapped,
                &t.foreign_keys,
                |_| true,
                |_| true,
                |a: &ForeignKeyDef, b: &ForeignKeyDef| names && ctx.same_opt(a.name.as_deref(), b.name.as_deref()),
                structure,
                |a, b| {
                    let mut ch = Vec::new();
                    if keys(&a.columns) != keys(&b.columns) {
                        ch.push(PropertyChange::new("columns", Some(list(&a.columns)), Some(list(&b.columns))));
                    }
                    if !(ctx.same(&a.ref_table, &b.ref_table) && keys(&a.ref_columns) == keys(&b.ref_columns) && same_ref_schema(a, b)) {
                        ch.push(PropertyChange::new("references", Some(fk_ref(a)), Some(fk_ref(b))));
                    }
                    if a.on_update != b.on_update {
                        ch.push(PropertyChange::new("on_update", Some(a.on_update.sql().into()), Some(b.on_update.sql().into())));
                    }
                    if a.on_delete != b.on_delete {
                        ch.push(PropertyChange::new("on_delete", Some(a.on_delete.sql().into()), Some(b.on_delete.sql().into())));
                    }
                    if a.deferrable != b.deferrable {
                        ch.push(PropertyChange::new("deferrable", Some(yes_no(a.deferrable)), Some(yes_no(b.deferrable))));
                    }
                    ch.extend(name_change(a.name.as_deref(), b.name.as_deref()));
                    ch
                },
            );
            // indices of src_fks_mapped equal those of s.foreign_keys
            m.pairs.sort_by_key(|p| p.0);
            m
        } else {
            SubMatch::default()
        };

        let indexes = if o.indexes {
            let idx_cols = |i: &IndexDef, side: Side, map: bool| -> Vec<String> {
                i.columns
                    .iter()
                    .map(|c| {
                        let base = if c.is_expression {
                            format!("({})", ctx.norm_sql(&c.name, side))
                        } else if map {
                            mcol(&c.name)
                        } else {
                            ctx.key(&c.name)
                        };
                        if c.descending { format!("{base} desc") } else { base }
                    })
                    .collect()
            };
            let method = |i: &IndexDef| i.method.as_deref().map(str::to_lowercase).unwrap_or_else(|| "btree".into());
            let pred = |i: &IndexDef, side| i.predicate.as_deref().map(|p| ctx.norm_sql(p, side));
            let include = |i: &IndexDef, map: bool| -> Vec<String> {
                let mut v: Vec<String> = i.include.iter().map(|c| if map { mcol(c) } else { ctx.key(c) }).collect();
                v.sort();
                v
            };
            let structure = |a: &IndexDef, b: &IndexDef| {
                idx_cols(a, Side::Source, true) == idx_cols(b, Side::Target, false)
                    && a.unique == b.unique
                    && pred(a, Side::Source) == pred(b, Side::Target)
                    && method(a) == method(b)
                    && include(a, true) == include(b, false)
            };
            let desc = |i: &IndexDef| i.columns.iter().map(|c| if c.descending { format!("{} DESC", c.name) } else { c.name.clone() }).collect::<Vec<_>>().join(", ");
            match_two_pass(
                &s.indexes,
                &t.indexes,
                |i| is_secondary_index(ctx, s, &s.indexes[i]),
                |i| is_secondary_index(ctx, t, &t.indexes[i]),
                |a: &IndexDef, b: &IndexDef| names && ctx.same(&a.name, &b.name),
                structure,
                |a, b| {
                    let mut ch = Vec::new();
                    if idx_cols(a, Side::Source, true) != idx_cols(b, Side::Target, false) {
                        ch.push(PropertyChange::new("columns", Some(desc(a)), Some(desc(b))));
                    }
                    if a.unique != b.unique {
                        ch.push(PropertyChange::new("unique", Some(yes_no(a.unique)), Some(yes_no(b.unique))));
                    }
                    if pred(a, Side::Source) != pred(b, Side::Target) {
                        ch.push(PropertyChange::new("predicate", a.predicate.clone(), b.predicate.clone()));
                    }
                    if method(a) != method(b) {
                        ch.push(PropertyChange::new("method", a.method.clone(), b.method.clone()));
                    }
                    if include(a, true) != include(b, false) {
                        ch.push(PropertyChange::new("include", Some(list(&a.include)), Some(list(&b.include))));
                    }
                    if names && !ctx.same(&a.name, &b.name) {
                        ch.push(PropertyChange::new("name", Some(a.name.clone()), Some(b.name.clone())));
                    }
                    ch
                },
            )
        } else {
            SubMatch::default()
        };

        let triggers = if o.triggers {
            match_two_pass(
                &s.triggers,
                &t.triggers,
                |_| true,
                |_| true,
                |a: &TriggerDef, b: &TriggerDef| ctx.same(&a.name, &b.name),
                |_, _| false,
                |a, b| {
                    let mut ch = Vec::new();
                    if !a.definition.trim().is_empty() && !b.definition.trim().is_empty() {
                        if ctx.norm_sql(&a.definition, Side::Source) != ctx.norm_sql(&b.definition, Side::Target) {
                            ch.push(PropertyChange::new("definition", Some(a.definition.clone()), Some(b.definition.clone())));
                        }
                    } else {
                        if !a.timing.eq_ignore_ascii_case(&b.timing) {
                            ch.push(PropertyChange::new("timing", Some(a.timing.clone()), Some(b.timing.clone())));
                        }
                        let ev = |t: &TriggerDef| {
                            let mut v: Vec<String> = t.events.iter().map(|e| e.to_uppercase()).collect();
                            v.sort();
                            v.join(", ")
                        };
                        if ev(a) != ev(b) {
                            ch.push(PropertyChange::new("events", Some(ev(a)), Some(ev(b))));
                        }
                        if a.for_each_row != b.for_each_row {
                            ch.push(PropertyChange::new("for_each_row", Some(yes_no(a.for_each_row)), Some(yes_no(b.for_each_row))));
                        }
                    }
                    if a.enabled != b.enabled {
                        ch.push(PropertyChange::new("enabled", Some(yes_no(a.enabled)), Some(yes_no(b.enabled))));
                    }
                    ch
                },
            )
        } else {
            SubMatch::default()
        };

        let partitions = if o.partitions {
            match_two_pass(
                &s.partitions,
                &t.partitions,
                |_| true,
                |_| true,
                |a: &PartitionDef, b: &PartitionDef| ctx.same(&a.name, &b.name),
                |_, _| false,
                |a, b| {
                    if ctx.norm_sql(&a.bound, Side::Source) != ctx.norm_sql(&b.bound, Side::Target) {
                        vec![PropertyChange::new("bound", Some(a.bound.clone()), Some(b.bound.clone()))]
                    } else {
                        Vec::new()
                    }
                },
            )
        } else {
            SubMatch::default()
        };

        // table level properties
        let mut changes = Vec::new();
        if o.comments && Ctx::norm_comment(s.comment.as_deref()) != Ctx::norm_comment(t.comment.as_deref()) {
            changes.push(PropertyChange::new("comment", s.comment.clone(), t.comment.clone()));
        }
        if o.table_options {
            let norm_opts = |m: &BTreeMap<String, String>| -> BTreeMap<String, String> {
                m.iter().filter(|(k, _)| !is_volatile_option(k)).map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_lowercase())).collect()
            };
            let (so, to) = (norm_opts(&s.options), norm_opts(&t.options));
            let keys: std::collections::BTreeSet<&String> = so.keys().chain(to.keys()).collect();
            for k in keys {
                if so.get(k) != to.get(k) {
                    let orig = |m: &BTreeMap<String, String>| m.iter().find(|(kk, _)| kk.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
                    changes.push(PropertyChange::new(format!("option {k}"), orig(&s.options), orig(&t.options)));
                }
            }
        }
        if o.partitions {
            if s.table_kind != t.table_kind {
                changes.push(PropertyChange::new("table_kind", Some(format!("{:?}", s.table_kind)), Some(format!("{:?}", t.table_kind))));
            }
            let part = |p: &Option<PartitioningDef>, side| p.as_ref().map(|p| format!("{} ({})", p.strategy.to_uppercase(), ctx.norm_sql(&p.key, side)));
            if part(&s.partitioning, Side::Source) != part(&t.partitioning, Side::Target) {
                let show = |p: &Option<PartitioningDef>| p.as_ref().map(|p| format!("{} ({})", p.strategy, p.key));
                changes.push(PropertyChange::new("partitioning", show(&s.partitioning), show(&t.partitioning)));
            }
        }
        if o.columns && !o.ignore_column_order {
            let mut by_target: Vec<(usize, usize)> = columns.pairs.iter().map(|(a, b, _)| (*a, *b)).collect();
            let src_order: Vec<usize> = {
                let mut v = by_target.clone();
                v.sort_by_key(|p| p.0);
                v.iter().map(|p| p.1).collect()
            };
            by_target.sort_by_key(|p| p.1);
            let tgt_order: Vec<usize> = by_target.iter().map(|p| p.1).collect();
            if src_order != tgt_order {
                let names = |order: &[usize]| order.iter().map(|&i| t.columns[i].name.clone()).collect::<Vec<_>>().join(", ");
                changes.push(PropertyChange::new("column_order", Some(names(&src_order)), Some(names(&tgt_order))));
            }
        }

        TablePair { source: s, target: t, columns, renames, col_map, primary_key, unique_keys, checks, foreign_keys, src_fks_mapped, indexes, triggers, partitions, changes }
    }

    /// Column maps of matched tables keyed by source table key.
    pub fn maps_by_source(&self) -> HashMap<String, (String, HashMap<String, String>)> {
        self.tables.iter().map(|p| (self.ctx.key(&p.source.name), (p.target.name.clone(), p.col_map.clone()))).collect()
    }

    // ------------------------------------------------------------ other objects

    fn match_views(&self) -> ObjMatch {
        let ctx = &self.ctx;
        let (src, tgt) = (&ctx.source.views, &ctx.target.views);
        let m = match_two_pass(
            src,
            tgt,
            |i| !ctx.excluded(&src[i].name),
            |i| !ctx.excluded(&tgt[i].name),
            |a: &ViewDef, b: &ViewDef| ctx.same(&a.name, &b.name),
            |_, _| false,
            |a, b| {
                let mut ch = Vec::new();
                if a.materialized != b.materialized {
                    ch.push(PropertyChange::new("materialized", Some(yes_no(a.materialized)), Some(yes_no(b.materialized))));
                }
                if ctx.norm_sql(&a.definition, Side::Source) != ctx.norm_sql(&b.definition, Side::Target) {
                    ch.push(PropertyChange::new("definition", Some(a.definition.clone()), Some(b.definition.clone())));
                }
                if ctx.opts.comments && Ctx::norm_comment(a.comment.as_deref()) != Ctx::norm_comment(b.comment.as_deref()) {
                    ch.push(PropertyChange::new("comment", a.comment.clone(), b.comment.clone()));
                }
                ch
            },
        );
        ObjMatch { pairs: m.pairs, added: m.added, removed: m.removed }
    }

    fn match_sequences(&self) -> ObjMatch {
        let ctx = &self.ctx;
        let (src, tgt) = (&ctx.source.sequences, &ctx.target.sequences);
        let m = match_two_pass(
            src,
            tgt,
            |i| !ctx.excluded(&src[i].name) && !is_implicit_sequence(ctx, ctx.source, &src[i]),
            |i| !ctx.excluded(&tgt[i].name) && !is_implicit_sequence(ctx, ctx.target, &tgt[i]),
            |a: &SequenceDef, b: &SequenceDef| ctx.same(&a.name, &b.name),
            |_, _| false,
            |a, b| {
                let mut ch = Vec::new();
                if let (Some(x), Some(y)) = (&a.data_type, &b.data_type)
                    && !ctx.opts.cross_dialect
                    && ctx.norm_type(x) != ctx.norm_type(y)
                {
                    ch.push(PropertyChange::new("data_type", Some(x.clone()), Some(y.clone())));
                }
                if a.increment.unwrap_or(1) != b.increment.unwrap_or(1) {
                    ch.push(PropertyChange::new("increment", opt_string(a.increment), opt_string(b.increment)));
                }
                for (prop, x, y) in [("min_value", a.min_value, b.min_value), ("max_value", a.max_value, b.max_value)] {
                    if let (Some(x), Some(y)) = (x, y)
                        && x != y
                    {
                        ch.push(PropertyChange::new(prop, Some(x.to_string()), Some(y.to_string())));
                    }
                }
                if a.cache.unwrap_or(1) != b.cache.unwrap_or(1) {
                    ch.push(PropertyChange::new("cache", opt_string(a.cache), opt_string(b.cache)));
                }
                if a.cycle != b.cycle {
                    ch.push(PropertyChange::new("cycle", Some(yes_no(a.cycle)), Some(yes_no(b.cycle))));
                }
                if ctx.opts.sequence_start_values
                    && let (Some(x), Some(y)) = (a.start, b.start)
                    && x != y
                {
                    ch.push(PropertyChange::new("start", Some(x.to_string()), Some(y.to_string())));
                }
                ch
            },
        );
        ObjMatch { pairs: m.pairs, added: m.added, removed: m.removed }
    }

    /// Identity key of a routine: name when unique among routines of the same
    /// kind on both sides, otherwise name + normalised argument list.
    pub fn routine_key(&self, r: &RoutineDef) -> String {
        let ctx = &self.ctx;
        let kind = routine_kind(r);
        let count = |list: &[RoutineDef]| list.iter().filter(|x| routine_kind(x) == kind && ctx.same(&x.name, &r.name)).count();
        let overloaded = count(&ctx.source.routines) > 1 || count(&ctx.target.routines) > 1;
        if overloaded {
            let args = normalize_sql(&r.arguments, &SqlNorm::default());
            format!("{:?}:{}({args})", kind, ctx.key(&r.name))
        } else {
            format!("{:?}:{}", kind, ctx.key(&r.name))
        }
    }

    fn match_routines(&self) -> ObjMatch {
        let ctx = &self.ctx;
        let (src, tgt) = (&ctx.source.routines, &ctx.target.routines);
        let m = match_two_pass(
            src,
            tgt,
            |i| !ctx.excluded(&src[i].name),
            |i| !ctx.excluded(&tgt[i].name),
            |a: &RoutineDef, b: &RoutineDef| self.routine_key(a) == self.routine_key(b),
            |_, _| false,
            |a, b| {
                let mut ch = Vec::new();
                let args = |r: &RoutineDef| normalize_sql(&r.arguments, &SqlNorm::default());
                if args(a) != args(b) {
                    ch.push(PropertyChange::new("arguments", Some(a.arguments.clone()), Some(b.arguments.clone())));
                }
                let ret = |r: &RoutineDef| r.returns.as_deref().map(|t| ctx.norm_type(t));
                if ret(a) != ret(b) {
                    ch.push(PropertyChange::new("returns", a.returns.clone(), b.returns.clone()));
                }
                let lang = |r: &RoutineDef| r.language.as_deref().map(str::to_lowercase);
                if lang(a).is_some() && lang(b).is_some() && lang(a) != lang(b) {
                    ch.push(PropertyChange::new("language", a.language.clone(), b.language.clone()));
                }
                if ctx.norm_sql(&a.definition, Side::Source) != ctx.norm_sql(&b.definition, Side::Target) {
                    ch.push(PropertyChange::new("definition", Some(a.definition.clone()), Some(b.definition.clone())));
                }
                ch
            },
        );
        ObjMatch { pairs: m.pairs, added: m.added, removed: m.removed }
    }

    fn match_types(&self) -> ObjMatch {
        let ctx = &self.ctx;
        let (src, tgt) = (&ctx.source.types, &ctx.target.types);
        let m = match_two_pass(
            src,
            tgt,
            |i| !ctx.excluded(&src[i].name),
            |i| !ctx.excluded(&tgt[i].name),
            |a: &TypeDef, b: &TypeDef| ctx.same(&a.name, &b.name),
            |_, _| false,
            |a, b| {
                let mut ch = Vec::new();
                if !a.category.eq_ignore_ascii_case(&b.category) {
                    ch.push(PropertyChange::new("category", Some(a.category.clone()), Some(b.category.clone())));
                }
                let el = |t: &TypeDef, side| t.elements.iter().map(|e| ctx.norm_sql(e, side)).collect::<Vec<_>>();
                if el(a, Side::Source) != el(b, Side::Target) {
                    ch.push(PropertyChange::new("elements", Some(a.elements.join(", ")), Some(b.elements.join(", "))));
                }
                if ctx.norm_sql(&a.definition, Side::Source) != ctx.norm_sql(&b.definition, Side::Target) {
                    ch.push(PropertyChange::new("definition", Some(a.definition.clone()), Some(b.definition.clone())));
                }
                ch
            },
        );
        ObjMatch { pairs: m.pairs, added: m.added, removed: m.removed }
    }
}
