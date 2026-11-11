//! The statistic a summary is read for.

use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::ColumnRef;
use asap_types::summary_type::SketchStatistic;

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
