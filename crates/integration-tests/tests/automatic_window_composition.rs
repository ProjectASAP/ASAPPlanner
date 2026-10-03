//! Automatically generated panes execute directly and through an enumerated materialization frontier.
mod physical_common;
use asap_physical_operators::{
    physical_planner::{compile_materialization_candidates, InputContract},
    runtime::Scope,
    values::{Batch, Value},
};
use asap_types::{
    ir::operator_properties::{Reduction, Source},
    ir::{ASAPOp, NonASAPOp, Operator, OperatorNode},
    post_asap::{
        GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SketchStatistic, SummaryUpdate,
    },
    pre_asap::{ColumnRef, DataType, Field, FieldDataType, Schema},
};
use std::{collections::BTreeMap, sync::Arc};

/// Generate panes from one five-minute state and find its producer/reader split automatically.
#[test]
fn automatic_panes_and_materialization_preserve_quantile() {
    execute_generated_panes(false);
}

/// Per-series PromQL panes preserve temporal metadata through the native wire compiler.
#[test]
fn automatic_per_series_panes_preserve_quantile() {
    execute_generated_panes(true);
}

fn execute_generated_panes(per_series: bool) {
    let family = FieldDataType::Sketch(
        SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
        Default::default(),
    );
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "events".into(),
        },
        predicates: vec![],
        schema: if per_series {
            Schema::lifted(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("value", DataType::Float64, false),
                ],
                Some(0),
            )
        } else {
            Schema::new(vec![Field::plain("value", DataType::Float64, false)])
        },
    }))
    .unwrap();
    let range = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: std::time::Duration::from_secs(300),
        kind: asap_types::ir::TimeRangeKind::Range,
        child: scan,
    }))
    .unwrap();
    let state = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: range,
        family,
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: if per_series {
            Reduction::PerEntity
        } else {
            Reduction::by(vec![])
        },
        grouping: GroupingStrategy::default(),
        filter: None,
    }))
    .unwrap();
    let entry = asap_types::workload::QueryWorkloadEntry {
        query: asap_types::workload::Query("quantile_over_time(0.99, events[5m])".into()),
        recurrence: asap_types::workload::QueryRecurrence::Repeated(
            asap_types::workload::RepeatedDemand::FixedInterval(
                asap_types::workload::RepetitionInterval(60_000),
            ),
        ),
        requirements: Default::default(),
        predictability: asap_types::workload::Predictability::AdHoc,
        time_selection: Default::default(),
    };
    let variants =
        asap_aware_mapping::window_composition::enumerate_window_compositions(&state, &entry, 64)
            .unwrap();
    assert_eq!(variants.len(), 2);
    let merged = variants[1].clone();
    let root = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: merged,
        query: SketchStatistic::Quantile { q: 0.99 },
    }))
    .unwrap();
    root.validate_structure().unwrap();
    let wire = physical_common::compile_post_asap_dag(&root).unwrap();
    wire.validate().unwrap();
    let mut inputs = BTreeMap::new();
    let mut batches = BTreeMap::new();
    for (pane, node) in wire
        .nodes
        .iter()
        .filter(|node| {
            matches!(
                node.payload,
                asap_types::ir::export::PostAsapOperatorPayload::Relational {
                    operator: asap_types::ir::export::NonASAPOpKind::TimeRange { .. }
                }
            )
        })
        .enumerate()
    {
        let schema = Arc::new(node.output_schema.clone());
        let id = u64::from(node.id.0);
        inputs.insert(id, InputContract::bounded(schema.clone()));
        batches.insert(
            id,
            Batch::try_new(
                schema,
                (0..20)
                    .map(|i| {
                        if per_series {
                            vec![
                                Value::Timestamp((pane * 20 + i) as i64 * 1000),
                                Value::Float64((pane * 20 + i) as f64),
                            ]
                        } else {
                            vec![Value::Float64((pane * 20 + i) as f64)]
                        }
                    })
                    .collect(),
            )
            .unwrap(),
        );
    }
    let choices =
        compile_materialization_candidates(&wire, inputs.clone(), &[u64::from(wire.root.0)], 1024)
            .unwrap();
    assert!(
        compile_materialization_candidates(&wire, inputs, &[u64::from(wire.root.0)], 1).is_err()
    );
    let plan = &choices
        .iter()
        .find(|candidate| candidate.frontier.is_empty())
        .unwrap()
        .realization
        .as_ref()
        .unwrap()
        .query;
    let outputs = physical_common::execute(
        plan,
        batches.clone(),
        Scope::Query {
            evaluation_time_ms: 300_000,
            revision: 1,
        },
    );
    // Pattern B1 stores five built panes; its query reads the same merge DAG
    // as Pattern B2, which builds every pane from raw rows on each read.
    let frontier: Vec<_> = wire
        .nodes
        .iter()
        .filter_map(|node| {
            matches!(
                node.payload,
                asap_types::ir::export::PostAsapOperatorPayload::SummaryAgg { .. }
            )
            .then_some(u64::from(node.id.0))
        })
        .collect();
    let materialized = choices
        .iter()
        .find(|candidate| {
            let mut candidate = candidate.frontier.clone();
            let mut frontier = frontier.clone();
            candidate.sort();
            frontier.sort();
            candidate == frontier
        })
        .unwrap()
        .realization
        .as_ref()
        .unwrap();
    let precompute = materialized.precompute.as_ref().unwrap();
    let stored = physical_common::execute(
        precompute,
        batches,
        Scope::Ingestion {
            window_start_ms: 0,
            window_end_ms: 300_000,
            revision: 1,
        },
    );
    let retained = precompute
        .roots()
        .iter()
        .copied()
        .zip(stored)
        .enumerate()
        .map(|(pane, (id, mut batches))| {
            assert_eq!(batches.len(), 1);
            let mut batch = batches.remove(0);
            if per_series {
                // Retained panes can have different build timestamps; the
                // merged answer must carry the query's evaluation timestamp.
                let mut rows = batch.rows().to_vec();
                let time = batch.schema().time_index.unwrap();
                for row in &mut rows {
                    row[time] = Value::Timestamp((pane as i64 + 1) * 60_000);
                }
                batch = Batch::try_new(batch.schema().clone(), rows).unwrap();
            }
            (id, batch)
        })
        .collect();
    let retained_outputs = physical_common::execute(
        &materialized.query,
        retained,
        Scope::Query {
            evaluation_time_ms: 300_000,
            revision: 1,
        },
    );
    if per_series {
        assert!(matches!(
            retained_outputs[0][0].rows()[0][0],
            Value::Timestamp(300_000)
        ));
        assert!(matches!(
            outputs[0][0].rows()[0][0],
            Value::Timestamp(300_000)
        ));
    }
    assert_eq!(retained_outputs[0][0].rows().len(), 1);
    assert!(retained_outputs[0][0].rows()[0]
        .iter()
        .any(|value| matches!(value, Value::Float64(value) if *value == 98.0)));
    assert_eq!(outputs[0][0].rows().len(), 1);
    assert!(
        outputs[0][0].rows()[0]
            .iter()
            .any(|value| matches!(value, Value::Float64(value) if *value == 98.0)),
        "{:?}",
        outputs[0][0].rows()
    );
}
