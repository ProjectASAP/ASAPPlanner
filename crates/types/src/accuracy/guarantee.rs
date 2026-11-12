//! Machine-readable accuracy guarantees: the vocabulary for what error a
//! finalized, caller-visible value has — the typed metric
//! ([`AccuracyGuarantee`]) and the bound and failure-probability
//! expressions ([`BoundExpr`], [`ProbabilityExpr`]). The theoretical
//! guarantee of a summary (`theoretical_guarantee`), each operator's error
//! contract and the deployment's measured accuracy (`AccuracyEvidence`)
//! live one layer up in `asap_accuracy`: this crate defines the shapes, the
//! planning crates decide.
//!
//! ## What a guarantee says
//!
//! [`AccuracyGuarantee`] describes a finalized, caller-visible value — a
//! `SummaryEstimate` evaluation or an exact accumulator — never raw summary
//! state. Its statement is:
//!
//! ```text
//! Pr[ err_metric(estimate, truth) > error ] <= failure_probability
//! ```
//!
//! for each output value separately ([`AccuracyScope::PerValue`]), where
//! `err_metric` is fixed by [`AccuracyMetric`]: an absolute error (in value
//! units or in multiples of a stream norm), a relative error, or a rank
//! error. Metrics are **not** interchangeable: converting between them
//! needs an explicit rule with evidence, never an implicit "compare the
//! epsilons". [`AccuracyGuarantee`] shares its outer shape, [`AccuracySpec`],
//! with the requirement it is checked against, [`AccuracyTarget`](super::AccuracyTarget).
//!
//! A failure probability may be *unknown* ([`ProbabilityExpr::Unknown`],
//! HLL's): [`ProbabilityExpr::evaluate`] returns `None`, never `0`, for it;
//! "unknown" and "zero" are different answers and a fail-closed planner
//! treats them differently.

use serde::{Deserialize, Serialize};

use super::target::{AccuracyMetric, AccuracyScope, AccuracySpec};

/// A serializable error bound, in the metric's own normalization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BoundExpr {
    /// A resolved numeric bound.
    Constant { value: f64 },
}

impl BoundExpr {
    /// Numeric value of this bound; `None` when it is negative or not
    /// finite.
    pub fn evaluate(&self) -> Option<f64> {
        let BoundExpr::Constant { value } = self;
        (value.is_finite() && *value >= 0.0).then_some(*value)
    }
}

/// A serializable failure-probability expression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ProbabilityExpr {
    /// The guarantee is deterministic.
    Zero,
    /// A resolved probability in `[0, 1]`.
    Constant { value: f64 },
    /// A probability nothing has supplied (HLL's). Evaluates to `None`,
    /// never `0`: an unknown probability is not a small one.
    Unknown { statistic: String },
}

impl ProbabilityExpr {
    /// Numeric value in `[0, 1]`; `None` when it is unknown or out of
    /// range.
    pub fn evaluate(&self) -> Option<f64> {
        match self {
            ProbabilityExpr::Zero => Some(0.0),
            ProbabilityExpr::Constant { value } if (0.0..=1.0).contains(value) => Some(*value),
            ProbabilityExpr::Constant { .. } | ProbabilityExpr::Unknown { .. } => None,
        }
    }
}

/// The machine-readable accuracy statement of a finalized post-ASAP value
/// — see the module docs for its semantics. A guarantee a sketch produces
/// covers each output value separately ([`AccuracyScope::PerValue`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccuracyGuarantee {
    pub spec: AccuracySpec<BoundExpr, ProbabilityExpr>,
}

impl AccuracyGuarantee {
    /// A per-value guarantee: error at most `error` in `metric`, failing
    /// with probability at most `failure_probability`.
    pub fn new(
        metric: AccuracyMetric,
        error: BoundExpr,
        failure_probability: ProbabilityExpr,
    ) -> Self {
        Self {
            spec: AccuracySpec {
                accuracy_metric: metric,
                error,
                failure_probability,
                accuracy_scope: AccuracyScope::PerValue,
            },
        }
    }

    /// The zero-error, zero-failure guarantee of a deterministic exact
    /// computation. `metric` is [`AccuracyMetric::ABSOLUTE`]: an exact value
    /// is exact under every metric, and absolute error is the one every
    /// same-metric rule accepts as a zero input.
    pub fn exact() -> Self {
        Self::new(
            AccuracyMetric::ABSOLUTE,
            BoundExpr::Constant { value: 0.0 },
            ProbabilityExpr::Zero,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unknown failure probability never evaluates to zero.
    #[test]
    fn an_unknown_failure_probability_never_evaluates_to_zero() {
        let unknown = ProbabilityExpr::Unknown {
            statistic: "hll_failure_probability".into(),
        };
        assert_eq!(unknown.evaluate(), None);
        assert_eq!(ProbabilityExpr::Zero.evaluate(), Some(0.0));
    }

    #[test]
    fn invalid_numeric_leaves_fail_closed() {
        assert_eq!(BoundExpr::Constant { value: -0.1 }.evaluate(), None);
        assert_eq!(BoundExpr::Constant { value: f64::NAN }.evaluate(), None);
        assert_eq!(ProbabilityExpr::Constant { value: -0.1 }.evaluate(), None);
        assert_eq!(ProbabilityExpr::Constant { value: 1.1 }.evaluate(), None);
    }

    #[test]
    fn guarantee_round_trips_through_json() {
        let g = AccuracyGuarantee::new(
            AccuracyMetric::ABSOLUTE_STREAM_L1,
            BoundExpr::Constant { value: 0.01 },
            ProbabilityExpr::Constant { value: 0.01 },
        );
        let json = serde_json::to_value(&g).unwrap();
        assert_eq!(json["spec"]["accuracy_metric"], "absolute_stream_l1");
        assert_eq!(json["spec"]["accuracy_scope"], "per_value");
        assert_eq!(json["spec"]["error"]["op"], "constant");
        let back: AccuracyGuarantee = serde_json::from_value(json).unwrap();
        assert_eq!(back, g);
    }
}
