//! Sizing: the target an approximate-capable intent's summary is sized
//! against, its `(ε, δ)` budget, the default sketch parameters for it, a
//! family sized for one equal share of that target, and the failure
//! probability each approximate input needs over the values the target
//! covers.
use std::rc::Rc;

use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{
    FieldDataType, GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SketchStatistic,
};
use asap_types::ir::{OperatorNode, QueryRoot, SchemaDerivationError};
use asap_types::workload::bounds::{series_read, values_per_result};
use asap_types::workload::DataWorkload;

use crate::estimators::{hll, local_guarantee, with_failure_probability};
use crate::per_input_target;
use crate::propagation::{approximate_inputs, distinct_bound_source, with_resized_summaries};

/// Confidence δ assumed when the target carries only an ε
/// (`AccuracyTarget::epsilon`): the (ε, δ)-parameterised sketches (CMS) need
/// one. `ln(1/0.01) → depth 5`, matching the conventional CMS sizing.
pub(crate) const DEFAULT_DELTA: f64 = 0.01;

/// The statistic a summary is read for to answer `intent`, or `None` when
/// no sketch statistic serves the intent.
pub fn sketch_statistic(intent: &AggIntent) -> Option<SketchStatistic> {
    Some(match intent {
        AggIntent::Quantile { q, .. } => SketchStatistic::Quantile { q: *q },
        AggIntent::Cardinality { .. } => SketchStatistic::Cardinality,
        AggIntent::FrequencyL2 { .. } => SketchStatistic::FrequencyL2,
        AggIntent::FrequencyEntropy { .. } => SketchStatistic::FrequencyEntropy,
        AggIntent::TopK { k, .. } => SketchStatistic::TopK { k: *k },
        AggIntent::Count { .. } => SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        },
        _ => return None,
    })
}

/// The [`AccuracyTarget`] an approximate-capable intent's summary is sized
/// and checked against, or `None` for every other intent (no sketch
/// candidate applies): the query's target restated in the intent's own metric
/// (#690 §4): an L1-scaled absolute error for counts and top-k frequencies,
/// a relative error for distinct counts and L2 frequency moments, an
/// absolute error for entropy, and for a quantile the query's metric or a
/// rank error when none is set. The query's own metric, set or not, is
/// checked at its root in Stage 3.
pub fn sizing_target(intent: &AggIntent) -> Option<AccuracyTarget> {
    let (target, metric) = match intent {
        AggIntent::Quantile { accuracy, .. } => (
            accuracy,
            accuracy.accuracy_metric.unwrap_or(AccuracyMetric::Rank),
        ),
        AggIntent::Cardinality { accuracy, .. } | AggIntent::FrequencyL2 { accuracy, .. } => {
            (accuracy, AccuracyMetric::Relative)
        }
        AggIntent::FrequencyEntropy { accuracy, .. } => (accuracy, AccuracyMetric::ABSOLUTE),
        AggIntent::Count { accuracy } | AggIntent::TopK { accuracy, .. } => {
            (accuracy, AccuracyMetric::ABSOLUTE_STREAM_L1)
        }
        _ => return None,
    };
    Some(target_in(target, metric))
}

/// `target`'s ε, δ and scope stated in `metric`. An exact target stays
/// exact with no metric: exact is exact in every metric.
fn target_in(target: &AccuracyTarget, metric: AccuracyMetric) -> AccuracyTarget {
    if target.is_exact() {
        return AccuracyTarget::exact().accuracy_scope(target.accuracy_scope);
    }
    target.clone().accuracy_metric(metric)
}

/// Resolve an [`AccuracyTarget`] into the `(eps, delta)` budget
/// `default_size_params` needs — one place this resolution happens, so
/// nothing can drift apart on it.
///
/// An exact target degrades to the tightest parameters for a caller that
/// resolves it anyway.
pub fn accuracy_budget(accuracy: &AccuracyTarget) -> (f64, f64) {
    if accuracy.is_exact() {
        return (f64::MIN_POSITIVE, DEFAULT_DELTA);
    }
    (
        accuracy.max_error,
        accuracy.failure_probability.unwrap_or(DEFAULT_DELTA),
    )
}

/// `asap-plan`'s built-in `SketchParams` sizing, keyed off the resolved
/// `(eps, delta)` accuracy budget.
///
/// Each formula inverts the sketch family's standard error bound to the
/// smallest parameter satisfying the target, clamped to the family's sane
/// range. A non-positive ε saturates to the clamp maximum (tightest
/// allowed).
pub(crate) fn default_size_params(
    kind: SketchAlgorithm,
    intent: &AggIntent,
    eps: f64,
    delta: f64,
) -> SketchParams {
    crate::estimators::size_params(kind, intent, eps, delta)
}

/// `algorithm` sized for one of `share` equal shares of `intent`'s accuracy
/// requirement ([`per_input_target`]), split in the metric of the summary's own
/// bound: a relative error splits so products and quotients of the shares
/// meet it.
pub fn sized(algorithm: &SketchAlgorithm, intent: &AggIntent, share: usize) -> SketchKind {
    let target = sizing_target(intent).unwrap_or_else(AccuracyTarget::exact);
    let size = |target: &AccuracyTarget| {
        let (epsilon, delta) = accuracy_budget(target);
        SketchKind::new(
            *algorithm,
            default_size_params(*algorithm, intent, epsilon, delta),
        )
    };
    let whole = size(&target);
    if share <= 1 {
        return whole;
    }
    let metric = sketch_statistic(intent)
        .and_then(|query| {
            local_guarantee(
                &FieldDataType::Sketch(whole.clone(), GroupingStrategy::default()),
                &query,
            )
        })
        .map(|guarantee| guarantee.spec.accuracy_metric)
        .or(target.accuracy_metric);
    // The split needs a concrete metric. A summary without an error model
    // whose intent's target sets none is not split (never a guessed
    // metric): it stays sized for the whole target, and Stage 3 rejects the
    // candidate if its composed output then misses the query's target.
    let Some(metric) = metric else {
        debug_assert!(
            target.is_exact(),
            "sizing {algorithm:?} for a share of an approximate target without a metric"
        );
        return whole;
    };
    per_input_target(&target, metric, share).map_or(whole, |share| size(&share))
}

/// The failure probability each of the `budget_parts` approximate inputs
/// of query `root`'s value is sized for, so that the value meets `target`
/// over every value the target covers: `δ / (n · V)`, the query's `δ`
/// shared equally by `n = budget_parts` inputs and union-bounded over the
/// `V` values its scope covers
/// ([`AccuracyScope::max_values_covered`](asap_types::ir::properties::AccuracyScope::max_values_covered):
/// 1 for a per-value target, at most `⌊1 / share⌋` for one over the groups
/// holding a record share, and `G` ([`values_per_result`]) for the whole
/// result). `None` for an exact target, one without a positive failure
/// probability, and a whole-result target whose `G` is unbound: its target
/// then stays unmet, and plan selection rejects the plan.
pub fn per_input_failure_probability(
    target: &AccuracyTarget,
    budget_parts: usize,
    root: &QueryRoot,
    data: &DataWorkload,
) -> Option<f64> {
    let delta = target
        .failure_probability
        .filter(|delta| !target.is_exact() && *delta > 0.0)?;
    let values = target.accuracy_scope.max_values_covered(values_per_result(
        root,
        series_read(root, data),
        data,
    ))?;
    Some(delta / (budget_parts.max(1) as f64 * values as f64))
}

/// Sketch summary `state`'s `params` sized for failure probability
/// `failure_probability`: the parameters that depend on it enlarged
/// ([`with_failure_probability`]), and classic HLL's precision from the
/// distinct bound of the source it counts (keeping the relative error it
/// was sized for). Never smaller than `params`. `None` keeps `params`: an
/// HLL whose source declares no distinct bound.
fn sized_for_failure_probability(
    state: &OperatorNode,
    params: &SketchParams,
    failure_probability: f64,
    data: &DataWorkload,
) -> Option<SketchParams> {
    Some(match params {
        SketchParams::Hll {
            precision,
            relative_error,
        } => {
            let source = distinct_bound_source(state)?;
            let bound = data.sources.get(&source)?.max_distinct_per_evaluation?;
            let relative_error = hll::classic_relative_error(*precision, *relative_error);
            SketchParams::Hll {
                precision: hll::min_precision(
                    relative_error,
                    failure_probability,
                    bound,
                    *precision,
                ),
                relative_error: Some(relative_error),
            }
        }
        params => with_failure_probability(params, failure_probability),
    })
}

/// `roots` with every approximate input of their values
/// ([`approximate_inputs`]) sized for `failure_probability`
/// (`sized_for_failure_probability`) and the guarantees above composed
/// again ([`with_resized_summaries`]). A summary whose output carries no
/// guarantee (e.g. a top-k's heap, which bounds item frequencies, not
/// membership) is no approximate input and keeps its size: its failure
/// probability bounds nothing the target checks. The same `Rc` where
/// nothing changes.
pub fn with_summaries_sized_for_failure_probability(
    roots: &[QueryRoot],
    failure_probability: f64,
    data: &DataWorkload,
) -> Result<Vec<QueryRoot>, SchemaDerivationError> {
    let inputs: std::collections::HashSet<*const OperatorNode> = roots
        .iter()
        .flat_map(approximate_inputs)
        .map(|state| Rc::as_ptr(&state))
        .collect();
    if inputs.is_empty() {
        return Ok(roots.to_vec());
    }
    with_resized_summaries(roots, &|state, params| {
        inputs
            .contains(&std::ptr::from_ref(state))
            .then(|| sized_for_failure_probability(state, params, failure_probability, data))
            .flatten()
    })
}

/// Whole-workload `roots` (query `i`'s root `roots[i]` with target
/// `targets[i]`), composed outside the memo, with their summaries sized as
/// Pass 1 sizes a memo candidate: each approximate input of query `i`'s
/// value, `n_i` in all ([`approximate_inputs`]), for
/// [`per_input_failure_probability`] `δ_i / (n_i · V_i)`, a summary several
/// roots read for the smallest among them, and the guarantees above
/// composed again. For plans built by hand (tests, the fuzzer's witnesses);
/// the planner sizes its candidates in Pass 1.
pub fn with_summaries_sized_for_targets(
    roots: &[QueryRoot],
    targets: &[AccuracyTarget],
    data: &DataWorkload,
) -> Result<Vec<QueryRoot>, SchemaDerivationError> {
    let mut required: std::collections::HashMap<*const OperatorNode, f64> =
        std::collections::HashMap::new();
    for (root, target) in roots.iter().zip(targets) {
        let inputs = approximate_inputs(root);
        let Some(failure_probability) =
            per_input_failure_probability(target, inputs.len(), root, data)
        else {
            continue;
        };
        for state in &inputs {
            let entry = required
                .entry(Rc::as_ptr(state))
                .or_insert(failure_probability);
            *entry = entry.min(failure_probability);
        }
    }
    if required.is_empty() {
        return Ok(roots.to_vec());
    }
    with_resized_summaries(roots, &|state, params| {
        let failure_probability = *required.get(&std::ptr::from_ref(state))?;
        sized_for_failure_probability(state, params, failure_probability, data)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CMS width is `⌈e/ε⌉` and depth `⌈ln(1/δ)⌉`, with an ε-only target
    /// taking `DEFAULT_DELTA` (depth 5).
    #[test]
    fn epsilon_delta_sizes_cms_depth() {
        let intent = AggIntent::Count {
            accuracy: AccuracyTarget::max_error(0.001).failure_probability(0.001),
        };
        assert_eq!(
            default_size_params(SketchAlgorithm::Cms, &intent, 0.001, 0.001),
            SketchParams::Cms {
                width: 2719,
                depth: 7
            } // ⌈e/0.001⌉, ⌈ln 1000⌉
        );
        let target = AccuracyTarget::max_error(0.001);
        let (eps, delta) = accuracy_budget(&target);
        assert_eq!((eps, delta), (0.001, DEFAULT_DELTA));
        let intent = AggIntent::Count { accuracy: target };
        assert_eq!(
            default_size_params(SketchAlgorithm::Cms, &intent, eps, delta),
            SketchParams::Cms {
                width: 2719,
                depth: 5
            }
        );
        assert_eq!(
            default_size_params(SketchAlgorithm::Cms, &intent, 0.01, 0.01),
            SketchParams::Cms {
                width: 272,
                depth: 5
            }
        );
    }
}
