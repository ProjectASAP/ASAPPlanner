//! What a summary is updated with and the statistics read from it.

use serde::{Deserialize, Serialize};

use crate::ir::operator::AggIntent;
use crate::ir::scalar::ColumnRef;

/// One summary-state update. `item` identifies a keyed item when the family
/// is keyed; `weight` is the observation/update expression applied to state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryUpdate {
    pub item: Option<SummaryInputExpr>,
    pub weight: SummaryInputExpr,
}

impl SummaryUpdate {
    pub fn column(column: ColumnRef) -> Self {
        Self {
            item: None,
            weight: SummaryInputExpr::Column(column),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityIdentity {
    PromqlLabelSet { excluding: Vec<ColumnRef> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SummaryInputExpr {
    Constant(f64),
    Column(ColumnRef),
    Tuple(Vec<SummaryInputExpr>),
    EntityIdentity(EntityIdentity),
}

/// The statistic computed from summary state by `SummaryEstimate`, for example
/// `Quantile { q: 0.99 }`. This is a result operation, not a workload query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SketchStatistic {
    /// sqrt(sum_v frequency(v)^2), not the norm of numeric input values.
    FrequencyL2,
    /// Shannon entropy of the value-frequency distribution, in bits.
    FrequencyEntropy,
    /// Extract the value at quantile rank `q` ∈ (0, 1].
    Quantile { q: f64 },
    /// Estimated count / frequency. `key` names which column is being
    /// queried (`ColumnRef::SampleValue` for the bare bucket total, with
    /// `value: None`); a `Named`/`Qualified` `key` paired with
    /// `value: Some(v)` is a per-item point lookup (e.g.
    /// `count(cms_metric{item="checkout"})` — `key` is `item`, `value` is
    /// `"checkout"`). `value` is carried here rather than resolved by the
    /// `SummaryExecutor` from a `Filter` predicate because a summary
    /// evaluation is read without access to the rest of the DAG.
    PointCount {
        key: ColumnRef,
        value: Option<String>,
    },
    /// Estimated number of distinct elements.
    Cardinality,
    /// Read the top-k entries from a heap-backed summary.
    TopK { k: usize },
}

impl SketchStatistic {
    /// The statistic a summary is read for to answer `intent`, or `None`
    /// when no sketch statistic serves the intent.
    pub fn for_intent(intent: &AggIntent) -> Option<Self> {
        Some(match intent {
            AggIntent::Quantile { q, .. } => Self::Quantile { q: *q },
            AggIntent::Cardinality { .. } => Self::Cardinality,
            AggIntent::FrequencyL2 { .. } => Self::FrequencyL2,
            AggIntent::FrequencyEntropy { .. } => Self::FrequencyEntropy,
            AggIntent::TopK { k, .. } => Self::TopK { k: *k },
            AggIntent::Count => Self::PointCount {
                key: ColumnRef::SampleValue,
                value: None,
            },
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyed_summary_input_and_topk_evaluation_round_trip() {
        for input in [
            SummaryUpdate {
                item: Some(SummaryInputExpr::EntityIdentity(
                    EntityIdentity::PromqlLabelSet { excluding: vec![] },
                )),
                weight: SummaryInputExpr::Constant(1.0),
            },
            SummaryUpdate {
                item: Some(SummaryInputExpr::Tuple(vec![SummaryInputExpr::Column(
                    ColumnRef::Named("service".into()),
                )])),
                weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
            },
        ] {
            let input_json = serde_json::to_string(&input).unwrap();
            assert_eq!(
                serde_json::from_str::<SummaryUpdate>(&input_json).unwrap(),
                input
            );
            let query = SketchStatistic::TopK { k: 10 };
            let json = serde_json::to_string(&query).unwrap();
            assert_eq!(
                serde_json::from_str::<SketchStatistic>(&json).unwrap(),
                query
            );
        }
    }

    /// The summary-sample rename keeps its JSON name, so committed plans
    /// and workloads still read.
    #[test]
    fn renamed_sample_terms_keep_their_wire_names() {
        use crate::ir::schema::FieldDataType;
        use crate::summary_type::{SamplingKind, SamplingParams};
        let json = |value: serde_json::Value| value.to_string();
        let summary_sample = FieldDataType::SummarySample(
            SamplingKind::Reservoir,
            SamplingParams::Reservoir { size: 64 },
        );
        assert_eq!(
            serde_json::to_string(&summary_sample).unwrap(),
            json(serde_json::json!({"Sample": ["Reservoir", {"Reservoir": {"size": 64}}]}))
        );
    }
}
