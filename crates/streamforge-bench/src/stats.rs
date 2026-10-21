//! Median and spread of repeated measurements.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Summary {
    pub median: f64,
    pub min: f64,
    pub max: f64,
    /// `(max - min) / median` in percent; `None` for a median of zero.
    pub spread_pct: Option<f64>,
}

/// `None` without values.
pub fn summarize(values: &[f64]) -> Option<Summary> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    let median = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    };
    let (min, max) = (sorted[0], sorted[n - 1]);
    Some(Summary {
        median,
        min,
        max,
        spread_pct: (median != 0.0).then(|| (max - min) / median * 100.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_and_spread() {
        assert_eq!(summarize(&[]), None);
        let one = summarize(&[4.0]).expect("summary");
        assert_eq!((one.median, one.spread_pct), (4.0, Some(0.0)));
        let odd = summarize(&[11.0, 9.0, 10.0]).expect("summary");
        assert_eq!((odd.median, odd.min, odd.max), (10.0, 9.0, 11.0));
        assert_eq!(odd.spread_pct, Some(20.0));
        let even = summarize(&[1.0, 4.0, 2.0, 3.0]).expect("summary");
        assert_eq!(even.median, 2.5);
        assert_eq!(summarize(&[0.0, 0.0]).expect("summary").spread_pct, None);
    }
}
