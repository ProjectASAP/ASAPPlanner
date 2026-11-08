//! The statistic a summary is read for. (Today's sizing for an `(ε, δ)` is
//! test-only, `estimators::size_params`: the predefined lists are checked
//! against it.)

use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::SketchStatistic;

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

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::schema::{SketchAlgorithm, SketchParams};

    /// CMS width is `⌈e/ε⌉` and depth `⌈ln(1/δ)⌉`.
    #[test]
    fn epsilon_delta_sizes_cms_depth() {
        let intent = AggIntent::Count;
        assert_eq!(
            crate::estimators::size_params(SketchAlgorithm::Cms, &intent, 0.001, 0.001),
            SketchParams::Cms {
                width: 2719,
                depth: 7
            } // ⌈e/0.001⌉, ⌈ln 1000⌉
        );
        assert_eq!(
            crate::estimators::size_params(SketchAlgorithm::Cms, &intent, 0.001, 0.01),
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
