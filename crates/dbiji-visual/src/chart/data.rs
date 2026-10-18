//! Chart data: a result set turned into series according to a
//! [`ChartConfig`]: numeric, temporal or categorical x axis, aggregation of
//! repeated x values, series split, folding of excess series into "Other"
//! and down-sampling (largest triangle three buckets) of long series.

use super::config::{ChartAggregate, ChartConfig, ChartKind};
use crate::common::colors::CATEGORICAL_SLOTS;
use crate::common::format::{compact, trim_decimals};
use chrono::{DateTime, NaiveDateTime, NaiveTime, Timelike};
use dbiji_core::value::{ColumnMeta, DataKind, Row, Value};
use dbiji_core::{tr, trf};
use std::collections::HashMap;

/// Kind of x axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum XKind {
    /// Row number (no x column).
    #[default]
    Index,
    /// Numbers.
    Numeric,
    /// Dates / timestamps (x = seconds since 1970, wall clock time).
    Temporal,
    /// Time of day (x = seconds since midnight).
    TimeOfDay,
    /// Labels (x = category index).
    Categorical,
}

/// One series of points.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartSeries {
    /// Legend name.
    pub name: String,
    /// `[x, y]` points (sorted by x for numeric/temporal axes of lines,
    /// areas and bars).
    pub points: Vec<[f64; 2]>,
    /// This is the "Other" bucket of folded series.
    pub is_other: bool,
}

/// A pie slice.
#[derive(Debug, Clone, PartialEq)]
pub struct PieSlice {
    /// Label.
    pub label: String,
    /// Value (> 0).
    pub value: f64,
    /// The "Other" bucket.
    pub is_other: bool,
}

/// Data ready to be plotted.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChartData {
    /// Kind of x axis.
    pub x_kind: XKind,
    /// X axis title.
    pub x_label: String,
    /// Y axis title.
    pub y_label: String,
    /// Category labels (x = index) for categorical axes.
    pub categories: Vec<String>,
    /// Series (at most eight; extra series are folded or dropped).
    pub series: Vec<ChartSeries>,
    /// Rows that contributed at least one point.
    pub rows_used: usize,
    /// Rows without a usable x or y value.
    pub rows_skipped: usize,
    /// Some series were down-sampled to `max_points`.
    pub downsampled: bool,
    /// Number of series folded into "Other" (or dropped for line and scatter charts).
    pub folded_series: usize,
    /// Aggregation applied although the configuration says `None`
    /// (repeated x values in line, bar, area and pie charts are summed).
    pub implicit_aggregate: Option<ChartAggregate>,
    /// Temporal values carry a time of day (otherwise only dates are shown).
    pub has_time: bool,
    /// Why nothing can be plotted.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct Acc {
    x: f64,
    sum: f64,
    count: usize,
    min: f64,
    max: f64,
}

impl Acc {
    fn new(x: f64, v: f64) -> Self {
        Acc {
            x,
            sum: v,
            count: 1,
            min: v,
            max: v,
        }
    }
    fn add(&mut self, v: f64) {
        self.sum += v;
        self.count += 1;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
    }
    fn value(&self, agg: ChartAggregate) -> f64 {
        match agg {
            ChartAggregate::None | ChartAggregate::Sum => self.sum,
            ChartAggregate::Count => self.count as f64,
            ChartAggregate::Avg => self.sum / self.count.max(1) as f64,
            ChartAggregate::Min => self.min,
            ChartAggregate::Max => self.max,
        }
    }
}

#[derive(Default)]
struct SeriesBuilder {
    name: String,
    raw: Vec<[f64; 2]>,
    acc: Vec<Acc>,
    by_x: HashMap<u64, usize>,
    duplicate: bool,
}

/// Seconds since 1970 of the wall clock time of a temporal value.
pub(crate) fn temporal_seconds(v: &Value) -> Option<(f64, bool)> {
    let ndt: NaiveDateTime = match v {
        Value::Date(d) => d.and_hms_opt(0, 0, 0)?,
        Value::DateTime(dt) => *dt,
        Value::DateTimeTz(dt) => dt.naive_local(),
        Value::Text(s) => dbiji_core::value::parse_datetime(s.trim())
            .or_else(|| dbiji_core::value::parse_datetime_tz(s.trim()).map(|d| d.naive_local()))?,
        Value::Int(i) => DateTime::from_timestamp(*i, 0)?.naive_utc(),
        _ => return None,
    };
    let secs =
        ndt.and_utc().timestamp() as f64 + ndt.and_utc().timestamp_subsec_nanos() as f64 * 1e-9;
    let has_time = ndt.time() != NaiveTime::MIN;
    Some((secs, has_time))
}

fn time_of_day_seconds(v: &Value) -> Option<f64> {
    let t = match v {
        Value::Time(t) => *t,
        Value::DateTime(dt) => dt.time(),
        Value::DateTimeTz(dt) => dt.naive_local().time(),
        Value::Text(s) => dbiji_core::value::parse_time(s.trim())?,
        _ => return None,
    };
    Some(t.num_seconds_from_midnight() as f64 + t.nanosecond() as f64 * 1e-9)
}

fn category_label(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "(null)".to_string(),
        Some(v) => v.to_text().into_owned(),
    }
}

fn finite(v: f64) -> Option<f64> {
    if v.is_finite() { Some(v) } else { None }
}

impl ChartData {
    /// Builds chart data from a result set.
    pub fn build(columns: &[ColumnMeta], rows: &[Row], config: &ChartConfig) -> ChartData {
        let mut data = ChartData::default();
        if let Some(x) = config.x_column
            && x >= columns.len()
        {
            data.error = Some(tr("The X column does not exist in this result").to_string());
            return data;
        }
        let ys: Vec<usize> = config
            .y_columns
            .iter()
            .copied()
            .filter(|&y| y < columns.len())
            .collect();
        let count_rows = ys.is_empty() && config.aggregate == ChartAggregate::Count;
        if ys.is_empty() && !count_rows {
            data.error = Some(tr("Choose at least one value (Y) column").to_string());
            return data;
        }
        let series_col = config.series_column.filter(|&s| s < columns.len());
        data.x_kind = match config.x_column {
            None => XKind::Index,
            Some(_) if config.kind == ChartKind::Pie => XKind::Categorical,
            Some(i) => match columns[i].kind {
                k if k.is_numeric() => XKind::Numeric,
                DataKind::Date | DataKind::DateTime | DataKind::DateTimeTz => XKind::Temporal,
                DataKind::Time | DataKind::TimeTz => XKind::TimeOfDay,
                _ => XKind::Categorical,
            },
        };
        data.x_label = config
            .x_column
            .map(|i| columns[i].name.clone())
            .unwrap_or_else(|| tr("Row").to_string());
        data.y_label = if count_rows {
            tr("Count").to_string()
        } else {
            ys.iter()
                .map(|&y| columns[y].name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let raw_mode =
            config.kind == ChartKind::Scatter && config.aggregate == ChartAggregate::None;

        let mut categories: Vec<String> = Vec::new();
        let mut cat_index: HashMap<String, usize> = HashMap::new();
        let mut builders: Vec<SeriesBuilder> = Vec::new();
        let mut series_index: HashMap<String, usize> = HashMap::new();
        let mut add = |name: String, x: f64, v: f64, builders: &mut Vec<SeriesBuilder>| {
            let si = *series_index.entry(name.clone()).or_insert_with(|| {
                builders.push(SeriesBuilder {
                    name,
                    ..Default::default()
                });
                builders.len() - 1
            });
            let Some(b) = builders.get_mut(si) else {
                return;
            };
            if raw_mode {
                b.raw.push([x, v]);
                return;
            }
            let key = x.to_bits();
            match b.by_x.get(&key) {
                Some(&i) => {
                    if let Some(a) = b.acc.get_mut(i) {
                        a.add(v);
                    }
                    b.duplicate = true;
                }
                None => {
                    b.by_x.insert(key, b.acc.len());
                    b.acc.push(Acc::new(x, v));
                }
            }
        };

        for (ri, row) in rows.iter().enumerate() {
            let xv = config.x_column.and_then(|i| row.get(i));
            let x = match data.x_kind {
                XKind::Index => Some(ri as f64),
                XKind::Numeric => xv.and_then(Value::as_f64).and_then(finite),
                XKind::Temporal => xv.and_then(temporal_seconds).map(|(s, t)| {
                    data.has_time |= t;
                    s
                }),
                XKind::TimeOfDay => xv.and_then(time_of_day_seconds),
                XKind::Categorical => {
                    let label = category_label(xv);
                    let idx = *cat_index.entry(label.clone()).or_insert_with(|| {
                        categories.push(label);
                        categories.len() - 1
                    });
                    Some(idx as f64)
                }
            };
            let Some(x) = x else {
                data.rows_skipped += 1;
                continue;
            };
            let prefix = series_col.map(|s| category_label(row.get(s)));
            let mut used = false;
            if count_rows {
                add(
                    prefix.unwrap_or_else(|| tr("Count").to_string()),
                    x,
                    1.0,
                    &mut builders,
                );
                used = true;
            } else {
                for &y in &ys {
                    let yv = row.get(y);
                    let val = if config.aggregate == ChartAggregate::Count {
                        yv.filter(|v| !v.is_null()).map(|_| 1.0)
                    } else {
                        yv.and_then(Value::as_f64).and_then(finite)
                    };
                    let Some(val) = val else { continue };
                    let name = match (&prefix, ys.len()) {
                        (Some(p), 1) => p.clone(),
                        (Some(p), _) => format!("{} · {p}", columns[y].name),
                        (None, _) => columns[y].name.clone(),
                    };
                    add(name, x, val, &mut builders);
                    used = true;
                }
            }
            if used {
                data.rows_used += 1;
            } else {
                data.rows_skipped += 1;
            }
        }

        // aggregation
        let duplicates = builders.iter().any(|b| b.duplicate);
        let agg = if config.aggregate == ChartAggregate::None && duplicates && !raw_mode {
            data.implicit_aggregate = Some(ChartAggregate::Sum);
            ChartAggregate::Sum
        } else {
            config.aggregate
        };
        let mut series: Vec<ChartSeries> = builders
            .into_iter()
            .map(|b| {
                let points = if raw_mode {
                    b.raw
                } else {
                    b.acc.iter().map(|a| [a.x, a.value(agg)]).collect()
                };
                ChartSeries {
                    name: b.name,
                    points,
                    is_other: false,
                }
            })
            .collect();

        // ordering
        if data.x_kind == XKind::Categorical && config.sort_x && categories.len() > 1 {
            let mut order: Vec<usize> = (0..categories.len()).collect();
            order.sort_by(|&a, &b| natural_cmp(&categories[a], &categories[b]));
            let mut remap = vec![0usize; categories.len()];
            for (new, &old) in order.iter().enumerate() {
                remap[old] = new;
            }
            categories = order.iter().map(|&i| categories[i].clone()).collect();
            for s in &mut series {
                for p in &mut s.points {
                    p[0] = remap.get(p[0] as usize).copied().unwrap_or(0) as f64;
                }
            }
        }
        let sort_points = match data.x_kind {
            XKind::Categorical => config.sort_x || config.kind != ChartKind::Scatter,
            XKind::Index => false,
            _ => {
                config.sort_x
                    || matches!(
                        config.kind,
                        ChartKind::Line | ChartKind::Area | ChartKind::Bar
                    )
            }
        };
        if sort_points {
            for s in &mut series {
                s.points.sort_by(|a, b| a[0].total_cmp(&b[0]));
            }
        }

        // folding: at most eight series (categorical palette slots)
        if series.len() > CATEGORICAL_SLOTS {
            let mut totals: Vec<(usize, f64)> = series
                .iter()
                .enumerate()
                .map(|(i, s)| (i, s.points.iter().map(|p| p[1].abs()).sum()))
                .collect();
            totals.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            let fold_into_other = matches!(
                config.kind,
                ChartKind::Bar | ChartKind::Area | ChartKind::Pie
            );
            let keep_n = if fold_into_other {
                CATEGORICAL_SLOTS - 1
            } else {
                CATEGORICAL_SLOTS
            };
            let keep: std::collections::BTreeSet<usize> =
                totals.iter().take(keep_n).map(|(i, _)| *i).collect();
            data.folded_series = series.len() - keep.len();
            let mut kept = Vec::new();
            let mut other: Vec<[f64; 2]> = Vec::new();
            let mut other_index: HashMap<u64, usize> = HashMap::new();
            for (i, s) in series.into_iter().enumerate() {
                if keep.contains(&i) {
                    kept.push(s);
                } else if fold_into_other {
                    for p in s.points {
                        match other_index.get(&p[0].to_bits()) {
                            Some(&k) => other[k][1] += p[1],
                            None => {
                                other_index.insert(p[0].to_bits(), other.len());
                                other.push(p);
                            }
                        }
                    }
                }
            }
            if fold_into_other && !other.is_empty() {
                other.sort_by(|a, b| a[0].total_cmp(&b[0]));
                kept.push(ChartSeries {
                    name: tr("Other").to_string(),
                    points: other,
                    is_other: true,
                });
            }
            series = kept;
        }

        // down-sampling
        let max_points = config.max_points.max(10);
        for s in &mut series {
            if s.points.len() <= max_points {
                continue;
            }
            data.downsampled = true;
            s.points = match (config.kind, data.x_kind) {
                (ChartKind::Bar, XKind::Categorical) | (ChartKind::Pie, _) => s
                    .points
                    .iter()
                    .copied()
                    .filter(|p| p[0] < max_points as f64)
                    .collect(),
                (ChartKind::Scatter, _) => stride_sample(&s.points, max_points),
                _ => lttb(&s.points, max_points),
            };
        }
        if data.downsampled && data.x_kind == XKind::Categorical && config.kind == ChartKind::Bar {
            categories.truncate(max_points);
        }
        data.categories = categories;
        data.series = series;
        if data.series.iter().all(|s| s.points.is_empty()) && data.error.is_none() {
            data.error = Some(if rows.is_empty() {
                tr("The result is empty").to_string()
            } else {
                trf("No plottable values in {} rows", &[&rows.len()])
            });
        }
        data
    }

    /// Nothing to draw.
    pub fn is_empty(&self) -> bool {
        self.series.iter().all(|s| s.points.is_empty())
    }

    /// Range of x values.
    pub fn x_range(&self) -> Option<(f64, f64)> {
        range(
            self.series
                .iter()
                .flat_map(|s| s.points.iter().map(|p| p[0])),
        )
    }

    /// Range of y values.
    pub fn y_range(&self) -> Option<(f64, f64)> {
        range(
            self.series
                .iter()
                .flat_map(|s| s.points.iter().map(|p| p[1])),
        )
    }

    /// Full text of an x value (tooltips).
    pub fn format_x(&self, x: f64) -> String {
        match self.x_kind {
            XKind::Index => format!("{}", x.round() as i64 + 1),
            XKind::Numeric => trim_decimals(x, 4),
            XKind::Temporal => {
                let fmt = if self.has_time {
                    "%Y-%m-%d %H:%M:%S"
                } else {
                    "%Y-%m-%d"
                };
                format_seconds(x, fmt)
            }
            XKind::TimeOfDay => format_seconds(x, "%H:%M:%S"),
            XKind::Categorical => {
                let i = x.round();
                if i >= 0.0 && (i - x).abs() < 0.25 {
                    self.categories.get(i as usize).cloned().unwrap_or_default()
                } else {
                    String::new()
                }
            }
        }
    }

    /// Short axis label of an x value given the visible span.
    pub fn axis_label_x(&self, x: f64, span: f64) -> String {
        match self.x_kind {
            XKind::Temporal => {
                let fmt = if span < 2.0 * 86_400.0 {
                    if self.has_time { "%H:%M" } else { "%m-%d" }
                } else if span < 150.0 * 86_400.0 {
                    "%m-%d"
                } else if span < 3.0 * 365.0 * 86_400.0 {
                    "%Y-%m"
                } else {
                    "%Y"
                };
                format_seconds(x, fmt)
            }
            XKind::TimeOfDay => format_seconds(x, if span < 600.0 { "%H:%M:%S" } else { "%H:%M" }),
            XKind::Numeric => compact(x, 2),
            _ => self.format_x(x),
        }
    }

    /// Pie slices: categories of the first series (or one slice per series
    /// when there are several), largest first; non-positive values are
    /// skipped and slices beyond eight are folded into "Other".
    pub fn pie_slices(&self) -> Vec<PieSlice> {
        let mut slices: Vec<PieSlice> = if self.series.len() == 1 {
            self.series
                .first()
                .map(|s| {
                    s.points
                        .iter()
                        .filter(|p| p[1].is_finite() && p[1] > 0.0)
                        .map(|p| PieSlice {
                            label: self.format_x(p[0]),
                            value: p[1],
                            is_other: false,
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            self.series
                .iter()
                .map(|s| PieSlice {
                    label: s.name.clone(),
                    value: s
                        .points
                        .iter()
                        .map(|p| p[1])
                        .filter(|v| v.is_finite())
                        .sum(),
                    is_other: s.is_other,
                })
                .filter(|s| s.value > 0.0)
                .collect()
        };
        slices.sort_by(|a, b| {
            a.is_other
                .cmp(&b.is_other)
                .then(b.value.total_cmp(&a.value))
        });
        if slices.len() > CATEGORICAL_SLOTS {
            let rest: f64 = slices[CATEGORICAL_SLOTS - 1..]
                .iter()
                .map(|s| s.value)
                .sum();
            slices.truncate(CATEGORICAL_SLOTS - 1);
            slices.push(PieSlice {
                label: tr("Other").to_string(),
                value: rest,
                is_other: true,
            });
        }
        slices
    }

    /// Stacked layers: for every series the x values (union over all
    /// series) with the lower and upper bound of its band. Missing values
    /// count as zero.
    pub fn stacked(&self) -> Vec<(Vec<f64>, Vec<f64>, Vec<f64>)> {
        let mut xs: Vec<f64> = self
            .series
            .iter()
            .flat_map(|s| s.points.iter().map(|p| p[0]))
            .collect();
        xs.sort_by(f64::total_cmp);
        xs.dedup();
        let mut base = vec![0.0f64; xs.len()];
        let mut out = Vec::with_capacity(self.series.len());
        for s in &self.series {
            let map: HashMap<u64, f64> = s.points.iter().map(|p| (p[0].to_bits(), p[1])).collect();
            let lower = base.clone();
            let upper: Vec<f64> = xs
                .iter()
                .zip(&base)
                .map(|(x, b)| b + map.get(&x.to_bits()).copied().unwrap_or(0.0))
                .collect();
            base = upper.clone();
            out.push((xs.clone(), lower, upper));
        }
        out
    }
}

fn range(values: impl Iterator<Item = f64>) -> Option<(f64, f64)> {
    let mut out: Option<(f64, f64)> = None;
    for v in values.filter(|v| v.is_finite()) {
        out = Some(match out {
            None => (v, v),
            Some((a, b)) => (a.min(v), b.max(v)),
        });
    }
    out
}

/// Formats seconds since 1970 (wall clock) with a chrono pattern.
pub(crate) fn format_seconds(secs: f64, fmt: &str) -> String {
    if !secs.is_finite() {
        return String::new();
    }
    let whole = secs.floor();
    let nanos = ((secs - whole) * 1e9).round().clamp(0.0, 999_999_999.0) as u32;
    match DateTime::from_timestamp(whole as i64, nanos) {
        Some(dt) => dt.naive_utc().format(fmt).to_string(),
        None => String::new(),
    }
}

/// Compares strings case-insensitively treating digit runs as numbers
/// ("item 2" < "item 10"); exact comparison breaks ties.
pub(crate) fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    natural_cmp_folded(a, b).then_with(|| a.cmp(b))
}

fn natural_cmp_folded(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(|c| c.is_ascii_digit()) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(|c| c.is_ascii_digit()) {
                    nb.push(c);
                    bi.next();
                }
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let o = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                let o = x.to_lowercase().cmp(y.to_lowercase());
                if o != Ordering::Equal {
                    return o;
                }
                ai.next();
                bi.next();
            }
        }
    }
}

/// Keeps every n-th point (first and last always kept).
fn stride_sample(points: &[[f64; 2]], max: usize) -> Vec<[f64; 2]> {
    if points.len() <= max || max < 2 {
        return points.to_vec();
    }
    let step = (points.len() - 1) as f64 / (max - 1) as f64;
    (0..max)
        .filter_map(|i| {
            points
                .get(((i as f64 * step).round() as usize).min(points.len() - 1))
                .copied()
        })
        .collect()
}

/// Largest-Triangle-Three-Buckets down-sampling of x-sorted points.
pub(crate) fn lttb(points: &[[f64; 2]], threshold: usize) -> Vec<[f64; 2]> {
    let n = points.len();
    if threshold >= n || threshold < 3 {
        return points.to_vec();
    }
    let mut out = Vec::with_capacity(threshold);
    let every = (n - 2) as f64 / (threshold - 2) as f64;
    let mut a = 0usize;
    out.push(points[0]);
    for i in 0..threshold - 2 {
        let avg_start = ((i + 1) as f64 * every).floor() as usize + 1;
        let avg_end = (((i + 2) as f64 * every).floor() as usize + 1).min(n);
        let (mut ax, mut ay) = (0.0, 0.0);
        let len = avg_end.saturating_sub(avg_start).max(1);
        for p in points.iter().take(avg_end).skip(avg_start) {
            ax += p[0];
            ay += p[1];
        }
        if avg_end <= avg_start {
            ax = points[n - 1][0];
            ay = points[n - 1][1];
        } else {
            ax /= len as f64;
            ay /= len as f64;
        }
        let range_start = (i as f64 * every).floor() as usize + 1;
        let range_end = (((i + 1) as f64 * every).floor() as usize + 1).min(n - 1);
        let [px, py] = points[a];
        let mut best = range_start.min(n - 1);
        let mut best_area = -1.0;
        for (j, p) in points.iter().enumerate().take(range_end).skip(range_start) {
            let area = ((px - ax) * (p[1] - py) - (px - p[0]) * (ay - py)).abs();
            if area > best_area {
                best_area = area;
                best = j;
            }
        }
        out.push(points[best]);
        a = best;
    }
    out.push(points[n - 1]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn cols() -> Vec<ColumnMeta> {
        vec![
            ColumnMeta::new("day", "date", DataKind::Date),
            ColumnMeta::new("region", "text", DataKind::Text),
            ColumnMeta::new("revenue", "numeric", DataKind::Decimal),
            ColumnMeta::new("orders", "int", DataKind::Int),
        ]
    }

    fn rows() -> Vec<Row> {
        let d = |n: u32| Value::Date(NaiveDate::from_ymd_opt(2026, 1, n).unwrap());
        vec![
            vec![
                d(2),
                Value::text("North"),
                Value::decimal("10.5"),
                Value::Int(2),
            ],
            vec![
                d(1),
                Value::text("South"),
                Value::decimal("20"),
                Value::Int(4),
            ],
            vec![
                d(1),
                Value::text("North"),
                Value::decimal("5"),
                Value::Int(1),
            ],
            vec![d(3), Value::Null, Value::text("oops"), Value::Null],
            vec![
                Value::Null,
                Value::text("North"),
                Value::decimal("1"),
                Value::Int(1),
            ],
        ]
    }

    #[test]
    fn temporal_line_with_implicit_sum() {
        let cfg = ChartConfig {
            kind: ChartKind::Line,
            x_column: Some(0),
            y_columns: vec![2],
            ..Default::default()
        };
        let d = ChartData::build(&cols(), &rows(), &cfg);
        assert_eq!(d.x_kind, XKind::Temporal);
        assert_eq!(d.series.len(), 1);
        let s = &d.series[0];
        // jan 1: 20 + 5 = 25, jan 2: 10.5; sorted by x
        assert_eq!(
            s.points.iter().map(|p| p[1]).collect::<Vec<_>>(),
            vec![25.0, 10.5]
        );
        assert!(s.points[0][0] < s.points[1][0]);
        assert_eq!(d.implicit_aggregate, Some(ChartAggregate::Sum));
        assert_eq!(d.rows_used, 3);
        assert_eq!(d.rows_skipped, 2);
        assert_eq!(d.format_x(s.points[0][0]), "2026-01-01");
        assert!(!d.has_time);
    }

    #[test]
    fn aggregates_and_series_split() {
        let base = ChartConfig {
            kind: ChartKind::Bar,
            x_column: Some(1),
            y_columns: vec![3],
            ..Default::default()
        };
        let avg = ChartData::build(
            &cols(),
            &rows(),
            &ChartConfig {
                aggregate: ChartAggregate::Avg,
                ..base.clone()
            },
        );
        assert_eq!(avg.x_kind, XKind::Categorical);
        // categories sorted: (null), North, South
        assert_eq!(avg.categories, vec!["(null)", "North", "South"]);
        let pts = &avg.series[0].points;
        let north = pts.iter().find(|p| p[0] == 1.0).unwrap()[1];
        assert!((north - 4.0 / 3.0).abs() < 1e-9, "{north}");
        let count = ChartData::build(
            &cols(),
            &rows(),
            &ChartConfig {
                aggregate: ChartAggregate::Count,
                ..base.clone()
            },
        );
        assert_eq!(
            count.series[0].points.iter().find(|p| p[0] == 1.0).unwrap()[1],
            3.0
        );
        let max = ChartData::build(
            &cols(),
            &rows(),
            &ChartConfig {
                aggregate: ChartAggregate::Max,
                ..base.clone()
            },
        );
        assert_eq!(
            max.series[0].points.iter().find(|p| p[0] == 1.0).unwrap()[1],
            2.0
        );
        // row counting without y columns
        let rc = ChartData::build(
            &cols(),
            &rows(),
            &ChartConfig {
                y_columns: vec![],
                aggregate: ChartAggregate::Count,
                ..base.clone()
            },
        );
        assert_eq!(rc.series[0].points.iter().map(|p| p[1]).sum::<f64>(), 5.0);
        // series split by region, x = day
        let split = ChartData::build(
            &cols(),
            &rows(),
            &ChartConfig {
                kind: ChartKind::Line,
                x_column: Some(0),
                y_columns: vec![2],
                series_column: Some(1),
                ..Default::default()
            },
        );
        let names: Vec<&str> = split.series.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["North", "South"]);
        // two y columns with a split are named "column · value"
        let two = ChartData::build(
            &cols(),
            &rows(),
            &ChartConfig {
                x_column: Some(0),
                y_columns: vec![2, 3],
                series_column: Some(1),
                ..Default::default()
            },
        );
        assert!(two.series.iter().any(|s| s.name == "revenue · North"));
        // errors
        let none = ChartData::build(
            &cols(),
            &rows(),
            &ChartConfig {
                y_columns: vec![],
                ..Default::default()
            },
        );
        assert!(none.error.is_some());
        let bad = ChartData::build(
            &cols(),
            &rows(),
            &ChartConfig {
                x_column: Some(9),
                y_columns: vec![2],
                ..Default::default()
            },
        );
        assert!(bad.error.is_some());
        let empty = ChartData::build(
            &cols(),
            &[],
            &ChartConfig {
                x_column: Some(0),
                y_columns: vec![2],
                ..Default::default()
            },
        );
        assert!(empty.is_empty() && empty.error.is_some());
    }

    #[test]
    fn scatter_keeps_raw_points_and_numeric_axis() {
        let cfg = ChartConfig {
            kind: ChartKind::Scatter,
            x_column: Some(3),
            y_columns: vec![2],
            ..Default::default()
        };
        let d = ChartData::build(&cols(), &rows(), &cfg);
        assert_eq!(d.x_kind, XKind::Numeric);
        assert_eq!(
            d.series[0].points.len(),
            4,
            "duplicates kept: {:?}",
            d.series[0].points
        );
        assert_eq!(d.implicit_aggregate, None);
        assert_eq!(d.x_range(), Some((1.0, 4.0)));
    }

    #[test]
    fn many_series_fold_into_other() {
        let columns = vec![
            ColumnMeta::new("k", "text", DataKind::Text),
            ColumnMeta::new("v", "int", DataKind::Int),
        ];
        let rows: Vec<Row> = (0..12)
            .map(|i| vec![Value::text(format!("s{i}")), Value::Int(i + 1)])
            .collect();
        let cfg = ChartConfig {
            kind: ChartKind::Bar,
            x_column: None,
            y_columns: vec![1],
            series_column: Some(0),
            ..Default::default()
        };
        let d = ChartData::build(&columns, &rows, &cfg);
        assert_eq!(d.series.len(), 8);
        assert!(d.series.last().unwrap().is_other);
        assert_eq!(d.folded_series, 5);
        let total: f64 = d
            .series
            .iter()
            .flat_map(|s| s.points.iter().map(|p| p[1]))
            .sum();
        assert_eq!(
            total,
            (1..=12).sum::<i64>() as f64,
            "nothing lost when folding"
        );
        let line = ChartData::build(
            &columns,
            &rows,
            &ChartConfig {
                kind: ChartKind::Line,
                ..cfg
            },
        );
        assert_eq!(line.series.len(), 8);
        assert!(line.series.iter().all(|s| !s.is_other));
        assert_eq!(line.folded_series, 4);
    }

    #[test]
    fn pie_slices_and_stacking() {
        let cfg = ChartConfig {
            kind: ChartKind::Pie,
            x_column: Some(1),
            y_columns: vec![2],
            ..Default::default()
        };
        let d = ChartData::build(&cols(), &rows(), &cfg);
        let slices = d.pie_slices();
        assert_eq!(slices[0].label, "South");
        assert_eq!(slices[0].value, 20.0);
        assert!(slices.iter().all(|s| s.value > 0.0));
        // stacking unions x values
        let st = ChartData::build(
            &cols(),
            &rows(),
            &ChartConfig {
                kind: ChartKind::Area,
                x_column: Some(0),
                y_columns: vec![2],
                series_column: Some(1),
                stacked: true,
                ..Default::default()
            },
        )
        .stacked();
        assert_eq!(st.len(), 2);
        let (xs, lower, upper) = &st[1];
        assert_eq!(xs.len(), 2);
        assert_eq!(lower, &st[0].2);
        assert!(upper.iter().zip(lower).all(|(u, l)| u >= l));
    }

    #[test]
    fn downsampling() {
        let pts: Vec<[f64; 2]> = (0..10_000)
            .map(|i| [i as f64, (i as f64 / 50.0).sin() * 100.0])
            .collect();
        let s = lttb(&pts, 500);
        assert_eq!(s.len(), 500);
        assert_eq!(s.first(), pts.first());
        assert_eq!(s.last(), pts.last());
        assert!(s.windows(2).all(|w| w[0][0] < w[1][0]));
        let max = s.iter().map(|p| p[1]).fold(f64::MIN, f64::max);
        assert!(max > 99.0, "peaks are preserved: {max}");
        assert_eq!(stride_sample(&pts, 100).len(), 100);
        let columns = vec![
            ColumnMeta::new("x", "int", DataKind::Int),
            ColumnMeta::new("y", "float", DataKind::Float),
        ];
        let rows: Vec<Row> = pts
            .iter()
            .map(|p| vec![Value::Int(p[0] as i64), Value::Float(p[1])])
            .collect();
        let d = ChartData::build(
            &columns,
            &rows,
            &ChartConfig {
                x_column: Some(0),
                y_columns: vec![1],
                max_points: 300,
                ..Default::default()
            },
        );
        assert!(d.downsampled);
        assert_eq!(d.series[0].points.len(), 300);
    }

    #[test]
    fn natural_ordering_and_formatting() {
        let mut v = vec!["item 10", "Item 2", "item 1", "apple"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["apple", "item 1", "Item 2", "item 10"]);
        assert_eq!(format_seconds(0.0, "%Y-%m-%d %H:%M"), "1970-01-01 00:00");
        let (secs, has_time) = temporal_seconds(&Value::text("2026-03-04 05:06:07")).unwrap();
        assert!(has_time);
        assert_eq!(
            format_seconds(secs, "%Y-%m-%d %H:%M:%S"),
            "2026-03-04 05:06:07"
        );
        assert_eq!(time_of_day_seconds(&Value::text("01:00:30")), Some(3630.0));
        let d = ChartData {
            x_kind: XKind::Temporal,
            ..Default::default()
        };
        assert_eq!(
            d.axis_label_x(86_400.0 * 400.0, 86_400.0 * 1000.0),
            "1971-02"
        );
    }
}
