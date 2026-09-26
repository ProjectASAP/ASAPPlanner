//! Registered propagation rules for computations over uncertain inputs.
use super::*;

impl DefaultAccuracyModel {
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

    /// Exact arithmetic mean over values with absolute-error guarantees.
    /// Averaging cannot amplify the largest absolute input error. The event
    /// that every row respects its bound is still protected conservatively
    /// by a union bound over the input row count.
    fn exact_average(
        op: &CompositionOperator,
        inputs: &[ResultGuarantee],
        stats: &PropagationStats,
    ) -> Result<ResultGuarantee, AccuracyError> {
        if inputs
            .iter()
            .any(|input| input.metric != ErrorMetric::AbsoluteValue)
        {
            return Err(AccuracyError::UnsupportedComposition {
                operator: op.clone(),
                input_metrics: inputs.iter().map(|g| g.metric).collect(),
                local_metric: None,
                reason: "exact average requires AbsoluteValue input guarantees".into(),
            });
        }
        let mut provenance = Vec::new();
        let count = row_count(stats, &mut provenance);
        let exact_local = ResultGuarantee::exact("ExactAggregate(Average)");
        provenance.extend(composed_provenance(
            op,
            inputs,
            &exact_local,
            "exact_average_union_bound",
        ));
        Ok(ResultGuarantee {
            metric: ErrorMetric::AbsoluteValue,
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
        if inputs
            .iter()
            .any(|g| g.metric != ErrorMetric::AbsoluteValue)
        {
            return Err(AccuracyError::UnsupportedComposition {
                operator: op.clone(),
                input_metrics: inputs.iter().map(|g| g.metric).collect(),
                local_metric: None,
                reason: "exact max/min requires AbsoluteValue input guarantees".into(),
            });
        }
        let mut provenance = Vec::new();
        let count = row_count(stats, &mut provenance);
        let exact_local = ResultGuarantee::exact("ExactAggregate(Max)");
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

    /// Exact division of two relative-value estimates. If the numerator is
    /// within `a` and the denominator within `b`, their ratio is within
    /// `(a + b) / (1 - b)`. DDSketch supplies those deterministic bounds.
    fn exact_division(
        op: &CompositionOperator,
        inputs: &[ResultGuarantee],
        stats: &PropagationStats,
    ) -> Result<ResultGuarantee, AccuracyError> {
        let unsupported = |reason: String| AccuracyError::UnsupportedComposition {
            operator: op.clone(),
            input_metrics: inputs.iter().map(|g| g.metric).collect(),
            local_metric: None,
            reason,
        };
        if inputs.len() != 2
            || inputs
                .iter()
                .any(|input| input.metric != ErrorMetric::RelativeValue)
        {
            return Err(unsupported(
                "division needs exactly two RelativeValue guarantees".into(),
            ));
        }
        let Some(numerator) = inputs[0].bound.evaluate() else {
            return Err(unsupported(
                "numerator relative bound is unavailable".into(),
            ));
        };
        let Some(denominator) = inputs[1].bound.evaluate() else {
            return Err(unsupported(
                "denominator relative bound is unavailable".into(),
            ));
        };
        if !(numerator.is_finite()
            && denominator.is_finite()
            && numerator >= 0.0
            && (0.0..1.0).contains(&denominator))
        {
            return Err(unsupported(
                "division needs finite non-negative bounds and a denominator bound below one"
                    .into(),
            ));
        }
        let Some(domains) = &stats.division_operand_domains else {
            return Err(unsupported(
                "division needs finite operand domains and a nonzero denominator proof".into(),
            ));
        };
        for domain in domains {
            if !domain.lower.is_finite()
                || !domain.upper.is_finite()
                || domain.lower > domain.upper
                || domain.contract.trim().is_empty()
            {
                return Err(unsupported("invalid division operand domain".into()));
            }
        }
        if domains[1].lower <= 0.0 && domains[1].upper >= 0.0 {
            return Err(unsupported("denominator domain includes zero".into()));
        }
        // Keep the true and perturbed quotients finite and out of the
        // subnormal range, where Float64 division loses relative accuracy.
        // A nonzero numerator interval touching zero cannot prove this.
        let num = &domains[0];
        if num.lower <= 0.0 && num.upper >= 0.0 && (num.lower != 0.0 || num.upper != 0.0) {
            return Err(unsupported(
                "numerator domain cannot exclude underflow near zero".into(),
            ));
        }
        for n in [num.lower, num.upper] {
            for d in [domains[1].lower, domains[1].upper] {
                for nf in [1.0 - numerator, 1.0 + numerator] {
                    for df in [1.0 - denominator, 1.0 + denominator] {
                        let quotient = (n * nf) / (d * df);
                        if !(n * nf).is_finite()
                            || !(d * df).is_finite()
                            || !quotient.is_finite()
                            || (n != 0.0 && quotient.abs() < f64::MIN_POSITIVE)
                        {
                            return Err(unsupported(
                                "division may overflow or underflow Float64".into(),
                            ));
                        }
                    }
                }
            }
        }
        Ok(ResultGuarantee {
            metric: ErrorMetric::RelativeValue,
            bound: BoundExpr::Constant {
                value: (numerator + denominator) / (1.0 - denominator),
            },
            failure_probability: ProbabilityExpr::UnionBound {
                terms: inputs
                    .iter()
                    .map(|input| input.failure_probability.clone())
                    .collect(),
            },
            provenance: inputs
                .iter()
                .enumerate()
                .map(|(input_index, guarantee)| GuaranteeSource::ChildGuarantee {
                    input_index,
                    guarantee: Box::new(guarantee.clone()),
                })
                .chain(domains.iter().enumerate().map(|(input_index, domain)| {
                    GuaranteeSource::InputValueDomain {
                        input_index,
                        lower: domain.lower,
                        upper: domain.upper,
                        max_samples: domain.max_samples,
                        contract: domain.contract.clone(),
                    }
                }))
                .chain(std::iter::once(GuaranteeSource::CompositionStep {
                    operator: op.clone(),
                    rule: "relative_division".into(),
                }))
                .collect(),
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

pub(super) fn exact_operation_rule(operation: &ExactOperation) -> Option<CompositionOperator> {
    let ExactOperation::Aggregate { measures, .. } = operation else {
        return None;
    };
    match measures.as_slice() {
        [intent] => crate::function_rules::function_rules(intent).map(|rules| rules.accuracy),
        // The remaining functions are exact over exact samples, but have
        // no definition-backed rule over approximate values yet.
        _ => None,
    }
}

pub(super) fn propagate(
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
        CompositionOperator::CheckedRelativeDivision => {
            if inputs.len() != 2 || local.is_some() || !same_metric(ErrorMetric::RelativeValue) {
                return Err(unsupported(
                    "checked division requires two exact/relative-value operands".into(),
                ));
            }
            let a = inputs[0]
                .bound
                .evaluate()
                .ok_or_else(|| unsupported("unknown numerator bound".into()))?;
            let b = inputs[1]
                .bound
                .evaluate()
                .ok_or_else(|| unsupported("unknown denominator bound".into()))?;
            if !(0.0..1.0).contains(&b) || a < 0.0 || !a.is_finite() {
                return Err(unsupported("invalid relative division bounds".into()));
            }
            Ok(ResultGuarantee {
                metric: ErrorMetric::RelativeValue,
                bound: BoundExpr::Constant {
                    value: (a + b) / (1.0 - b) + 4.0 * f64::EPSILON,
                },
                failure_probability: ProbabilityExpr::UnionBound {
                    terms: inputs
                        .iter()
                        .map(|g| g.failure_probability.clone())
                        .collect(),
                },
                provenance: composed_provenance(
                    op,
                    inputs,
                    &ResultGuarantee::exact("checked floating-point division"),
                    "checked_relative_division_union_bound",
                ),
            })
        }
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
                ErrorMetric::AbsoluteValue => Ok(DefaultAccuracyModel::additive(
                    op,
                    inputs,
                    local,
                    "additive_union_bound",
                )),
                ErrorMetric::RelativeValue => {
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
                            statistic: "values_non_negative".into(),
                        });
                        return Ok(ResultGuarantee {
                            metric: ErrorMetric::RelativeValue,
                            bound: BoundExpr::Unknown {
                                statistic: "values_non_negative".into(),
                            },
                            failure_probability: ProbabilityExpr::Unknown {
                                statistic: "values_non_negative".into(),
                            },
                            provenance,
                        });
                    }
                    Ok(DefaultAccuracyModel::multiplicative(op, inputs, local))
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
            Ok(DefaultAccuracyModel::lipschitz(
                op, *constant, inputs, local,
            ))
        }
        CompositionOperator::ExactSum => DefaultAccuracyModel::exact_sum(op, inputs, stats),
        CompositionOperator::ExactAverage => DefaultAccuracyModel::exact_average(op, inputs, stats),
        CompositionOperator::ExactExtremum => {
            DefaultAccuracyModel::exact_extremum(op, inputs, stats)
        }
        CompositionOperator::ExactDivision => {
            DefaultAccuracyModel::exact_division(op, inputs, stats)
        }
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
            Ok(ResultGuarantee {
                metric: ErrorMetric::TopKMembership,
                bound: if certified {
                    BoundExpr::Zero
                } else {
                    BoundExpr::Unknown {
                        statistic: "topk_membership_margin".into(),
                    }
                },
                failure_probability: delta.map_or_else(
                    || ProbabilityExpr::Unknown {
                        statistic: "topk_interval_failure_probability".into(),
                    },
                    |value| ProbabilityExpr::Constant { value },
                ),
                provenance,
            })
        }
        // An operator this crate does not know has no registered rule.
        _ => Err(unsupported("no registered rule for this operator".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::pre_asap::AggIntent;
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
    fn domain(lower: f64, upper: f64) -> QuantileInputDomain {
        QuantileInputDomain {
            lower,
            upper,
            max_samples: 1000,
            contract: "enforced test population".into(),
        }
    }
    fn with_metric(metric: ErrorMetric, bound: f64) -> ResultGuarantee {
        ResultGuarantee {
            metric,
            ..abs(bound, 0.0)
        }
    }
    #[test]
    fn checked_division_propagates_value_bounds_and_rejects_rank_bounds() {
        let op = CompositionOperator::CheckedRelativeDivision;
        let inputs = [rel(0.01), rel(0.01)];
        let g = DefaultAccuracyModel
            .propagate(&op, &inputs, None, &Default::default())
            .unwrap();
        assert!((g.bound.evaluate().unwrap() - 0.02 / 0.99).abs() < 1e-14);
        let mut rank = inputs[0].clone();
        rank.metric = ErrorMetric::Rank;
        assert!(DefaultAccuracyModel
            .propagate(&op, &[rank.clone(), rank], None, &Default::default())
            .is_err());
        assert!(DefaultAccuracyModel
            .propagate(&op, &[rel(0.01), rel(1.0)], None, &Default::default())
            .is_err());
    }
    #[test]
    fn relative_division_requires_operand_domains() {
        assert!(DefaultAccuracyModel
            .propagate(
                &CompositionOperator::ExactDivision,
                &[rel(0.01), rel(0.01)],
                None,
                &PropagationStats::default()
            )
            .is_err());
    }
    #[test]
    fn relative_division_preserves_asymmetric_bound_and_domain_provenance() {
        for denominator in [domain(1., 10.), domain(-10., -1.)] {
            let stats = PropagationStats {
                division_operand_domains: Some([domain(-20., -2.), denominator]),
                ..Default::default()
            };
            let got = DefaultAccuracyModel
                .propagate(
                    &CompositionOperator::ExactDivision,
                    &[rel(0.02), rel(0.03)],
                    None,
                    &stats,
                )
                .unwrap();
            assert!((got.bound.evaluate().unwrap() - 0.05 / 0.97).abs() < 1e-14);
            assert_eq!(got.failure_probability.evaluate(), Some(0.));
            assert_eq!(
                got.provenance
                    .iter()
                    .filter(|p| matches!(p, GuaranteeSource::InputValueDomain { .. }))
                    .count(),
                2
            );
        }
    }
    #[test]
    fn relative_division_rejects_zero_special_and_extreme_domains() {
        for domains in [
            [domain(1., 2.), domain(0., 0.)],
            [domain(1., 2.), domain(-1., 1.)],
            [domain(f64::NAN, 2.), domain(1., 2.)],
            [domain(1., 2.), domain(1., f64::INFINITY)],
            [domain(1e250, 1e250), domain(1e-250, 1e-250)],
            [domain(1e-250, 1e-250), domain(1e250, 1e250)],
        ] {
            let stats = PropagationStats {
                division_operand_domains: Some(domains),
                ..Default::default()
            };
            assert!(DefaultAccuracyModel
                .propagate(
                    &CompositionOperator::ExactDivision,
                    &[rel(0.01), rel(0.01)],
                    None,
                    &stats
                )
                .is_err());
        }
        let stats = PropagationStats {
            division_operand_domains: Some([domain(1., 2.), domain(1., 2.)]),
            ..Default::default()
        };
        for bound in [1., f64::NAN, f64::INFINITY, -0.1] {
            assert!(DefaultAccuracyModel
                .propagate(
                    &CompositionOperator::ExactDivision,
                    &[rel(0.01), rel(bound)],
                    None,
                    &stats
                )
                .is_err());
        }
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
    fn relative_error_without_sign_knowledge_remains_symbolic() {
        let unknown = DefaultAccuracyModel
            .propagate(
                &CompositionOperator::ApproximateAggregate,
                &[rel(0.1)],
                Some(&rel(0.2)),
                &PropagationStats::default(),
            )
            .unwrap();
        assert!(unknown.has_unknown());
        let signed = DefaultAccuracyModel.propagate(
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
    fn exact_average_has_its_own_absolute_error_rule() {
        let out = DefaultAccuracyModel
            .propagate(
                &CompositionOperator::ExactAverage,
                &[abs(0.25, 0.01)],
                None,
                &PropagationStats {
                    input_row_count: Some(4),
                    ..PropagationStats::default()
                },
            )
            .unwrap();
        assert_eq!(out.metric, ErrorMetric::AbsoluteValue);
        assert_eq!(out.bound.evaluate(), Some(0.25));
        assert_eq!(out.failure_probability.evaluate(), Some(0.04));
    }
    #[test]
    fn counter_functions_have_distinct_definition_rules() {
        let operation = |intent| ExactOperation::Aggregate {
            reduction: asap_types::pre_asap::Reduction::PerEntity,
            measures: vec![intent],
            output_names: vec![],
            having: None,
            filters: vec![],
        };
        assert_eq!(
            DefaultAccuracyModel.exact_operation_rule(&operation(AggIntent::Rate)),
            Some(CompositionOperator::CounterRate)
        );
        assert_eq!(
            DefaultAccuracyModel.exact_operation_rule(&operation(AggIntent::IRate)),
            Some(CompositionOperator::InstantCounterRate)
        );
        assert_eq!(
            DefaultAccuracyModel.exact_operation_rule(&operation(AggIntent::Increase)),
            Some(CompositionOperator::CounterIncrease)
        );
    }
    #[test]
    fn topk_selection_requires_a_separated_margin_certificate() {
        let unknown = DefaultAccuracyModel
            .propagate(
                &CompositionOperator::TopKSelection,
                &[abs(0.1, 0.01)],
                None,
                &PropagationStats::default(),
            )
            .unwrap();
        assert!(unknown.has_unknown());

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

        let partial = DefaultAccuracyModel
            .propagate(
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
        assert_eq!(partial.failure_probability.evaluate(), Some(0.005));

        let invalid_partial = DefaultAccuracyModel.propagate(
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
