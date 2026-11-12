use super::*;
use crate::ir::operator::arguments::Reduction;
use crate::ir::schema::{DataType, Field, Schema};
use crate::ir::{OperatorNode, TimeRangeKind};
use std::rc::Rc;
use std::time::Duration;

/// The raw data samples of `m` reach a node through a range, an
/// `offset`-only time shift and a plain `sum`; a rate ends them.
#[test]
fn raw_data_samples_of_metric_follows_ranges_shifts_and_plain_sums() {
    let node = |operator: Operator<Rc<OperatorNode>>| OperatorNode::new_shared(operator).unwrap();
    let rows = node(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "m".into() },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
            ],
            0,
            vec![],
        ),
    }));
    let window = node(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(60),
        kind: TimeRangeKind::Range,
        child: rows,
    }));
    let per_series = |measure: AggIntent| {
        node(Operator::NonASAP(NonASAPOp::Aggregate {
            child: Rc::clone(&window),
            reduction: Reduction::PerEntity,
            measures: vec![measure],
            output_names: vec![String::new()],
            filters: vec![],
            having: None,
        }))
    };
    assert_eq!(raw_data_samples_of_metric(&window), Some("m".to_string()));
    let shifted = node(Operator::NonASAP(NonASAPOp::TimeShift {
        shift: crate::ir::operator::arguments::TimeShift {
            offset_ms: 60_000,
            at: None,
        },
        child: Rc::clone(&window),
    }));
    assert_eq!(raw_data_samples_of_metric(&shifted), Some("m".to_string()));
    assert_eq!(
        raw_data_samples_of_metric(&per_series(AggIntent::Sum { col: None })),
        Some("m".to_string())
    );
    assert_eq!(
        raw_data_samples_of_metric(&per_series(AggIntent::Rate)),
        None
    );
}

#[test]
fn keyed_summary_input_and_topk_evaluation_round_trip() {
    for input in [
        SummaryUpdate {
            item: Some(SummaryInputExpr::EntityIdentity(
                EntityIdentity::PromqlLabelSet { excluding: vec![] },
            )),
            weight: SummaryInputExpr::Constant(1.0),
            weight_domain: WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::UnitCount,
            },
        },
        SummaryUpdate {
            item: Some(SummaryInputExpr::Tuple(vec![SummaryInputExpr::Column(
                ColumnRef::Named("service".into()),
            )])),
            weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
            weight_domain: WeightDomain::UnknownOrSigned,
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

/// The raw-data-sample and summary-sample renames keep their JSON names,
/// so committed plans and workloads still read.
#[test]
fn renamed_sample_terms_keep_their_wire_names() {
    use crate::ir::schema::FieldDataType;
    use crate::summary_type::{SamplingKind, SamplingParams};
    let json = |value: serde_json::Value| value.to_string();
    let raw = WeightDomain::RawDataSamplesOfMetric {
        metric: "m".to_string(),
    };
    assert_eq!(
        serde_json::to_string(&raw).unwrap(),
        json(serde_json::json!({"kind": "samples_of_metric", "metric": "m"}))
    );
    for (proof, name) in [
        (
            NonNegativeWeightProof::CounterRawDataSamples,
            "counter_samples",
        ),
        (
            NonNegativeWeightProof::DeclaredNonNegativeRawDataSamples,
            "declared_non_negative_samples",
        ),
    ] {
        assert_eq!(
            serde_json::to_string(&proof).unwrap(),
            format!("\"{name}\"")
        );
    }
    let summary_sample = FieldDataType::SummarySample(
        SamplingKind::Reservoir,
        SamplingParams::Reservoir { size: 64 },
    );
    assert_eq!(
        serde_json::to_string(&summary_sample).unwrap(),
        json(serde_json::json!({"Sample": ["Reservoir", {"Reservoir": {"size": 64}}]}))
    );
}
