//! Bounded metric histories. Counters are converted to per-second rates
//! from consecutive samples; a counter that decreases is treated as a reset
//! (server restart, statistics reset) and counted from zero.

use dbiji_core::admin::{Metric, MetricKind};
use std::collections::{BTreeMap, HashMap, VecDeque};

/// Default number of samples kept per metric (one hour at one sample per second).
pub const DEFAULT_CAPACITY: usize = 3600;

/// History of one metric.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricHistory {
    /// Stable key.
    pub key: String,
    /// Display label.
    pub label: String,
    /// Display unit.
    pub unit: String,
    /// Gauge or counter.
    pub kind: MetricKind,
    /// Chart group.
    pub group: String,
    samples: VecDeque<(f64, f64)>,
    last_raw: Option<(f64, f64)>,
    capacity: usize,
    resets: usize,
}

/// Summary of a time window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowStats {
    /// Smallest value.
    pub min: f64,
    /// Largest value.
    pub max: f64,
    /// Mean value.
    pub avg: f64,
    /// Most recent value.
    pub last: f64,
    /// Number of samples.
    pub count: usize,
}

impl MetricHistory {
    /// Empty history described by `metric` keeping at most `capacity` samples.
    pub fn new(metric: &Metric, capacity: usize) -> Self {
        MetricHistory {
            key: metric.key.clone(),
            label: metric.label.clone(),
            unit: metric.unit.clone(),
            kind: metric.kind,
            group: metric.group.clone(),
            samples: VecDeque::new(),
            last_raw: None,
            capacity: capacity.max(2),
            resets: 0,
        }
    }

    /// Values are per-second rates (the metric is a counter).
    pub fn is_rate(&self) -> bool {
        self.kind == MetricKind::Counter
    }

    /// Adds a raw sample taken at `t` seconds. Gauges are stored as is;
    /// counters produce a rate once two samples exist. Samples with a
    /// timestamp before the latest one restart the history.
    pub fn push(&mut self, t: f64, raw: f64) {
        if !t.is_finite() || !raw.is_finite() {
            return;
        }
        if let Some(&(last_t, _)) = self.samples.back()
            && t < last_t {
                // time went backwards (clock change, new time base)
                self.samples.clear();
                self.last_raw = None;
            }
        match self.kind {
            MetricKind::Gauge => {
                if self.samples.back().is_some_and(|&(lt, _)| lt == t) {
                    self.samples.pop_back();
                }
                self.samples.push_back((t, raw));
            }
            MetricKind::Counter => match self.last_raw {
                None => self.last_raw = Some((t, raw)),
                Some((t0, v0)) => {
                    let dt = t - t0;
                    if dt > 0.0 {
                        let delta = raw - v0;
                        let rate = if delta >= 0.0 {
                            delta / dt
                        } else {
                            self.resets += 1;
                            raw.max(0.0) / dt
                        };
                        self.samples.push_back((t, rate));
                        self.last_raw = Some((t, raw));
                    } else if dt < 0.0 {
                        self.last_raw = Some((t, raw));
                    }
                }
            },
        }
        while self.samples.len() > self.capacity {
            self.samples.pop_front();
        }
    }

    /// All samples `(t, value)` oldest first.
    pub fn samples(&self) -> &VecDeque<(f64, f64)> {
        &self.samples
    }

    /// Most recent sample.
    pub fn latest(&self) -> Option<(f64, f64)> {
        self.samples.back().copied()
    }

    /// Number of counter resets seen.
    pub fn resets(&self) -> usize {
        self.resets
    }

    /// Samples with `from <= t <= to`.
    pub fn window(&self, from: f64, to: f64) -> impl Iterator<Item = (f64, f64)> + '_ {
        // samples are time ordered: skip quickly to the start
        let start = self.samples.partition_point(|&(t, _)| t < from);
        self.samples.range(start..).copied().take_while(move |&(t, _)| t <= to)
    }

    /// Statistics of the samples in a window.
    pub fn stats(&self, from: f64, to: f64) -> Option<WindowStats> {
        let mut count = 0usize;
        let (mut min, mut max, mut sum, mut last) = (f64::INFINITY, f64::NEG_INFINITY, 0.0, 0.0);
        for (_, v) in self.window(from, to) {
            count += 1;
            min = min.min(v);
            max = max.max(v);
            sum += v;
            last = v;
        }
        if count == 0 { None } else { Some(WindowStats { min, max, avg: sum / count as f64, last, count }) }
    }

    /// Drops samples older than `t`.
    pub fn trim_before(&mut self, t: f64) {
        while self.samples.front().is_some_and(|&(st, _)| st < t) {
            self.samples.pop_front();
        }
    }
}

/// Histories of all metrics of a server, fed by periodic polling.
#[derive(Debug, Clone)]
pub struct DashboardState {
    metrics: Vec<MetricHistory>,
    index: HashMap<String, usize>,
    capacity: usize,
    /// Samples older than this many seconds (relative to the newest sample)
    /// are dropped.
    pub max_age_secs: f64,
    last_t: Option<f64>,
}

impl Default for DashboardState {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl DashboardState {
    /// Empty state keeping one hour of one-second samples.
    pub fn new() -> Self {
        Self::default()
    }

    /// Empty state keeping at most `capacity` samples per metric.
    pub fn with_capacity(capacity: usize) -> Self {
        DashboardState { metrics: Vec::new(), index: HashMap::new(), capacity, max_age_secs: 3600.0, last_t: None }
    }

    /// Records one poll of server metrics taken at `t_secs` (any monotonic
    /// clock in seconds, e.g. seconds since the dashboard was opened).
    pub fn push(&mut self, t_secs: f64, metrics: &[Metric]) {
        for m in metrics {
            let i = match self.index.get(&m.key) {
                Some(&i) => i,
                None => {
                    self.index.insert(m.key.clone(), self.metrics.len());
                    self.metrics.push(MetricHistory::new(m, self.capacity));
                    self.metrics.len() - 1
                }
            };
            if let Some(h) = self.metrics.get_mut(i) {
                // label / unit may be refined by the driver over time
                h.label.clone_from(&m.label);
                h.unit.clone_from(&m.unit);
                h.group.clone_from(&m.group);
                if h.kind != m.kind {
                    *h = MetricHistory::new(m, self.capacity);
                }
                h.push(t_secs, m.value);
            }
        }
        if t_secs.is_finite() {
            self.last_t = Some(self.last_t.map_or(t_secs, |l| l.max(t_secs)));
            let cutoff = t_secs - self.max_age_secs;
            for h in &mut self.metrics {
                h.trim_before(cutoff);
            }
        }
    }

    /// All metric histories in first-seen order.
    pub fn metrics(&self) -> &[MetricHistory] {
        &self.metrics
    }

    /// History of one metric.
    pub fn metric(&self, key: &str) -> Option<&MetricHistory> {
        self.index.get(key).and_then(|&i| self.metrics.get(i))
    }

    /// Metrics grouped by `Metric::group`, groups in first-seen order.
    pub fn groups(&self) -> Vec<(String, Vec<&MetricHistory>)> {
        let mut order: Vec<String> = Vec::new();
        let mut map: BTreeMap<String, Vec<&MetricHistory>> = BTreeMap::new();
        for m in &self.metrics {
            if !map.contains_key(&m.group) {
                order.push(m.group.clone());
            }
            map.entry(m.group.clone()).or_default().push(m);
        }
        order.into_iter().filter_map(|g| map.remove(&g).map(|v| (g, v))).collect()
    }

    /// Time of the newest sample.
    pub fn latest_time(&self) -> Option<f64> {
        self.last_t
    }

    /// Forgets everything.
    pub fn clear(&mut self) {
        self.metrics.clear();
        self.index.clear();
        self.last_t = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_rates_and_resets() {
        let m = Metric::counter("Transactions", "xact.commit", "Commits", 0.0, "");
        let mut h = MetricHistory::new(&m, 100);
        h.push(0.0, 1000.0);
        assert!(h.latest().is_none(), "first counter sample has no rate");
        h.push(2.0, 1100.0);
        assert_eq!(h.latest(), Some((2.0, 50.0)));
        h.push(2.0, 1200.0); // duplicate timestamp ignored
        assert_eq!(h.samples().len(), 1);
        h.push(3.0, 30.0); // reset: counted from zero
        assert_eq!(h.latest(), Some((3.0, 30.0)));
        assert_eq!(h.resets(), 1);
        h.push(4.0, 40.0);
        assert_eq!(h.latest(), Some((4.0, 10.0)));
        h.push(f64::NAN, 5.0);
        assert_eq!(h.samples().len(), 3);
        let s = h.stats(0.0, 10.0).unwrap();
        assert_eq!((s.min, s.max, s.last, s.count), (10.0, 50.0, 10.0, 3));
        assert!((s.avg - 30.0).abs() < 1e-9);
    }

    #[test]
    fn gauge_capacity_window_and_time_reset() {
        let m = Metric::gauge("Connections", "conn.active", "Active", 0.0, "");
        let mut h = MetricHistory::new(&m, 5);
        for t in 0..10 {
            h.push(t as f64, t as f64 * 2.0);
        }
        assert_eq!(h.samples().len(), 5);
        assert_eq!(h.samples().front(), Some(&(5.0, 10.0)));
        assert_eq!(h.window(6.0, 8.0).count(), 3);
        assert!(h.stats(100.0, 200.0).is_none());
        h.push(1.0, 7.0); // time went backwards
        assert_eq!(h.samples().len(), 1);
        h.trim_before(5.0);
        assert!(h.samples().is_empty());
    }

    #[test]
    fn dashboard_state_groups_and_age() {
        let mut s = DashboardState::with_capacity(1000);
        s.max_age_secs = 10.0;
        for t in 0..30 {
            s.push(
                t as f64,
                &[
                    Metric::gauge("Connections", "c.active", "Active", 5.0, ""),
                    Metric::counter("Transactions", "x.commit", "Commits", t as f64 * 10.0, ""),
                    Metric::gauge("Connections", "c.idle", "Idle", 2.0, ""),
                ],
            );
        }
        let groups = s.groups();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "Connections");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(s.latest_time(), Some(29.0));
        let c = s.metric("x.commit").unwrap();
        assert!(c.samples().iter().all(|&(t, v)| t >= 19.0 && (v - 10.0).abs() < 1e-9));
        assert!(s.metric("missing").is_none());
        s.clear();
        assert!(s.metrics().is_empty());
    }
}
