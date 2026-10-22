//! Registered propagation rules for computations over uncertain inputs.
//! Stage 1 composes each candidate's guarantees with them
//! (`per_computation_candidates`); a rule that needs a statistic leaves an
//! `Unknown` leaf named by one of the constants below, which Stage 3 binds.
use super::*;

/// Rows an exact aggregation folds (per group): bounded by the series
/// count of the sources the query reads.
pub const INPUT_ROW_COUNT: &str = "input_row_count";
/// Rows one group of a SQL fold over an approximate relation folds: not
/// bounded by the series count of the sources it reads, so plan selection
/// leaves it unbound.
pub(crate) const GROUP_ROW_COUNT: &str = "group_row_count";
/// Whether every input value is non-negative.
pub(crate) const VALUES_NON_NEGATIVE: &str = "values_non_negative";
/// The absolute bound of a product of two estimates,
/// `|x|·e_y + |y|·e_x + e_x·e_y`, given enforced operand domains.
pub const PRODUCT_OPERAND_DOMAINS: &str = "product_operand_domains";
/// The absolute bound of a quotient with an approximate denominator,
/// `(|x|·e_y + |y|·e_x) / (|y|·(|y| − e_y))`, given enforced operand domains
/// with the denominator's bounded away from zero.
pub(crate) const QUOTIENT_OPERAND_DOMAINS: &str = "quotient_operand_domains";
/// `‖f‖₁` of the stream a Count-Min bound is normalized by.
pub const STREAM_L1_NORM: &str = "stream_l1_norm";
/// `‖f‖₂` of the stream a Count Sketch bound is normalized by.
pub(crate) const STREAM_L2_NORM: &str = "stream_l2_norm";
/// Rows of one source's stream a summary folds: with a constant update
/// weight `c`, `‖f‖₁ = |c| · rows` and `‖f‖₂ ≤ ‖f‖₁`.
pub(crate) const STREAM_ROW_COUNT: &str = "stream_row_count";

/// Which stream norm bounds a Count-Min or Count Sketch estimate, given its
/// summary's update weight.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StreamNorm {
    /// The weight is the source's raw value column: the source's norm
    /// ([`source_norm_statistic`]).
    Source(String),
    /// A constant weight (a count): `|weight|` times the source's row count.
    ConstantWeight { weight: f64, source: String },
}

/// The statistic for `norm` ([`STREAM_L1_NORM`] or [`STREAM_L2_NORM`]) of
/// one source's stream, e.g. `stream_l1_norm[m]`.
pub(crate) fn source_norm_statistic(norm: &str, source: &str) -> String {
    format!("{norm}[{source}]")
}

/// Whether every value folded is non-negative, for values that are raw
/// samples of `metric` or plain sums of them, e.g.
/// `values_non_negative[m]`: an indicator, 1 when proven. Plan selection
/// binds it to 1 when the data workload declares `metric` a counter or
/// non-negative, and leaves it unbound otherwise.
pub fn raw_data_samples_sign_statistic(metric: &str) -> String {
    format!("{VALUES_NON_NEGATIVE}[{metric}]")
}

/// The metric a [`raw_data_samples_sign_statistic`] names.
pub fn parse_raw_data_samples_sign_statistic(statistic: &str) -> Option<&str> {
    statistic
        .strip_prefix(VALUES_NON_NEGATIVE)?
        .strip_prefix('[')?
        .strip_suffix(']')
}

/// The norm and source a [`source_norm_statistic`] names.
pub fn parse_source_norm_statistic(statistic: &str) -> Option<(&'static str, &str)> {
    [STREAM_L1_NORM, STREAM_L2_NORM]
        .into_iter()
        .find_map(|norm| {
            let source = statistic
                .strip_prefix(norm)?
                .strip_prefix('[')?
                .strip_suffix(']')?;
            Some((norm, source))
        })
}

fn additive(
    op: &CompositionOperator,
    inputs: &[AccuracyGuarantee],
    local: &AccuracyGuarantee,
    rule: &str,
) -> AccuracyGuarantee {
    let mut terms: Vec<BoundExpr> = inputs.iter().map(|g| g.spec.error.clone()).collect();
    terms.push(local.spec.error.clone());
    let mut deltas: Vec<ProbabilityExpr> = inputs
        .iter()
        .map(|g| g.spec.failure_probability.clone())
        .collect();
    deltas.push(local.spec.failure_probability.clone());
    AccuracyGuarantee::new(
        local.spec.accuracy_metric,
        BoundExpr::Sum { terms },
        ProbabilityExpr::UnionBound { terms: deltas },
        composed_provenance(op, inputs, local, rule),
    )
}

/// `(1 + ε_total) = Π (1 + ε_i)` ⇒ for two factors
/// `ε_in + ε_out + ε_in·ε_out`; written out as the sum of all
/// cross-products so the expression DAG is exact for any input count.
fn multiplicative(
    op: &CompositionOperator,
    inputs: &[AccuracyGuarantee],
    local: &AccuracyGuarantee,
) -> AccuracyGuarantee {
    let factors: Vec<&BoundExpr> = inputs
        .iter()
        .map(|g| &g.spec.error)
        .chain(std::iter::once(&local.spec.error))
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
        .map(|g| g.spec.failure_probability.clone())
        .collect();
    deltas.push(local.spec.failure_probability.clone());
    AccuracyGuarantee::new(
        AccuracyMetric::Relative,
        BoundExpr::Sum { terms },
        ProbabilityExpr::UnionBound { terms: deltas },
        composed_provenance(op, inputs, local, "relative_cross_term_union_bound"),
    )
}

fn lipschitz(
    op: &CompositionOperator,
    constant: f64,
    inputs: &[AccuracyGuarantee],
    local: Option<&AccuracyGuarantee>,
) -> AccuracyGuarantee {
    let input = &inputs[0];
    let scaled = BoundExpr::Scaled {
        factor: constant,
        inner: Box::new(input.spec.error.clone()),
    };
    let (bound, delta) = match local {
        Some(local) => (
            BoundExpr::Sum {
                terms: vec![scaled, local.spec.error.clone()],
            },
            ProbabilityExpr::UnionBound {
                terms: vec![
                    input.spec.failure_probability.clone(),
                    local.spec.failure_probability.clone(),
                ],
            },
        ),
        None => (scaled, input.spec.failure_probability.clone()),
    };
    let exact_local = AccuracyGuarantee::exact("deterministic Lipschitz transformation");
    AccuracyGuarantee::new(
        AccuracyMetric::ABSOLUTE,
        bound,
        delta,
        composed_provenance(
            op,
            inputs,
            local.unwrap_or(&exact_local),
            "lipschitz_union_bound",
        ),
    )
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
    inputs: &[AccuracyGuarantee],
    stats: &PropagationStats,
) -> Result<AccuracyGuarantee, AccuracyError> {
    if all_relative(inputs) {
        return relative_fold(op, inputs, stats, true, "exact_sum_relative");
    }
    let mut terms = Vec::with_capacity(inputs.len());
    let mut deltas = Vec::with_capacity(inputs.len());
    let mut provenance = Vec::new();
    for (i, input) in inputs.iter().enumerate() {
        let absolute =
            absolute_bound(input).ok_or_else(|| AccuracyError::UnsupportedComposition {
                operator: op.clone(),
                input_metrics: inputs.iter().map(|g| g.spec.accuracy_metric).collect(),
                local_metric: None,
                reason: format!(
                    "input {i} carries a {:?} guarantee, which has no registered \
                         conversion to an absolute value error",
                    input.spec.accuracy_metric
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
        deltas.push(input.spec.failure_probability.clone());
    }
    let count = row_count(stats, &mut provenance);
    let exact_local = AccuracyGuarantee::exact("ExactAggregate(Sum)");
    provenance.extend(composed_provenance(
        op,
        inputs,
        &exact_local,
        "exact_sum_union_bound",
    ));
    Ok(AccuracyGuarantee::new(
        AccuracyMetric::ABSOLUTE,
        BoundExpr::Product {
            factors: vec![count.clone(), BoundExpr::Sum { terms }],
        },
        ProbabilityExpr::Scaled {
            count,
            inner: Box::new(ProbabilityExpr::UnionBound { terms: deltas }),
        },
        provenance,
    ))
}

/// Exact arithmetic mean over values with absolute-error guarantees.
/// Averaging cannot amplify the largest absolute input error. The event
/// that every row respects its bound is still protected conservatively
/// by a union bound over the input row count.
fn exact_average(
    op: &CompositionOperator,
    inputs: &[AccuracyGuarantee],
    stats: &PropagationStats,
) -> Result<AccuracyGuarantee, AccuracyError> {
    if all_relative(inputs) {
        return relative_fold(op, inputs, stats, true, "exact_average_relative");
    }
    if inputs
        .iter()
        .any(|input| input.spec.accuracy_metric != AccuracyMetric::ABSOLUTE)
    {
        return Err(AccuracyError::UnsupportedComposition {
            operator: op.clone(),
            input_metrics: inputs.iter().map(|g| g.spec.accuracy_metric).collect(),
            local_metric: None,
            reason: "exact average requires absolute-error input guarantees".into(),
        });
    }
    let mut provenance = Vec::new();
    let count = row_count(stats, &mut provenance);
    let exact_local = AccuracyGuarantee::exact("ExactAggregate(Average)");
    provenance.extend(composed_provenance(
        op,
        inputs,
        &exact_local,
        "exact_average_union_bound",
    ));
    Ok(AccuracyGuarantee::new(
        AccuracyMetric::ABSOLUTE,
        BoundExpr::Max {
            terms: inputs.iter().map(|g| g.spec.error.clone()).collect(),
        },
        ProbabilityExpr::Scaled {
            count,
            inner: Box::new(ProbabilityExpr::UnionBound {
                terms: inputs
                    .iter()
                    .map(|g| g.spec.failure_probability.clone())
                    .collect(),
            }),
        },
        provenance,
    ))
}

/// Exact `max`/`min` over approximate inputs of one shared metric: the
/// returned value's error is at most the largest input bound (order
/// statistics are monotone under a uniform perturbation), with
/// probability by the union bound over every input row. This bounds the
/// returned *value*; it does not identify the true winning key.
fn exact_extremum(
    op: &CompositionOperator,
    inputs: &[AccuracyGuarantee],
    stats: &PropagationStats,
) -> Result<AccuracyGuarantee, AccuracyError> {
    if all_relative(inputs) {
        return relative_fold(op, inputs, stats, false, "exact_extremum_relative");
    }
    let metric = inputs[0].spec.accuracy_metric;
    if inputs
        .iter()
        .any(|g| g.spec.accuracy_metric != AccuracyMetric::ABSOLUTE)
    {
        return Err(AccuracyError::UnsupportedComposition {
            operator: op.clone(),
            input_metrics: inputs.iter().map(|g| g.spec.accuracy_metric).collect(),
            local_metric: None,
            reason: "exact max/min requires absolute-error input guarantees".into(),
        });
    }
    let mut provenance = Vec::new();
    let count = row_count(stats, &mut provenance);
    let exact_local = AccuracyGuarantee::exact("ExactAggregate(Max)");
    provenance.extend(composed_provenance(
        op,
        inputs,
        &exact_local,
        "exact_extremum_union_bound",
    ));
    Ok(AccuracyGuarantee::new(
        metric,
        BoundExpr::Max {
            terms: inputs.iter().map(|g| g.spec.error.clone()).collect(),
        },
        ProbabilityExpr::Scaled {
            count,
            inner: Box::new(ProbabilityExpr::UnionBound {
                terms: inputs
                    .iter()
                    .map(|g| g.spec.failure_probability.clone())
                    .collect(),
            }),
        },
        provenance,
    ))
}

/// An exact sum, mean, max or min of values each within relative `α_i`
/// is within relative `max α_i`. A max or min holds whatever the signs;
/// a sum or mean only over non-negative values (no cancellation), so
/// with `needs_sign` and the sign unknown the bound is the symbolic
/// [`VALUES_NON_NEGATIVE`]. The failure probability is the union bound
/// over every input row.
fn relative_fold(
    op: &CompositionOperator,
    inputs: &[AccuracyGuarantee],
    stats: &PropagationStats,
    needs_sign: bool,
    rule: &str,
) -> Result<AccuracyGuarantee, AccuracyError> {
    if needs_sign && stats.values_non_negative == Some(false) {
        return Err(AccuracyError::UnsupportedComposition {
            operator: op.clone(),
            input_metrics: inputs.iter().map(|g| g.spec.accuracy_metric).collect(),
            local_metric: None,
            reason: "relative error of a sum over signed values is unbounded".into(),
        });
    }
    let mut provenance = Vec::new();
    let count = row_count(stats, &mut provenance);
    let bound = if needs_sign && stats.values_non_negative.is_none() {
        match &stats.sign_statistic {
            // The sign of one metric's samples: the bound, gated on it.
            Some(statistic) => {
                provenance.push(GuaranteeSource::UnavailableStatistic {
                    statistic: statistic.clone(),
                });
                BoundExpr::Product {
                    factors: vec![
                        BoundExpr::Max {
                            terms: inputs.iter().map(|g| g.spec.error.clone()).collect(),
                        },
                        BoundExpr::Unknown {
                            statistic: statistic.clone(),
                        },
                    ],
                }
            }
            None => {
                provenance.push(GuaranteeSource::UnavailableStatistic {
                    statistic: VALUES_NON_NEGATIVE.into(),
                });
                BoundExpr::Unknown {
                    statistic: VALUES_NON_NEGATIVE.into(),
                }
            }
        }
    } else {
        BoundExpr::Max {
            terms: inputs.iter().map(|g| g.spec.error.clone()).collect(),
        }
    };
    provenance.extend(composed_provenance(
        op,
        inputs,
        &AccuracyGuarantee::exact("exact aggregation"),
        rule,
    ));
    Ok(AccuracyGuarantee::new(
        AccuracyMetric::Relative,
        bound,
        ProbabilityExpr::Scaled {
            count,
            inner: Box::new(ProbabilityExpr::UnionBound {
                terms: inputs
                    .iter()
                    .map(|g| g.spec.failure_probability.clone())
                    .collect(),
            }),
        },
        provenance,
    ))
}

/// Exact division of two relative-value estimates (an exact operand has
/// bound 0). If the numerator is within relative `a` and the denominator
/// within `b < 1`, the ratio is within
/// `max((1 + a)/(1 − b) − 1, 1 − (1 − a)/(1 + b)) = (a + b)/(1 − b)`, and
/// fails with the union bound of their failure probabilities. No operand
/// domain is needed: a relative bound below 1 keeps a zero denominator at
/// zero and a numerator's sign, so the true and estimated ratios are then
/// the same non-finite value. Both bounds must be constants: a symbolic one
/// has no rule.
/// Float64 overflow and underflow of the quotient are not bounded, as for a
/// product.
fn exact_division(
    op: &CompositionOperator,
    inputs: &[AccuracyGuarantee],
) -> Result<AccuracyGuarantee, AccuracyError> {
    let unsupported = |reason: String| AccuracyError::UnsupportedComposition {
        operator: op.clone(),
        input_metrics: inputs.iter().map(|g| g.spec.accuracy_metric).collect(),
        local_metric: None,
        reason,
    };
    if inputs.len() != 2
        || inputs.iter().any(|input| {
            input.spec.accuracy_metric != AccuracyMetric::Relative && !input.is_exact()
        })
    {
        return Err(unsupported(
            "division needs two relative-value or exact guarantees".into(),
        ));
    }
    let Some(numerator) = inputs[0].spec.error.evaluate() else {
        return Err(unsupported(
            "numerator relative bound is unavailable".into(),
        ));
    };
    let Some(denominator) = inputs[1].spec.error.evaluate() else {
        return Err(unsupported(
            "denominator relative bound is unavailable".into(),
        ));
    };
    if !(numerator.is_finite() && numerator >= 0.0 && (0.0..1.0).contains(&denominator)) {
        return Err(unsupported(
            "division needs finite non-negative bounds and a denominator bound below one".into(),
        ));
    }
    Ok(AccuracyGuarantee::new(
        AccuracyMetric::Relative,
        BoundExpr::Constant {
            value: (numerator + denominator) / (1.0 - denominator),
        },
        ProbabilityExpr::UnionBound {
            terms: inputs
                .iter()
                .map(|input| input.spec.failure_probability.clone())
                .collect(),
        },
        composed_provenance(
            op,
            inputs,
            &AccuracyGuarantee::exact("exact division"),
            "relative_division_union_bound",
        ),
    ))
}

/// Whether `guarantee` is a sketch's distinct-count estimate.
fn estimates_distinct_count(guarantee: &AccuracyGuarantee) -> bool {
    let cardinality = format!("{:?}", asap_types::ir::schema::SketchStatistic::Cardinality);
    guarantee.provenance.iter().any(|source| {
        matches!(source, GuaranteeSource::SketchEvaluation { query, .. } if *query == cardinality)
    })
}

/// Every input carries a relative-value guarantee.
fn all_relative(inputs: &[AccuracyGuarantee]) -> bool {
    !inputs.is_empty()
        && inputs
            .iter()
            .all(|g| g.spec.accuracy_metric == AccuracyMetric::Relative)
}

/// `stats.input_row_count` as a bound factor, or an `Unknown` leaf named by
/// `stats.row_count_statistic` (recorded in `provenance`) when absent.
fn row_count(stats: &PropagationStats, provenance: &mut Vec<GuaranteeSource>) -> BoundExpr {
    match stats.input_row_count {
        Some(n) => BoundExpr::Constant { value: n as f64 },
        None => {
            let statistic = stats.row_count_statistic.unwrap_or(INPUT_ROW_COUNT);
            provenance.push(GuaranteeSource::UnavailableStatistic {
                statistic: statistic.into(),
            });
            BoundExpr::Unknown {
                statistic: statistic.into(),
            }
        }
    }
}

/// `input` with a Count-Min ([`AccuracyMetric::ABSOLUTE_STREAM_L1`]) or Count Sketch
/// ([`AccuracyMetric::ABSOLUTE_STREAM_L2`]) bound restated as an additive absolute
/// error: the normalized bound times its stream norm, a symbolic statistic
/// until bound. The norm is the one `norm` names ([`StreamNorm`]), which
/// plan selection can bind; without one (a weight computed from the
/// values) it is a statistic nothing binds. Any other guarantee is
/// returned unchanged.
pub(crate) fn additive_as_absolute(
    input: AccuracyGuarantee,
    norm: Option<&StreamNorm>,
) -> AccuracyGuarantee {
    if !matches!(
        input.spec.accuracy_metric,
        AccuracyMetric::ABSOLUTE_STREAM_L1 | AccuracyMetric::ABSOLUTE_STREAM_L2
    ) || input.is_exact()
    {
        return input;
    }
    let mut bound = absolute_bound(&input).expect("additive metrics convert to absolute errors");
    if let (Some(norm), BoundExpr::Product { factors }) = (norm, &mut bound) {
        for factor in factors {
            if let BoundExpr::Unknown { statistic } = factor {
                *factor = match norm {
                    StreamNorm::Source(source) => BoundExpr::Unknown {
                        statistic: source_norm_statistic(statistic, source),
                    },
                    StreamNorm::ConstantWeight { weight, source } => BoundExpr::Product {
                        factors: vec![
                            BoundExpr::Constant {
                                value: weight.abs(),
                            },
                            BoundExpr::Unknown {
                                statistic: source_norm_statistic(STREAM_ROW_COUNT, source),
                            },
                        ],
                    },
                };
            }
        }
    }
    let mut provenance = input.provenance;
    unknown_statistics(&bound, &mut |statistic| {
        provenance.push(GuaranteeSource::UnavailableStatistic {
            statistic: statistic.into(),
        })
    });
    let mut absolute = AccuracyGuarantee {
        provenance,
        ..input
    };
    absolute.spec.accuracy_metric = AccuracyMetric::ABSOLUTE;
    absolute.spec.error = bound;
    absolute
}

/// Every `Unknown` statistic in `bound`.
fn unknown_statistics(bound: &BoundExpr, found: &mut dyn FnMut(&str)) {
    match bound {
        BoundExpr::Unknown { statistic } => found(statistic),
        BoundExpr::Sum { terms }
        | BoundExpr::Product { factors: terms }
        | BoundExpr::Max { terms } => {
            for term in terms {
                unknown_statistics(term, found);
            }
        }
        BoundExpr::Scaled { inner, .. } => unknown_statistics(inner, found),
        _ => {}
    }
}

/// `input`'s bound converted to absolute value units, multiplying a
/// normalized metric by the (unknown) statistic it is normalized by. `None`
/// for a metric with no such conversion (`Rank`).
fn absolute_bound(input: &AccuracyGuarantee) -> Option<BoundExpr> {
    let normalizer = match input.spec.accuracy_metric {
        AccuracyMetric::ABSOLUTE => return Some(input.spec.error.clone()),
        AccuracyMetric::Relative => "true_value_magnitude",
        AccuracyMetric::ABSOLUTE_STREAM_L1 => STREAM_L1_NORM,
        AccuracyMetric::ABSOLUTE_STREAM_L2 => STREAM_L2_NORM,
        // `Rank` has no distribution-free conversion to a value error; a
        // metric this crate does not know has no registered conversion.
        AccuracyMetric::Rank | _ => return None,
    };
    if input.spec.error.is_zero() {
        return Some(BoundExpr::Zero);
    }
    Some(BoundExpr::Product {
        factors: vec![
            input.spec.error.clone(),
            BoundExpr::Unknown {
                statistic: normalizer.into(),
            },
        ],
    })
}

fn composed_provenance(
    op: &CompositionOperator,
    inputs: &[AccuracyGuarantee],
    local: &AccuracyGuarantee,
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

/// The built-in composition rule: `inputs`' guarantees (in the parent's
/// child order) composed with the parent's own `local` guarantee under `op`,
/// given the statistics in `stats` (all unknown in Stage 1; plan selection
/// binds some). `Err` is the fail-closed answer: no registered rule, or a
/// missing input guarantee. It means "no rule", not "infeasible": Pass 1
/// keeps the candidate with `guarantee = None`.
pub fn propagate(
    op: &CompositionOperator,
    inputs: &[AccuracyGuarantee],
    local: Option<&AccuracyGuarantee>,
    stats: &PropagationStats,
) -> Result<AccuracyGuarantee, AccuracyError> {
    // Exact input: only the local guarantee remains (or the value is exact).
    if inputs.iter().all(AccuracyGuarantee::is_exact)
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
                let mut out = AccuracyGuarantee::exact(format!("{op:?} over exact inputs"));
                out.provenance.push(GuaranteeSource::CompositionStep {
                    operator: op.clone(),
                    rule: "exact_input".into(),
                });
                out
            }
        });
    }

    let input_metrics: Vec<AccuracyMetric> =
        inputs.iter().map(|g| g.spec.accuracy_metric).collect();
    let unsupported = |reason: String| AccuracyError::UnsupportedComposition {
        operator: op.clone(),
        input_metrics: input_metrics.clone(),
        local_metric: local.map(|g| g.spec.accuracy_metric),
        reason,
    };
    // An exact input is compatible with every metric; only approximate
    // inputs constrain the rule.
    let approximate: Vec<&AccuracyGuarantee> = inputs.iter().filter(|g| !g.is_exact()).collect();
    let same_metric =
        |metric: AccuracyMetric| approximate.iter().all(|g| g.spec.accuracy_metric == metric);

    match op {
        CompositionOperator::CheckedRelativeDivision => {
            if inputs.len() != 2 || local.is_some() || !same_metric(AccuracyMetric::Relative) {
                return Err(unsupported(
                    "checked division requires two exact/relative-value operands".into(),
                ));
            }
            let a = inputs[0]
                .spec
                .error
                .evaluate()
                .ok_or_else(|| unsupported("unknown numerator bound".into()))?;
            let b = inputs[1]
                .spec
                .error
                .evaluate()
                .ok_or_else(|| unsupported("unknown denominator bound".into()))?;
            if !(0.0..1.0).contains(&b) || a < 0.0 || !a.is_finite() {
                return Err(unsupported("invalid relative division bounds".into()));
            }
            Ok(AccuracyGuarantee::new(
                AccuracyMetric::Relative,
                BoundExpr::Constant {
                    value: (a + b) / (1.0 - b) + 4.0 * f64::EPSILON,
                },
                ProbabilityExpr::UnionBound {
                    terms: inputs
                        .iter()
                        .map(|g| g.spec.failure_probability.clone())
                        .collect(),
                },
                composed_provenance(
                    op,
                    inputs,
                    &AccuracyGuarantee::exact("checked floating-point division"),
                    "checked_relative_division_union_bound",
                ),
            ))
        }
        CompositionOperator::ApproximateAggregate => {
            let local = local.ok_or_else(|| {
                unsupported("approximate operator has no local guarantee to compose".into())
            })?;
            if !same_metric(local.spec.accuracy_metric) {
                return Err(unsupported(format!(
                    "no registered cross-metric rule from {input_metrics:?} to {:?}",
                    local.spec.accuracy_metric
                )));
            }
            // A distinct count over approximate values is not within the
            // inputs' relative error: no rule, as for any summary whose
            // estimate is not a value of its inputs.
            if estimates_distinct_count(local) {
                return Err(unsupported(
                    "no registered rule for a distinct count over approximate values".into(),
                ));
            }
            match local.spec.accuracy_metric {
                AccuracyMetric::ABSOLUTE => Ok(additive(op, inputs, local, "additive_union_bound")),
                AccuracyMetric::Relative => {
                    if stats.values_non_negative == Some(false) {
                        return Err(unsupported(
                            "relative-error composition cannot use a known signed input".into(),
                        ));
                    }
                    if stats.values_non_negative.is_none() {
                        let mut provenance = composed_provenance(
                            op,
                            inputs,
                            local,
                            "relative_value_sign_unverified",
                        );
                        provenance.extend(stats.evidence_provenance.clone());
                        provenance.push(GuaranteeSource::UnavailableStatistic {
                            statistic: VALUES_NON_NEGATIVE.into(),
                        });
                        return Ok(AccuracyGuarantee::new(
                            AccuracyMetric::Relative,
                            BoundExpr::Unknown {
                                statistic: VALUES_NON_NEGATIVE.into(),
                            },
                            ProbabilityExpr::Unknown {
                                statistic: VALUES_NON_NEGATIVE.into(),
                            },
                            provenance,
                        ));
                    }
                    Ok(multiplicative(op, inputs, local))
                }
                AccuracyMetric::Rank
                | AccuracyMetric::ABSOLUTE_STREAM_L1
                | AccuracyMetric::ABSOLUTE_STREAM_L2
                | _ => Err(unsupported(format!(
                    "no registered same-metric composition rule for {:?} over {:?}",
                    local.spec.accuracy_metric, local.spec.accuracy_metric
                ))),
            }
        }
        CompositionOperator::Lipschitz { constant } => {
            if !(constant.is_finite() && *constant >= 0.0) {
                return Err(unsupported(format!(
                    "Lipschitz constant {constant} is not a finite non-negative number"
                )));
            }
            if inputs.len() != 1 || !same_metric(AccuracyMetric::ABSOLUTE) {
                return Err(unsupported(
                    "Lipschitz rule is registered for exactly one absolute-error input".into(),
                ));
            }
            if local.is_some_and(|g| g.spec.accuracy_metric != AccuracyMetric::ABSOLUTE) {
                return Err(unsupported(
                    "Lipschitz rule needs an absolute-error local guarantee".into(),
                ));
            }
            Ok(lipschitz(op, *constant, inputs, local))
        }
        CompositionOperator::ExactSum => exact_sum(op, inputs, stats),
        CompositionOperator::ExactAverage => exact_average(op, inputs, stats),
        CompositionOperator::ExactExtremum => exact_extremum(op, inputs, stats),
        CompositionOperator::ExactDivision => exact_division(op, inputs),
        CompositionOperator::CounterRate
        | CompositionOperator::InstantCounterRate
        | CompositionOperator::CounterIncrease => Err(unsupported(
            "counter reset detection and boundary extrapolation have no distribution-free \
                 accuracy bound over approximate samples; exact samples remain exact"
                .into(),
        )),
        CompositionOperator::TopKSelection => {
            let selected = stats.topk_selected_lower_bound;
            let excluded = stats.topk_excluded_upper_bound;
            let delta = stats.topk_interval_failure_probability;
            if selected.is_some_and(|value| !value.is_finite())
                || excluded.is_some_and(|value| !value.is_finite())
                || delta.is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
                || selected
                    .zip(excluded)
                    .is_some_and(|(lower, upper)| lower <= upper)
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
            for (name, missing) in [
                ("topk_selected_lower_bound", selected.is_none()),
                ("topk_excluded_upper_bound", excluded.is_none()),
                ("topk_interval_failure_probability", delta.is_none()),
            ] {
                if missing {
                    provenance.push(GuaranteeSource::UnavailableStatistic {
                        statistic: name.into(),
                    });
                }
            }
            provenance.push(GuaranteeSource::CompositionStep {
                operator: op.clone(),
                rule: "topk_membership_margin_certificate".into(),
            });
            let certified = selected.is_some() && excluded.is_some() && delta.is_some();
            // A certified margin makes the returned key set the true one, so
            // each returned value keeps its score's error; without one,
            // membership and hence the values are unknown.
            let metric = approximate
                .first()
                .map_or(AccuracyMetric::ABSOLUTE, |score| score.spec.accuracy_metric);
            if !same_metric(metric) {
                return Err(unsupported(
                    "top-k selection needs scores of one metric".into(),
                ));
            }
            let error = if certified {
                BoundExpr::Max {
                    terms: inputs.iter().map(|g| g.spec.error.clone()).collect(),
                }
            } else {
                BoundExpr::Unknown {
                    statistic: "topk_membership_margin".into(),
                }
            };
            Ok(AccuracyGuarantee::new(
                metric,
                error,
                delta.map_or_else(
                    || ProbabilityExpr::Unknown {
                        statistic: "topk_interval_failure_probability".into(),
                    },
                    |value| ProbabilityExpr::Constant { value },
                ),
                provenance,
            ))
        }
        // An operator this crate does not know has no registered rule.
        _ => Err(unsupported("no registered rule for this operator".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source-norm statistic names its norm and source, whatever
    /// characters the source holds; any other statistic is not one.
    #[test]
    fn source_norm_statistics_round_trip() {
        for source in ["m", "a[b]", "]x[", "t.c", ""] {
            for norm in [STREAM_L1_NORM, STREAM_L2_NORM] {
                let statistic = source_norm_statistic(norm, source);
                assert_eq!(
                    parse_source_norm_statistic(&statistic),
                    Some((norm, source))
                );
            }
        }
        for other in [
            INPUT_ROW_COUNT,
            STREAM_L1_NORM,
            "stream_l3_norm[m]",
            "stream_l1_norm[m",
        ] {
            assert_eq!(parse_source_norm_statistic(other), None, "{other}");
        }
    }
    fn abs(bound: f64, delta: f64) -> AccuracyGuarantee {
        AccuracyGuarantee::new(
            AccuracyMetric::ABSOLUTE,
            BoundExpr::Constant { value: bound },
            ProbabilityExpr::Constant { value: delta },
            vec![],
        )
    }
    fn rel(bound: f64) -> AccuracyGuarantee {
        AccuracyGuarantee::new(
            AccuracyMetric::Relative,
            BoundExpr::Constant { value: bound },
            ProbabilityExpr::Zero,
            vec![],
        )
    }
    fn with_metric(metric: AccuracyMetric, bound: f64) -> AccuracyGuarantee {
        let mut g = abs(bound, 0.0);
        g.spec.accuracy_metric = metric;
        g
    }
    #[test]
    fn checked_division_propagates_value_bounds_and_rejects_rank_bounds() {
        let op = CompositionOperator::CheckedRelativeDivision;
        let inputs = [rel(0.01), rel(0.01)];
        let g = propagate(&op, &inputs, None, &Default::default()).unwrap();
        assert!((g.spec.error.evaluate().unwrap() - 0.02 / 0.99).abs() < 1e-14);
        let mut rank = inputs[0].clone();
        rank.spec.accuracy_metric = AccuracyMetric::Rank;
        assert!(propagate(&op, &[rank.clone(), rank], None, &Default::default()).is_err());
        assert!(propagate(&op, &[rel(0.01), rel(1.0)], None, &Default::default()).is_err());
    }
    /// A ratio of relative bounds `a`, `b` is within `(a + b)/(1 − b)`,
    /// the larger of its over- and under-estimate bounds, with the union
    /// bound on failure; no operand domain is needed. A denominator bound of
    /// 1 or more, or a bound that is not finite and non-negative, has no
    /// rule.
    #[test]
    fn relative_division_bounds_the_ratio_without_operand_domains() {
        let mut numerator = rel(0.02);
        numerator.spec.failure_probability = ProbabilityExpr::Constant { value: 0.001 };
        let got = propagate(
            &CompositionOperator::ExactDivision,
            &[numerator, rel(0.03)],
            None,
            &PropagationStats::default(),
        )
        .unwrap();
        let (a, b): (f64, f64) = (0.02, 0.03);
        let over = (1.0 + a) / (1.0 - b) - 1.0;
        let under = 1.0 - (1.0 - a) / (1.0 + b);
        assert!((got.spec.error.evaluate().unwrap() - over.max(under)).abs() < 1e-14);
        assert!((got.spec.failure_probability.evaluate().unwrap() - 0.001).abs() < 1e-15);
        for bound in [1., f64::NAN, f64::INFINITY, -0.1] {
            assert!(propagate(
                &CompositionOperator::ExactDivision,
                &[rel(0.01), rel(bound)],
                None,
                &PropagationStats::default()
            )
            .is_err());
        }
        let rank = with_metric(AccuracyMetric::Rank, 0.01);
        assert!(propagate(
            &CompositionOperator::ExactDivision,
            &[rank.clone(), rank],
            None,
            &PropagationStats::default()
        )
        .is_err());
    }
    #[test]
    fn exact_child_contributes_zero_error() {
        let local = abs(0.05, 0.01);
        let out = propagate(
            &CompositionOperator::ApproximateAggregate,
            &[AccuracyGuarantee::exact("sum")],
            Some(&local),
            &PropagationStats::default(),
        )
        .unwrap();
        assert_eq!(out.spec.error.evaluate(), Some(0.05));
        assert_eq!(out.spec.failure_probability.evaluate(), Some(0.01));
        assert_eq!(out.spec.accuracy_metric, AccuracyMetric::ABSOLUTE);
    }
    #[test]
    fn additive_bounds_and_delta_union_bound_compose() {
        let out = propagate(
            &CompositionOperator::ApproximateAggregate,
            &[abs(0.02, 0.01)],
            Some(&abs(0.03, 0.02)),
            &PropagationStats::default(),
        )
        .unwrap();
        assert!((out.spec.error.evaluate().unwrap() - 0.05).abs() < 1e-12);
        // Union bound, not 1 − (1−0.01)(1−0.02) = 0.0298.
        assert!((out.spec.failure_probability.evaluate().unwrap() - 0.03).abs() < 1e-12);
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
        let out = propagate(
            &CompositionOperator::ApproximateAggregate,
            &[rel(0.1)],
            Some(&rel(0.2)),
            &stats,
        )
        .unwrap();
        // 0.1 + 0.2 + 0.1·0.2 = 0.32, not 0.3.
        assert!((out.spec.error.evaluate().unwrap() - 0.32).abs() < 1e-12);
        assert_eq!(out.spec.accuracy_metric, AccuracyMetric::Relative);
    }
    #[test]
    fn relative_error_without_sign_knowledge_remains_symbolic() {
        let unknown = propagate(
            &CompositionOperator::ApproximateAggregate,
            &[rel(0.1)],
            Some(&rel(0.2)),
            &PropagationStats::default(),
        )
        .unwrap();
        assert!(unknown.has_unknown());
        let signed = propagate(
            &CompositionOperator::ApproximateAggregate,
            &[rel(0.1)],
            Some(&rel(0.2)),
            &PropagationStats {
                values_non_negative: Some(false),
                ..Default::default()
            },
        );
        assert!(signed.is_err());
    }
    #[test]
    fn incompatible_metrics_are_rejected_not_treated_as_exact() {
        // HLL cardinality error under a CMS frequency guarantee.
        let err = propagate(
            &CompositionOperator::ApproximateAggregate,
            &[with_metric(AccuracyMetric::Relative, 0.01)],
            Some(&with_metric(AccuracyMetric::ABSOLUTE_STREAM_L1, 0.01)),
            &PropagationStats::default(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            AccuracyError::UnsupportedComposition {
                input_metrics,
                local_metric: Some(AccuracyMetric::ABSOLUTE_STREAM_L1),
                ..
            } if input_metrics == vec![AccuracyMetric::Relative]
        ));
        // Quantile rank error under value-additive logic.
        let err = propagate(
            &CompositionOperator::ApproximateAggregate,
            &[with_metric(AccuracyMetric::Rank, 0.01)],
            Some(&abs(0.01, 0.0)),
            &PropagationStats::default(),
        )
        .unwrap_err();
        assert!(matches!(err, AccuracyError::UnsupportedComposition { .. }));
    }
    #[test]
    fn same_metric_rank_over_rank_has_no_registered_rule() {
        let err = propagate(
            &CompositionOperator::ApproximateAggregate,
            &[with_metric(AccuracyMetric::Rank, 0.01)],
            Some(&with_metric(AccuracyMetric::Rank, 0.01)),
            &PropagationStats::default(),
        )
        .unwrap_err();
        assert!(matches!(err, AccuracyError::UnsupportedComposition { .. }));
    }
    #[test]
    fn lipschitz_scales_the_input_bound() {
        let out = propagate(
            &CompositionOperator::Lipschitz { constant: 3.0 },
            &[abs(0.1, 0.01)],
            Some(&abs(0.05, 0.02)),
            &PropagationStats::default(),
        )
        .unwrap();
        assert!((out.spec.error.evaluate().unwrap() - 0.35).abs() < 1e-12);
        assert!((out.spec.failure_probability.evaluate().unwrap() - 0.03).abs() < 1e-12);
    }
    #[test]
    fn exact_sum_over_approximate_sums_bounds_and_keeps_unknown_row_count_unknown() {
        let out = propagate(
            &CompositionOperator::ExactSum,
            &[abs(0.1, 0.01)],
            None,
            &PropagationStats::default(),
        )
        .unwrap();
        assert_eq!(out.spec.accuracy_metric, AccuracyMetric::ABSOLUTE);
        assert_eq!(
            out.spec.error.evaluate(),
            None,
            "unknown row count stays unknown"
        );
        assert!(out.provenance.iter().any(|s| matches!(
            s,
            GuaranteeSource::UnavailableStatistic { statistic } if statistic == "input_row_count"
        )));
        assert!(!satisfies(&out, &AccuracyTarget::max_error(1.0)));

        let known = PropagationStats {
            input_row_count: Some(4),
            ..Default::default()
        };
        let out = propagate(
            &CompositionOperator::ExactSum,
            &[abs(0.1, 0.01)],
            None,
            &known,
        )
        .unwrap();
        assert!((out.spec.error.evaluate().unwrap() - 0.4).abs() < 1e-12);
        assert!((out.spec.failure_probability.evaluate().unwrap() - 0.04).abs() < 1e-12);
    }
    #[test]
    fn exact_extremum_takes_the_max_bound() {
        let known = PropagationStats {
            input_row_count: Some(2),
            ..Default::default()
        };
        let out = propagate(
            &CompositionOperator::ExactExtremum,
            &[abs(0.1, 0.01), abs(0.3, 0.01)],
            None,
            &known,
        )
        .unwrap();
        assert!((out.spec.error.evaluate().unwrap() - 0.3).abs() < 1e-12);
        assert!((out.spec.failure_probability.evaluate().unwrap() - 0.04).abs() < 1e-12);
    }
    /// `max`/`min` of values each within relative `α_i` is within relative
    /// `max α_i` of the true extremum, whatever the signs.
    #[test]
    fn exact_extremum_keeps_a_relative_value_bound() {
        let out = propagate(
            &CompositionOperator::ExactExtremum,
            &[rel(0.01), rel(0.02)],
            None,
            &PropagationStats {
                input_row_count: Some(3),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(out.spec.accuracy_metric, AccuracyMetric::Relative);
        assert_eq!(out.spec.error.evaluate(), Some(0.02));
        assert_eq!(out.spec.failure_probability.evaluate(), Some(0.0));
    }
    /// A sum or mean of relative-error values keeps the relative bound only
    /// over values known non-negative (cancellation otherwise); with the sign
    /// unknown, the bound stays symbolic, and a known sign change rejects.
    #[test]
    fn exact_sum_and_average_keep_a_relative_bound_only_without_a_sign_change() {
        for op in [
            CompositionOperator::ExactSum,
            CompositionOperator::ExactAverage,
        ] {
            let with = |values_non_negative| {
                propagate(
                    &op,
                    &[rel(0.01)],
                    None,
                    &PropagationStats {
                        values_non_negative,
                        input_row_count: Some(3),
                        ..Default::default()
                    },
                )
            };
            let known = with(Some(true)).unwrap();
            assert_eq!(known.spec.accuracy_metric, AccuracyMetric::Relative);
            assert_eq!(known.spec.error.evaluate(), Some(0.01), "{op:?}");
            let unknown = with(None).unwrap();
            assert_eq!(unknown.spec.accuracy_metric, AccuracyMetric::Relative);
            assert_eq!(
                unknown.spec.error,
                BoundExpr::Unknown {
                    statistic: VALUES_NON_NEGATIVE.into()
                }
            );
            assert!(with(Some(false)).is_err(), "{op:?}");
        }
    }
    /// Over the samples of one metric, an unknown sign is named by that
    /// metric ([`raw_data_samples_sign_statistic`]) and gates the bound: binding it
    /// to 1 (proven) gives the relative bound, and left unbound it stays
    /// unknown.
    #[test]
    fn a_sign_over_one_metrics_samples_gates_the_relative_bound() {
        let statistic = raw_data_samples_sign_statistic("m");
        assert_eq!(parse_raw_data_samples_sign_statistic(&statistic), Some("m"));
        assert_eq!(
            parse_raw_data_samples_sign_statistic(VALUES_NON_NEGATIVE),
            None
        );
        for op in [
            CompositionOperator::ExactSum,
            CompositionOperator::ExactAverage,
        ] {
            let out = propagate(
                &op,
                &[rel(0.01)],
                None,
                &PropagationStats {
                    sign_statistic: Some(statistic.clone()),
                    input_row_count: Some(3),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(out.spec.error.evaluate(), None, "{op:?}");
            let proven = out.bind(&|s| (s == statistic).then_some(1.0));
            assert_eq!(proven.spec.error.evaluate(), Some(0.01), "{op:?}");
            let other = out.bind(&|s| (s == raw_data_samples_sign_statistic("n")).then_some(1.0));
            assert_eq!(other.spec.error.evaluate(), None, "{op:?}");
        }
    }

    #[test]
    fn exact_average_has_its_own_absolute_error_rule() {
        let out = propagate(
            &CompositionOperator::ExactAverage,
            &[abs(0.25, 0.01)],
            None,
            &PropagationStats {
                input_row_count: Some(4),
                ..PropagationStats::default()
            },
        )
        .unwrap();
        assert_eq!(out.spec.accuracy_metric, AccuracyMetric::ABSOLUTE);
        assert_eq!(out.spec.error.evaluate(), Some(0.25));
        assert_eq!(out.spec.failure_probability.evaluate(), Some(0.04));
    }
    #[test]
    fn topk_selection_requires_a_separated_margin_certificate() {
        let unknown = propagate(
            &CompositionOperator::TopKSelection,
            &[abs(0.1, 0.01)],
            None,
            &PropagationStats::default(),
        )
        .unwrap();
        assert!(unknown.has_unknown());

        let certified = propagate(
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
        assert_eq!(certified.spec.accuracy_metric, AccuracyMetric::ABSOLUTE);
        assert_eq!(certified.spec.error.evaluate(), Some(0.1));
        assert_eq!(certified.spec.failure_probability.evaluate(), Some(0.005));

        let overlapping = propagate(
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

        let partial = propagate(
            &CompositionOperator::TopKSelection,
            &[abs(0.1, 0.01)],
            None,
            &PropagationStats {
                topk_selected_lower_bound: Some(101.0),
                topk_interval_failure_probability: Some(0.005),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(partial.has_unknown());
        assert_eq!(partial.spec.failure_probability.evaluate(), Some(0.005));

        let invalid_partial = propagate(
            &CompositionOperator::TopKSelection,
            &[abs(0.1, 0.01)],
            None,
            &PropagationStats {
                topk_selected_lower_bound: Some(f64::NAN),
                ..Default::default()
            },
        );
        assert!(invalid_partial.is_err());
    }
}
