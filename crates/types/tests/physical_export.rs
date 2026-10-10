//! A timed plan exports as a PhysicalASAPDAG that keeps timing.
use asap_types::ir::physical_export::{
    compile_physical_asap_dag, PhysicalASAPDAGDocument, PhysicalASAPDAGValidationError,
};
use asap_types::ir::{
    apply_lifecycle_timings, ASAPOp, LifecycleAssignment, NonASAPOp, Operator, OperatorNode,
    TimingMemo,
};
use asap_types::post_asap::{ExactKind, ExactParams, ExecutionTiming, SummaryUpdate};
use asap_types::pre_asap::{ColumnRef, DataType, Field, FieldDataType, Reduction, Schema, Source};
use std::rc::Rc;

/// Scan(t) → SummaryAgg(sum by key) → FinalizeExactAccumulator, untimed.
fn plan() -> Rc<OperatorNode> {
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "t".into(),
        },
        predicates: vec![],
        schema: Schema::new(vec![
            Field::plain("key", DataType::Utf8, false),
            Field::plain("value", DataType::Float64, false),
        ]),
    }))
    .unwrap();
    let state = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryAgg {
        child: scan,
        family: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
        input: SummaryUpdate::column(ColumnRef::Named("value".into())),
        reduction: Reduction::by(vec![0]),
        grouping: Default::default(),
        filter: None,
    }))
    .unwrap();
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
        child: Rc::new(state),
    }))
    .unwrap()
}

/// Default lifecycle: the summary is maintained at ingestion time and read at query time.
#[test]
fn timed_plan_exports_with_timing() {
    let timed = apply_lifecycle_timings(
        &plan(),
        &LifecycleAssignment::default_maintained(),
        &mut TimingMemo::new(),
    )
    .unwrap();
    let dag = compile_physical_asap_dag(&timed).unwrap();
    let document = PhysicalASAPDAGDocument::new(dag.clone());
    document.validate().unwrap();

    let timings: Vec<_> = dag.nodes.iter().map(|n| n.output_state.timing).collect();
    assert_eq!(
        timings,
        vec![
            ExecutionTiming::IngestionTime,
            ExecutionTiming::IngestionTime,
            ExecutionTiming::QueryTime
        ]
    );
    // Timing keeps what the summary covers.
    assert!(timed.children()[0].coverage().is_some());

    let decoded: PhysicalASAPDAGDocument =
        serde_json::from_str(&serde_json::to_string(&document).unwrap()).unwrap();
    assert_eq!(decoded, document);
}

/// A query-time producer cannot feed an ingestion-time consumer.
#[test]
fn query_time_input_to_ingestion_is_rejected() {
    let timed = apply_lifecycle_timings(
        &plan(),
        &LifecycleAssignment::default_maintained(),
        &mut TimingMemo::new(),
    )
    .unwrap();
    let mut dag = compile_physical_asap_dag(&timed).unwrap();
    dag.nodes[0].output_state.timing = ExecutionTiming::QueryTime;
    dag.edges[0].data_state = dag.nodes[0].output_state;
    assert!(matches!(
        dag.validate(),
        Err(PhysicalASAPDAGValidationError::QueryDependencyInIngestion { .. })
    ));
}

/// Untimed plans cannot be exported as physical plans.
#[test]
fn untimed_plan_is_rejected() {
    assert!(compile_physical_asap_dag(&plan()).is_err());
}

/// Two queries reading one summary state export once, with one root per query.
#[test]
fn batch_shares_the_summary_and_keeps_one_root_per_query() {
    use asap_types::ir::physical_export::compile_physical_asap_workload;
    let first = plan();
    let state = first.children()[0].clone();
    let second = OperatorNode::new_shared(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
        child: state,
    }))
    .unwrap();
    let assignment = LifecycleAssignment::default_maintained();
    let mut memo = TimingMemo::new();
    let timed: Vec<_> = [first, second]
        .iter()
        .map(|root| apply_lifecycle_timings(root, &assignment, &mut memo).unwrap())
        .collect();
    let dag = compile_physical_asap_workload(&timed).unwrap();
    dag.validate().unwrap();
    assert_eq!(dag.roots.len(), 2);
    assert_eq!(dag.nodes.len(), 4, "scan and summary are exported once");
}
