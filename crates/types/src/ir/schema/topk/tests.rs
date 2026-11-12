use std::rc::Rc;
use std::time::Duration;

use crate::ir::operator::arguments::{Reduction, Source};
use crate::ir::operator::AggIntent;
use crate::ir::scalar::ColumnRef;
use crate::ir::schema::series_identity::with_promql_series_identity;
use crate::ir::schema::{DataType, Field, FieldDataType, Schema};
use crate::ir::{
    ASAPOp, NonASAPOp, Operator, OperatorNode, ProjectItem, ScalarExpr, TimeRangeKind,
};
use crate::summary_type::{SketchAlgorithm, SketchStatistic, SummaryUpdate};

fn node(operator: Operator) -> Rc<OperatorNode> {
    OperatorNode::new_shared(operator).unwrap()
}

/// `topk by (job) (10, sum_over_time(m[1m]))` over series whose identity
/// is realized: the exact top-k and a heap's ranked readout of the same
/// rows (a `Project` naming the estimate's columns as the top-k's)
/// derive the same unique keys, one row per (job, series).
#[test]
fn exact_topk_and_heap_readout_derive_the_same_unique_keys() {
    let open = Schema::with_time_index(
        vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("value", DataType::Float64, false),
            Field::plain("job", DataType::Utf8, true),
        ],
        0,
        vec![],
    );
    let rows = with_promql_series_identity(&node(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "m".into() },
        predicates: vec![],
        schema: open,
    })))
    .unwrap();
    let window = node(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_secs(60),
        kind: TimeRangeKind::Range,
        child: rows,
    }));
    let per_series = node(Operator::NonASAP(NonASAPOp::Aggregate {
        child: Rc::clone(&window),
        reduction: Reduction::PerEntity,
        measures: vec![AggIntent::Sum { col: None }],
        output_names: vec![String::new()],
        filters: vec![None],
        having: None,
    }));
    let exact = node(Operator::NonASAP(NonASAPOp::Aggregate {
        child: per_series,
        reduction: Reduction::by(vec![2]),
        measures: vec![AggIntent::TopK { k: 10 }],
        output_names: vec![String::new()],
        filters: vec![None],
        having: None,
    }));
    let heap = node(Operator::ASAP(ASAPOp::SummaryAgg {
        summary_type: FieldDataType::Sketch(SketchAlgorithm::CountSketchWithHeap),
        input: SummaryUpdate {
            item: Some(super::series_identity_item(&window.schema, &[2])),
            weight: crate::summary_type::SummaryInputExpr::Column(ColumnRef::SampleValue),
        },
        reduction: Reduction::by(vec![2]),
        filter: None,
        child: window,
    }));
    let estimate = node(Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: heap,
        query: SketchStatistic::TopK { k: 10 },
    }));
    let estimated = estimate.schema.fields.len() - 1;
    let cols = exact
        .schema
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| ProjectItem {
            alias: Some(field.name.clone()),
            expr: ScalarExpr::Column(if index == 1 {
                estimated
            } else {
                estimate
                    .schema
                    .fields
                    .iter()
                    .position(|input| input.name == field.name)
                    .unwrap()
            }),
        })
        .collect();
    let readout = node(Operator::NonASAP(NonASAPOp::Project {
        cols,
        child: estimate,
    }));
    assert!(!exact.schema.unique_keys.is_empty());
    assert_eq!(exact.schema.unique_keys, readout.schema.unique_keys);
}
