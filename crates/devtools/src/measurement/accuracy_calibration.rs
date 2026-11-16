//! Accuracy calibration (`asap_run calibrate`): for each sketch
//! configuration × readout statistic × metric, seeded independent trials on
//! the executor's own sketches at each measured merge count, recorded as
//! the (1 − δ) quantile of the per-value errors, the worst over the merge
//! counts (`MeasuredSketchAccuracy`).
//!
//! Example: KLL `k = 547` read for `Quantile { q: 0.99 }` has rank errors
//! 0.0015, 0.0018 and 0.0017 at δ = 0.01 after 1, 6 and 360 merges; the
//! table records 0.0018.

use asap_accuracy::{ErrorAtFailureProbability, MeasuredAccuracyEntry};
use asap_types::accuracy::AccuracyMetric;
use asap_types::summary_type::{SketchParams, SketchStatistic};

/// The nearest-rank (1 − δ) quantile of `errors`: the ⌈(1 − δ)·n⌉-th
/// smallest.
pub fn error_quantile(errors: &[f64], failure_probability: f64) -> f64 {
    let _ = (errors, failure_probability);
    todo!("asap_run: implement after test approval")
}

/// One configuration's errors after `merge_count` merges, as
/// `calibrate-run.json` records them.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeCountErrors {
    pub sketch: SketchParams,
    pub statistic: SketchStatistic,
    pub accuracy_metric: AccuracyMetric,
    pub merge_count: u32,
    pub trials: u32,
    pub error_at: Vec<ErrorAtFailureProbability>,
}

/// One table entry per (sketch, statistic, metric) of `results`: at each
/// failure probability, the worst error over the merge counts.
pub fn worst_over_merge_counts(results: &[MergeCountErrors]) -> Vec<MeasuredAccuracyEntry> {
    let _ = results;
    todo!("asap_run: implement after test approval")
}
