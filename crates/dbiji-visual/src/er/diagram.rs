//! Saved diagram document: model, box positions, notes and display settings.

use super::geometry::{TableGeometry, table_geometry};
use super::layout::{LayoutOptions, auto_layout, layout_incremental};
use super::model::ErModel;
use crate::common::serde_helpers;
use dbiji_core::{DbError, DbResult, trf};
use egui::{Pos2, Rect, Vec2, vec2};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Which columns a box lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ColumnDisplay {
    /// Every column.
    #[default]
    All,
    /// Primary and foreign key columns only.
    KeysOnly,
    /// Header only.
    None,
}

impl ColumnDisplay {
    /// All variants in menu order.
    pub const ALL: [ColumnDisplay; 3] = [
        ColumnDisplay::All,
        ColumnDisplay::KeysOnly,
        ColumnDisplay::None,
    ];

    /// English label (route through `tr` for display).
    pub fn label(self) -> &'static str {
        match self {
            ColumnDisplay::All => "All columns",
            ColumnDisplay::KeysOnly => "Keys only",
            ColumnDisplay::None => "No columns",
        }
    }
}

/// How relation ends are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Notation {
    /// Information engineering (crow's foot) cardinality markers;
    /// non-identifying relations are dashed.
    #[default]
    CrowsFoot,
    /// Plain lines with an arrow pointing at the referenced table.
    Simple,
}

impl Notation {
    /// English label (route through `tr` for display).
    pub fn label(self) -> &'static str {
        match self {
            Notation::CrowsFoot => "Crow's foot",
            Notation::Simple => "Simple arrows",
        }
    }
}

/// Display settings saved with a diagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct ErSettings {
    /// Which columns are listed.
    pub columns: ColumnDisplay,
    /// Show column types next to names.
    pub show_types: bool,
    /// Show table comments below the header.
    pub show_comments: bool,
    /// Relation notation.
    pub notation: Notation,
}

impl Default for ErSettings {
    fn default() -> Self {
        ErSettings {
            columns: ColumnDisplay::All,
            show_types: true,
            show_comments: false,
            notation: Notation::CrowsFoot,
        }
    }
}

/// A free text note placed on the diagram.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ErNote {
    /// Note text (may contain line breaks).
    pub text: String,
    /// Top-left corner in diagram coordinates.
    #[serde(with = "serde_helpers::pos2")]
    pub pos: Pos2,
    /// Size in diagram coordinates.
    #[serde(with = "serde_helpers::vec2")]
    pub size: Vec2,
}

impl Default for ErNote {
    fn default() -> Self {
        ErNote {
            text: String::new(),
            pos: Pos2::ZERO,
            size: vec2(180.0, 90.0),
        }
    }
}

impl ErNote {
    /// Creates a note at `pos` with the default size.
    pub fn new(text: impl Into<String>, pos: Pos2) -> Self {
        ErNote {
            text: text.into(),
            pos,
            ..Default::default()
        }
    }

    /// Rectangle of the note in diagram coordinates.
    pub fn rect(&self) -> Rect {
        Rect::from_min_size(self.pos, self.size.max(vec2(40.0, 24.0)))
    }
}

/// A diagram: the model plus everything a user can customise. Serialised as
/// JSON for custom (saved) diagrams; positions survive model refreshes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ErDiagram {
    /// Diagram name.
    pub name: String,
    /// Tables and relations.
    pub model: ErModel,
    /// Top-left corner of every box, by table key.
    #[serde(with = "serde_helpers::pos2_map")]
    pub positions: BTreeMap<String, Pos2>,
    /// Notes.
    pub notes: Vec<ErNote>,
    /// Display settings.
    pub settings: ErSettings,
}

impl ErDiagram {
    /// Creates a diagram and lays it out automatically.
    pub fn new(name: impl Into<String>, model: ErModel) -> Self {
        let settings = ErSettings::default();
        let positions = auto_layout(&model, &LayoutOptions::from_settings(settings));
        ErDiagram {
            name: name.into(),
            model,
            positions,
            notes: Vec::new(),
            settings,
        }
    }

    /// Recomputes every position with the automatic layout.
    pub fn auto_layout(&mut self, options: &LayoutOptions) {
        let mut opts = options.clone();
        opts.settings = self.settings;
        self.positions = auto_layout(&self.model, &opts);
    }

    /// Lays out with the default options for the current settings.
    pub fn relayout(&mut self) {
        self.auto_layout(&LayoutOptions::from_settings(self.settings));
    }

    /// Replaces the model (e.g. after re-reading the schema). Positions of
    /// tables that still exist are kept, new tables are placed next to
    /// related ones, positions of vanished tables are dropped.
    pub fn refresh_model(&mut self, model: ErModel) {
        self.model = model;
        self.place_missing();
    }

    /// Adds tables and relations (e.g. "add related tables"); returns the
    /// keys of the tables that were added. New boxes are placed near related
    /// ones without moving existing boxes.
    pub fn add_model(&mut self, other: ErModel) -> Vec<String> {
        let added = self.model.merge(other);
        self.place_missing();
        added
    }

    /// Removes tables (and their relations) from the diagram.
    pub fn remove_tables(&mut self, keys: &BTreeSet<String>) {
        self.model.remove_tables(keys);
        self.positions.retain(|k, _| !keys.contains(k));
    }

    /// Positions boxes that have no position yet.
    pub fn place_missing(&mut self) {
        let opts = LayoutOptions::from_settings(self.settings);
        self.positions = layout_incremental(&self.model, &self.positions, &opts);
    }

    /// Geometry of a box with the current settings.
    pub fn geometry(&self, key: &str) -> Option<TableGeometry> {
        self.model
            .table(key)
            .map(|t| table_geometry(t, &self.settings))
    }

    /// Rectangle of a box in diagram coordinates.
    pub fn table_rect(&self, key: &str) -> Option<Rect> {
        let pos = self.positions.get(key)?;
        let g = self.geometry(key)?;
        Some(Rect::from_min_size(*pos, g.size))
    }

    /// Bounding rectangle of all boxes and notes.
    pub fn bounds(&self) -> Option<Rect> {
        let mut out: Option<Rect> = None;
        for t in &self.model.tables {
            if let Some(pos) = self.positions.get(&t.key) {
                let r = Rect::from_min_size(*pos, table_geometry(t, &self.settings).size);
                out = Some(out.map_or(r, |o| o.union(r)));
            }
        }
        for n in &self.notes {
            let r = n.rect();
            out = Some(out.map_or(r, |o| o.union(r)));
        }
        out
    }

    /// Serialises the diagram as JSON (custom diagram file).
    pub fn to_json(&self) -> DbResult<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Loads a diagram saved with [`Self::to_json`]; boxes without a
    /// position are placed automatically.
    pub fn from_json(json: &str) -> DbResult<Self> {
        let mut d: ErDiagram = serde_json::from_str(json)
            .map_err(|e| DbError::config(trf("Invalid diagram file: {}", &[&e])))?;
        d.place_missing();
        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::er::model::{ErColumn, ErRelation, ErTable, ErTableKind};

    fn model() -> ErModel {
        let mut m = ErModel::default();
        m.tables.push(
            ErTable::new(None, "a", ErTableKind::Table)
                .with_column(ErColumn::new("id", "int").primary_key()),
        );
        m.tables.push(
            ErTable::new(None, "b", ErTableKind::Table)
                .with_column(ErColumn::new("id", "int").primary_key())
                .with_column(ErColumn::new("a_id", "int").foreign_key()),
        );
        m.relations.push(ErRelation {
            name: "b_a".into(),
            from_table: "b".into(),
            from_columns: vec!["a_id".into()],
            to_table: "a".into(),
            to_columns: vec!["id".into()],
            identifying: false,
            optional: true,
        });
        m
    }

    #[test]
    fn json_roundtrip_keeps_positions() {
        let mut d = ErDiagram::new("test", model());
        d.positions.insert("a".into(), Pos2::new(123.0, 456.0));
        d.notes.push(ErNote::new("hello", Pos2::new(1.0, 2.0)));
        let json = d.to_json().unwrap();
        let back = ErDiagram::from_json(&json).unwrap();
        assert_eq!(back, d);
        assert!(ErDiagram::from_json("{not json").is_err());
    }

    #[test]
    fn refresh_keeps_existing_positions() {
        let mut d = ErDiagram::new("test", model());
        d.positions.insert("a".into(), Pos2::new(5000.0, 5000.0));
        let mut m = model();
        m.tables.push(
            ErTable::new(None, "c", ErTableKind::Table).with_column(ErColumn::new("id", "int")),
        );
        m.tables.retain(|t| t.key != "b");
        m.relations.clear();
        d.refresh_model(m);
        assert_eq!(d.positions.get("a"), Some(&Pos2::new(5000.0, 5000.0)));
        assert!(d.positions.contains_key("c"));
        assert!(!d.positions.contains_key("b"));
        assert!(d.bounds().is_some());
    }
}
