//! Sizing: the target an approximate-capable intent's summary is sized
//! against, its `(ε, δ)` budget, the default sketch parameters for it, and
//! a family sized for one equal share of that target.
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{
    FieldDataType, GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SketchStatistic,
};

use crate::estimators::local_guarantee;
use crate::per_input_target;

/// Confidence δ assumed when the target carries only an ε
/// (`AccuracyTarget::epsilon`): the (ε, δ)-parameterised sketches (CMS) need
/// one. `ln(1/0.01) → depth 5`, matching the conventional CMS sizing.
pub const DEFAULT_DELTA: f64 = 0.01;

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
/// [`default_size_params`] needs — one place this resolution happens, so
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
pub fn default_size_params(
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
