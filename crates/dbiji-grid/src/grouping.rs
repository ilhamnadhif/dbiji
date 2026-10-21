//! Grouping of fetched rows (the Grouping panel, FR-D5).

use crate::expr::RowValues;
use crate::view::{ClientFilter, FilterOp, ViewContext, sort_cmp};
use dbiji_core::{SharedRowStore, Value};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

const CHUNK_ROWS: usize = 4096;

/// What to group by.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupSpec {
    /// Model columns forming the group key.
    pub columns: Vec<usize>,
    /// Optional numeric column aggregated per group (SUM / AVG / MIN / MAX).
    pub measure: Option<usize>,
}

/// One group.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupRow {
    /// Key values (one per [`GroupSpec::columns`]).
    pub keys: Vec<Value>,
    /// Number of rows.
    pub count: usize,
    /// Sum of the measure over numeric values.
    pub sum: Option<f64>,
    /// Average of the measure.
    pub avg: Option<f64>,
    /// Minimum of the measure.
    pub min: Option<f64>,
    /// Maximum of the measure.
    pub max: Option<f64>,
}

/// Result of grouping.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupResult {
    /// Groups, largest first.
    pub groups: Vec<GroupRow>,
    /// Rows examined.
    pub rows: usize,
    /// More distinct groups existed than `max_groups`; the rest were merged away.
    pub truncated: bool,
}

struct Ext<'a> {
    base: &'a [Value],
    base_len: usize,
    virt: &'a [Value],
}

impl RowValues for Ext<'_> {
    fn value(&self, col: usize) -> Option<&Value> {
        if col < self.base_len { self.base.get(col) } else { self.virt.get(col - self.base_len) }
    }
}

#[derive(Default)]
struct Acc {
    keys: Vec<Value>,
    count: usize,
    n: usize,
    sum: f64,
    min: f64,
    max: f64,
}

fn key_part(v: &Value) -> (u8, String) {
    match v {
        Value::Null => (0, String::new()),
        // Equal numbers group together regardless of representation (3, 3.0,
        // 3.00), as exact decimal text: big integers stay apart.
        Value::Int(i) => (1, i.to_string()),
        Value::UInt(u) => (1, u.to_string()),
        Value::Float(f) => (1, if *f == 0.0 { "0".into() } else { format!("{f}") }),
        Value::Decimal(d) => match plain_decimal(d) {
            Some(t) => (1, t),
            None => match v.as_f64() {
                Some(f) => (1, format!("{f}")),
                None => (2, d.to_string()),
            },
        },
        other => (2, other.to_text().into_owned()),
    }
}

/// `d` without a plus sign, leading zeros or trailing fraction zeros
/// (`+001.50` → `1.5`, `-0.0` → `0`); `None` when it is not a plain number.
fn plain_decimal(d: &str) -> Option<String> {
    let (neg, body) = match d.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, d.strip_prefix('+').unwrap_or(d)),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if int.is_empty() && frac.is_empty() || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let int = int.trim_start_matches('0');
    let frac = frac.trim_end_matches('0');
    let mut out = String::new();
    if neg && !(int.is_empty() && frac.is_empty()) {
        out.push('-');
    }
    out.push_str(if int.is_empty() { "0" } else { int });
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    Some(out)
}

/// Group the rows `rows` of `store` (all stored rows when `None`).
/// Returns `None` when cancelled.
pub fn compute_groups(store: &SharedRowStore, rows: Option<&[u32]>, spec: &GroupSpec, ctx: &ViewContext, max_groups: usize, cancel: &AtomicBool) -> Option<GroupResult> {
    let total = match rows {
        Some(r) => r.len(),
        None => store.lock().len(),
    };
    let mut map: HashMap<Vec<(u8, String)>, Acc> = HashMap::new();
    let mut truncated = false;
    let mut virt = vec![Value::Null; ctx.virtual_columns.len()];
    let mut needed: Vec<usize> = spec.columns.clone();
    needed.extend(spec.measure);
    let virt_needed: Vec<usize> = needed.iter().filter(|c| **c >= ctx.base_columns && **c - ctx.base_columns < ctx.virtual_columns.len()).map(|c| c - ctx.base_columns).collect();
    let mut scanned = 0usize;
    let mut process = |row: &[Value], idx: u32, map: &mut HashMap<Vec<(u8, String)>, Acc>| {
        if ctx.removed.contains(idx) {
            return;
        }
        for &v in &virt_needed {
            virt[v] = ctx.virtual_columns[v].eval(row).unwrap_or(Value::Null);
        }
        let ext = Ext { base: row, base_len: ctx.base_columns, virt: &virt };
        let key: Vec<(u8, String)> = spec.columns.iter().map(|&c| key_part(ext.value(c).unwrap_or(&Value::Null))).collect();
        let measure = spec.measure.and_then(|m| ext.value(m)).and_then(Value::as_f64).filter(|f| f.is_finite());
        scanned += 1;
        if !map.contains_key(&key) {
            if map.len() >= max_groups {
                truncated = true;
                return;
            }
            let keys = spec.columns.iter().map(|&c| ext.value(c).cloned().unwrap_or(Value::Null)).collect();
            map.insert(key.clone(), Acc { keys, min: f64::INFINITY, max: f64::NEG_INFINITY, ..Default::default() });
        }
        if let Some(acc) = map.get_mut(&key) {
            acc.count += 1;
            if let Some(m) = measure {
                acc.n += 1;
                acc.sum += m;
                acc.min = acc.min.min(m);
                acc.max = acc.max.max(m);
            }
        }
    };
    let mut start = 0usize;
    while start < total {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let end = (start + CHUNK_ROWS).min(total);
        let mut guard = store.lock();
        match rows {
            None => guard.for_each_in(start, end, |i, r| process(r, i as u32, &mut map)),
            Some(list) => {
                for &idx in &list[start..end] {
                    if let Some(r) = guard.get(idx as usize) {
                        process(r, idx, &mut map);
                    }
                }
            }
        }
        drop(guard);
        start = end;
    }
    let mut groups: Vec<GroupRow> = map
        .into_values()
        .map(|a| GroupRow {
            keys: a.keys,
            count: a.count,
            sum: (a.n > 0).then_some(a.sum),
            avg: (a.n > 0).then(|| a.sum / a.n as f64),
            min: (a.n > 0).then_some(a.min),
            max: (a.n > 0).then_some(a.max),
        })
        .collect();
    groups.sort_by(|a, b| {
        b.count.cmp(&a.count).then_with(|| {
            for (x, y) in a.keys.iter().zip(&b.keys) {
                let o = sort_cmp(x, y);
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
            }
            std::cmp::Ordering::Equal
        })
    });
    Some(GroupResult { groups, rows: scanned, truncated })
}

/// Client filter selecting the rows of one group.
pub fn group_filter(spec: &GroupSpec, group: &GroupRow, column_names: &[String]) -> Option<ClientFilter> {
    let mut filter: Option<ClientFilter> = None;
    for (&c, v) in spec.columns.iter().zip(&group.keys) {
        let name = column_names.get(c)?;
        let op = if v.is_null() { FilterOp::IsNull } else { FilterOp::Eq };
        let cond = ClientFilter::condition(c, name, op, v);
        filter = Some(match filter {
            None => cond,
            Some(f) => f.and(cond),
        });
    }
    filter
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::RowStore;

    #[test]
    fn number_keys_are_exact() {
        let k = |v: Value| key_part(&v);
        assert_ne!(k(Value::Int(9_007_199_254_740_993)), k(Value::Int(9_007_199_254_740_992)));
        assert_eq!(k(Value::Int(3)), k(Value::Float(3.0)));
        assert_eq!(k(Value::Int(3)), k(Value::decimal("3.00")));
        assert_eq!(k(Value::Float(0.1)), k(Value::decimal("+0.10")));
        assert_eq!(k(Value::Int(0)), k(Value::decimal("-0.0")));
        assert_eq!(k(Value::UInt(u64::MAX)), k(Value::decimal(u64::MAX.to_string())));
    }

    fn store() -> SharedRowStore {
        let s = RowStore::shared(1 << 24);
        {
            let mut g = s.lock();
            for i in 0..1000i64 {
                let cat = match i % 3 {
                    0 => Value::text("a"),
                    1 => Value::text("b"),
                    _ => Value::Null,
                };
                g.push(vec![cat, Value::Int(i % 2), Value::Float(i as f64)]);
            }
        }
        s
    }

    #[test]
    fn groups_with_measure() {
        let s = store();
        let ctx = ViewContext { base_columns: 3, ..Default::default() };
        let spec = GroupSpec { columns: vec![0], measure: Some(2) };
        let r = compute_groups(&s, None, &spec, &ctx, 1000, &AtomicBool::new(false)).unwrap();
        assert_eq!(r.rows, 1000);
        assert_eq!(r.groups.len(), 3);
        assert_eq!(r.groups[0].count, 334);
        assert_eq!(r.groups[0].keys, vec![Value::text("a")]);
        let total: usize = r.groups.iter().map(|g| g.count).sum();
        assert_eq!(total, 1000);
        let a = &r.groups[0];
        assert_eq!(a.min, Some(0.0));
        assert_eq!(a.max, Some(999.0));
        assert!((a.avg.unwrap() - 499.5).abs() < 1e-9);
        // Two key columns, subset of rows, group limit.
        let spec = GroupSpec { columns: vec![0, 1], measure: None };
        let rows: Vec<u32> = (0..100).collect();
        let r = compute_groups(&s, Some(&rows), &spec, &ctx, 4, &AtomicBool::new(false)).unwrap();
        assert_eq!(r.groups.len(), 4);
        assert!(r.truncated);
        assert!(r.groups.iter().all(|g| g.sum.is_none()));
        assert!(compute_groups(&s, None, &spec, &ctx, 10, &AtomicBool::new(true)).is_none());
    }

    #[test]
    fn filter_for_group() {
        let spec = GroupSpec { columns: vec![0, 1], measure: None };
        let g = GroupRow { keys: vec![Value::Null, Value::Int(1)], count: 1, sum: None, avg: None, min: None, max: None };
        let names = vec!["cat".to_string(), "flag".to_string(), "v".to_string()];
        let f = group_filter(&spec, &g, &names).unwrap();
        assert_eq!(f.text(), "(cat IS NULL) AND (flag = 1)");
        assert!(f.matches(&vec![Value::Null, Value::Int(1), Value::Null]));
        assert!(!f.matches(&vec![Value::text("a"), Value::Int(1), Value::Null]));
    }
}
