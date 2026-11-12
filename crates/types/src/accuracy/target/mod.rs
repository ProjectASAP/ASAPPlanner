//! Accuracy requirements and the error metrics they are stated in.
//!
//! A requirement and a guarantee share one outer shape, [`AccuracySpec`]:
//! an error metric, an error ε, a failure probability δ and a scope. A
//! requirement ([`AccuracyTarget`]) holds concrete numbers and may leave its
//! metric unset; a guarantee ([`crate::accuracy::AccuracyGuarantee`])
//! always states its metric and holds symbolic expressions that plan
//! selection binds. A target without a metric accepts a guarantee in any
//! metric that meets its ε, δ and scope.

use serde::{Deserialize, Serialize};

/// The stream norm an [`AccuracyMetric::Absolute`] error is stated in
/// multiples of. A scaled error `ε` means `|estimate − truth| ≤ ε · ‖f‖`,
/// where `f` is the frequency vector of the one stream the estimate's
/// summaries read: `‖f‖₁` for Count-Min, `‖f‖₂` for Count Sketch. Two scaled
/// errors compare directly only over the same stream; restating one in value
/// units multiplies it by that stream's norm, a statistic plan selection
/// binds from the data workload (`stream_l1_norm[source]`,
/// `stream_l2_norm[source]`). Since `‖f‖₂ ≤ ‖f‖₁`, an L2-scaled error meets
/// an L1-scaled requirement of the same stream with the same ε.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NormScale {
    StreamL1,
    StreamL2,
}

/// Which error quantity an accuracy statement bounds. `#[non_exhaustive]`:
/// a metric is added only when a primitive needs a notion these cannot
/// express (e.g. a false-positive rate), and downstream matches must not
/// assume the list is closed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "MetricName", from = "MetricName")]
pub enum AccuracyMetric {
    /// `|estimate − truth| ≤ ε`: in the value's own units when `scale` is
    /// `None`, or in multiples of a stream norm ([`NormScale`]).
    Absolute { scale: Option<NormScale> },
    /// `|estimate − truth| ≤ ε · |truth|`, e.g. DDSketch's α or a distinct
    /// count's relative error.
    Relative,
    /// The returned value's rank in the input multiset is within `ε · n` of
    /// the requested rank (a quantile's KLL ε). Says nothing about how far
    /// the returned value is from the true quantile value.
    Rank,
}

impl AccuracyMetric {
    /// Absolute error in the value's own units.
    pub const ABSOLUTE: Self = Self::Absolute { scale: None };
    /// Absolute error in multiples of the stream's L1 norm (Count-Min).
    pub const ABSOLUTE_STREAM_L1: Self = Self::Absolute {
        scale: Some(NormScale::StreamL1),
    };
    /// Absolute error in multiples of the stream's L2 norm (Count Sketch).
    pub const ABSOLUTE_STREAM_L2: Self = Self::Absolute {
        scale: Some(NormScale::StreamL2),
    };
    /// Every metric, for a contract or a target read in each of them.
    pub const ALL: [Self; 5] = [
        Self::ABSOLUTE,
        Self::ABSOLUTE_STREAM_L1,
        Self::ABSOLUTE_STREAM_L2,
        Self::Relative,
        Self::Rank,
    ];
}

/// The wire name of an [`AccuracyMetric`].
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MetricName {
    Absolute,
    AbsoluteStreamL1,
    AbsoluteStreamL2,
    Relative,
    Rank,
}

impl From<AccuracyMetric> for MetricName {
    fn from(metric: AccuracyMetric) -> Self {
        match metric {
            AccuracyMetric::Absolute { scale: None } => Self::Absolute,
            AccuracyMetric::Absolute {
                scale: Some(NormScale::StreamL1),
            } => Self::AbsoluteStreamL1,
            AccuracyMetric::Absolute {
                scale: Some(NormScale::StreamL2),
            } => Self::AbsoluteStreamL2,
            AccuracyMetric::Relative => Self::Relative,
            AccuracyMetric::Rank => Self::Rank,
        }
    }
}

impl From<MetricName> for AccuracyMetric {
    fn from(name: MetricName) -> Self {
        match name {
            MetricName::Absolute => Self::ABSOLUTE,
            MetricName::AbsoluteStreamL1 => Self::ABSOLUTE_STREAM_L1,
            MetricName::AbsoluteStreamL2 => Self::ABSOLUTE_STREAM_L2,
            MetricName::Relative => Self::Relative,
            MetricName::Rank => Self::Rank,
        }
    }
}

/// Which output values one (ε, δ) statement covers *simultaneously*.
///
/// A scope bounds only the failure probability: a per-value guarantee with
/// failure probability `δ` holds over the `n` values a scope covers with
/// `n · δ` by the union bound ([`Self::max_values_covered`]), whatever their
/// dependence. Not `Eq`: a record share is an `f64`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccuracyScope {
    /// Each output value (one series or row, at one evaluation) is within ε
    /// of its true value with probability ≥ 1 − δ, separately. Values may
    /// fail in different runs; nothing is said about all of them at once.
    /// Sketches and composition produce guarantees of this scope.
    PerValue,
    /// Every value in one evaluation's returned result is within ε
    /// simultaneously, with probability ≥ 1 − δ. The default of a target: a
    /// target at a query root applies to the entire returned result.
    WholeResult,
    /// Every group (result row) holding at least this share of the records
    /// the query aggregates in one evaluation is within ε simultaneously,
    /// with probability ≥ 1 − δ; smaller groups are not covered. The share
    /// is a fraction in (0, 1] (0.05 for 5%). When a row reads several
    /// aggregations, its group must hold the share of each one's records.
    /// At most `⌊1 / share⌋` groups can qualify, whatever the number of
    /// groups. JSON: `{"groups_with_record_share_at_least": 0.05}`.
    GroupsWithRecordShareAtLeast(#[serde(deserialize_with = "record_share")] f64),
}

/// Relative tolerance for `⌊1 / share⌋`, so a share such as 0.05 whose
/// reciprocal computes just below an integer counts that integer.
const RECORD_SHARE_ROUNDING_TOLERANCE: f64 = 1e-9;

/// A record share, rejected unless it is a fraction in (0, 1].
fn record_share<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    let share = f64::deserialize(deserializer)?;
    if share > 0.0 && share <= 1.0 {
        Ok(share)
    } else {
        Err(serde::de::Error::custom(format!(
            "a record share is a fraction in (0, 1], got {share}"
        )))
    }
}

impl AccuracyScope {
    /// How many values one evaluation's statement over this scope covers at
    /// most, given the `values_per_result` (`G`) the result returns, `None`
    /// when unbound: one for [`Self::PerValue`], `G` for
    /// [`Self::WholeResult`], and `min(⌊1 / share⌋, G)` for
    /// [`Self::GroupsWithRecordShareAtLeast`], which is bound even when `G`
    /// is not. Sizing divides a target's failure probability by it, and the
    /// guarantee check multiplies a per-value one by it, so the strictest of
    /// several scopes is the one that covers the most values.
    pub fn max_values_covered(self, values_per_result: Option<u64>) -> Option<u64> {
        match self {
            Self::PerValue => Some(1),
            Self::WholeResult => values_per_result,
            // A share outside (0, 1] (possible only through the Rust API;
            // JSON rejects it) bounds nothing, so its target stays unmet.
            Self::GroupsWithRecordShareAtLeast(share) if !(share > 0.0 && share <= 1.0) => None,
            Self::GroupsWithRecordShareAtLeast(share) => {
                // More than `1 / share` groups cannot each hold `share` of
                // the records. Rounding up near an integer keeps `1 / 0.05`
                // at 20 when it computes just below; it only enlarges the
                // count, which is conservative.
                let qualifying =
                    (share.recip() * (1.0 + RECORD_SHARE_ROUNDING_TOLERANCE)).floor() as u64;
                Some(values_per_result.map_or(qualifying, |values| values.min(qualifying)))
            }
        }
    }
}

/// The outer shape of an accuracy guarantee
/// ([`crate::accuracy::AccuracyGuarantee`]):
/// `Pr[err_metric(estimate, truth) > error] ≤ failure_probability` over the
/// values `accuracy_scope` names. An [`AccuracyTarget`] has the same fields, with
/// concrete numbers, an optional metric and `max_error` for `error`: a target
/// states the largest error a query accepts, a guarantee the error bound
/// (an expression over the plan's parameters) its plan achieves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccuracySpec<E, P> {
    pub accuracy_metric: AccuracyMetric,
    /// ε: the error bound, in `accuracy_metric`'s units.
    pub error: E,
    /// δ.
    pub failure_probability: P,
    pub accuracy_scope: AccuracyScope,
}

/// An accuracy requirement, as a workload, a caller or a frontend states
/// it: `Pr[err_metric(estimate, truth) > max_error] ≤ failure_probability` over
/// the values `accuracy_scope` names.
///
/// `accuracy_metric: None` means the user did not set one: plan selection
/// then accepts a guarantee in any metric whose ε, δ and scope meet this
/// target's, and reports the metric it accepted. A set metric accepts only
/// guarantees in that metric (or convertible to it with evidence).
/// `failure_probability: None` imposes no δ requirement (sizing then uses
/// the planner's default δ). Exact is `max_error = 0`,
/// `failure_probability = Some(0)`, whatever the metric.
///
/// Built with [`Self::exact`] or [`Self::max_error`] plus the optional
/// builders [`Self::failure_probability`], [`Self::accuracy_metric`] and
/// [`Self::accuracy_scope`].
///
/// JSON: `{"accuracy_metric": "rank", "max_error": 0.01, "failure_probability":
/// 0.001, "accuracy_scope": "whole_result"}`; every field but `max_error` may be
/// omitted (`accuracy_scope` defaults to `whole_result`). Exact is
/// `{"max_error": 0.0, "failure_probability": 0.0}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccuracyTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accuracy_metric: Option<AccuracyMetric>,
    /// ε: the largest error the query accepts, in the metric's units.
    pub max_error: f64,
    /// δ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_probability: Option<f64>,
    #[serde(default = "whole_result")]
    pub accuracy_scope: AccuracyScope,
}

fn whole_result() -> AccuracyScope {
    AccuracyScope::WholeResult
}

impl AccuracyTarget {
    /// No approximation permitted.
    pub const fn exact() -> Self {
        Self {
            accuracy_metric: None,
            max_error: 0.0,
            failure_probability: Some(0.0),
            accuracy_scope: AccuracyScope::WholeResult,
        }
    }

    /// Error at most `max_error` over the whole result, with no δ
    /// requirement and no metric set.
    pub const fn max_error(max_error: f64) -> Self {
        Self {
            accuracy_metric: None,
            max_error,
            failure_probability: None,
            accuracy_scope: AccuracyScope::WholeResult,
        }
    }

    /// This target, met with probability at least `1 − failure_probability`.
    pub const fn failure_probability(self, failure_probability: f64) -> Self {
        Self {
            failure_probability: Some(failure_probability),
            ..self
        }
    }

    /// This target with its error stated in `accuracy_metric`.
    pub const fn accuracy_metric(self, accuracy_metric: AccuracyMetric) -> Self {
        Self {
            accuracy_metric: Some(accuracy_metric),
            ..self
        }
    }

    /// This target over `accuracy_scope`.
    pub const fn accuracy_scope(self, accuracy_scope: AccuracyScope) -> Self {
        Self {
            accuracy_scope,
            ..self
        }
    }

    /// Whether this target permits no approximation.
    pub fn is_exact(&self) -> bool {
        self.max_error == 0.0 && self.failure_probability == Some(0.0)
    }

    /// The metrics this target accepts an error in: its metric, every
    /// metric when it names none.
    pub fn metrics(&self) -> Vec<AccuracyMetric> {
        AccuracyMetric::ALL
            .into_iter()
            .filter(|&metric| self.accuracy_metric.is_none_or(|named| named == metric))
            .collect()
    }
}

#[cfg(test)]
mod tests;
