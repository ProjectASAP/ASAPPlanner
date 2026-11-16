//! The deployment's measured sketch accuracy: the errors a calibration run
//! measured on the executor's own sketches, which Pass 2 reads through
//! [`AccuracyEvidence`] before each summary's theoretical guarantee.
//!
//! Example: KLL with `k = 800`, read for `Quantile { q: 0.99 }`, measured
//! over 1,000 trials, has rank error 0.00176 at failure probability 0.01
//! and 0.00121 at 0.05. A request at failure probability 0.02 gets
//! 0.00176 (the largest recorded failure probability at most 0.02); a
//! request at 0.001 gets no measured guarantee, so the theoretical one
//! applies.

use asap_types::accuracy::{AccuracyGuarantee, AccuracyMetric};
use asap_types::ir::schema::FieldDataType;
use asap_types::summary_type::{SketchParams, SketchStatistic};
use serde::{Deserialize, Serialize};

use crate::AccuracyEvidence;

/// Errors measured on the executor's own sketches. Default: empty, every
/// sketch keeps its theoretical guarantee.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredSketchAccuracy {
    pub entries: Vec<MeasuredAccuracyEntry>,
}

/// One sketch configuration, read for one statistic, in one metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredAccuracyEntry {
    /// The exact parameters measured; only these match.
    pub sketch: SketchParams,
    /// The readout, e.g. `Quantile { q: 0.99 }`; only the same statistic
    /// matches.
    pub statistic: SketchStatistic,
    /// The metric every recorded error is in, e.g. `Rank` for KLL.
    pub accuracy_metric: AccuracyMetric,
    /// Independent trials; every recorded failure probability is at least
    /// 3 / trials.
    pub trials: u32,
    /// Ascending failure probability; the error does not increase as the
    /// failure probability grows.
    pub error_at: Vec<ErrorAtFailureProbability>,
}

/// One measured point: the error not exceeded except with probability
/// `failure_probability`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorAtFailureProbability {
    pub failure_probability: f64,
    /// The (1 − δ) quantile of the per-value errors, the worst over the
    /// measured merge counts, in the entry's `accuracy_metric`.
    pub error: f64,
}

/// Why a measured accuracy table is not valid.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum MeasuredSketchAccuracyError {
    /// Entry `entry` has a failure probability outside (0, 1] or below
    /// 3 / trials.
    FailureProbabilityOutOfRange {
        entry: usize,
        failure_probability: f64,
    },
    /// Entry `entry` has a negative or non-finite error.
    InvalidError { entry: usize, error: f64 },
    /// Entry `entry`'s failure probabilities are not ascending, or its
    /// error increases as the failure probability grows.
    NotMonotone { entry: usize },
    /// Entries `first` and `second` measure the same sketch, statistic and
    /// metric.
    DuplicateEntry { first: usize, second: usize },
}

impl MeasuredSketchAccuracy {
    /// Check every entry: failure probabilities in (0, 1] and at least
    /// 3 / trials, errors finite and non-negative, ascending failure
    /// probability with non-increasing error, and no two entries for one
    /// (sketch, statistic, metric).
    pub fn validate(&self) -> Result<(), MeasuredSketchAccuracyError> {
        todo!("measured accuracy table: implement after the tests are approved")
    }
}

impl AccuracyEvidence for MeasuredSketchAccuracy {
    fn measured_guarantee(
        &self,
        _summary_type: &FieldDataType,
        _parameters: &SketchParams,
        _statistic: &SketchStatistic,
        _failure_probability: f64,
    ) -> Option<AccuracyGuarantee> {
        todo!("measured accuracy table: implement after the tests are approved")
    }
}
