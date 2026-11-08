//! Sizing: the statistic a summary is read for, and the default sketch
//! parameters for an `(ε, δ)`.

use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{SketchAlgorithm, SketchParams, SketchStatistic};

/// The statistic a summary is read for to answer `intent`, or `None` when
/// no sketch statistic serves the intent.
pub fn sketch_statistic(intent: &AggIntent) -> Option<SketchStatistic> {
    Some(match intent {
        AggIntent::Quantile { q, .. } => SketchStatistic::Quantile { q: *q },
        AggIntent::Cardinality { .. } => SketchStatistic::Cardinality,
        AggIntent::FrequencyL2 { .. } => SketchStatistic::FrequencyL2,
        AggIntent::FrequencyEntropy { .. } => SketchStatistic::FrequencyEntropy,
        AggIntent::TopK { k, .. } => SketchStatistic::TopK { k: *k },
        AggIntent::Count => SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        },
        _ => return None,
    })
}

/// `asap-plan`'s built-in `SketchParams` sizing, keyed off the resolved
/// `(eps, delta)` accuracy budget.
///
/// Each formula inverts the sketch family's standard error bound to the
/// smallest parameter satisfying the target, clamped to the family's sane
/// range. A non-positive ε saturates to the clamp maximum (tightest
/// allowed).
#[allow(dead_code)] // Read by `test_support` until #1207 PR 1 replaces it with `parameter_list`.
pub(crate) fn default_size_params(
    kind: SketchAlgorithm,
    intent: &AggIntent,
    eps: f64,
    delta: f64,
) -> SketchParams {
    crate::estimators::size_params(kind, intent, eps, delta)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CMS width is `⌈e/ε⌉` and depth `⌈ln(1/δ)⌉`.
    #[test]
    fn epsilon_delta_sizes_cms_depth() {
        let intent = AggIntent::Count;
        assert_eq!(
            default_size_params(SketchAlgorithm::Cms, &intent, 0.001, 0.001),
            SketchParams::Cms {
                width: 2719,
                depth: 7
            } // ⌈e/0.001⌉, ⌈ln 1000⌉
        );
        assert_eq!(
            default_size_params(SketchAlgorithm::Cms, &intent, 0.001, 0.01),
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
