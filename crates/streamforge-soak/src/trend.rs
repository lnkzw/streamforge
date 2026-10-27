//! Whether a resource grew over a run: the median of an early window after
//! the warm-up against the median of the last window, plus a fitted slope.
//! Medians ignore the short peaks a frame pool or an encoder flush causes.

use serde::Serialize;

/// Readings before this are warm-up: pools, caches and encoders fill.
pub const WARM_UP_S: f64 = 300.0;
/// The length of the early and the late window.
pub const WINDOW_S: f64 = 600.0;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Trend {
    /// Median of the window right after the warm-up.
    pub early: f64,
    /// Median of the run's last window.
    pub late: f64,
    /// The highest values of both windows.
    pub early_max: f64,
    pub late_max: f64,
    /// Least-squares slope over every reading after the warm-up, per hour.
    pub slope_per_hour: f64,
}

/// The trend of `(seconds, value)` readings, or `None` for a run too short
/// to have both windows apart: warm-up plus two windows.
pub fn trend(readings: &[(f64, f64)]) -> Option<Trend> {
    let end = readings.last()?.0;
    if end < WARM_UP_S + 2.0 * WINDOW_S {
        return None;
    }
    let window = |from: f64, to: f64| -> Vec<f64> {
        readings
            .iter()
            .filter(|(t, _)| *t >= from && *t < to)
            .map(|(_, value)| *value)
            .collect()
    };
    let early = window(WARM_UP_S, WARM_UP_S + WINDOW_S);
    let late = window(end - WINDOW_S, f64::INFINITY);
    let settled: Vec<_> = readings
        .iter()
        .copied()
        .filter(|(t, _)| *t >= WARM_UP_S)
        .collect();
    Some(Trend {
        early: median(&early)?,
        late: median(&late)?,
        early_max: early.iter().copied().fold(f64::MIN, f64::max),
        late_max: late.iter().copied().fold(f64::MIN, f64::max),
        slope_per_hour: slope(&settled)? * 3600.0,
    })
}

fn median(values: &[f64]) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    match sorted.len() {
        0 => None,
        n if n % 2 == 1 => Some(sorted[middle]),
        _ => Some((sorted[middle - 1] + sorted[middle]) / 2.0),
    }
}

/// Least-squares slope of value over time (per second).
fn slope(readings: &[(f64, f64)]) -> Option<f64> {
    let n = readings.len() as f64;
    if readings.len() < 2 {
        return None;
    }
    let mean_t = readings.iter().map(|(t, _)| t).sum::<f64>() / n;
    let mean_v = readings.iter().map(|(_, v)| v).sum::<f64>() / n;
    let (mut covariance, mut variance) = (0.0, 0.0);
    for (t, v) in readings {
        covariance += (t - mean_t) * (v - mean_v);
        variance += (t - mean_t) * (t - mean_t);
    }
    (variance > 0.0).then(|| covariance / variance)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reading every 10 s for `seconds`.
    fn run(seconds: u32, value: impl Fn(f64) -> f64) -> Vec<(f64, f64)> {
        (0..=seconds / 10)
            .map(|i| {
                let t = f64::from(i * 10);
                (t, value(t))
            })
            .collect()
    }

    #[test]
    fn a_flat_run_has_no_growth_whatever_its_warm_up_and_peaks() {
        let readings = run(3600, |t| {
            if t < 120.0 {
                // Warm-up: pools filling.
                100.0 + t
            } else if (t as u32 / 10).is_multiple_of(7) {
                // A short peak now and then.
                400.0
            } else {
                300.0
            }
        });
        let trend = trend(&readings).expect("long enough");
        assert_eq!((trend.early, trend.late), (300.0, 300.0));
        assert_eq!((trend.early_max, trend.late_max), (400.0, 400.0));
        assert!(trend.slope_per_hour.abs() < 5.0, "{trend:?}");
    }

    #[test]
    fn a_steady_leak_shows_in_the_medians_and_the_slope() {
        // 16 units per hour.
        let readings = run(3600, |t| 300.0 + t * 16.0 / 3600.0);
        let trend = trend(&readings).expect("long enough");
        // Early window 300..900 s, late 3000..3600 s: 2700 s apart.
        assert!((trend.late - trend.early - 12.0).abs() < 0.1, "{trend:?}");
        assert!((trend.slope_per_hour - 16.0).abs() < 1e-6, "{trend:?}");
    }

    #[test]
    fn a_short_run_is_not_judged() {
        assert_eq!(trend(&run(1490, |_| 1.0)), None);
        assert!(trend(&run(1500, |_| 1.0)).is_some());
        assert_eq!(trend(&[]), None);
    }
}
