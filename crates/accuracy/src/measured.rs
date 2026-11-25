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

use asap_types::accuracy::{AccuracyGuarantee, AccuracyMetric, BoundExpr, ProbabilityExpr};
use asap_types::ir::schema::FieldDataType;
use asap_types::stages::ValueSign;
use asap_types::summary_type::{SketchParams, SketchStatistic};
use serde::{Deserialize, Serialize};

use crate::guarantee::guarantee_before_failure_probability_check;
use crate::readout::readout_error_metric;
use crate::AccuracyEvidence;

/// Errors measured on the executor's own sketches. Default: empty, every
/// sketch keeps its theoretical guarantee.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredSketchAccuracy {
    pub entries: Vec<MeasuredAccuracyEntry>,
}

/// One sketch configuration, read for one statistic, in one metric. Each
/// recorded error is the worst case over the merge counts the calibration
/// measured: a state built over one window and a state merged from many
/// windows of the same θ share the entry, so its error must hold for
/// every one of them. Example: KLL k = 547 measured unmerged and merged
/// from 5 and 60 windows, with rank errors 0.0021, 0.0024 and 0.0023 at
/// δ = 0.01, records 0.0024 at 0.01.
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
    /// Entry `entry` measures a DDSketch α outside (0, 1), which no sketch
    /// is built with.
    InvalidDDSketchAlpha { entry: usize, alpha: f64 },
    /// Entry `entry` has a failure probability outside (0, 1] or below
    /// 3 / trials.
    FailureProbabilityOutOfRange {
        entry: usize,
        failure_probability: f64,
    },
    /// Entry `entry` records no point, so it can answer no request.
    NoPoints { entry: usize },
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
    /// Check every entry: a DDSketch α in (0, 1), at least one point, failure probabilities in
    /// (0, 1] and at least 3 / trials, errors finite and non-negative, ascending failure
    /// probability with non-increasing error, and no two entries for one
    /// (sketch, statistic, metric).
    pub fn validate(&self) -> Result<(), MeasuredSketchAccuracyError> {
        for (index, entry) in self.entries.iter().enumerate() {
            entry.validate(index)?;
            if let Some(first) = self.entries[..index].iter().position(|earlier| {
                earlier.measures(&entry.sketch, &entry.statistic, entry.accuracy_metric)
            }) {
                return Err(MeasuredSketchAccuracyError::DuplicateEntry {
                    first,
                    second: index,
                });
            }
        }
        Ok(())
    }

    /// The entry measuring `sketch` read for `statistic` in `metric`.
    fn entry(
        &self,
        sketch: &SketchParams,
        statistic: &SketchStatistic,
        metric: AccuracyMetric,
    ) -> Option<&MeasuredAccuracyEntry> {
        self.entries
            .iter()
            .find(|entry| entry.measures(sketch, statistic, metric))
    }
}

impl MeasuredAccuracyEntry {
    /// Check this entry, the `index`th of its table.
    fn validate(&self, index: usize) -> Result<(), MeasuredSketchAccuracyError> {
        if let SketchParams::DDSketch { alpha } = self.sketch {
            if !(alpha > 0.0 && alpha < 1.0) {
                return Err(MeasuredSketchAccuracyError::InvalidDDSketchAlpha {
                    entry: index,
                    alpha,
                });
            }
        }
        // Fewer than 3 failures in `trials` cannot estimate the (1 − δ)
        // quantile: 1,000 trials record δ ≥ 0.003.
        let smallest_failure_probability = 3.0 / f64::from(self.trials);
        if self.error_at.is_empty() {
            return Err(MeasuredSketchAccuracyError::NoPoints { entry: index });
        }
        for point in &self.error_at {
            let failure_probability = point.failure_probability;
            if !(failure_probability > 0.0
                && failure_probability <= 1.0
                && failure_probability >= smallest_failure_probability)
            {
                return Err(MeasuredSketchAccuracyError::FailureProbabilityOutOfRange {
                    entry: index,
                    failure_probability,
                });
            }
            if !(point.error.is_finite() && point.error >= 0.0) {
                return Err(MeasuredSketchAccuracyError::InvalidError {
                    entry: index,
                    error: point.error,
                });
            }
        }
        let monotone = self.error_at.windows(2).all(|pair| {
            pair[0].failure_probability < pair[1].failure_probability
                && pair[0].error >= pair[1].error
        });
        match monotone {
            true => Ok(()),
            false => Err(MeasuredSketchAccuracyError::NotMonotone { entry: index }),
        }
    }

    /// Whether this entry measures `sketch` read for `statistic` in
    /// `metric`: the one key a table holds at most one entry for.
    fn measures(
        &self,
        sketch: &SketchParams,
        statistic: &SketchStatistic,
        metric: AccuracyMetric,
    ) -> bool {
        self.sketch == *sketch && self.statistic == *statistic && self.accuracy_metric == metric
    }

    /// The point at the largest recorded failure probability at most
    /// `failure_probability`; `None` below the smallest (never
    /// extrapolated). Example: recorded at 0.003, 0.01 and 0.05, a request
    /// at 0.02 gets the point at 0.01.
    fn point_at_most(&self, failure_probability: f64) -> Option<ErrorAtFailureProbability> {
        self.error_at
            .iter()
            .filter(|point| point.failure_probability <= failure_probability)
            .max_by(|a, b| a.failure_probability.total_cmp(&b.failure_probability))
            .copied()
    }
}

/// Whether θ's theoretical guarantee for `statistic` can be deterministic
/// (failure probability zero) under the data it requires: then a
/// measurement never replaces it. Example: DDSketch's relative error α
/// over values of one sign; a KLL's rank error always has a failure
/// probability.
fn has_deterministic_guarantee(
    summary_type: &FieldDataType,
    parameters: &SketchParams,
    statistic: &SketchStatistic,
) -> bool {
    guarantee_before_failure_probability_check(
        summary_type,
        parameters,
        statistic,
        1.0,
        Some(ValueSign::NonNegative),
        None,
    )
    .is_some_and(|guarantee| guarantee.spec.failure_probability == ProbabilityExpr::Zero)
}

/// Loaders must call [`MeasuredSketchAccuracy::validate`] before handing a
/// table to the planner: `measured_guarantee` does not re-check it, so an
/// unvalidated table could give Pass 2 non-monotone or empty entries.
impl AccuracyEvidence for MeasuredSketchAccuracy {
    /// The entry for exactly `parameters` and `statistic`, in the
    /// readout's own metric (`Rank` for a KLL quantile), at its largest
    /// recorded failure probability at most `failure_probability`; the
    /// guarantee states that recorded failure probability. `None` when no
    /// entry matches, below its smallest recorded failure probability, at
    /// a failure probability that is not positive, and for a θ whose
    /// theoretical guarantee can be deterministic (DDSketch).
    fn measured_guarantee(
        &self,
        summary_type: &FieldDataType,
        parameters: &SketchParams,
        statistic: &SketchStatistic,
        failure_probability: f64,
    ) -> Option<AccuracyGuarantee> {
        if failure_probability.is_nan() || failure_probability <= 0.0 {
            return None;
        }
        let metric = readout_error_metric(summary_type, statistic)?;
        // Look the entry up first: the deterministic-guarantee check below
        // computes a theoretical guarantee, so it runs only for a θ that
        // has an entry.
        let entry = self.entry(parameters, statistic, metric)?;
        if has_deterministic_guarantee(summary_type, parameters, statistic) {
            return None;
        }
        let point = entry.point_at_most(failure_probability)?;
        Some(AccuracyGuarantee::new(
            metric,
            BoundExpr::Constant { value: point.error },
            ProbabilityExpr::Constant {
                value: point.failure_probability,
            },
        ))
    }
}
