//! Planning-time accuracy algebra (issue #172): the [`AccuracyModel`]
//! extension point, its conservative default, and end-to-end
//! accuracy-budget allocation.
//!
//! ## Why a second trait next to `CostModel`
//!
//! Accuracy legality and cost ranking are different responsibilities.
//! [`crate::cost_model::CostModel`] answers "which legal candidate is
//! cheapest"; this module answers "which candidates are legal at all". The
//! pipeline [`crate::replacement`] runs is, in order:
//!
//! ```text
//! candidate generation
//!     -> guarantee propagation            (AccuracyModel::propagate)
//!     -> AccuracyTarget satisfaction      (AccuracyModel::satisfies)
//!     -> legal candidates only            (illegal ones become MemoGroup::rejected)
//!     -> cost ranking / global selection  (CostModel)
//! ```
//!
//! A `CostModel` only ever sees the survivors, so it cannot override a
//! legality decision — the same "permutation only, never prune" contract
//! `CostModel::rank_candidates` already has, applied one stage earlier.
//!
//! ## What the default model admits
//!
//! [`DefaultAccuracyModel`] is deliberately conservative and fail-closed:
//!
//! | operator | rule | result |
//! |---|---|---|
//! | any, all inputs exact | exact input | the local guarantee (or exact) |
//! | `ApproximateAggregate`, all `AbsoluteValue` | additive | `Σ B`, `δ` by union bound |
//! | `ApproximateAggregate`, all `RelativeValue`, values known non-negative | multiplicative | `ε_in + ε_out + ε_in·ε_out`, `δ` by union bound |
//! | `Lipschitz { L }`, one `AbsoluteValue` input | Lipschitz | `L·B_in + B_local`, `δ` by union bound |
//! | `ExactSum`, value-like inputs | sum | `Σ B_i` (`AbsoluteValue`), `δ` by union bound over inputs |
//! | `ExactExtremum`, same-metric inputs | max/min | `max B_i`, `δ` by union bound over inputs |
//! | anything else | — | [`AccuracyError::UnsupportedComposition`] |
//!
//! Cross-metric compositions (a `Rank` error under a value-additive rule,
//! a `Cardinality` error under a `Frequency` sketch, …) have no registered
//! rule and are rejected. The child is **never** treated as exact. Nothing
//! assumes independence: every probability combinator is the union bound.
//! A statistic the rule needs but [`PropagationStats`] does not supply
//! (an input row count, a stream's L1 norm) stays a
//! [`BoundExpr::Unknown`] leaf — the guarantee is still produced, but it
//! cannot satisfy any target until something instantiates the statistic.
//!
//! ## Precedence between root and per-node targets
//!
//! - A root `QueryRequirements.accuracy`, when supplied to
//!   [`crate::replacement::search_workload_with_targets`], is the
//!   end-to-end target for that query's root value. It is checked against
//!   the root group's candidates *before* cost ranking; a candidate whose
//!   guarantee is unknown, or misses the target, is moved to
//!   `MemoGroup::rejected`.
//! - For an approximate node over an **exact** child, the node's own
//!   `AggIntent.accuracy` sizes its sketch, exactly as before this module
//!   existed, and the readout's guarantee is that sketch's local guarantee.
//! - For an approximate node over an **approximate** child, the outer
//!   node's `AggIntent.accuracy` is the end-to-end target *for that value*.
//!   The inner node's `AggIntent.accuracy` is only its declared local
//!   requirement: the as-declared composition is evaluated and kept only if
//!   it satisfies the outer target, and the [`AccuracyBudgetAllocator`]
//!   additionally proposes re-sized splits of the outer target. A front end
//!   that copied the same target onto every node has therefore *not*
//!   produced a valid end-to-end allocation — the composed guarantee is
//!   what decides.
//! - `AccuracyTarget::Exact` on a node admits only exact realizations
//!   (unchanged), and an approximate layer can never satisfy it.

use asap_types::post_asap::{
    AccuracyError, BoundExpr, CompositionOperator, ErrorMetric, GuaranteeSource, ProbabilityExpr,
    ResultGuarantee, SketchAlgorithm, SketchParams, SketchQuery, SummaryFamilyType,
};
use asap_types::types::AccuracyTarget;

/// Statistics a propagation rule may consult. Every field is optional and
/// defaults to "unknown": a rule that needs a missing statistic emits a
/// [`BoundExpr::Unknown`] leaf (or rejects) rather than guessing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PropagationStats {
    /// Provenance for supplied evidence (source, observation identity, etc.).
    pub evidence_provenance: Vec<GuaranteeSource>,
    /// Whether every input value is known to be non-negative — required by
    /// the multiplicative relative-error rule, which is unsound across a
    /// sign change.
    pub values_non_negative: Option<bool>,
    /// Number of input rows an exact aggregation consumes (e.g. the number
    /// of groups a `sum` folds), for `ExactSum`/`ExactExtremum`'s union
    /// bound over per-input failures.
    pub input_row_count: Option<u64>,
    /// Fresh key-frequency distribution evidence from the data workload.
    /// Built-in rules preserve it for deployment-specific accuracy models;
    /// they do not assume a favorable distribution when it is absent.
    pub data_distribution: Option<asap_types::workload::DataDistribution>,
    /// Lower confidence bound of the kth selected TopK item, after widening
    /// the interval by the sketch's own estimation error.
    pub topk_selected_lower_bound: Option<f64>,
    /// Greatest upper confidence bound among excluded TopK items, after
    /// widening the interval by the sketch's own estimation error.
    pub topk_excluded_upper_bound: Option<f64>,
    /// Union-bound failure probability of all intervals used by the margin
    /// certificate.
    pub topk_interval_failure_probability: Option<f64>,
    /// Hydra shared-grid collision error in the inner guarantee's metric.
    pub hydra_shared_grid_collision_bound: Option<f64>,
    /// Failure probability assigned to the Hydra shared-grid term.
    pub hydra_shared_grid_failure_probability: Option<f64>,
}

/// Supplies typed planning-time evidence required by propagation rules.
pub trait AccuracyEvidenceProvider {
    fn propagation_stats(
        &self,
        _op: &CompositionOperator,
        _family: &SummaryFamilyType,
        _query: Option<&SketchQuery>,
    ) -> PropagationStats {
        PropagationStats::default()
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoAccuracyEvidence;

impl AccuracyEvidenceProvider for NoAccuracyEvidence {}

/// Accuracy evidence backed by the normalized data workload. Freshness is
/// checked at the planning time before values reach any accuracy rule.
#[derive(Debug, Clone, Copy)]
pub struct WorkloadAccuracyEvidence<'a> {
    pub data: &'a asap_types::workload::DataWorkload,
    pub now_ms: u64,
}

impl AccuracyEvidenceProvider for WorkloadAccuracyEvidence<'_> {
    fn propagation_stats(
        &self,
        _op: &CompositionOperator,
        _family: &SummaryFamilyType,
        _query: Option<&SketchQuery>,
    ) -> PropagationStats {
        PropagationStats {
            input_row_count: self.data.input_cardinality.value_at(self.now_ms).copied(),
            data_distribution: self.data.distribution.value_at(self.now_ms).cloned(),
            ..PropagationStats::default()
        }
    }
}

/// The deployment-extensible accuracy algebra. `asap-aware-mapping` ships
/// [`DefaultAccuracyModel`]; a deployment with a proof for a composition the
/// default rejects (a registered cross-metric conversion, say) implements
/// this trait and passes it to
/// [`crate::replacement::SketchAlgorithmStrategy::with_models`].
pub trait AccuracyModel {
    /// The guarantee of reading `query` out of a summary of family `family`
    /// built over an **exact** input — derived from the family's committed
    /// parameters by inverting the same sizing formulas
    /// [`crate::replacement::default_size_params`] uses. `None` when this
    /// model has no error model for the family (the default has none for
    /// `Sample`/`Wavelet`/`StatModel`).
    fn local_guarantee(
        &self,
        family: &SummaryFamilyType,
        query: &SketchQuery,
    ) -> Option<ResultGuarantee>;

    /// Compose `inputs`' guarantees (in the parent's child order) with the
    /// parent's own `local` guarantee under `op`. `Err` is the fail-closed
    /// answer: no registered rule, or a missing input guarantee.
    fn propagate(
        &self,
        op: &CompositionOperator,
        inputs: &[ResultGuarantee],
        local: Option<&ResultGuarantee>,
        stats: &PropagationStats,
    ) -> Result<ResultGuarantee, AccuracyError>;

    /// Does `guarantee` meet `target`? An unevaluable bound or probability
    /// never satisfies anything.
    fn satisfies(&self, guarantee: &ResultGuarantee, target: &AccuracyTarget) -> bool;
}

/// The conservative, fail-closed default — see the module docs' table.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultAccuracyModel;

/// Small relative tolerance for comparing an evaluated bound against a
/// target, so a parameter sized by `⌈·⌉` to *exactly* meet ε is not rejected
/// by floating-point noise.
const SATISFACTION_TOLERANCE: f64 = 1e-9;

pub(crate) const KLL_RANK_ERROR_COEFFICIENT_99: f64 = 2.296;
pub(crate) const KLL_RANK_ERROR_EXPONENT_99: f64 = 0.9723;

pub(crate) fn kll_rank_error_99(k: u32) -> f64 {
    KLL_RANK_ERROR_COEFFICIENT_99 / f64::from(k).powf(KLL_RANK_ERROR_EXPONENT_99)
}

fn count_sketch_failure_probability(depth: u32) -> Option<f64> {
    if depth == 0 || depth.is_multiple_of(2) {
        return None;
    }
    Some((-f64::from(depth) / 18.0).exp())
}

impl DefaultAccuracyModel {
    /// The local guarantee of one sketch `(algorithm, params)` for `query`
    /// — each arm inverts the matching formula in
    /// [`crate::replacement::default_size_params`].
    pub fn sketch_guarantee(
        algorithm: &SketchAlgorithm,
        params: &SketchParams,
        query: &SketchQuery,
    ) -> Option<ResultGuarantee> {
        let (metric, bound, delta) = match params {
            // Apache DataSketches' single-sided KLL fit is the empirical 99th
            // percentile normalized rank error for quantile/rank queries.
            // Tighter confidence needs an amplification contract.
            SketchParams::Kll { k } => (
                ErrorMetric::Rank,
                kll_rank_error_99(*k),
                ProbabilityExpr::Constant { value: 0.01 },
            ),
            // DDSketch: deterministic relative value error α.
            SketchParams::DDSketch { alpha } => {
                (ErrorMetric::RelativeValue, *alpha, ProbabilityExpr::Zero)
            }
            // HLL parameters encode precision, not a confidence-level budget.
            // They therefore provide an RSE magnitude here but no failure
            // probability against true cardinality.
            SketchParams::Hll { precision } => (
                ErrorMetric::Cardinality,
                1.04 / 2f64.powi(i32::from(*precision)).sqrt(),
                ProbabilityExpr::Unknown {
                    statistic: "hll_estimator_failure_probability".into(),
                },
            ),
            // KMV / Theta: RSE <= 1/√(k-2); the same Chebyshev conversion
            // gives a conservative parameter-derived 99% confidence bound.
            SketchParams::Kmv { k } | SketchParams::Theta { k } => (
                ErrorMetric::Cardinality,
                10.0 / f64::from(k.saturating_sub(2).max(1)).sqrt(),
                ProbabilityExpr::Constant { value: 0.01 },
            ),
            // CMS: over-count ≤ (e/w)·‖f‖₁ with probability ≥ 1 − e^{−d}.
            SketchParams::Cms { width, depth } | SketchParams::CmsWithHeap { width, depth, .. } => {
                (
                    ErrorMetric::Frequency,
                    std::f64::consts::E / f64::from(*width),
                    ProbabilityExpr::Constant {
                        value: (-f64::from(*depth)).exp(),
                    },
                )
            }
            // CountSketch: one row has variance at most ‖f‖₂²/w. With
            // ε=√(3/w), Chebyshev makes a row bad with probability <=1/3;
            // the median across independent odd-depth rows has the binomial
            // tail bounded by Hoeffding below.
            SketchParams::CountSketch { width, depth }
            | SketchParams::CountSketchWithHeap { width, depth, .. } => (
                ErrorMetric::L2Frequency,
                (3.0 / f64::from(*width)).sqrt(),
                ProbabilityExpr::Constant {
                    value: count_sketch_failure_probability(*depth)?,
                },
            ),
        };
        let provenance = vec![GuaranteeSource::SketchReadout {
            algorithm: format!("{algorithm:?}"),
            contract: match params {
                SketchParams::Kll { .. } => "apache_datasketches_kll_empirical_99_a9b42755072b",
                SketchParams::DDSketch { .. } => "ddsketch_relative_error_alpha_v1",
                SketchParams::Hll { .. } => "generic_hll_rse_only_no_confidence_v1",
                SketchParams::Kmv { .. } => "kmv_unbiased_variance_chebyshev_99_v1",
                SketchParams::Theta { .. } => "theta_variance_chebyshev_99_v1",
                SketchParams::Cms { .. } | SketchParams::CmsWithHeap { .. } => {
                    "count_min_l1_markov_v1"
                }
                SketchParams::CountSketch { .. } | SketchParams::CountSketchWithHeap { .. } => {
                    "count_sketch_l2_median_hoeffding_v1"
                }
            }
            .into(),
            params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
            query: format!("{query:?}"),
        }];
        Some(ResultGuarantee {
            metric,
            bound: BoundExpr::Constant { value: bound },
            failure_probability: delta,
            provenance,
        })
    }

    fn additive(
        op: &CompositionOperator,
        inputs: &[ResultGuarantee],
        local: &ResultGuarantee,
        rule: &str,
    ) -> ResultGuarantee {
        let mut terms: Vec<BoundExpr> = inputs.iter().map(|g| g.bound.clone()).collect();
        terms.push(local.bound.clone());
        let mut deltas: Vec<ProbabilityExpr> = inputs
            .iter()
            .map(|g| g.failure_probability.clone())
            .collect();
        deltas.push(local.failure_probability.clone());
        ResultGuarantee {
            metric: local.metric,
            bound: BoundExpr::Sum { terms },
            failure_probability: ProbabilityExpr::UnionBound { terms: deltas },
            provenance: composed_provenance(op, inputs, local, rule),
        }
    }

    /// `(1 + ε_total) = Π (1 + ε_i)` ⇒ for two factors
    /// `ε_in + ε_out + ε_in·ε_out`; written out as the sum of all
    /// cross-products so the expression tree is exact for any input count.
    fn multiplicative(
        op: &CompositionOperator,
        inputs: &[ResultGuarantee],
        local: &ResultGuarantee,
    ) -> ResultGuarantee {
        let factors: Vec<&BoundExpr> = inputs
            .iter()
            .map(|g| &g.bound)
            .chain(std::iter::once(&local.bound))
            .collect();
        // Every non-empty subset's product: Π(1+ε_i) − 1 = Σ_{S≠∅} Π_{i∈S} ε_i.
        let mut terms = Vec::new();
        for mask in 1..(1u32 << factors.len()) {
            let subset: Vec<BoundExpr> = factors
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, b)| (*b).clone())
                .collect();
            terms.push(if subset.len() == 1 {
                subset.into_iter().next().expect("one element")
            } else {
                BoundExpr::Product { factors: subset }
            });
        }
        let mut deltas: Vec<ProbabilityExpr> = inputs
            .iter()
            .map(|g| g.failure_probability.clone())
            .collect();
        deltas.push(local.failure_probability.clone());
        ResultGuarantee {
            metric: ErrorMetric::RelativeValue,
            bound: BoundExpr::Sum { terms },
            failure_probability: ProbabilityExpr::UnionBound { terms: deltas },
            provenance: composed_provenance(op, inputs, local, "relative_cross_term_union_bound"),
        }
    }

    fn lipschitz(
        op: &CompositionOperator,
        constant: f64,
        inputs: &[ResultGuarantee],
        local: Option<&ResultGuarantee>,
    ) -> ResultGuarantee {
        let input = &inputs[0];
        let scaled = BoundExpr::Scaled {
            factor: constant,
            inner: Box::new(input.bound.clone()),
        };
        let (bound, delta) = match local {
            Some(local) => (
                BoundExpr::Sum {
                    terms: vec![scaled, local.bound.clone()],
                },
                ProbabilityExpr::UnionBound {
                    terms: vec![
                        input.failure_probability.clone(),
                        local.failure_probability.clone(),
                    ],
                },
            ),
            None => (scaled, input.failure_probability.clone()),
        };
        let exact_local = ResultGuarantee::exact("deterministic Lipschitz transformation");
        ResultGuarantee {
            metric: ErrorMetric::AbsoluteValue,
            bound,
            failure_probability: delta,
            provenance: composed_provenance(
                op,
                inputs,
                local.unwrap_or(&exact_local),
                "lipschitz_union_bound",
            ),
        }
    }

    /// Exact `sum` over approximate inputs: `B ≤ Σ B_i`, `δ ≤ Σ δ_i`. The
    /// planner composes one *per-value* child guarantee over an unknown
    /// number of input rows, so both the bound and the union bound scale by
    /// `stats.input_row_count` — an [`BoundExpr::Unknown`] leaf when it is
    /// not supplied. Each input's normalized bound is first converted to
    /// absolute units via the statistic its metric is normalized by (also
    /// unknown unless supplied); a `Rank` input has no such conversion.
    fn exact_sum(
        op: &CompositionOperator,
        inputs: &[ResultGuarantee],
        stats: &PropagationStats,
    ) -> Result<ResultGuarantee, AccuracyError> {
        let mut terms = Vec::with_capacity(inputs.len());
        let mut deltas = Vec::with_capacity(inputs.len());
        let mut provenance = Vec::new();
        for (i, input) in inputs.iter().enumerate() {
            let absolute =
                absolute_bound(input).ok_or_else(|| AccuracyError::UnsupportedComposition {
                    operator: op.clone(),
                    input_metrics: inputs.iter().map(|g| g.metric).collect(),
                    local_metric: None,
                    reason: format!(
                        "input {i} carries a {:?} guarantee, which has no registered \
                         conversion to an absolute value error",
                        input.metric
                    ),
                })?;
            if let BoundExpr::Product { factors } = &absolute {
                for f in factors {
                    if let BoundExpr::Unknown { statistic } = f {
                        provenance.push(GuaranteeSource::UnavailableStatistic {
                            statistic: statistic.clone(),
                        });
                    }
                }
            }
            terms.push(absolute);
            deltas.push(input.failure_probability.clone());
        }
        let count = row_count(stats, &mut provenance);
        let exact_local = ResultGuarantee::exact("ExactAggregate(Sum)");
        provenance.extend(composed_provenance(
            op,
            inputs,
            &exact_local,
            "exact_sum_union_bound",
        ));
        Ok(ResultGuarantee {
            metric: ErrorMetric::AbsoluteValue,
            bound: BoundExpr::Product {
                factors: vec![count.clone(), BoundExpr::Sum { terms }],
            },
            failure_probability: ProbabilityExpr::Scaled {
                count,
                inner: Box::new(ProbabilityExpr::UnionBound { terms: deltas }),
            },
            provenance,
        })
    }

    /// Exact `max`/`min` over approximate inputs of one shared metric: the
    /// returned value's error is at most the largest input bound (order
    /// statistics are monotone under a uniform perturbation), with
    /// probability by the union bound over every input row. This bounds the
    /// returned *value*; it does not identify the true winning key.
    fn exact_extremum(
        op: &CompositionOperator,
        inputs: &[ResultGuarantee],
        stats: &PropagationStats,
    ) -> Result<ResultGuarantee, AccuracyError> {
        let metric = inputs[0].metric;
        if inputs.iter().any(|g| g.metric != metric) || metric == ErrorMetric::TopKMembership {
            return Err(AccuracyError::UnsupportedComposition {
                operator: op.clone(),
                input_metrics: inputs.iter().map(|g| g.metric).collect(),
                local_metric: None,
                reason: "exact max/min needs every input under one value-like metric".into(),
            });
        }
        let mut provenance = Vec::new();
        let count = row_count(stats, &mut provenance);
        let exact_local = ResultGuarantee::exact("ExactAggregate(MinMax)");
        provenance.extend(composed_provenance(
            op,
            inputs,
            &exact_local,
            "exact_extremum_union_bound",
        ));
        Ok(ResultGuarantee {
            metric,
            bound: BoundExpr::Max {
                terms: inputs.iter().map(|g| g.bound.clone()).collect(),
            },
            failure_probability: ProbabilityExpr::Scaled {
                count,
                inner: Box::new(ProbabilityExpr::UnionBound {
                    terms: inputs
                        .iter()
                        .map(|g| g.failure_probability.clone())
                        .collect(),
                }),
            },
            provenance,
        })
    }
}

/// `stats.input_row_count` as a bound factor, or an `Unknown` leaf (recorded
/// in `provenance`) when absent.
fn row_count(stats: &PropagationStats, provenance: &mut Vec<GuaranteeSource>) -> BoundExpr {
    match stats.input_row_count {
        Some(n) => BoundExpr::Constant { value: n as f64 },
        None => {
            provenance.push(GuaranteeSource::UnavailableStatistic {
                statistic: "input_row_count".into(),
            });
            BoundExpr::Unknown {
                statistic: "input_row_count".into(),
            }
        }
    }
}

/// `input`'s bound converted to absolute value units, multiplying a
/// normalized metric by the (unknown) statistic it is normalized by. `None`
/// for a metric with no such conversion (`Rank`, `TopKMembership`).
fn absolute_bound(input: &ResultGuarantee) -> Option<BoundExpr> {
    let normalizer = match input.metric {
        ErrorMetric::AbsoluteValue => return Some(input.bound.clone()),
        ErrorMetric::RelativeValue => "true_value_magnitude",
        ErrorMetric::Cardinality => "true_cardinality",
        ErrorMetric::Frequency => "stream_l1_norm",
        ErrorMetric::L2Frequency => "stream_l2_norm",
        // `Rank` has no distribution-free conversion to a value error; a
        // metric this crate does not know has no registered conversion.
        ErrorMetric::Rank | ErrorMetric::TopKMembership | _ => return None,
    };
    if input.bound.is_zero() {
        return Some(BoundExpr::Zero);
    }
    Some(BoundExpr::Product {
        factors: vec![
            input.bound.clone(),
            BoundExpr::Unknown {
                statistic: normalizer.into(),
            },
        ],
    })
}

fn composed_provenance(
    op: &CompositionOperator,
    inputs: &[ResultGuarantee],
    local: &ResultGuarantee,
    rule: &str,
) -> Vec<GuaranteeSource> {
    let mut provenance: Vec<GuaranteeSource> = inputs
        .iter()
        .enumerate()
        .map(|(input_index, g)| GuaranteeSource::ChildGuarantee {
            input_index,
            guarantee: Box::new(g.clone()),
        })
        .collect();
    provenance.extend(local.provenance.iter().cloned());
    provenance.push(GuaranteeSource::CompositionStep {
        operator: op.clone(),
        rule: rule.into(),
    });
    provenance
}

impl AccuracyModel for DefaultAccuracyModel {
    fn local_guarantee(
        &self,
        family: &SummaryFamilyType,
        query: &SketchQuery,
    ) -> Option<ResultGuarantee> {
        match family {
            SummaryFamilyType::Plain(_) => Some(ResultGuarantee::exact("Plain value")),
            SummaryFamilyType::ExactAggregate(kind, _) => {
                Some(ResultGuarantee::exact(format!("ExactAggregate({kind:?})")))
            }
            SummaryFamilyType::Sketch(kind, _) => {
                Self::sketch_guarantee(kind.algorithm(), kind.params(), query)
            }
            // No error model is registered for these families.
            SummaryFamilyType::Sample(..)
            | SummaryFamilyType::Wavelet(..)
            | SummaryFamilyType::StatModel(..) => None,
        }
    }

    fn propagate(
        &self,
        op: &CompositionOperator,
        inputs: &[ResultGuarantee],
        local: Option<&ResultGuarantee>,
        stats: &PropagationStats,
    ) -> Result<ResultGuarantee, AccuracyError> {
        // Exact input: only the local guarantee remains (or the value is exact).
        if inputs.iter().all(ResultGuarantee::is_exact)
            && !matches!(op, CompositionOperator::TopKSelection)
        {
            return Ok(match local {
                Some(local) => {
                    let mut out = local.clone();
                    out.provenance
                        .extend(inputs.iter().enumerate().map(|(input_index, g)| {
                            GuaranteeSource::ChildGuarantee {
                                input_index,
                                guarantee: Box::new(g.clone()),
                            }
                        }));
                    out.provenance.push(GuaranteeSource::CompositionStep {
                        operator: op.clone(),
                        rule: "exact_input".into(),
                    });
                    out
                }
                None => {
                    let mut out = ResultGuarantee::exact(format!("{op:?} over exact inputs"));
                    out.provenance.push(GuaranteeSource::CompositionStep {
                        operator: op.clone(),
                        rule: "exact_input".into(),
                    });
                    out
                }
            });
        }

        let input_metrics: Vec<ErrorMetric> = inputs.iter().map(|g| g.metric).collect();
        let unsupported = |reason: String| AccuracyError::UnsupportedComposition {
            operator: op.clone(),
            input_metrics: input_metrics.clone(),
            local_metric: local.map(|g| g.metric),
            reason,
        };
        // An exact input is compatible with every metric; only approximate
        // inputs constrain the rule.
        let approximate: Vec<&ResultGuarantee> = inputs.iter().filter(|g| !g.is_exact()).collect();
        let same_metric = |metric: ErrorMetric| approximate.iter().all(|g| g.metric == metric);

        match op {
            CompositionOperator::ApproximateAggregate => {
                let local = local.ok_or_else(|| {
                    unsupported("approximate operator has no local guarantee to compose".into())
                })?;
                if !same_metric(local.metric) {
                    return Err(unsupported(format!(
                        "no registered cross-metric rule from {input_metrics:?} to {:?}",
                        local.metric
                    )));
                }
                match local.metric {
                    ErrorMetric::AbsoluteValue => {
                        Ok(Self::additive(op, inputs, local, "additive_union_bound"))
                    }
                    ErrorMetric::RelativeValue => {
                        if stats.values_non_negative != Some(true) {
                            return Err(unsupported(
                                "relative-error composition needs values of known sign \
                                 (PropagationStats::values_non_negative)"
                                    .into(),
                            ));
                        }
                        Ok(Self::multiplicative(op, inputs, local))
                    }
                    ErrorMetric::Rank
                    | ErrorMetric::Cardinality
                    | ErrorMetric::Frequency
                    | ErrorMetric::L2Frequency
                    | ErrorMetric::TopKMembership
                    | _ => Err(unsupported(format!(
                        "no registered same-metric composition rule for {:?} over {:?}",
                        local.metric, local.metric
                    ))),
                }
            }
            CompositionOperator::Lipschitz { constant } => {
                if !(constant.is_finite() && *constant >= 0.0) {
                    return Err(unsupported(format!(
                        "Lipschitz constant {constant} is not a finite non-negative number"
                    )));
                }
                if inputs.len() != 1 || !same_metric(ErrorMetric::AbsoluteValue) {
                    return Err(unsupported(
                        "Lipschitz rule is registered for exactly one AbsoluteValue input".into(),
                    ));
                }
                if local.is_some_and(|g| g.metric != ErrorMetric::AbsoluteValue) {
                    return Err(unsupported(
                        "Lipschitz rule needs an AbsoluteValue local guarantee".into(),
                    ));
                }
                Ok(Self::lipschitz(op, *constant, inputs, local))
            }
            CompositionOperator::ExactSum => Self::exact_sum(op, inputs, stats),
            CompositionOperator::ExactExtremum => Self::exact_extremum(op, inputs, stats),
            CompositionOperator::TopKSelection => {
                let (Some(selected_lower), Some(excluded_upper), Some(delta)) = (
                    stats.topk_selected_lower_bound,
                    stats.topk_excluded_upper_bound,
                    stats.topk_interval_failure_probability,
                ) else {
                    return Err(unsupported(
                        "top-k membership needs selected-lower, excluded-upper, and interval \
                         failure-probability evidence"
                            .into(),
                    ));
                };
                if !(selected_lower.is_finite()
                    && excluded_upper.is_finite()
                    && delta.is_finite()
                    && (0.0..=1.0).contains(&delta)
                    && selected_lower > excluded_upper)
                {
                    return Err(unsupported(
                        "top-k confidence intervals overlap or contain invalid evidence".into(),
                    ));
                }
                let mut provenance = inputs
                    .iter()
                    .enumerate()
                    .map(|(input_index, guarantee)| GuaranteeSource::ChildGuarantee {
                        input_index,
                        guarantee: Box::new(guarantee.clone()),
                    })
                    .collect::<Vec<_>>();
                provenance.extend(stats.evidence_provenance.clone());
                if let Some(local) = local {
                    provenance.extend(local.provenance.clone());
                }
                provenance.push(GuaranteeSource::CompositionStep {
                    operator: op.clone(),
                    rule: "topk_membership_margin_certificate".into(),
                });
                Ok(ResultGuarantee {
                    metric: ErrorMetric::TopKMembership,
                    bound: BoundExpr::Zero,
                    failure_probability: ProbabilityExpr::Constant { value: delta },
                    provenance,
                })
            }
            // An operator this crate does not know has no registered rule.
            _ => Err(unsupported("no registered rule for this operator".into())),
        }
    }

    fn satisfies(&self, guarantee: &ResultGuarantee, target: &AccuracyTarget) -> bool {
        let within = |value: Option<f64>, limit: f64| {
            value.is_some_and(|v| v <= limit * (1.0 + SATISFACTION_TOLERANCE) + f64::EPSILON)
        };
        match target {
            AccuracyTarget::Exact => guarantee.is_exact(),
            AccuracyTarget::Epsilon(eps) => within(guarantee.bound.evaluate(), *eps),
            AccuracyTarget::EpsilonDelta { epsilon, delta } => {
                within(guarantee.bound.evaluate(), *epsilon)
                    && within(guarantee.failure_probability.evaluate(), *delta)
            }
        }
    }
}

// ── Budget allocation ───────────────────────────────────────────────────────

/// The shape of a composition an allocator splits a budget across.
#[derive(Debug, Clone, PartialEq)]
pub struct CompositionShape {
    /// The metric the composed guarantee will carry — decides whether the
    /// budget composes additively (`Σ ε_i ≤ ε`) or multiplicatively
    /// (`Π(1+ε_i) ≤ 1+ε`).
    pub metric: ErrorMetric,
    /// How many approximate layers share the budget (≥ 1).
    pub approximate_layer_count: usize,
}

/// One way of splitting an end-to-end target across a composition's
/// approximate layers. `layers[0]` is the outermost layer's local target;
/// the remainder are the inner layers', outermost first.
#[derive(Debug, Clone, PartialEq)]
pub struct AccuracyAllocation {
    pub allocator: &'static str,
    pub layers: Vec<AccuracyTarget>,
}

impl AccuracyAllocation {
    /// The end-to-end budget left for everything below `layers[0]` — what
    /// the inner subtree must satisfy as a whole (it re-splits internally).
    /// `None` for a single-layer allocation.
    pub fn inner_target(&self, shape: &CompositionShape) -> Option<AccuracyTarget> {
        let inner = &self.layers[1..];
        if inner.is_empty() {
            return None;
        }
        let (eps, delta): (Vec<f64>, Vec<Option<f64>>) = inner
            .iter()
            .map(|t| match t {
                AccuracyTarget::Exact => (0.0, Some(0.0)),
                AccuracyTarget::Epsilon(e) => (*e, None),
                AccuracyTarget::EpsilonDelta { epsilon, delta } => (*epsilon, Some(*delta)),
            })
            .unzip();
        let epsilon = match shape.metric {
            ErrorMetric::RelativeValue => eps.iter().map(|e| 1.0 + e).product::<f64>() - 1.0,
            _ => eps.iter().sum(),
        };
        Some(match delta.iter().copied().sum::<Option<f64>>() {
            Some(delta) => AccuracyTarget::EpsilonDelta { epsilon, delta },
            None => AccuracyTarget::Epsilon(epsilon),
        })
    }
}

/// Enumerates the finite set of budget splits the search tries for one
/// composition. Exposed as its own hook because equal splitting is rarely
/// cost-optimal; a deployment can return several candidate splits and let
/// cost ranking pick among the legal ones.
pub trait AccuracyBudgetAllocator {
    fn allocations(
        &self,
        target: &AccuracyTarget,
        composition: &CompositionShape,
    ) -> Vec<AccuracyAllocation>;
}

/// The initial deterministic allocator: every approximate layer gets an
/// equal share — `ε_i = ε / n`, `δ_i = δ / n` for an additively composed
/// metric, and `ε_i = (1 + ε)^{1/n} − 1` for a multiplicatively composed
/// one — so the composed bound meets the target exactly with no slack.
/// `AccuracyTarget::Exact` yields no allocation: no approximate layer can
/// meet it.
#[derive(Debug, Default, Clone, Copy)]
pub struct EqualSplitAllocator;

impl AccuracyBudgetAllocator for EqualSplitAllocator {
    fn allocations(
        &self,
        target: &AccuracyTarget,
        composition: &CompositionShape,
    ) -> Vec<AccuracyAllocation> {
        let n = composition.approximate_layer_count.max(1);
        let (epsilon, delta) = match target {
            AccuracyTarget::Exact => return Vec::new(),
            AccuracyTarget::Epsilon(e) => (*e, None),
            AccuracyTarget::EpsilonDelta { epsilon, delta } => (*epsilon, Some(*delta)),
        };
        if !(epsilon.is_finite() && epsilon > 0.0) {
            return Vec::new();
        }
        let local_epsilon = match composition.metric {
            ErrorMetric::RelativeValue => (1.0 + epsilon).powf(1.0 / n as f64) - 1.0,
            _ => epsilon / n as f64,
        };
        let layer = match delta {
            Some(delta) => AccuracyTarget::EpsilonDelta {
                epsilon: local_epsilon,
                delta: delta / n as f64,
            },
            None => AccuracyTarget::Epsilon(local_epsilon),
        };
        vec![AccuracyAllocation {
            allocator: "EqualSplitAllocator",
            layers: vec![layer; n],
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::post_asap::{GroupingStrategy, SketchKind, TopKWeight};
    use asap_types::workload::{DataDistribution, DataWorkload, Evidence, EvidenceSource};

    fn abs(bound: f64, delta: f64) -> ResultGuarantee {
        ResultGuarantee {
            metric: ErrorMetric::AbsoluteValue,
            bound: BoundExpr::Constant { value: bound },
            failure_probability: ProbabilityExpr::Constant { value: delta },
            provenance: vec![],
        }
    }

    fn rel(bound: f64) -> ResultGuarantee {
        ResultGuarantee {
            metric: ErrorMetric::RelativeValue,
            bound: BoundExpr::Constant { value: bound },
            failure_probability: ProbabilityExpr::Zero,
            provenance: vec![],
        }
    }

    fn with_metric(metric: ErrorMetric, bound: f64) -> ResultGuarantee {
        ResultGuarantee {
            metric,
            ..abs(bound, 0.0)
        }
    }

    #[test]
    fn workload_accuracy_evidence_uses_only_fresh_data_characteristics() {
        let data = DataWorkload {
            input_cardinality: Evidence {
                value: Some(42),
                source: EvidenceSource::Observed,
                observed_at_ms: Some(1_000),
                valid_for_ms: Some(500),
            },
            distribution: Evidence {
                value: Some(DataDistribution::Bursty),
                source: EvidenceSource::Observed,
                observed_at_ms: Some(1_000),
                valid_for_ms: Some(500),
            },
            ..Default::default()
        };
        let provider = WorkloadAccuracyEvidence {
            data: &data,
            now_ms: 1_500,
        };
        let fresh = provider.propagation_stats(
            &CompositionOperator::ExactSum,
            &SummaryFamilyType::ExactAggregate(
                asap_types::post_asap::ExactKind::Sum,
                asap_types::post_asap::ExactParams::Sum,
            ),
            None,
        );
        assert_eq!(fresh.input_row_count, Some(42));
        assert_eq!(fresh.data_distribution, Some(DataDistribution::Bursty));

        let stale = WorkloadAccuracyEvidence {
            data: &data,
            now_ms: 1_501,
        }
        .propagation_stats(
            &CompositionOperator::ExactSum,
            &SummaryFamilyType::ExactAggregate(
                asap_types::post_asap::ExactKind::Sum,
                asap_types::post_asap::ExactParams::Sum,
            ),
            None,
        );
        assert_eq!(stale.input_row_count, None);
        assert_eq!(stale.data_distribution, None);
    }

    #[test]
    fn exact_child_contributes_zero_error() {
        let local = abs(0.05, 0.01);
        let out = DefaultAccuracyModel
            .propagate(
                &CompositionOperator::ApproximateAggregate,
                &[ResultGuarantee::exact("sum")],
                Some(&local),
                &PropagationStats::default(),
            )
            .unwrap();
        assert_eq!(out.bound.evaluate(), Some(0.05));
        assert_eq!(out.failure_probability.evaluate(), Some(0.01));
        assert_eq!(out.metric, ErrorMetric::AbsoluteValue);
    }

    #[test]
    fn additive_bounds_and_delta_union_bound_compose() {
        let out = DefaultAccuracyModel
            .propagate(
                &CompositionOperator::ApproximateAggregate,
                &[abs(0.02, 0.01)],
                Some(&abs(0.03, 0.02)),
                &PropagationStats::default(),
            )
            .unwrap();
        assert!((out.bound.evaluate().unwrap() - 0.05).abs() < 1e-12);
        // Union bound, not 1 − (1−0.01)(1−0.02) = 0.0298.
        assert!((out.failure_probability.evaluate().unwrap() - 0.03).abs() < 1e-12);
        assert!(out.provenance.iter().any(|s| matches!(
            s,
            GuaranteeSource::CompositionStep { rule, .. } if rule == "additive_union_bound"
        )));
    }

    #[test]
    fn relative_error_includes_the_cross_term() {
        let stats = PropagationStats {
            values_non_negative: Some(true),
            ..Default::default()
        };
        let out = DefaultAccuracyModel
            .propagate(
                &CompositionOperator::ApproximateAggregate,
                &[rel(0.1)],
                Some(&rel(0.2)),
                &stats,
            )
            .unwrap();
        // 0.1 + 0.2 + 0.1·0.2 = 0.32, not 0.3.
        assert!((out.bound.evaluate().unwrap() - 0.32).abs() < 1e-12);
        assert_eq!(out.metric, ErrorMetric::RelativeValue);
    }

    #[test]
    fn relative_error_without_sign_knowledge_is_rejected() {
        let err = DefaultAccuracyModel
            .propagate(
                &CompositionOperator::ApproximateAggregate,
                &[rel(0.1)],
                Some(&rel(0.2)),
                &PropagationStats::default(),
            )
            .unwrap_err();
        assert!(matches!(err, AccuracyError::UnsupportedComposition { .. }));
    }

    #[test]
    fn incompatible_metrics_are_rejected_not_treated_as_exact() {
        // HLL cardinality error under a CMS frequency guarantee.
        let err = DefaultAccuracyModel
            .propagate(
                &CompositionOperator::ApproximateAggregate,
                &[with_metric(ErrorMetric::Cardinality, 0.01)],
                Some(&with_metric(ErrorMetric::Frequency, 0.01)),
                &PropagationStats::default(),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            AccuracyError::UnsupportedComposition {
                input_metrics,
                local_metric: Some(ErrorMetric::Frequency),
                ..
            } if input_metrics == vec![ErrorMetric::Cardinality]
        ));
        // Quantile rank error under value-additive logic.
        let err = DefaultAccuracyModel
            .propagate(
                &CompositionOperator::ApproximateAggregate,
                &[with_metric(ErrorMetric::Rank, 0.01)],
                Some(&abs(0.01, 0.0)),
                &PropagationStats::default(),
            )
            .unwrap_err();
        assert!(matches!(err, AccuracyError::UnsupportedComposition { .. }));
    }

    #[test]
    fn same_metric_rank_over_rank_has_no_registered_rule() {
        let err = DefaultAccuracyModel
            .propagate(
                &CompositionOperator::ApproximateAggregate,
                &[with_metric(ErrorMetric::Rank, 0.01)],
                Some(&with_metric(ErrorMetric::Rank, 0.01)),
                &PropagationStats::default(),
            )
            .unwrap_err();
        assert!(matches!(err, AccuracyError::UnsupportedComposition { .. }));
    }

    #[test]
    fn lipschitz_scales_the_input_bound() {
        let out = DefaultAccuracyModel
            .propagate(
                &CompositionOperator::Lipschitz { constant: 3.0 },
                &[abs(0.1, 0.01)],
                Some(&abs(0.05, 0.02)),
                &PropagationStats::default(),
            )
            .unwrap();
        assert!((out.bound.evaluate().unwrap() - 0.35).abs() < 1e-12);
        assert!((out.failure_probability.evaluate().unwrap() - 0.03).abs() < 1e-12);
    }

    #[test]
    fn exact_sum_over_approximate_sums_bounds_and_keeps_unknown_row_count_unknown() {
        let out = DefaultAccuracyModel
            .propagate(
                &CompositionOperator::ExactSum,
                &[abs(0.1, 0.01)],
                None,
                &PropagationStats::default(),
            )
            .unwrap();
        assert_eq!(out.metric, ErrorMetric::AbsoluteValue);
        assert_eq!(
            out.bound.evaluate(),
            None,
            "unknown row count stays unknown"
        );
        assert!(out.provenance.iter().any(|s| matches!(
            s,
            GuaranteeSource::UnavailableStatistic { statistic } if statistic == "input_row_count"
        )));
        assert!(!DefaultAccuracyModel.satisfies(&out, &AccuracyTarget::Epsilon(1.0)));

        let known = PropagationStats {
            input_row_count: Some(4),
            ..Default::default()
        };
        let out = DefaultAccuracyModel
            .propagate(
                &CompositionOperator::ExactSum,
                &[abs(0.1, 0.01)],
                None,
                &known,
            )
            .unwrap();
        assert!((out.bound.evaluate().unwrap() - 0.4).abs() < 1e-12);
        assert!((out.failure_probability.evaluate().unwrap() - 0.04).abs() < 1e-12);
    }

    #[test]
    fn exact_extremum_takes_the_max_bound() {
        let known = PropagationStats {
            input_row_count: Some(2),
            ..Default::default()
        };
        let out = DefaultAccuracyModel
            .propagate(
                &CompositionOperator::ExactExtremum,
                &[abs(0.1, 0.01), abs(0.3, 0.01)],
                None,
                &known,
            )
            .unwrap();
        assert!((out.bound.evaluate().unwrap() - 0.3).abs() < 1e-12);
        assert!((out.failure_probability.evaluate().unwrap() - 0.04).abs() < 1e-12);
    }

    #[test]
    fn topk_selection_requires_a_separated_margin_certificate() {
        let err = DefaultAccuracyModel
            .propagate(
                &CompositionOperator::TopKSelection,
                &[abs(0.1, 0.01)],
                None,
                &PropagationStats::default(),
            )
            .unwrap_err();
        assert!(matches!(err, AccuracyError::UnsupportedComposition { .. }));

        let certified = DefaultAccuracyModel
            .propagate(
                &CompositionOperator::TopKSelection,
                &[abs(0.1, 0.01)],
                None,
                &PropagationStats {
                    topk_selected_lower_bound: Some(101.0),
                    topk_excluded_upper_bound: Some(100.0),
                    topk_interval_failure_probability: Some(0.005),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(certified.metric, ErrorMetric::TopKMembership);
        assert_eq!(certified.bound.evaluate(), Some(0.0));
        assert_eq!(certified.failure_probability.evaluate(), Some(0.005));

        let overlapping = DefaultAccuracyModel.propagate(
            &CompositionOperator::TopKSelection,
            &[abs(0.1, 0.01)],
            None,
            &PropagationStats {
                topk_selected_lower_bound: Some(100.0),
                topk_excluded_upper_bound: Some(100.0),
                topk_interval_failure_probability: Some(0.005),
                ..Default::default()
            },
        );
        assert!(overlapping.is_err());
    }

    #[test]
    fn local_guarantee_inverts_the_sizing_formulas() {
        use crate::replacement::default_size_params;
        use asap_types::pre_asap::agg_intent::{default_cardinality, default_quantile};

        let q = default_quantile(0.99);
        let params = default_size_params(SketchAlgorithm::Kll, &q, 0.01, 0.01);
        let g = DefaultAccuracyModel
            .local_guarantee(
                &SummaryFamilyType::Sketch(
                    SketchKind::new(SketchAlgorithm::Kll, params),
                    GroupingStrategy::default(),
                ),
                &SketchQuery::Quantile { q: 0.99 },
            )
            .unwrap();
        assert_eq!(g.metric, ErrorMetric::Rank);
        assert!(DefaultAccuracyModel.satisfies(&g, &AccuracyTarget::Epsilon(0.01)));
        assert_eq!(g.failure_probability.evaluate(), Some(0.01));
        assert!(DefaultAccuracyModel.satisfies(
            &g,
            &AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            }
        ));
        assert_eq!(g.approximate_layer_count(), 1);
        assert!(g.provenance.iter().any(|source| matches!(
            source,
            GuaranteeSource::SketchReadout { contract, .. }
                if contract == "apache_datasketches_kll_empirical_99_a9b42755072b"
        )));

        let c = default_cardinality();
        let params = default_size_params(SketchAlgorithm::Hll, &c, 0.01, 0.01);
        let g = DefaultAccuracyModel
            .local_guarantee(
                &SummaryFamilyType::Sketch(
                    SketchKind::new(SketchAlgorithm::Hll, params),
                    GroupingStrategy::default(),
                ),
                &SketchQuery::Cardinality,
            )
            .unwrap();
        assert_eq!(g.metric, ErrorMetric::Cardinality);
        assert_eq!(g.failure_probability.evaluate(), None);
        assert!(DefaultAccuracyModel.satisfies(&g, &AccuracyTarget::Epsilon(0.01)));
        assert!(!DefaultAccuracyModel.satisfies(
            &g,
            &AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            }
        ));

        let params = default_size_params(SketchAlgorithm::Cms, &c, 0.01, 0.001);
        let g = DefaultAccuracyModel
            .local_guarantee(
                &SummaryFamilyType::Sketch(
                    SketchKind::new(SketchAlgorithm::Cms, params),
                    GroupingStrategy::default(),
                ),
                &SketchQuery::Cardinality,
            )
            .unwrap();
        assert_eq!(g.metric, ErrorMetric::Frequency);
        assert!(DefaultAccuracyModel.satisfies(
            &g,
            &AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.001
            }
        ));
    }

    #[test]
    fn count_sketch_uses_an_l2_guarantee() {
        use crate::replacement::default_size_params;
        use asap_types::pre_asap::agg_intent::default_cardinality;

        let intent = default_cardinality();
        let count_sketch = default_size_params(SketchAlgorithm::CountSketch, &intent, 0.01, 0.01);
        let guarantee = DefaultAccuracyModel
            .local_guarantee(
                &SummaryFamilyType::Sketch(
                    SketchKind::new(SketchAlgorithm::CountSketch, count_sketch),
                    GroupingStrategy::default(),
                ),
                &SketchQuery::PointCount {
                    key: asap_types::pre_asap::expr_ir::ColumnRef::SampleValue,
                    value: None,
                },
            )
            .expect("CountSketch has a parameter-derived L2 guarantee");
        assert_eq!(guarantee.metric, ErrorMetric::L2Frequency);
        assert!(DefaultAccuracyModel.satisfies(
            &guarantee,
            &AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            }
        ));

        let cms_heap = SketchParams::CmsWithHeap {
            width: 272,
            depth: 5,
            heap_size: 10,
        };
        let topk_frequency = DefaultAccuracyModel
            .local_guarantee(
                &SummaryFamilyType::Sketch(
                    SketchKind::new(SketchAlgorithm::CmsWithHeap, cms_heap),
                    GroupingStrategy::default(),
                ),
                &SketchQuery::TopK {
                    k: 10,
                    weight: TopKWeight::Count,
                },
            )
            .expect("heap sketch still provides per-key frequency intervals");
        assert_eq!(topk_frequency.metric, ErrorMetric::Frequency);
    }

    #[test]
    fn satisfies_is_fail_closed_on_unknowns_and_exact() {
        let unknown = ResultGuarantee {
            bound: BoundExpr::Unknown {
                statistic: "x".into(),
            },
            ..abs(0.0, 0.0)
        };
        assert!(!DefaultAccuracyModel.satisfies(&unknown, &AccuracyTarget::Epsilon(1.0)));
        assert!(!DefaultAccuracyModel.satisfies(&abs(0.0, 0.01), &AccuracyTarget::Exact));
        assert!(
            DefaultAccuracyModel.satisfies(&ResultGuarantee::exact("x"), &AccuracyTarget::Exact)
        );
    }

    #[test]
    fn equal_split_respects_the_root_epsilon_and_delta() {
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.1,
            delta: 0.02,
        };
        let shape = CompositionShape {
            metric: ErrorMetric::AbsoluteValue,
            approximate_layer_count: 2,
        };
        let allocations = EqualSplitAllocator.allocations(&target, &shape);
        assert_eq!(allocations.len(), 1);
        let layers = &allocations[0].layers;
        assert_eq!(layers.len(), 2);
        let (eps, deltas): (Vec<f64>, Vec<f64>) = layers
            .iter()
            .map(|t| match t {
                AccuracyTarget::EpsilonDelta { epsilon, delta } => (*epsilon, *delta),
                other => panic!("unexpected {other:?}"),
            })
            .unzip();
        assert!((eps.iter().sum::<f64>() - 0.1).abs() < 1e-12);
        assert!((deltas.iter().sum::<f64>() - 0.02).abs() < 1e-12);
        assert_eq!(
            allocations[0].inner_target(&shape),
            Some(AccuracyTarget::EpsilonDelta {
                epsilon: 0.05,
                delta: 0.01
            })
        );

        // Multiplicative composition: (1+ε_i)^2 = 1+ε, not 2ε_i = ε.
        let rel_shape = CompositionShape {
            metric: ErrorMetric::RelativeValue,
            approximate_layer_count: 2,
        };
        let allocations =
            EqualSplitAllocator.allocations(&AccuracyTarget::Epsilon(0.21), &rel_shape);
        let AccuracyTarget::Epsilon(e) = allocations[0].layers[0] else {
            panic!()
        };
        assert!((e - 0.1).abs() < 1e-12);

        assert!(EqualSplitAllocator
            .allocations(&AccuracyTarget::Exact, &shape)
            .is_empty());
    }
}
