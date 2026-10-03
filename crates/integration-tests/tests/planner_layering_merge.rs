//! The window-composition example merges KLL panes through the unified IR and native runtime.
mod physical_common;
use asap_physical_operators::{
    physical_planner::{compile, cut_candidate, InputContract},
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

/// Build five one-minute KLLs, merge them, then read p99 through the real wire compiler.
#[test]
fn five_minute_quantile_merges_five_one_minute_states() {
    let family = FieldDataType::Sketch(
        SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
        Default::default(),
    );
    let states = (0..5)
        .map(|pane| {
            let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
                source: Source::Table {
                    table_ref: format!("pane_{pane}"),
                },
                predicates: vec![],
                schema: Schema::new(vec![Field::plain("value", DataType::Float64, false)]),
            }))
            .unwrap();
            OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
                child: scan,
                family: family.clone(),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::by(vec![]),
                grouping: GroupingStrategy::default(),
                filter: None,
            }))
            .unwrap()
        })
        .collect();
    let merged =
        OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge { children: states }))
            .unwrap();
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
    for node in &wire.nodes {
        if let asap_types::ir::export::PostAsapOperatorPayload::Relational {
            operator:
                asap_types::ir::export::NonASAPOpKind::Scan {
                    source: Source::Table { table_ref },
                    ..
                },
        } = &node.payload
        {
            let pane: usize = table_ref.strip_prefix("pane_").unwrap().parse().unwrap();
            let schema = Arc::new(node.output_schema.clone());
            let id = u64::from(node.id.0);
            inputs.insert(id, InputContract::bounded(schema.clone()));
            batches.insert(
                id,
                Batch::try_new(
                    schema,
                    (0..20)
                        .map(|i| vec![Value::Float64((pane * 20 + i) as f64)])
                        .collect(),
                )
                .unwrap(),
            );
        }
    }
    let plan = compile(&wire, inputs, &[u64::from(wire.root.0)]).unwrap();
    let outputs = physical_common::execute(
        &plan,
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
    let materialized = cut_candidate(&plan, &frontier).unwrap();
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
        .map(|(id, mut batches)| {
            assert_eq!(batches.len(), 1);
            (id, batches.remove(0))
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
    assert_eq!(retained_outputs[0][0].rows().len(), 1);
    assert!(
        matches!(retained_outputs[0][0].rows()[0].as_slice(), [Value::Float64(value)] if *value == 98.0)
    );
    assert_eq!(outputs[0][0].rows().len(), 1);
    assert!(
        matches!(outputs[0][0].rows()[0].as_slice(), [Value::Float64(value)] if *value == 98.0),
        "{:?}",
        outputs[0][0].rows()
    );
}
