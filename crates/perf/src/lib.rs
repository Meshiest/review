//! Timing samples for the benchmarks.
//!
//! A benchmark collects [`Samples`], then prints one [`Report`] per metric as
//! a JSON line prefixed with `PERF ` on stdout. `scripts/checks.py` reads those
//! lines and checks each report's own budget and pass flag.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;

/// Durations of one metric, in the order they were recorded.
#[derive(Clone, Debug, Default)]
pub struct Samples {
    durations: Vec<Duration>,
}

impl Samples {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            durations: Vec::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, duration: Duration) {
        self.durations.push(duration);
    }

    pub fn len(&self) -> usize {
        self.durations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.durations.is_empty()
    }

    /// Summarizes the samples. `budget_ms` is checked against `percentile`
    /// (for example 99.0 for a p99 budget).
    pub fn report(&self, metric: &str, percentile: f64, budget_ms: f64) -> Report {
        let mut ms: Vec<f64> = self
            .durations
            .iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .collect();
        ms.sort_by(f64::total_cmp);
        let at = |p: f64| percentile_of(&ms, p);
        let measured = at(percentile);
        Report {
            metric: metric.to_owned(),
            count: ms.len(),
            p50_ms: at(50.0),
            p95_ms: at(95.0),
            p99_ms: at(99.0),
            max_ms: ms.last().copied().unwrap_or(0.0),
            percentile,
            budget_ms,
            pass: !ms.is_empty() && measured <= budget_ms,
            extra: BTreeMap::new(),
        }
    }
}

/// Nearest-rank percentile of sorted values; 0 when there are none.
fn percentile_of(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub metric: String,
    pub count: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub percentile: f64,
    pub budget_ms: f64,
    pub pass: bool,
    /// Context `checks.py` prints but does not judge, such as dropped frames.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Report {
    /// Adds a context field to the printed line.
    pub fn with(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        self.extra.insert(key.to_owned(), value.into());
        self
    }

    /// Prints the report as one `PERF {json}` line on stdout.
    pub fn print(&self) {
        println!(
            "PERF {}",
            serde_json::to_string(self).expect("report serializes")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles() {
        let mut s = Samples::default();
        for ms in 1..=100 {
            s.push(Duration::from_millis(ms));
        }
        let r = s.report("t", 99.0, 99.0);
        assert_eq!(r.p50_ms, 50.0);
        assert_eq!(r.p99_ms, 99.0);
        assert_eq!(r.max_ms, 100.0);
        assert!(r.pass);
        assert!(!s.report("t", 99.0, 98.0).pass);
    }

    #[test]
    fn empty_fails() {
        assert!(!Samples::default().report("t", 99.0, 8.0).pass);
    }
}
