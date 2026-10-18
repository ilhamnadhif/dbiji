//! "Nice" axis tick generation (1-2-5 steps).

/// Step from {1, 2, 5} x 10^k so that `span` is divided into roughly
/// `target` intervals.
pub(crate) fn nice_step(span: f64, target: usize) -> f64 {
    if !span.is_finite() || span <= 0.0 {
        return 1.0;
    }
    let raw = span / target.max(1) as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let norm = raw / mag;
    let nice = if norm <= 1.0 {
        1.0
    } else if norm <= 2.0 {
        2.0
    } else if norm <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * mag
}

/// Tick values covering `min..=max`.
pub(crate) fn ticks(min: f64, max: f64, target: usize) -> Vec<f64> {
    if !min.is_finite() || !max.is_finite() {
        return Vec::new();
    }
    let (lo, hi) = if min <= max { (min, max) } else { (max, min) };
    if hi - lo <= f64::EPSILON * hi.abs().max(1.0) {
        return vec![lo];
    }
    let step = nice_step(hi - lo, target);
    let first = (lo / step).ceil() * step;
    let mut out = Vec::new();
    let mut v = first;
    let mut guard = 0;
    while v <= hi + step * 1e-9 && guard < 1000 {
        // avoid -0 and accumulated float noise
        let clean = (v / step).round() * step;
        out.push(if clean == 0.0 { 0.0 } else { clean });
        v += step;
        guard += 1;
    }
    out
}

/// Expands `min..max` outwards to the nearest ticks (for axis ranges).
pub(crate) fn nice_range(min: f64, max: f64, target: usize) -> (f64, f64) {
    if !min.is_finite() || !max.is_finite() {
        return (0.0, 1.0);
    }
    if (max - min).abs() < f64::EPSILON * max.abs().max(1.0) {
        let pad = if min == 0.0 { 1.0 } else { min.abs() * 0.1 };
        return (min - pad, max + pad);
    }
    let step = nice_step(max - min, target);
    ((min / step).floor() * step, (max / step).ceil() * step)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_and_ticks() {
        assert_eq!(nice_step(10.0, 5), 2.0);
        assert_eq!(nice_step(100.0, 4), 50.0);
        assert!((nice_step(0.9, 9) - 0.1).abs() < 1e-12);
        assert_eq!(ticks(0.0, 10.0, 5), vec![0.0, 2.0, 4.0, 6.0, 8.0, 10.0]);
        assert_eq!(ticks(-3.0, 3.0, 3), vec![-2.0, 0.0, 2.0]);
        assert_eq!(ticks(5.0, 5.0, 4), vec![5.0]);
        assert!(ticks(f64::NAN, 1.0, 4).is_empty());
        assert_eq!(nice_range(3.0, 97.0, 5), (0.0, 100.0));
        assert_eq!(nice_range(0.0, 0.0, 5), (-1.0, 1.0));
    }
}
