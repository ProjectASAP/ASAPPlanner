//! Scoped source contracts and statistics the accuracy rules consult.
use super::*;

/// An enforced domain for the true values of one operand of an exact
/// arithmetic operation, in every evaluation: they are finite and lie in
/// `[lower, upper]`. Plan selection uses it to bound a product or quotient
/// of estimates; for a quantile operand of a ratio (one use) it also
/// promises a nonempty sample population in this interval, which the
/// DDSketch ratio rule needs. Sampled min/max statistics are not a proof:
/// the contract must be enforced by the source or execution layer.
#[derive(Debug, Clone, PartialEq)]
pub struct OperandDomain {
    pub lower: f64,
    pub upper: f64,
    /// Upper bound on samples per evaluation, matching the pinned evaluation's
    /// exact Float64 rank limit; the population must also be nonempty. Only
    /// the quantile ratio rule reads it.
    pub max_samples: u64,
    pub contract: String,
}

/// Statistics a propagation rule may consult. Every field is optional and
/// defaults to "unknown": a rule that needs a missing statistic emits a
/// [`BoundExpr::Unknown`] leaf (or rejects) rather than guessing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PropagationStats {
    /// Certified finite ranges for the true numerator and denominator values.
    /// Quantile ratios obtain these from their enforced input domains.
    pub division_operand_domains: Option<[OperandDomain; 2]>,
    /// Provenance for supplied evidence (source, observation identity, etc.).
    pub evidence_provenance: Vec<GuaranteeSource>,
    /// Whether every input value is known to be non-negative — required by
    /// the multiplicative relative-error rule, which is unsound across a
    /// sign change.
    pub values_non_negative: Option<bool>,
    /// The statistic an unknown sign is named by when the folded values are
    /// one metric's raw data samples or plain sums of them
    /// ([`raw_data_samples_sign_statistic`](crate::accuracy::composition::raw_data_samples_sign_statistic)):
    /// the relative bound is then gated on it, and plan selection binds it
    /// to 1 when the metric is declared a counter. `None`: the bare
    /// `values_non_negative`, which nothing binds.
    pub sign_statistic: Option<String>,
    /// Number of input rows an exact aggregation consumes (e.g. the number
    /// of groups a function folds), for exact aggregate union bounds
    /// bound over per-input failures.
    pub input_row_count: Option<u64>,
    /// The statistic an unknown row count is named by (default
    /// `input_row_count`), so a fold whose rows the data workload's input
    /// cardinality does not bound can leave a statistic nothing binds.
    pub row_count_statistic: Option<&'static str>,
    /// Lower confidence bound of the kth selected TopK item, after widening
    /// the interval by the sketch's own estimation error.
    pub topk_selected_lower_bound: Option<f64>,
    /// Greatest upper confidence bound among excluded TopK items, after
    /// widening the interval by the sketch's own estimation error.
    pub topk_excluded_upper_bound: Option<f64>,
    /// Union-bound failure probability of all intervals used by the margin
    /// certificate.
    pub topk_interval_failure_probability: Option<f64>,
}
