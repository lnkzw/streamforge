//! Fixed-size latency histogram for per-frame/per-block processing times.

use std::time::Duration;

use serde::Serialize;

/// Whole milliseconds of `duration`, saturating (after about 584 million years).
pub fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

const BUCKET_NS: u64 = 50_000;
/// 50 µs buckets up to 100 ms; slower samples land in the last bucket.
const BUCKETS: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LatencySummary {
    pub min_ms: f64,
    pub mean_ms: f64,
    /// Percentiles are bucket upper bounds (50 µs resolution).
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

/// Records durations without allocating after construction.
#[derive(Debug, Clone)]
pub struct LatencyHistogram {
    buckets: Box<[u32]>,
    count: u64,
    sum_ns: u128,
    min_ns: u64,
    max_ns: u64,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyHistogram {
    pub fn new() -> Self {
        Self {
            buckets: vec![0; BUCKETS].into_boxed_slice(),
            count: 0,
            sum_ns: 0,
            min_ns: u64::MAX,
            max_ns: 0,
        }
    }

    pub fn record(&mut self, duration: Duration) {
        let ns = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        let bucket = usize::try_from(ns / BUCKET_NS).map_or(BUCKETS - 1, |b| b.min(BUCKETS - 1));
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
        self.count += 1;
        self.sum_ns += u128::from(ns);
        self.min_ns = self.min_ns.min(ns);
        self.max_ns = self.max_ns.max(ns);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn summary(&self) -> Option<LatencySummary> {
        if self.count == 0 {
            return None;
        }
        let ms = |ns: u64| ns as f64 / 1e6;
        Some(LatencySummary {
            min_ms: ms(self.min_ns),
            mean_ms: self.sum_ns as f64 / self.count as f64 / 1e6,
            p95_ms: ms(self.percentile_ns(0.95)),
            p99_ms: ms(self.percentile_ns(0.99)),
            max_ms: ms(self.max_ns),
        })
    }

    fn percentile_ns(&self, quantile: f64) -> u64 {
        let rank = ((self.count as f64 * quantile).ceil() as u64).max(1);
        let mut seen = 0u64;
        for (i, &n) in self.buckets.iter().enumerate() {
            seen += u64::from(n);
            if seen >= rank {
                // The last bucket collects everything slower; its bound is the maximum.
                if i == BUCKETS - 1 {
                    return self.max_ns;
                }
                return ((i as u64 + 1) * BUCKET_NS).min(self.max_ns);
            }
        }
        self.max_ns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram_has_no_summary() {
        assert_eq!(LatencyHistogram::new().summary(), None);
    }

    #[test]
    fn percentiles_follow_the_distribution() {
        let mut h = LatencyHistogram::new();
        for _ in 0..95 {
            h.record(Duration::from_micros(1_000));
        }
        for _ in 0..5 {
            h.record(Duration::from_micros(10_000));
        }
        let s = h.summary().expect("summary");
        assert_eq!(s.min_ms, 1.0);
        assert_eq!(s.max_ms, 10.0);
        assert!((s.mean_ms - 1.45).abs() < 1e-9);
        assert!((s.p95_ms - 1.05).abs() < 1e-9, "{}", s.p95_ms);
        assert_eq!(s.p99_ms, 10.0);
    }

    #[test]
    fn huge_samples_saturate_into_the_last_bucket() {
        let mut h = LatencyHistogram::new();
        h.record(Duration::from_secs(5));
        let s = h.summary().expect("summary");
        assert_eq!(s.max_ms, 5000.0);
        assert_eq!(s.p99_ms, 5000.0);
    }
}
