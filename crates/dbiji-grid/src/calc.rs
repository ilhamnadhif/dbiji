//! Statistics over selected cells (the Calc panel, FR-D5).

use crate::view::sort_cmp;
use dbiji_core::Value;
use std::collections::HashSet;

/// Aggregates over a set of cells.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CalcStats {
    /// Number of cells.
    pub count: usize,
    /// NULL cells.
    pub nulls: usize,
    /// Distinct non-NULL values.
    pub distinct: usize,
    /// Cells holding numbers (or numeric text).
    pub numeric_count: usize,
    /// Exact sum when all numeric cells are integers.
    pub int_sum: Option<i128>,
    /// Sum of numeric cells.
    pub sum: Option<f64>,
    /// Average of numeric cells.
    pub avg: Option<f64>,
    /// Median of numeric cells.
    pub median: Option<f64>,
    /// Smallest non-NULL value (any type).
    pub min: Option<Value>,
    /// Largest non-NULL value (any type).
    pub max: Option<Value>,
    /// Shortest text length (characters) among text cells.
    pub min_len: Option<usize>,
    /// Longest text length (characters) among text cells.
    pub max_len: Option<usize>,
    /// Only the first `limit` cells were analysed.
    pub truncated: bool,
}

impl CalcStats {
    /// Sum as display text (exact for integers).
    pub fn sum_text(&self) -> Option<String> {
        if let Some(i) = self.int_sum {
            return Some(i.to_string());
        }
        self.sum.map(format_number)
    }
}

/// Short representation of an f64 for statistics.
pub fn format_number(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else if f.is_finite() {
        let s = format!("{f:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        f.to_string()
    }
}

fn numeric(v: &Value) -> Option<(f64, Option<i128>)> {
    match v {
        Value::Int(i) => Some((*i as f64, Some(*i as i128))),
        Value::UInt(u) => Some((*u as f64, Some(*u as i128))),
        Value::Float(f) if f.is_finite() => Some((*f, None)),
        Value::Decimal(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()).map(|f| (f, None)),
        _ => None,
    }
}

/// Compute statistics over at most `limit` values.
pub fn compute_stats<'a>(values: impl IntoIterator<Item = &'a Value>, limit: usize) -> CalcStats {
    let mut s = CalcStats::default();
    let mut distinct: HashSet<(u8, String)> = HashSet::new();
    let mut nums: Vec<f64> = Vec::new();
    let mut int_sum: Option<i128> = Some(0);
    let mut sum = 0.0f64;
    // Min / max of numeric cells; preferred over the overall min / max when present.
    let mut num_min: Option<Value> = None;
    let mut num_max: Option<Value> = None;
    for (i, v) in values.into_iter().enumerate() {
        if i >= limit {
            s.truncated = true;
            break;
        }
        s.count += 1;
        if v.is_null() {
            s.nulls += 1;
            continue;
        }
        distinct.insert((variant_tag(v), v.to_text().into_owned()));
        if let Some((f, exact)) = numeric(v) {
            s.numeric_count += 1;
            sum += f;
            int_sum = match (int_sum, exact) {
                (Some(acc), Some(x)) => acc.checked_add(x),
                _ => None,
            };
            nums.push(f);
            match &num_min {
                Some(m) if sort_cmp(v, m) != std::cmp::Ordering::Less => {}
                _ => num_min = Some(v.clone()),
            }
            match &num_max {
                Some(m) if sort_cmp(v, m) != std::cmp::Ordering::Greater => {}
                _ => num_max = Some(v.clone()),
            }
        }
        if let Value::Text(t) = v {
            let n = t.chars().count();
            s.min_len = Some(s.min_len.map_or(n, |m| m.min(n)));
            s.max_len = Some(s.max_len.map_or(n, |m| m.max(n)));
        }
        match &s.min {
            Some(m) if sort_cmp(v, m) != std::cmp::Ordering::Less => {}
            _ => s.min = Some(v.clone()),
        }
        match &s.max {
            Some(m) if sort_cmp(v, m) != std::cmp::Ordering::Greater => {}
            _ => s.max = Some(v.clone()),
        }
    }
    s.distinct = distinct.len();
    if num_min.is_some() {
        s.min = num_min;
        s.max = num_max;
    }
    if s.numeric_count > 0 {
        s.sum = Some(sum);
        s.int_sum = int_sum;
        s.avg = Some(sum / s.numeric_count as f64);
        nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = nums.len();
        s.median = Some(if n % 2 == 1 { nums[n / 2] } else { (nums[n / 2 - 1] + nums[n / 2]) / 2.0 });
    }
    s
}

fn variant_tag(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_) => 2,
        Value::Text(_) => 3,
        Value::Bytes(_) => 4,
        Value::Date(_) | Value::Time(_) | Value::DateTime(_) | Value::DateTimeTz(_) => 5,
        Value::Uuid(_) => 6,
        Value::Json(_) => 7,
        Value::Array(_) => 8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_stats() {
        let vals = vec![Value::Int(1), Value::Int(2), Value::Null, Value::Int(3), Value::Int(10), Value::Int(2)];
        let s = compute_stats(&vals, usize::MAX);
        assert_eq!(s.count, 6);
        assert_eq!(s.nulls, 1);
        assert_eq!(s.distinct, 4);
        assert_eq!(s.numeric_count, 5);
        assert_eq!(s.int_sum, Some(18));
        assert_eq!(s.sum_text().unwrap(), "18");
        assert_eq!(s.avg, Some(3.6));
        assert_eq!(s.median, Some(2.0));
        assert_eq!(s.min, Some(Value::Int(1)));
        assert_eq!(s.max, Some(Value::Int(10)));
        assert!(!s.truncated);
    }

    #[test]
    fn mixed_and_text() {
        let vals = vec![Value::Float(1.5), Value::decimal("2.5"), Value::text("abc"), Value::text("a")];
        let s = compute_stats(&vals, usize::MAX);
        assert_eq!(s.numeric_count, 2);
        assert_eq!(s.int_sum, None);
        assert_eq!(s.sum_text().unwrap(), "4");
        assert_eq!(s.median, Some(2.0));
        assert_eq!(s.min_len, Some(1));
        assert_eq!(s.max_len, Some(3));
        let s = compute_stats(&vals, 2);
        assert!(s.truncated);
        assert_eq!(s.count, 2);
        assert_eq!(format_number(1.0 / 3.0), "0.333333");
        assert_eq!(format_number(2.5), "2.5");
    }
}
