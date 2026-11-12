use super::*;

#[test]
fn topk_statistic_round_trips() {
    let query = SketchStatistic::TopK { k: 10 };
    let json = serde_json::to_string(&query).unwrap();
    assert_eq!(
        serde_json::from_str::<SketchStatistic>(&json).unwrap(),
        query
    );
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
