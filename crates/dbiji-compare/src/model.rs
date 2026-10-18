//! Result model of a schema comparison.

use crate::options::CompareOptions;
use dbiji_core::ObjectKind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Status of an object (or sub-object) in a comparison, seen from the
/// migration point of view (making the TARGET identical to the SOURCE).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStatus {
    /// Only in the source: will be created in the target.
    Added,
    /// Only in the target: will be dropped (when requested).
    Removed,
    /// In both, with differences.
    Modified,
    /// In both, identical under the comparison options.
    Unchanged,
}

impl DiffStatus {
    /// English label, routed through i18n by the UI.
    pub fn label(self) -> &'static str {
        match self {
            DiffStatus::Added => "Added",
            DiffStatus::Removed => "Removed",
            DiffStatus::Modified => "Modified",
            DiffStatus::Unchanged => "Unchanged",
        }
    }

    /// Anything but [`DiffStatus::Unchanged`].
    pub fn is_change(self) -> bool {
        self != DiffStatus::Unchanged
    }
}

/// One differing property of an object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropertyChange {
    /// Property name, e.g. `type`, `nullable`, `default`, `definition`, `columns`.
    pub property: String,
    /// Value in the source (`None` = absent / not set).
    pub source: Option<String>,
    /// Value in the target (`None` = absent / not set).
    pub target: Option<String>,
}

impl PropertyChange {
    /// Convenience constructor.
    pub fn new(property: impl Into<String>, source: Option<String>, target: Option<String>) -> Self {
        PropertyChange { property: property.into(), source, target }
    }
}

/// A compared object: a top level object (table, view, sequence, routine,
/// type) or, inside [`DiffItem::children`], a table child (column, key,
/// index, foreign key, trigger, partition).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffItem {
    /// Kind of object.
    pub kind: ObjectKind,
    /// Schema of the object: the source schema when the object exists in the
    /// source, otherwise the target schema.
    pub schema: Option<String>,
    /// Name of the object (source name when present in the source).
    pub name: String,
    /// Name in the target when the object exists there (may differ through
    /// table mapping or identifier case).
    pub target_name: Option<String>,
    /// Argument list for routines (disambiguates overloads).
    pub signature: Option<String>,
    /// Comparison result.
    pub status: DiffStatus,
    /// Differing properties of a modified object.
    pub changes: Vec<PropertyChange>,
    /// Table children (columns, keys, indexes, foreign keys, triggers, partitions).
    pub children: Vec<DiffItem>,
}

impl DiffItem {
    /// `schema.name` (or just `name`).
    pub fn qualified_name(&self) -> String {
        match &self.schema {
            Some(s) if !s.is_empty() => format!("{s}.{}", self.name),
            _ => self.name.clone(),
        }
    }

    /// Name shown in reports: `name`, or `name -> target_name` when they differ.
    pub fn display_name(&self) -> String {
        let mut s = self.name.clone();
        if let Some(sig) = &self.signature {
            s.push('(');
            s.push_str(sig);
            s.push(')');
        }
        match &self.target_name {
            Some(t) if t != &self.name => format!("{s} \u{2192} {t}"),
            _ => s,
        }
    }

    /// True when the item or any child differs.
    pub fn has_changes(&self) -> bool {
        self.status.is_change() || self.children.iter().any(DiffItem::has_changes)
    }
}

/// Number of objects per status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusCounts {
    /// Objects only in the source.
    pub added: usize,
    /// Objects only in the target.
    pub removed: usize,
    /// Objects that differ.
    pub modified: usize,
    /// Identical objects.
    pub unchanged: usize,
}

impl StatusCounts {
    /// Count one more object with `status`.
    pub fn add(&mut self, status: DiffStatus) {
        match status {
            DiffStatus::Added => self.added += 1,
            DiffStatus::Removed => self.removed += 1,
            DiffStatus::Modified => self.modified += 1,
            DiffStatus::Unchanged => self.unchanged += 1,
        }
    }
    /// All objects.
    pub fn total(&self) -> usize {
        self.added + self.removed + self.modified + self.unchanged
    }
    /// Objects that differ.
    pub fn changed(&self) -> usize {
        self.added + self.removed + self.modified
    }
}

/// Counts of top level objects.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSummary {
    /// Counts over all top level objects.
    pub total: StatusCounts,
    /// Counts per object kind.
    pub by_kind: BTreeMap<ObjectKind, StatusCounts>,
}

impl DiffSummary {
    /// Summary of a list of top level items.
    pub fn of(items: &[DiffItem]) -> Self {
        let mut s = DiffSummary::default();
        for i in items {
            s.total.add(i.status);
            s.by_kind.entry(i.kind).or_default().add(i.status);
        }
        s
    }
}

/// Result of [`crate::schema_diff`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaDiff {
    /// Top level objects, ordered by kind (types, sequences, tables, views,
    /// routines) and name. Unchanged objects are included.
    pub items: Vec<DiffItem>,
    /// Counts of [`SchemaDiff::items`].
    pub summary: DiffSummary,
    /// Schema of the source snapshot.
    pub source_schema: Option<String>,
    /// Schema of the target snapshot.
    pub target_schema: Option<String>,
    /// Options used for the comparison; the migration script and changelog
    /// generators match objects with the same rules.
    pub options: CompareOptions,
}

impl SchemaDiff {
    /// True when any object differs.
    pub fn has_differences(&self) -> bool {
        self.items.iter().any(DiffItem::has_changes)
    }

    /// Items that differ.
    pub fn changed_items(&self) -> impl Iterator<Item = &DiffItem> {
        self.items.iter().filter(|i| i.has_changes())
    }

    /// Keep only the items for which `keep` returns true (e.g. objects the
    /// user selected in the compare view) and refresh the summary. Only the
    /// remaining items are scripted.
    pub fn retain(&mut self, keep: impl FnMut(&DiffItem) -> bool) {
        self.items.retain(keep);
        self.summary = DiffSummary::of(&self.items);
    }
}
