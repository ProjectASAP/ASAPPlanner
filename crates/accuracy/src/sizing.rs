//! Sizing: the target an approximate-capable intent's summary is sized
//! against, its `(ε, δ)` budget, the default sketch parameters for it, a
//! family sized for one equal share of that target, and the failure
//! probability each approximate input needs over the values the target
//! covers.

use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::SketchStatistic;

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
/// today's sizing needs — one place this resolution happens, so
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

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::schema::{SketchAlgorithm, SketchParams};

    /// CMS width is `⌈e/ε⌉` and depth `⌈ln(1/δ)⌉`, with an ε-only target
    /// taking `DEFAULT_DELTA` (depth 5).
    #[test]
    fn epsilon_delta_sizes_cms_depth() {
        let intent = AggIntent::Count {
            accuracy: AccuracyTarget::max_error(0.001).failure_probability(0.001),
        };
        assert_eq!(
            crate::estimators::size_params(SketchAlgorithm::Cms, &intent, 0.001, 0.001),
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
            crate::estimators::size_params(SketchAlgorithm::Cms, &intent, eps, delta),
            SketchParams::Cms {
                width: 2719,
                depth: 5
            }
        );
        assert_eq!(
            crate::estimators::size_params(SketchAlgorithm::Cms, &intent, 0.01, 0.01),
            SketchParams::Cms {
                width: 272,
                depth: 5
            }
        );
    }
}
