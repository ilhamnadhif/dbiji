//! Spatial layers built from result set columns.

use super::geometry::{Bounds, Geometry};
use super::wkb::parse_geometry_value;
use dbiji_core::value::{ColumnMeta, DataKind, Row};
use egui::Color32;

/// One row's geometry.
#[derive(Debug, Clone, PartialEq)]
pub struct SpatialItem {
    /// Row index in the result set (reported back on click / hover).
    pub row: usize,
    /// The geometry.
    pub geometry: Geometry,
    /// Label shown in tooltips.
    pub label: Option<String>,
}

/// A set of geometries drawn with one color.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SpatialLayer {
    /// Layer name (legend / tooltip).
    pub name: String,
    /// Geometries.
    pub items: Vec<SpatialItem>,
    /// Fill/stroke color (`None`: palette color by layer index).
    pub color: Option<Color32>,
    /// Rows whose value could not be parsed: (row, message).
    pub errors: Vec<(usize, String)>,
}

impl SpatialLayer {
    /// Layer of the geometries in `rows[..][geometry_column]`; NULL values
    /// are skipped, unparsable values are listed in `errors`. The label
    /// column (if any) is rendered as text for tooltips.
    pub fn from_column(
        name: impl Into<String>,
        rows: &[Row],
        geometry_column: usize,
        label_column: Option<usize>,
    ) -> Self {
        let mut layer = SpatialLayer {
            name: name.into(),
            ..Default::default()
        };
        for (i, row) in rows.iter().enumerate() {
            let Some(v) = row.get(geometry_column) else {
                continue;
            };
            if v.is_null() {
                continue;
            }
            match parse_geometry_value(v) {
                Ok(geometry) => {
                    let label = label_column
                        .and_then(|c| row.get(c))
                        .filter(|v| !v.is_null())
                        .map(|v| v.to_text().into_owned());
                    layer.items.push(SpatialItem {
                        row: i,
                        geometry,
                        label,
                    });
                }
                Err(e) => layer.errors.push((i, e)),
            }
        }
        layer
    }

    /// Bounding box of all items.
    pub fn bounds(&self) -> Option<Bounds> {
        self.items
            .iter()
            .filter_map(|i| i.geometry.bounds())
            .reduce(Bounds::union)
    }

    /// SRID shared by the items (the first one found).
    pub fn srid(&self) -> Option<i32> {
        self.items.iter().find_map(|i| i.geometry.srid)
    }
}

/// Indices of columns that hold geometries (by logical kind or type name).
pub fn geometry_columns(columns: &[ColumnMeta]) -> Vec<usize> {
    columns
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.kind == DataKind::Geometry
                || DataKind::guess_from_type_name(&c.type_name) == DataKind::Geometry
        })
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::value::Value;

    #[test]
    fn layer_from_rows() {
        let rows: Vec<Row> = vec![
            vec![
                Value::Int(1),
                Value::text("SRID=4326;POINT(1 2)"),
                Value::text("a"),
            ],
            vec![Value::Int(2), Value::Null, Value::text("b")],
            vec![Value::Int(3), Value::text("not a geometry"), Value::Null],
            vec![
                Value::Int(4),
                Value::text("LINESTRING(0 0, 5 5)"),
                Value::Null,
            ],
        ];
        let layer = SpatialLayer::from_column("geom", &rows, 1, Some(2));
        assert_eq!(layer.items.len(), 2);
        assert_eq!(layer.items[0].label.as_deref(), Some("a"));
        assert_eq!(layer.items[1].row, 3);
        assert_eq!(layer.errors.len(), 1);
        assert_eq!(layer.errors[0].0, 2);
        assert_eq!(layer.srid(), Some(4326));
        let b = layer.bounds().unwrap();
        assert_eq!((b.min_x, b.min_y, b.max_x, b.max_y), (0.0, 0.0, 5.0, 5.0));
        let cols = vec![
            ColumnMeta::new("id", "int4", DataKind::Int),
            ColumnMeta::new("geom", "geometry", DataKind::Other),
            ColumnMeta::new("shape", "blob", DataKind::Geometry),
        ];
        assert_eq!(geometry_columns(&cols), vec![1, 2]);
    }
}
