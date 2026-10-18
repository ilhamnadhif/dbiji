//! Logical simulation of a [`MigrationPlan`] on a schema model.
//!
//! [`apply_plan`] applies every step's [`MigrationOp`] to a copy of the target
//! snapshot and enforces the dependency rules a strict database (PostgreSQL)
//! applies: no dropping of objects still referenced, no references to objects
//! created later in the plan, foreign keys need a matching unique key, index
//! names are unique per schema, and so on. A plan that applies cleanly and
//! yields a snapshot without differences to the source is complete and
//! correctly ordered.

use crate::normalize::{SqlNorm, normalize_sql, references};
use crate::plan::{MigrationOp, MigrationPlan};
use dbiji_core::ddl::*;
use std::collections::BTreeSet;

/// Apply the logical effect of `plan` to `target`. Returns the resulting
/// schema, or a description of the first step that would fail.
pub fn apply_plan(plan: &MigrationPlan, target: &SchemaSnapshot) -> Result<SchemaSnapshot, String> {
    let mut sim = Sim { snap: target.clone(), plan, step: 0 };
    for (i, step) in plan.steps.iter().enumerate() {
        sim.step = i;
        sim.apply(&step.op).map_err(|e| format!("step {} ({}): {e}", i + 1, step.description))?;
    }
    Ok(sim.snap)
}

struct Sim<'a> {
    snap: SchemaSnapshot,
    plan: &'a MigrationPlan,
    step: usize,
}

fn args_key(a: &str) -> String {
    normalize_sql(a, &SqlNorm::default())
}

fn same_fk(a: &ForeignKeyDef, b: &ForeignKeyDef) -> bool {
    match (&a.name, &b.name) {
        (Some(x), Some(y)) => x == y,
        _ => a.columns == b.columns && a.ref_table == b.ref_table && a.ref_columns == b.ref_columns,
    }
}

fn set(cols: &[String]) -> BTreeSet<String> {
    cols.iter().cloned().collect()
}

fn base_type(t: &str) -> String {
    let t = t.trim().trim_end_matches("[]").trim();
    let t = t.split('(').next().unwrap_or(t).trim();
    let t = t.rsplit('.').next().unwrap_or(t);
    t.trim_matches('"').to_lowercase()
}

impl Sim<'_> {
    fn own(&self) -> [Option<&str>; 2] {
        [None, crate::analysis::snapshot_schema(&self.snap)]
    }

    fn is_own_schema(&self, s: Option<&str>) -> bool {
        match (s, crate::analysis::snapshot_schema(&self.snap)) {
            (None, _) => true,
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            (Some(a), None) => a.is_empty(),
        }
    }

    fn table(&self, name: &str) -> Result<&TableDef, String> {
        self.snap.tables.iter().find(|t| t.name == name).ok_or_else(|| format!("table {name} does not exist"))
    }

    fn table_mut(&mut self, name: &str) -> Result<&mut TableDef, String> {
        self.snap.tables.iter_mut().find(|t| t.name == name).ok_or_else(|| format!("table {name} does not exist"))
    }

    /// Is `name` (of the given category) created by a later step of the plan?
    fn created_later(&self, name: &str, category: &str) -> bool {
        self.plan.steps.iter().skip(self.step + 1).any(|s| {
            let n = match (&s.op, category) {
                (MigrationOp::CreateTable { table, .. }, "relation") => Some(&table.name),
                (MigrationOp::CreateView { view, .. }, "relation") => Some(&view.name),
                (MigrationOp::CreateSequence { sequence }, "sequence") => Some(&sequence.name),
                (MigrationOp::CreateRoutine { routine, .. }, "routine") => Some(&routine.name),
                (MigrationOp::CreateType { ty }, "type") => Some(&ty.name),
                _ => None,
            };
            n.is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
    }

    fn relation_exists(&self, name: &str) -> bool {
        self.snap.tables.iter().any(|t| t.name.eq_ignore_ascii_case(name)) || self.snap.views.iter().any(|v| v.name.eq_ignore_ascii_case(name))
    }

    /// Check expressions (defaults, checks, index expressions, view bodies,
    /// trigger definitions) for references to objects created later.
    fn check_expression_refs(&self, text: &str, what: &str) -> Result<(), String> {
        let r = references(text);
        let own = self.own();
        for s in &r.sequences {
            if r.uses_sequence(&s.name, &own) && !self.snap.sequences.iter().any(|x| x.name.eq_ignore_ascii_case(&s.name)) && self.created_later(&s.name, "sequence") {
                return Err(format!("{what} uses sequence {} before it is created", s.name));
            }
        }
        for c in &r.calls {
            if r.calls(&c.name, &own)
                && !self.snap.routines.iter().any(|x| x.name.eq_ignore_ascii_case(&c.name))
                && self.created_later(&c.name, "routine")
            {
                return Err(format!("{what} calls routine {} before it is created", c.name));
            }
        }
        Ok(())
    }

    fn check_view_refs(&self, v: &ViewDef) -> Result<(), String> {
        let r = references(&v.definition);
        let own = self.own();
        for i in &r.idents {
            if i.name.eq_ignore_ascii_case(&v.name) {
                continue;
            }
            if r.mentions(&i.name, &own) && !self.relation_exists(&i.name) && self.created_later(&i.name, "relation") {
                return Err(format!("view {} references {} before it is created", v.name, i.name));
            }
        }
        self.check_expression_refs(&v.definition, &format!("view {}", v.name))
    }

    fn check_column_refs(&self, table: &str, c: &ColumnDef) -> Result<(), String> {
        let what = format!("column {table}.{}", c.name);
        if let Some(d) = &c.default {
            self.check_expression_refs(d, &what)?;
        }
        if let Some(g) = &c.generated {
            self.check_expression_refs(g, &what)?;
        }
        let bt = base_type(&c.data_type);
        if !self.snap.types.iter().any(|t| t.name.eq_ignore_ascii_case(&bt)) && self.created_later(&bt, "type") {
            return Err(format!("{what} uses type {bt} before it is created"));
        }
        Ok(())
    }

    fn index_name_taken(&self, name: &str) -> Option<String> {
        for t in &self.snap.tables {
            if t.indexes.iter().any(|i| i.name == name) {
                return Some(t.name.clone());
            }
        }
        None
    }

    /// Does the table offer a primary key / unique key / unique index on exactly `cols`?
    fn has_unique_on(t: &TableDef, cols: &BTreeSet<String>) -> bool {
        t.primary_key.as_ref().is_some_and(|k| set(&k.columns) == *cols)
            || t.unique_keys.iter().any(|k| set(&k.columns) == *cols)
            || t.indexes.iter().any(|i| i.unique && i.columns.iter().map(|c| c.name.clone()).collect::<BTreeSet<_>>() == *cols)
    }

    /// Foreign keys (owner table, fk) referencing `table` on exactly `cols`.
    fn fks_on(&self, table: &str, cols: &BTreeSet<String>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for t in &self.snap.tables {
            for f in &t.foreign_keys {
                if f.ref_table == table && self.is_own_schema(f.ref_schema.as_deref()) && set(&f.ref_columns) == *cols {
                    out.push((t.name.clone(), f.name.clone().unwrap_or_default()));
                }
            }
        }
        out
    }

    fn views_mentioning(&self, name: &str) -> Vec<String> {
        let own = self.own();
        self.snap
            .views
            .iter()
            .filter(|v| !v.name.eq_ignore_ascii_case(name) && references(&v.definition).mentions(name, &own))
            .map(|v| v.name.clone())
            .collect()
    }

    fn drop_key_backing_index(t: &mut TableDef, key: &KeyDef, primary: bool) {
        t.indexes.retain(|i| {
            let by_name = key.name.as_deref().is_some_and(|n| n == i.name);
            !(by_name || (primary && i.primary))
        });
    }

    fn apply(&mut self, op: &MigrationOp) -> Result<(), String> {
        match op {
            MigrationOp::DropForeignKey { table, fk } => {
                let t = self.table_mut(table)?;
                let pos = t.foreign_keys.iter().position(|f| same_fk(f, fk)).ok_or_else(|| format!("foreign key {:?} not found on {table}", fk.name))?;
                t.foreign_keys.remove(pos);
            }
            MigrationOp::DropTrigger { table, trigger } => {
                let t = self.table_mut(table)?;
                let pos = t.triggers.iter().position(|x| x.name == trigger.name).ok_or_else(|| format!("trigger {} not found on {table}", trigger.name))?;
                t.triggers.remove(pos);
            }
            MigrationOp::DropView { view } => {
                let pos = self.snap.views.iter().position(|v| v.name == view.name).ok_or_else(|| format!("view {} does not exist", view.name))?;
                let deps = self.views_mentioning(&view.name);
                if !deps.is_empty() {
                    return Err(format!("view {} is used by {}", view.name, deps.join(", ")));
                }
                self.snap.views.remove(pos);
            }
            MigrationOp::DropRoutine { routine } => {
                let pos = self
                    .snap
                    .routines
                    .iter()
                    .position(|r| r.name == routine.name && args_key(&r.arguments) == args_key(&routine.arguments))
                    .ok_or_else(|| format!("routine {} does not exist", routine.name))?;
                let own = self.own();
                if let Some(v) = self.snap.views.iter().find(|v| references(&v.definition).calls(&routine.name, &own)) {
                    return Err(format!("routine {} is used by view {}", routine.name, v.name));
                }
                for t in &self.snap.tables {
                    if let Some(tr) = t.triggers.iter().find(|tr| {
                        let r = references(&tr.definition);
                        r.calls(&routine.name, &own) || r.mentions(&routine.name, &own)
                    }) {
                        return Err(format!("routine {} is used by trigger {} on {}", routine.name, tr.name, t.name));
                    }
                }
                self.snap.routines.remove(pos);
            }
            MigrationOp::DropIndex { table, index } => {
                let t = self.table_mut(table)?;
                let pos = t.indexes.iter().position(|i| i.name == index.name).ok_or_else(|| format!("index {} not found on {table}", index.name))?;
                t.indexes.remove(pos);
            }
            MigrationOp::DropPrimaryKey { table, key } => {
                let cols = set(&key.columns);
                let users = self.fks_on(table, &cols);
                let t = self.table(table)?;
                let others = t.unique_keys.iter().any(|k| set(&k.columns) == cols);
                if !users.is_empty() && !others {
                    return Err(format!("primary key of {table} is referenced by foreign keys {users:?}"));
                }
                let t = self.table_mut(table)?;
                if t.primary_key.is_none() {
                    return Err(format!("table {table} has no primary key"));
                }
                t.primary_key = None;
                Self::drop_key_backing_index(t, key, true);
            }
            MigrationOp::DropUniqueKey { table, key } => {
                let cols = set(&key.columns);
                let users = self.fks_on(table, &cols);
                let t = self.table(table)?;
                let others = t.primary_key.as_ref().is_some_and(|k| set(&k.columns) == cols)
                    || t.unique_keys.iter().filter(|k| *k != key).any(|k| set(&k.columns) == cols);
                if !users.is_empty() && !others {
                    return Err(format!("unique key {:?} of {table} is referenced by foreign keys {users:?}", key.name));
                }
                let t = self.table_mut(table)?;
                let pos = t.unique_keys.iter().position(|k| k == key).ok_or_else(|| format!("unique key {:?} not found on {table}", key.name))?;
                t.unique_keys.remove(pos);
                Self::drop_key_backing_index(t, key, false);
            }
            MigrationOp::DropCheck { table, check } => {
                let t = self.table_mut(table)?;
                let pos = t.checks.iter().position(|c| c == check).ok_or_else(|| format!("check {:?} not found on {table}", check.name))?;
                t.checks.remove(pos);
            }
            MigrationOp::DropTable { table, cascade } => {
                let name = table.name.clone();
                self.table(&name)?;
                let referencing: Vec<String> = self
                    .snap
                    .tables
                    .iter()
                    .filter(|t| t.name != name && t.foreign_keys.iter().any(|f| f.ref_table == name && self.is_own_schema(f.ref_schema.as_deref())))
                    .map(|t| t.name.clone())
                    .collect();
                let views = self.views_mentioning(&name);
                if !cascade && (!referencing.is_empty() || !views.is_empty()) {
                    return Err(format!("table {name} is still referenced by tables {referencing:?} / views {views:?}"));
                }
                if *cascade {
                    for t in &mut self.snap.tables {
                        t.foreign_keys.retain(|f| f.ref_table != name);
                    }
                    let mut gone: BTreeSet<String> = views.into_iter().collect();
                    loop {
                        let more: Vec<String> = self
                            .snap
                            .views
                            .iter()
                            .filter(|v| !gone.contains(&v.name) && gone.iter().any(|g| references(&v.definition).mentions(g, &[None])))
                            .map(|v| v.name.clone())
                            .collect();
                        if more.is_empty() {
                            break;
                        }
                        gone.extend(more);
                    }
                    self.snap.views.retain(|v| !gone.contains(&v.name));
                }
                self.snap.tables.retain(|t| t.name != name);
                self.snap.sequences.retain(|s| {
                    !s.owned_by.as_deref().and_then(|o| o.rsplit_once('.')).is_some_and(|(t, _)| t.rsplit('.').next() == Some(name.as_str()))
                });
            }
            MigrationOp::DropSequence { sequence } => {
                let pos = self.snap.sequences.iter().position(|s| s.name == sequence.name).ok_or_else(|| format!("sequence {} does not exist", sequence.name))?;
                let own = self.own();
                for t in &self.snap.tables {
                    for c in &t.columns {
                        if c.default.as_deref().is_some_and(|d| references(d).uses_sequence(&sequence.name, &own)) {
                            return Err(format!("sequence {} is used by {}.{}", sequence.name, t.name, c.name));
                        }
                    }
                }
                self.snap.sequences.remove(pos);
            }
            MigrationOp::DropType { ty } => {
                let pos = self.snap.types.iter().position(|t| t.name == ty.name).ok_or_else(|| format!("type {} does not exist", ty.name))?;
                for t in &self.snap.tables {
                    if let Some(c) = t.columns.iter().find(|c| base_type(&c.data_type) == ty.name.to_lowercase()) {
                        return Err(format!("type {} is used by {}.{}", ty.name, t.name, c.name));
                    }
                }
                self.snap.types.remove(pos);
            }
            MigrationOp::CreateType { ty } => {
                if self.snap.types.iter().any(|t| t.name == ty.name) {
                    return Err(format!("type {} already exists", ty.name));
                }
                self.snap.types.push(ty.clone());
            }
            MigrationOp::CreateSequence { sequence } => {
                if self.snap.sequences.iter().any(|s| s.name == sequence.name) {
                    return Err(format!("sequence {} already exists", sequence.name));
                }
                self.snap.sequences.push(sequence.clone());
            }
            MigrationOp::AlterSequence { from, to } => {
                let s = self.snap.sequences.iter_mut().find(|s| s.name == from.name).ok_or_else(|| format!("sequence {} does not exist", from.name))?;
                *s = to.clone();
            }
            MigrationOp::CreateRoutine { routine, replace } => {
                let existing = self
                    .snap
                    .routines
                    .iter()
                    .position(|r| r.name == routine.name && args_key(&r.arguments) == args_key(&routine.arguments));
                match (existing, replace) {
                    (Some(_), false) => return Err(format!("routine {} already exists", routine.name)),
                    (Some(p), true) => self.snap.routines[p] = routine.clone(),
                    (None, _) => self.snap.routines.push(routine.clone()),
                }
            }
            MigrationOp::CreateTable { table, inline_foreign_keys } => {
                if self.relation_exists(&table.name) {
                    return Err(format!("relation {} already exists", table.name));
                }
                if !inline_foreign_keys && !table.foreign_keys.is_empty() {
                    return Err(format!("table {} is created with foreign keys", table.name));
                }
                if !table.triggers.is_empty() {
                    return Err(format!("table {} is created with triggers", table.name));
                }
                for c in &table.columns {
                    self.check_column_refs(&table.name, c)?;
                }
                for ch in &table.checks {
                    self.check_expression_refs(&ch.expression, &format!("check on {}", table.name))?;
                }
                for i in &table.indexes {
                    if let Some(owner) = self.index_name_taken(&i.name) {
                        return Err(format!("index name {} already used on {owner}", i.name));
                    }
                }
                self.snap.tables.push(table.clone());
            }
            MigrationOp::AlterTable { from, to, renamed_columns } => self.alter_table(from, to, renamed_columns)?,
            MigrationOp::AddPrimaryKey { table, key } => {
                let t = self.table_mut(table)?;
                if t.primary_key.is_some() {
                    return Err(format!("table {table} already has a primary key"));
                }
                if let Some(c) = key.columns.iter().find(|c| t.column(c).is_none()) {
                    return Err(format!("column {c} does not exist on {table}"));
                }
                t.primary_key = Some(key.clone());
            }
            MigrationOp::AddUniqueKey { table, key } => {
                if let Some(n) = &key.name
                    && let Some(owner) = self.index_name_taken(n) {
                        return Err(format!("name {n} already used by an index on {owner}"));
                    }
                let t = self.table_mut(table)?;
                if let Some(c) = key.columns.iter().find(|c| t.column(c).is_none()) {
                    return Err(format!("column {c} does not exist on {table}"));
                }
                if key.name.is_some() && t.unique_keys.iter().any(|k| k.name == key.name) {
                    return Err(format!("unique key {:?} already exists on {table}", key.name));
                }
                t.unique_keys.push(key.clone());
            }
            MigrationOp::AddCheck { table, check } => {
                self.check_expression_refs(&check.expression, &format!("check on {table}"))?;
                let t = self.table_mut(table)?;
                if check.name.is_some() && t.checks.iter().any(|c| c.name == check.name) {
                    return Err(format!("check {:?} already exists on {table}", check.name));
                }
                t.checks.push(check.clone());
            }
            MigrationOp::CreateIndex { table, index } => {
                if let Some(owner) = self.index_name_taken(&index.name) {
                    return Err(format!("index name {} already used on {owner}", index.name));
                }
                for c in index.columns.iter().filter(|c| c.is_expression) {
                    self.check_expression_refs(&c.name, &format!("index {}", index.name))?;
                }
                let t = self.table_mut(table)?;
                if let Some(c) = index.columns.iter().filter(|c| !c.is_expression).find(|c| t.column(&c.name).is_none()) {
                    return Err(format!("column {} does not exist on {table}", c.name));
                }
                t.indexes.push(index.clone());
            }
            MigrationOp::AddForeignKey { table, fk } => {
                let t = self.table(table)?;
                if let Some(c) = fk.columns.iter().find(|c| t.column(c).is_none()) {
                    return Err(format!("column {c} does not exist on {table}"));
                }
                if fk.name.is_some() && t.foreign_keys.iter().any(|f| f.name == fk.name) {
                    return Err(format!("foreign key {:?} already exists on {table}", fk.name));
                }
                if self.is_own_schema(fk.ref_schema.as_deref()) {
                    let r = self.table(&fk.ref_table)?;
                    if let Some(c) = fk.ref_columns.iter().find(|c| r.column(c).is_none()) {
                        return Err(format!("referenced column {}.{c} does not exist", fk.ref_table));
                    }
                    if !Self::has_unique_on(r, &set(&fk.ref_columns)) {
                        return Err(format!("no unique constraint on {}({}) for foreign key {:?}", fk.ref_table, fk.ref_columns.join(", "), fk.name));
                    }
                }
                self.table_mut(table)?.foreign_keys.push(fk.clone());
            }
            MigrationOp::CreateView { view, replace } => {
                self.check_view_refs(view)?;
                match self.snap.views.iter().position(|v| v.name == view.name) {
                    Some(p) => {
                        if !replace {
                            return Err(format!("view {} already exists", view.name));
                        }
                        let old = &self.snap.views[p];
                        if !old.columns.is_empty()
                            && !view.columns.is_empty()
                            && (view.columns.len() < old.columns.len() || old.columns.iter().zip(&view.columns).any(|(a, b)| a.name != b.name))
                        {
                            return Err(format!("view {} cannot be replaced: its columns change", view.name));
                        }
                        self.snap.views[p] = view.clone();
                    }
                    None => {
                        if self.snap.tables.iter().any(|t| t.name == view.name) {
                            return Err(format!("relation {} already exists", view.name));
                        }
                        self.snap.views.push(view.clone());
                    }
                }
            }
            MigrationOp::CreateTrigger { table, trigger } => {
                self.check_expression_refs(&trigger.definition, &format!("trigger {}", trigger.name))?;
                let r = references(&trigger.definition);
                let own = self.own();
                for c in r.idents.iter().chain(r.calls.iter()) {
                    if (r.calls(&c.name, &own) || r.mentions(&c.name, &own))
                        && !self.snap.routines.iter().any(|x| x.name.eq_ignore_ascii_case(&c.name))
                        && self.created_later(&c.name, "routine")
                    {
                        return Err(format!("trigger {} uses routine {} before it is created", trigger.name, c.name));
                    }
                }
                let t = self.table_mut(table)?;
                if t.triggers.iter().any(|x| x.name == trigger.name) {
                    return Err(format!("trigger {} already exists on {table}", trigger.name));
                }
                t.triggers.push(trigger.clone());
            }
        }
        Ok(())
    }

    fn alter_table(&mut self, from: &TableDef, to: &TableDef, renamed: &[(String, String)]) -> Result<(), String> {
        let name = from.name.clone();
        let current = self.table(&name)?.clone();
        // the intermediate state the planner assumed must be the real one
        let names = |v: &[ForeignKeyDef]| v.iter().map(|f| f.name.clone().unwrap_or_else(|| f.columns.join(","))).collect::<BTreeSet<_>>();
        if names(&current.foreign_keys) != names(&from.foreign_keys) {
            return Err(format!("foreign keys of {name} are {:?}, expected {:?}", names(&current.foreign_keys), names(&from.foreign_keys)));
        }
        let idx = |t: &TableDef| t.indexes.iter().map(|i| i.name.clone()).collect::<BTreeSet<_>>();
        if idx(&current) != idx(from) {
            return Err(format!("indexes of {name} are {:?}, expected {:?}", idx(&current), idx(from)));
        }
        let trg = |t: &TableDef| t.triggers.iter().map(|i| i.name.clone()).collect::<BTreeSet<_>>();
        if trg(&current) != trg(from) {
            return Err(format!("triggers of {name} are {:?}, expected {:?}", trg(&current), trg(from)));
        }
        if current.primary_key != from.primary_key || current.unique_keys != from.unique_keys || current.checks != from.checks {
            return Err(format!("constraints of {name} differ from the planned intermediate state"));
        }
        let cur_cols: Vec<String> = current.columns.iter().map(|c| c.name.clone()).collect();
        let from_cols: Vec<String> = from.columns.iter().map(|c| c.name.clone()).collect();
        if cur_cols != from_cols {
            return Err(format!("columns of {name} are {cur_cols:?}, expected {from_cols:?}"));
        }
        let renamed_to = |old: &str| renamed.iter().find(|(o, _)| o == old).map(|(_, n)| n.clone()).unwrap_or_else(|| old.to_string());
        let new_names: BTreeSet<String> = to.columns.iter().map(|c| c.name.clone()).collect();
        let dropped: Vec<String> = cur_cols.iter().filter(|c| !new_names.contains(&renamed_to(c))).cloned().collect();
        let retyped: Vec<String> = current
            .columns
            .iter()
            .filter(|c| to.columns.iter().any(|n| n.name == renamed_to(&c.name) && n.data_type != c.data_type))
            .map(|c| c.name.clone())
            .collect();
        let own = self.own();
        let view_users = |col: &str| -> Vec<String> {
            self.snap
                .views
                .iter()
                .filter(|v| {
                    let r = references(&v.definition);
                    r.mentions(&name, &own) && r.mentions_anywhere(col)
                })
                .map(|v| v.name.clone())
                .collect()
        };
        for col in dropped.iter().chain(retyped.iter()) {
            let users = view_users(col);
            if !users.is_empty() {
                return Err(format!("column {name}.{col} is used by views {users:?}"));
            }
            for t in &self.snap.tables {
                for f in &t.foreign_keys {
                    let refs_col = f.ref_table == name && self.is_own_schema(f.ref_schema.as_deref()) && f.ref_columns.iter().any(|c| c == col);
                    let own_col = t.name == name && f.columns.iter().any(|c| c == col);
                    if refs_col || own_col {
                        return Err(format!("column {name}.{col} is used by foreign key {:?} on {}", f.name, t.name));
                    }
                }
            }
        }
        for col in &dropped {
            if let Some(i) = current.indexes.iter().find(|i| i.columns.iter().any(|c| !c.is_expression && c.name == *col)) {
                return Err(format!("column {name}.{col} is used by index {}", i.name));
            }
            if current.primary_key.as_ref().is_some_and(|k| k.columns.contains(col)) || current.unique_keys.iter().any(|k| k.columns.contains(col)) {
                return Err(format!("column {name}.{col} is used by a key constraint"));
            }
        }
        for c in &to.columns {
            self.check_column_refs(&name, c)?;
        }
        // apply: renames are propagated to referencing foreign keys
        for t in &mut self.snap.tables {
            for f in &mut t.foreign_keys {
                if f.ref_table == name && t.name != name {
                    for c in &mut f.ref_columns {
                        *c = renamed_to(c);
                    }
                }
            }
        }
        let dropped_owned: Vec<String> = dropped.iter().map(|c| format!("{name}.{c}")).collect();
        self.snap.sequences.retain(|s| {
            !s.owned_by.as_deref().is_some_and(|o| dropped_owned.iter().any(|d| o == d || o.ends_with(&format!(".{d}"))))
        });
        let t = self.table_mut(&name)?;
        let mut new = to.clone();
        new.name = name.clone();
        *t = new;
        Ok(())
    }
}
