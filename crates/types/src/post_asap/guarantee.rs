//! Machine-readable accuracy guarantees for finalized post-ASAP values
//! (issue #172).
//!
//! A selected post-ASAP plan used to carry no statement about the error of
//! the value it produces: every approximate layer was sized from its own
//! [`AccuracyTarget`] as if its input were exact, so an approximate parent
//! could silently consume an approximate child. This module is the
//! *vocabulary* that fixes that — the typed metric, the symbolic bound and
//! failure-probability expressions, the provenance trail, and the typed
//! rejection reasons. The *algebra* that composes these (the `AccuracyModel`
//! trait, its default conservative rules, and budget allocation) lives one
//! layer up in `asap_aware_mapping::accuracy`, the same layering
//! [`crate::dag_export`] keeps for cost decisions: this crate defines the
//! shapes, the planning crate decides.
//!
//! ## What a guarantee says
//!
//! [`ResultGuarantee`] is attached to a finalized, caller-visible value —
//! [`super::SummaryNode::guarantee`] on a `SummaryEstimate` readout, an
//! exact accumulator, or a kept pre-ASAP subtree — never to raw summary
//! state (a `SummaryAgg` sketch node carries `None`; its readout carries the
//! guarantee). Its statement is:
//!
//! ```text
//! Pr[ err_metric(estimate, truth) > bound ] <= failure_probability
//! ```
//!
//! where `err_metric` is fixed by [`ErrorMetric`] and each metric has its
//! own normalization (documented per variant). Metrics are **not**
//! interchangeable: a cardinality error and a frequency error are different
//! quantities, and composing them needs an explicit rule, never an implicit
//! "add the epsilons".
//!
//! ## Why expressions, not numbers
//!
//! [`BoundExpr`]/[`ProbabilityExpr`] are tiny serializable expression trees
//! rather than bare `f64`s so a planning-time guarantee can reference a
//! statistic it does not have (a group count, a stream's L1 norm) and stay
//! honestly *unknown* until something instantiates it — a deployment's own
//! cardinality estimate, or a runtime posterior observation (issue #239).
//! [`BoundExpr::evaluate`] returns `None`, never `0`, for such a bound;
//! "unknown" and "zero" are different answers and a fail-closed planner
//! treats them differently.
//!
//! ## What is deliberately *not* here
//!
//! No `CorrectnessPolicy`-style enum: [`AccuracyTarget`] remains the one
//! authoritative requirement type, and [`AccuracyError`] is the typed reason
//! a candidate failed against it. No independence assumptions: the only
//! probability combinator is the union bound.

use serde::{Deserialize, Serialize};

use crate::types::AccuracyTarget;

/// Which error quantity a [`ResultGuarantee`] bounds. `#[non_exhaustive]`:
/// a deployment's own `AccuracyModel` may need a metric this crate does not
/// enumerate yet, and downstream matches must not assume the list is closed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorMetric {
    /// `|estimate − truth| ≤ bound`, in the value's own units.
    AbsoluteValue,
    /// `|estimate − truth| ≤ bound · |truth|` — a multiplicative guarantee,
    /// only meaningful for values of known sign (DDSketch's α).
    RelativeValue,
    /// The returned value's *rank* in the input multiset is within
    /// `bound · n` of the requested rank (KLL's ε). Says nothing about how
    /// far the returned *value* is from the true quantile value.
    Rank,
    /// `|estimate − truth| ≤ bound · truth` for a distinct count (HLL/Theta/
    /// KMV's relative standard error).
    Cardinality,
    /// `|estimate − truth| ≤ bound · ‖f‖₁` for a point-frequency query
    /// (CMS's ε, normalized by the stream's L1 norm).
    Frequency,
    /// `|estimate − truth| ≤ bound · ‖f‖₂` for a point-frequency query.
    /// CountSketch uses this normalization; it is intentionally distinct
    /// from CMS's [`ErrorMetric::Frequency`] (`L1`) guarantee.
    L2Frequency,
    /// The returned key set equals the true top-k set. The built-in model
    /// produces this only from a supplied per-key interval margin certificate;
    /// a frequency bound alone is insufficient.
    TopKMembership,
}

/// A symbolic, serializable error-bound expression. Non-negative real
/// arithmetic only — there is no subtraction, so a bound can never be
/// tightened by construction, only by evaluating a known statistic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BoundExpr {
    /// Exactly zero error (deterministic exact computation).
    Zero,
    /// A resolved numeric bound in the metric's own normalization.
    Constant { value: f64 },
    /// `Σ terms` — the additive composition rule.
    Sum { terms: Vec<BoundExpr> },
    /// `Π factors` — e.g. the relative-error cross term, or a normalized
    /// bound times the (possibly unknown) statistic it is normalized by.
    Product { factors: Vec<BoundExpr> },
    /// `factor · inner` — an explicitly registered Lipschitz constant.
    Scaled { factor: f64, inner: Box<BoundExpr> },
    /// `max(terms)` — exact max/min over bounded inputs.
    Max { terms: Vec<BoundExpr> },
    /// A statistic this bound needs but nothing has supplied yet. Evaluates
    /// to `None`, never `0`: an unknown quantity is not a small one.
    Unknown { statistic: String },
}

impl BoundExpr {
    /// Numeric value of this bound, or `None` if any [`BoundExpr::Unknown`]
    /// leaf is reachable.
    pub fn evaluate(&self) -> Option<f64> {
        let value = match self {
            BoundExpr::Zero => Some(0.0),
            BoundExpr::Constant { value } => value.is_finite().then_some(*value),
            BoundExpr::Sum { terms } => terms.iter().map(BoundExpr::evaluate).sum(),
            BoundExpr::Product { factors } => factors.iter().map(BoundExpr::evaluate).product(),
            BoundExpr::Scaled { factor, inner } => factor
                .is_finite()
                .then_some(*factor)
                .zip(inner.evaluate())
                .map(|(factor, bound)| factor * bound),
            BoundExpr::Max { terms } => terms
                .iter()
                .map(BoundExpr::evaluate)
                .try_fold(0.0_f64, |acc, t| t.map(|t| acc.max(t))),
            BoundExpr::Unknown { .. } => None,
        }?;
        (value.is_finite() && value >= 0.0).then_some(value)
    }

    /// `true` iff this bound is structurally zero (every leaf is
    /// [`BoundExpr::Zero`], or a `Product`/`Scaled` contains a zero factor).
    /// Distinct from `evaluate() == Some(0.0)` only in that it never
    /// depends on floating-point evaluation.
    pub fn is_zero(&self) -> bool {
        match self {
            BoundExpr::Zero => true,
            BoundExpr::Constant { value } => *value == 0.0,
            BoundExpr::Sum { terms } | BoundExpr::Max { terms } => {
                terms.iter().all(BoundExpr::is_zero)
            }
            BoundExpr::Product { factors } => factors.iter().any(BoundExpr::is_zero),
            BoundExpr::Scaled { factor, inner } => *factor == 0.0 || inner.is_zero(),
            BoundExpr::Unknown { .. } => false,
        }
    }
}

/// A symbolic, serializable failure-probability expression. The only
/// combinator over several events is the union bound — the default model
/// never assumes independence between sketch errors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ProbabilityExpr {
    /// The guarantee is deterministic.
    Zero,
    /// A resolved probability in `[0, 1]`.
    Constant { value: f64 },
    /// `min(1, Σ terms)` — Boole's inequality over the listed events.
    UnionBound { terms: Vec<ProbabilityExpr> },
    /// `min(1, count · inner)` — the union bound over `count` events that
    /// each fail with probability at most `inner` (e.g. one per input row of
    /// an exact aggregation). `count` is a [`BoundExpr`] so it may be an
    /// [`BoundExpr::Unknown`] statistic.
    Scaled {
        count: BoundExpr,
        inner: Box<ProbabilityExpr>,
    },
    /// A probability nothing has supplied yet — same stance as
    /// [`BoundExpr::Unknown`].
    Unknown { statistic: String },
}

impl ProbabilityExpr {
    /// Numeric value clamped to `[0, 1]`, or `None` if any unknown leaf is
    /// reachable.
    pub fn evaluate(&self) -> Option<f64> {
        let raw = match self {
            ProbabilityExpr::Zero => 0.0,
            ProbabilityExpr::Constant { value } if (0.0..=1.0).contains(value) => *value,
            ProbabilityExpr::Constant { .. } => return None,
            ProbabilityExpr::UnionBound { terms } => terms
                .iter()
                .map(ProbabilityExpr::evaluate)
                .sum::<Option<f64>>()?,
            ProbabilityExpr::Scaled { count, inner } => count.evaluate()? * inner.evaluate()?,
            ProbabilityExpr::Unknown { .. } => return None,
        };
        raw.is_finite().then(|| raw.clamp(0.0, 1.0))
    }

    /// `true` iff this probability is structurally zero.
    pub fn is_zero(&self) -> bool {
        match self {
            ProbabilityExpr::Zero => true,
            ProbabilityExpr::Constant { value } => *value == 0.0,
            ProbabilityExpr::UnionBound { terms } => terms.iter().all(ProbabilityExpr::is_zero),
            ProbabilityExpr::Scaled { count, inner } => count.is_zero() || inner.is_zero(),
            ProbabilityExpr::Unknown { .. } => false,
        }
    }
}

/// How a parent operator consumes its inputs' values — the shape an
/// `AccuracyModel::propagate` rule is registered against. `#[non_exhaustive]`
/// for the same reason [`ErrorMetric`] is.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CompositionOperator {
    /// An approximate summary built over its inputs' (approximate) values
    /// — the sketch-over-sketch case. Its own `local` guarantee composes
    /// with the inputs' under a same-metric rule.
    ApproximateAggregate,
    /// A deterministic transformation with an explicitly registered global
    /// Lipschitz constant: `B_out ≤ constant · B_in + B_local`. The planner
    /// never derives `constant` itself; only a caller that has proved it
    /// may construct this operator.
    Lipschitz { constant: f64 },
    /// An exact sum over approximate inputs: `B ≤ Σ B_i`, `δ ≤ Σ δ_i`.
    ExactSum,
    /// An exact max/min over approximate inputs — bounds the returned
    /// *value* (`max` of the input bounds) but does not identify which key
    /// is the true winner.
    ExactExtremum,
    /// A top-k selection over approximate inputs. Unsupported by the default
    /// model until the margin certificate of issue #172 PR 3 exists.
    TopKSelection,
}

/// One entry in a [`ResultGuarantee`]'s provenance trail — enough for a
/// reader to reconstruct *why* the bound is what it is without re-running
/// the planner. `#[non_exhaustive]` so runtime evidence (issue #239) and
/// deployment-specific sources can be appended later.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GuaranteeSource {
    /// Deterministic exact computation — zero error by construction.
    Exact {
        /// What made it exact (e.g. `"ExactAggregate(Sum)"`,
        /// `"KeepPreAsap"`).
        reason: String,
    },
    /// The target this readout's sketch was sized against.
    AccuracyTarget { target: AccuracyTarget },
    /// The concrete sketch a readout's local guarantee was derived from.
    SketchReadout {
        algorithm: String,
        /// Stable estimator/analysis contract used to derive this guarantee.
        #[serde(default)]
        contract: String,
        params: serde_json::Value,
        query: String,
    },
    /// A composed input's own guarantee, carried verbatim so the trail is
    /// self-contained. `input_index` is the input's position in the
    /// composition (0-based, in the parent's child order).
    ChildGuarantee {
        input_index: usize,
        guarantee: Box<ResultGuarantee>,
    },
    /// The propagation rule that produced this guarantee from its inputs.
    CompositionStep {
        operator: CompositionOperator,
        /// Stable rule name (e.g. `"additive_union_bound"`).
        rule: String,
    },
    /// The budget split that produced this layer's local target — present
    /// only when an `AccuracyBudgetAllocator` re-sized a layer.
    BudgetAllocation {
        allocator: String,
        layer: usize,
        layer_count: usize,
        local_target: AccuracyTarget,
        end_to_end_target: AccuracyTarget,
    },
    /// A statistic the bound needs but nothing supplied — the reason a
    /// [`BoundExpr::Unknown`]/[`ProbabilityExpr::Unknown`] leaf exists.
    UnavailableStatistic { statistic: String },
    /// Query-time evidence (issue #239's posterior bounds). Never produced
    /// at planning time; reserved so a runtime can append its observation
    /// to the same trail instead of inventing a parallel one.
    RuntimeObservation {
        source: String,
        detail: serde_json::Value,
    },
}

/// The machine-readable accuracy statement attached to a finalized
/// post-ASAP value — see the module docs for its semantics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultGuarantee {
    pub metric: ErrorMetric,
    pub bound: BoundExpr,
    pub failure_probability: ProbabilityExpr,
    pub provenance: Vec<GuaranteeSource>,
}

impl ResultGuarantee {
    /// The zero-error, zero-failure guarantee of a deterministic exact
    /// computation. `metric` is [`ErrorMetric::AbsoluteValue`]: an exact
    /// value is exact under every metric, and absolute error is the one
    /// every same-metric rule accepts as a zero input.
    pub fn exact(reason: impl Into<String>) -> Self {
        Self {
            metric: ErrorMetric::AbsoluteValue,
            bound: BoundExpr::Zero,
            failure_probability: ProbabilityExpr::Zero,
            provenance: vec![GuaranteeSource::Exact {
                reason: reason.into(),
            }],
        }
    }

    /// `true` iff this guarantee promises zero error with certainty.
    pub fn is_exact(&self) -> bool {
        self.bound.is_zero() && self.failure_probability.is_zero()
    }

    /// How many approximate sketch readouts contributed to this value —
    /// `1` for a plain readout, `0` for an exact value, and the transitive
    /// count through every [`GuaranteeSource::ChildGuarantee`] for a
    /// composition. An `AccuracyBudgetAllocator` uses this as the number
    /// of layers a budget must be split across.
    pub fn approximate_layer_count(&self) -> usize {
        self.provenance
            .iter()
            .map(|source| match source {
                GuaranteeSource::SketchReadout { .. } => 1,
                GuaranteeSource::ChildGuarantee { guarantee, .. } => {
                    guarantee.approximate_layer_count()
                }
                _ => 0,
            })
            .sum()
    }
}

/// Why an accuracy check rejected a candidate. Typed, serializable, and
/// carried through to DAG export so a rejection is as inspectable as a
/// selection. Never a reason to "treat the child as exact".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccuracyError {
    /// No registered propagation rule covers this operator over these
    /// input metrics (or this local metric).
    #[error(
        "unsupported accuracy composition: {operator:?} over inputs {input_metrics:?} \
         with local {local_metric:?} — {reason}"
    )]
    UnsupportedComposition {
        operator: CompositionOperator,
        input_metrics: Vec<ErrorMetric>,
        local_metric: Option<ErrorMetric>,
        reason: String,
    },
    /// An approximate input carries no guarantee at all, so nothing can be
    /// composed over it.
    #[error("input {input_index} of {operator:?} carries no accuracy guarantee")]
    MissingInputGuarantee {
        operator: CompositionOperator,
        input_index: usize,
    },
    /// The composed guarantee does not satisfy the applicable
    /// [`AccuracyTarget`]. `bound`/`failure_probability` are the evaluated
    /// values when known.
    #[error(
        "composed guarantee ({metric:?}, bound {bound:?}, failure probability \
         {failure_probability:?}) does not satisfy {target:?}"
    )]
    TargetNotSatisfied {
        metric: ErrorMetric,
        bound: Option<f64>,
        failure_probability: Option<f64>,
        target: AccuracyTarget,
    },
    /// No budget allocation could make the composition legal under the
    /// end-to-end target.
    #[error("no legal accuracy-budget allocation for {target:?} across {layer_count} layers")]
    NoLegalAllocation {
        target: AccuracyTarget,
        layer_count: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_statistic_never_evaluates_to_zero() {
        let b = BoundExpr::Product {
            factors: vec![
                BoundExpr::Constant { value: 0.01 },
                BoundExpr::Unknown {
                    statistic: "input_row_count".into(),
                },
            ],
        };
        assert_eq!(b.evaluate(), None);
        assert!(!b.is_zero());
        let p = ProbabilityExpr::Scaled {
            count: BoundExpr::Unknown {
                statistic: "input_row_count".into(),
            },
            inner: Box::new(ProbabilityExpr::Constant { value: 0.01 }),
        };
        assert_eq!(p.evaluate(), None);
    }

    #[test]
    fn invalid_numeric_leaves_fail_closed() {
        assert_eq!(BoundExpr::Constant { value: -0.1 }.evaluate(), None);
        assert_eq!(
            BoundExpr::Scaled {
                factor: -1.0,
                inner: Box::new(BoundExpr::Constant { value: 0.1 }),
            }
            .evaluate(),
            None
        );
        assert_eq!(ProbabilityExpr::Constant { value: -0.1 }.evaluate(), None);
        assert_eq!(ProbabilityExpr::Constant { value: 1.1 }.evaluate(), None);
    }

    #[test]
    fn union_bound_sums_and_clamps() {
        let p = ProbabilityExpr::UnionBound {
            terms: vec![
                ProbabilityExpr::Constant { value: 0.7 },
                ProbabilityExpr::Constant { value: 0.6 },
            ],
        };
        assert_eq!(p.evaluate(), Some(1.0));
    }

    #[test]
    fn exact_guarantee_is_zero_layers() {
        let g = ResultGuarantee::exact("test");
        assert!(g.is_exact());
        assert_eq!(g.approximate_layer_count(), 0);
    }

    #[test]
    fn guarantee_round_trips_through_json() {
        let g = ResultGuarantee {
            metric: ErrorMetric::Frequency,
            bound: BoundExpr::Sum {
                terms: vec![
                    BoundExpr::Constant { value: 0.01 },
                    BoundExpr::Scaled {
                        factor: 2.0,
                        inner: Box::new(BoundExpr::Constant { value: 0.005 }),
                    },
                ],
            },
            failure_probability: ProbabilityExpr::UnionBound {
                terms: vec![ProbabilityExpr::Constant { value: 0.01 }],
            },
            provenance: vec![GuaranteeSource::CompositionStep {
                operator: CompositionOperator::Lipschitz { constant: 2.0 },
                rule: "lipschitz".into(),
            }],
        };
        let json = serde_json::to_value(&g).unwrap();
        assert_eq!(json["metric"], "frequency");
        assert_eq!(json["bound"]["op"], "sum");
        let back: ResultGuarantee = serde_json::from_value(json).unwrap();
        assert_eq!(back, g);
    }
}
