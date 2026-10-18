//! [`schema_diff`]: turn the analysis of two snapshots into a [`SchemaDiff`].

use crate::analysis::{Analysis, Ctx, SubMatch, TablePair, pk_vec, snapshot_schema};
use crate::model::{DiffItem, DiffStatus, DiffSummary, PropertyChange, SchemaDiff};
use crate::options::CompareOptions;
use dbiji_core::ObjectKind;
use dbiji_core::ddl::*;

/// Compare `source` with `target`. Statuses are expressed from the point of
/// view of migrating the target to the source: [`DiffStatus::Added`] objects
/// exist only in the source, [`DiffStatus::Removed`] only in the target.
pub fn schema_diff(source: &SchemaSnapshot, target: &SchemaSnapshot, opts: &CompareOptions) -> SchemaDiff {
    let a = Analysis::new(source, target, opts);
    let mut items = Vec::new();
    let src_schema = snapshot_schema(source).map(str::to_string);
    let tgt_schema = snapshot_schema(target).map(str::to_string);

    for pair in &a.tables {
        items.push(table_item(pair, src_schema.clone()));
    }
    for &i in &a.added_tables {
        let t = &source.tables[i];
        items.push(DiffItem {
            kind: ObjectKind::Table,
            schema: src_schema.clone(),
            name: t.name.clone(),
            target_name: None,
            signature: None,
            status: DiffStatus::Added,
            changes: vec![PropertyChange::new("columns", Some(column_list(t)), None)],
            children: Vec::new(),
        });
    }
    for &i in &a.removed_tables {
        let t = &target.tables[i];
        items.push(DiffItem {
            kind: ObjectKind::Table,
            schema: tgt_schema.clone(),
            name: t.name.clone(),
            target_name: Some(t.name.clone()),
            signature: None,
            status: DiffStatus::Removed,
            changes: vec![PropertyChange::new("columns", None, Some(column_list(t)))],
            children: Vec::new(),
        });
    }

    let view_kind = |v: &ViewDef| if v.materialized { ObjectKind::MaterializedView } else { ObjectKind::View };
    push_objects(&mut items, &a.views, &source.views, &target.views, |v| (view_kind(v), v.name.clone(), None), &src_schema, &tgt_schema, |v| Some(v.definition.clone()));
    push_objects(&mut items, &a.sequences, &source.sequences, &target.sequences, |s| (ObjectKind::Sequence, s.name.clone(), None), &src_schema, &tgt_schema, |_| None);
    let routine_kind = |r: &RoutineDef| match r.kind {
        Some(k @ (ObjectKind::Procedure | ObjectKind::Aggregate)) => k,
        _ => ObjectKind::Function,
    };
    push_objects(
        &mut items,
        &a.routines,
        &source.routines,
        &target.routines,
        |r| (routine_kind(r), r.name.clone(), Some(r.arguments.clone())),
        &src_schema,
        &tgt_schema,
        |r| Some(r.definition.clone()),
    );
    let type_kind = |t: &TypeDef| if t.category.eq_ignore_ascii_case("domain") { ObjectKind::Domain } else { ObjectKind::Type };
    push_objects(
        &mut items,
        &a.types,
        &source.types,
        &target.types,
        |t| (type_kind(t), t.name.clone(), None),
        &src_schema,
        &tgt_schema,
        |t| Some(if t.definition.is_empty() { t.elements.join(", ") } else { t.definition.clone() }),
    );

    items.sort_by(|x, y| {
        kind_rank(x.kind)
            .cmp(&kind_rank(y.kind))
            .then_with(|| x.name.to_lowercase().cmp(&y.name.to_lowercase()))
            .then_with(|| x.name.cmp(&y.name))
            .then_with(|| x.signature.cmp(&y.signature))
    });
    let summary = DiffSummary::of(&items);
    SchemaDiff { items, summary, source_schema: src_schema, target_schema: tgt_schema, options: opts.clone() }
}

/// Sort order of object kinds in a diff (dependencies first).
pub(crate) fn kind_rank(k: ObjectKind) -> u8 {
    match k {
        ObjectKind::Type | ObjectKind::Domain => 0,
        ObjectKind::Sequence => 1,
        ObjectKind::Table | ObjectKind::ForeignTable | ObjectKind::Partition => 2,
        ObjectKind::View => 3,
        ObjectKind::MaterializedView => 4,
        ObjectKind::Function => 5,
        ObjectKind::Procedure => 6,
        ObjectKind::Aggregate => 7,
        _ => 8,
    }
}

fn column_list(t: &TableDef) -> String {
    t.columns.iter().map(Ctx::column_summary).collect::<Vec<_>>().join(", ")
}

#[allow(clippy::too_many_arguments)]
fn push_objects<T>(
    items: &mut Vec<DiffItem>,
    m: &crate::analysis::ObjMatch,
    src: &[T],
    tgt: &[T],
    ident: impl Fn(&T) -> (ObjectKind, String, Option<String>),
    src_schema: &Option<String>,
    tgt_schema: &Option<String>,
    definition: impl Fn(&T) -> Option<String>,
) {
    for (si, ti, changes) in &m.pairs {
        let (kind, name, signature) = ident(&src[*si]);
        let (_, tname, _) = ident(&tgt[*ti]);
        items.push(DiffItem {
            kind,
            schema: src_schema.clone(),
            name,
            target_name: Some(tname),
            signature,
            status: if changes.is_empty() { DiffStatus::Unchanged } else { DiffStatus::Modified },
            changes: changes.clone(),
            children: Vec::new(),
        });
    }
    for &si in &m.added {
        let (kind, name, signature) = ident(&src[si]);
        let changes = definition(&src[si]).map(|d| vec![PropertyChange::new("definition", Some(d), None)]).unwrap_or_default();
        items.push(DiffItem { kind, schema: src_schema.clone(), name, target_name: None, signature, status: DiffStatus::Added, changes, children: Vec::new() });
    }
    for &ti in &m.removed {
        let (kind, name, signature) = ident(&tgt[ti]);
        let changes = definition(&tgt[ti]).map(|d| vec![PropertyChange::new("definition", None, Some(d))]).unwrap_or_default();
        items.push(DiffItem {
            kind,
            schema: tgt_schema.clone(),
            name: name.clone(),
            target_name: Some(name),
            signature,
            status: DiffStatus::Removed,
            changes,
            children: Vec::new(),
        });
    }
}

fn child(kind: ObjectKind, name: String, target_name: Option<String>, status: DiffStatus, changes: Vec<PropertyChange>) -> DiffItem {
    DiffItem { kind, schema: None, name, target_name, signature: None, status, changes, children: Vec::new() }
}

/// Children for one kind of sub-object; unchanged pairs are omitted.
fn sub_children<T>(out: &mut Vec<DiffItem>, kind: ObjectKind, m: &SubMatch, src: &[T], tgt: &[T], name: impl Fn(&T) -> String, describe: impl Fn(&T) -> String) {
    for (si, ti, changes) in &m.pairs {
        if !changes.is_empty() {
            let tn = name(&tgt[*ti]);
            out.push(child(kind, name(&src[*si]), Some(tn), DiffStatus::Modified, changes.clone()));
        }
    }
    for &si in &m.added {
        out.push(child(kind, name(&src[si]), None, DiffStatus::Added, vec![PropertyChange::new("definition", Some(describe(&src[si])), None)]));
    }
    for &ti in &m.removed {
        let n = name(&tgt[ti]);
        out.push(child(kind, n.clone(), Some(n), DiffStatus::Removed, vec![PropertyChange::new("definition", None, Some(describe(&tgt[ti])))]));
    }
}

fn key_name(kind: &str, k: &KeyDef) -> String {
    k.name.clone().unwrap_or_else(|| format!("{kind} ({})", k.columns.join(", ")))
}

fn fk_describe(f: &ForeignKeyDef) -> String {
    format!(
        "FOREIGN KEY ({}) REFERENCES {}{}({}) ON UPDATE {} ON DELETE {}",
        f.columns.join(", "),
        f.ref_schema.as_ref().map(|s| format!("{s}.")).unwrap_or_default(),
        f.ref_table,
        f.ref_columns.join(", "),
        f.on_update.sql(),
        f.on_delete.sql()
    )
}

fn index_describe(i: &IndexDef) -> String {
    let cols = i
        .columns
        .iter()
        .map(|c| {
            let base = if c.is_expression { format!("({})", c.name) } else { c.name.clone() };
            if c.descending { format!("{base} DESC") } else { base }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut s = format!("{}INDEX ({cols})", if i.unique { "UNIQUE " } else { "" });
    if let Some(m) = &i.method {
        s.push_str(&format!(" USING {m}"));
    }
    if let Some(p) = &i.predicate {
        s.push_str(&format!(" WHERE {p}"));
    }
    s
}

fn table_item(p: &TablePair<'_>, schema: Option<String>) -> DiffItem {
    let (s, t) = (p.source, p.target);
    let mut children = Vec::new();
    sub_children(&mut children, ObjectKind::Column, &p.columns, &s.columns, &t.columns, |c| c.name.clone(), Ctx::column_summary);
    let (spk, tpk) = (pk_vec(s), pk_vec(t));
    sub_children(&mut children, ObjectKind::PrimaryKey, &p.primary_key, &spk, &tpk, |k| key_name("PRIMARY KEY", k), |k| format!("PRIMARY KEY ({})", k.columns.join(", ")));
    sub_children(&mut children, ObjectKind::UniqueKey, &p.unique_keys, &s.unique_keys, &t.unique_keys, |k| key_name("UNIQUE", k), |k| format!("UNIQUE ({})", k.columns.join(", ")));
    sub_children(
        &mut children,
        ObjectKind::CheckConstraint,
        &p.checks,
        &s.checks,
        &t.checks,
        |c| c.name.clone().unwrap_or_else(|| format!("CHECK ({})", c.expression)),
        |c| format!("CHECK ({})", c.expression),
    );
    sub_children(
        &mut children,
        ObjectKind::ForeignKey,
        &p.foreign_keys,
        &s.foreign_keys,
        &t.foreign_keys,
        |f| f.name.clone().unwrap_or_else(|| format!("FOREIGN KEY ({}) -> {}", f.columns.join(", "), f.ref_table)),
        fk_describe,
    );
    sub_children(&mut children, ObjectKind::Index, &p.indexes, &s.indexes, &t.indexes, |i| i.name.clone(), index_describe);
    sub_children(
        &mut children,
        ObjectKind::Trigger,
        &p.triggers,
        &s.triggers,
        &t.triggers,
        |tr| tr.name.clone(),
        |tr| {
            if tr.definition.is_empty() { format!("{} {}", tr.timing, tr.events.join(" OR ")) } else { tr.definition.clone() }
        },
    );
    sub_children(&mut children, ObjectKind::Partition, &p.partitions, &s.partitions, &t.partitions, |x| x.name.clone(), |x| x.bound.clone());
    let unchanged = p.is_unchanged();
    DiffItem {
        kind: ObjectKind::Table,
        schema,
        name: s.name.clone(),
        target_name: Some(t.name.clone()),
        signature: None,
        status: if unchanged { DiffStatus::Unchanged } else { DiffStatus::Modified },
        changes: p.changes.clone(),
        children,
    }
}
