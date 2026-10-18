//! Chart configuration (saved with dashboards and result tabs).

use dbiji_core::value::{ColumnMeta, DataKind};
use serde::{Deserialize, Serialize};

/// Chart type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChartKind {
    /// Lines through the points of each series.
    #[default]
    Line,
    /// Bars (grouped or stacked).
    Bar,
    /// Unconnected points.
    Scatter,
    /// Pie / donut of category shares.
    Pie,
    /// Filled areas (optionally stacked).
    Area,
}

impl ChartKind {
    /// All kinds in menu order.
    pub const ALL: [ChartKind; 5] = [
        ChartKind::Line,
        ChartKind::Bar,
        ChartKind::Area,
        ChartKind::Scatter,
        ChartKind::Pie,
    ];

    /// English label (route through `tr`).
    pub fn label(self) -> &'static str {
        match self {
            ChartKind::Line => "Line",
            ChartKind::Bar => "Bar",
            ChartKind::Scatter => "Scatter",
            ChartKind::Pie => "Pie",
            ChartKind::Area => "Area",
        }
    }

    /// Phosphor icon.
    pub fn icon(self) -> &'static str {
        use egui_phosphor::regular as icon;
        match self {
            ChartKind::Line => icon::CHART_LINE,
            ChartKind::Bar => icon::CHART_BAR,
            ChartKind::Scatter => icon::CHART_SCATTER,
            ChartKind::Pie => icon::CHART_PIE,
            ChartKind::Area => icon::CHART_LINE_UP,
        }
    }

    /// Kinds that can stack series.
    pub fn can_stack(self) -> bool {
        matches!(self, ChartKind::Bar | ChartKind::Area)
    }
}

/// Aggregation applied when several rows share the same x (and series).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChartAggregate {
    /// Raw values (line, bar and area charts still sum repeated x values).
    #[default]
    None,
    /// Sum of values.
    Sum,
    /// Number of rows (non-null values when a Y column is chosen).
    Count,
    /// Average.
    Avg,
    /// Minimum.
    Min,
    /// Maximum.
    Max,
}

impl ChartAggregate {
    /// All aggregates in menu order.
    pub const ALL: [ChartAggregate; 6] = [
        ChartAggregate::None,
        ChartAggregate::Sum,
        ChartAggregate::Count,
        ChartAggregate::Avg,
        ChartAggregate::Min,
        ChartAggregate::Max,
    ];

    /// English label (route through `tr`).
    pub fn label(self) -> &'static str {
        match self {
            ChartAggregate::None => "None",
            ChartAggregate::Sum => "Sum",
            ChartAggregate::Count => "Count",
            ChartAggregate::Avg => "Average",
            ChartAggregate::Min => "Minimum",
            ChartAggregate::Max => "Maximum",
        }
    }
}

/// What to plot from a result set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChartConfig {
    /// Chart type.
    pub kind: ChartKind,
    /// Column for the x axis (`None`: row number).
    pub x_column: Option<usize>,
    /// Value columns (one series each, or split by `series_column`).
    pub y_columns: Vec<usize>,
    /// Column whose distinct values split the data into series.
    pub series_column: Option<usize>,
    /// Aggregation of rows sharing an x value.
    pub aggregate: ChartAggregate,
    /// Sort by x (categories alphabetically); numeric and time axes of
    /// lines and areas are always sorted.
    pub sort_x: bool,
    /// Stack series (bar and area charts).
    pub stacked: bool,
    /// Maximum points per series; larger series are down-sampled.
    pub max_points: usize,
    /// Draw pies as donuts.
    pub donut: bool,
    /// Optional chart title.
    pub title: String,
}

impl Default for ChartConfig {
    fn default() -> Self {
        ChartConfig {
            kind: ChartKind::Line,
            x_column: None,
            y_columns: Vec::new(),
            series_column: None,
            aggregate: ChartAggregate::None,
            sort_x: true,
            stacked: false,
            max_points: 2000,
            donut: true,
            title: String::new(),
        }
    }
}

impl ChartConfig {
    /// A sensible configuration for a result set: x = first temporal
    /// column (line chart) or first text column (bar chart) or first
    /// numeric column (scatter); y = up to three other numeric columns.
    pub fn suggest(columns: &[ColumnMeta]) -> ChartConfig {
        let numeric: Vec<usize> = columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.kind.is_numeric())
            .map(|(i, _)| i)
            .collect();
        let temporal = columns.iter().position(|c| {
            matches!(
                c.kind,
                DataKind::Date | DataKind::DateTime | DataKind::DateTimeTz | DataKind::Time
            )
        });
        let text = columns
            .iter()
            .position(|c| c.kind.is_textual() || c.kind == DataKind::Bool);
        let (kind, x) = match (temporal, text) {
            (Some(t), _) => (ChartKind::Line, Some(t)),
            (None, Some(t)) => (ChartKind::Bar, Some(t)),
            (None, None) if numeric.len() >= 2 => (ChartKind::Scatter, numeric.first().copied()),
            _ => (ChartKind::Line, None),
        };
        let y: Vec<usize> = numeric
            .into_iter()
            .filter(|i| Some(*i) != x)
            .take(3)
            .collect();
        let aggregate = if y.is_empty() {
            ChartAggregate::Count
        } else {
            ChartAggregate::None
        };
        ChartConfig {
            kind,
            x_column: x,
            y_columns: y,
            aggregate,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions() {
        let cols = vec![
            ColumnMeta::new("day", "date", DataKind::Date),
            ColumnMeta::new("region", "text", DataKind::Text),
            ColumnMeta::new("revenue", "numeric", DataKind::Decimal),
        ];
        let c = ChartConfig::suggest(&cols);
        assert_eq!(
            (c.kind, c.x_column, c.y_columns.clone()),
            (ChartKind::Line, Some(0), vec![2])
        );
        let c = ChartConfig::suggest(&cols[1..]);
        assert_eq!(
            (c.kind, c.x_column, c.y_columns.clone()),
            (ChartKind::Bar, Some(0), vec![1])
        );
        let nums = vec![
            ColumnMeta::new("a", "int", DataKind::Int),
            ColumnMeta::new("b", "float", DataKind::Float),
        ];
        let c = ChartConfig::suggest(&nums);
        assert_eq!(
            (c.kind, c.x_column, c.y_columns.clone()),
            (ChartKind::Scatter, Some(0), vec![1])
        );
        let only_text = vec![ColumnMeta::new("t", "text", DataKind::Text)];
        assert_eq!(
            ChartConfig::suggest(&only_text).aggregate,
            ChartAggregate::Count
        );
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<ChartConfig>(&json).unwrap(), c);
    }
}
