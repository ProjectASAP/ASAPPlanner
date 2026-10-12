//! A timed plan exports as a PhysicalASAPDAG that keeps timing.
use asap_types::ir::physical_export::{
    compile_physical_asap_dag, compile_physical_asap_workload, PhysicalASAPDAGDocument,
    PhysicalASAPDAGValidationError,
};
use asap_types::ir::{
    apply_lifecycle_timings, ASAPOp, LifecycleAssignment, NonASAPOp, Operator, OperatorNode,
    QueryRoot, TimingMemo,
};
use asap_types::post_asap::{
    ExactKind, ExactParams, ExecutionDataStateError, ExecutionTiming, SummaryUpdate,
};
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
    let roots: Vec<QueryRoot> = timed.into_iter().map(QueryRoot::Operator).collect();
    let dag = compile_physical_asap_workload(&roots).unwrap();
    dag.validate().unwrap();
    assert_eq!(dag.roots.len(), 2);
    assert_eq!(dag.nodes.len(), 4, "scan and summary are exported once");
}

/// A standalone scalar query exports as a scalar root keyed by its position
/// in the batch, reading the shared nodes by id; operator queries keep `roots`.
#[test]
fn scalar_roots_export_in_batch_order_and_reference_nodes() {
    use asap_types::ir::{ExprSemantics, ScalarExpr};
    use asap_types::pre_asap::ArithmeticOpKind;
    let timed = apply_lifecycle_timings(
        &plan(),
        &LifecycleAssignment::default_maintained(),
        &mut TimingMemo::new(),
    )
    .unwrap();
    let scalar = ScalarExpr::Arithmetic {
        op: ArithmeticOpKind::Add,
        left: Box::new(ScalarExpr::PromqlScalarFromVector(Rc::clone(&timed))),
        right: Box::new(ScalarExpr::literal_f64(1.0)),
        semantics: ExprSemantics::Promql,
    };
    let dag = compile_physical_asap_workload(&[
        QueryRoot::Scalar(scalar),
        QueryRoot::Operator(Rc::clone(&timed)),
    ])
    .unwrap();
    assert_eq!(dag.nodes.len(), 3, "the read plan is exported once");
    assert_eq!(dag.roots.len(), 1);
    assert_eq!(dag.scalar_roots.len(), 1);
    let (position, expr) = &dag.scalar_roots[0];
    assert_eq!(*position, 0);
    assert_eq!(expr.operator_refs(), vec![&dag.roots[0]]);
    let document = PhysicalASAPDAGDocument::new(dag.clone());
    let decoded: PhysicalASAPDAGDocument =
        serde_json::from_str(&serde_json::to_string(&document).unwrap()).unwrap();
    assert_eq!(decoded, document);

    // A scalar-only batch is valid: its referenced nodes are reachable.
    let mut scalar_only = dag.clone();
    scalar_only.roots.clear();
    scalar_only.validate().unwrap();

    let mut missing = dag.clone();
    missing.scalar_roots[0].1 = ScalarExpr::PromqlScalarFromVector(99);
    assert_eq!(
        missing.validate(),
        Err(PhysicalASAPDAGValidationError::MissingRoot(99))
    );
    let mut outside = dag.clone();
    outside.scalar_roots[0].0 = 2;
    assert_eq!(
        outside.validate(),
        Err(PhysicalASAPDAGValidationError::ScalarRootPosition(2))
    );

    // Keys must strictly ascend: a duplicate or a descending key is rejected.
    let literal: ScalarExpr<usize> =
        ScalarExpr::Literal(asap_types::pre_asap::ScalarValue::Float64(2.0));
    let mut duplicate = scalar_only.clone();
    duplicate.scalar_roots.push((0, literal.clone()));
    assert_eq!(
        duplicate.validate(),
        Err(PhysicalASAPDAGValidationError::ScalarRootPosition(0))
    );
    let mut descending = scalar_only.clone();
    descending.scalar_roots[0].0 = 1;
    descending.scalar_roots.push((0, literal));
    assert_eq!(
        descending.validate(),
        Err(PhysicalASAPDAGValidationError::ScalarRootPosition(0))
    );
}

/// The roles of a binary operator's edges must match its inputs: swapping
/// `Left` and `Right` would reorder the operands of `a - b`.
#[test]
fn swapped_edge_roles_are_rejected() {
    use asap_types::ir::physical_export::EdgeRole;
    use asap_types::ir::{Predicate, ScalarExpr};
    use asap_types::pre_asap::{JoinKind, ScalarValue};
    let scan = |table: &str| {
        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: table.into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Field::plain("k", DataType::Utf8, false)]),
        }))
        .unwrap()
    };
    let join = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Join {
        kind: JoinKind::Cross,
        pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
        left: scan("a"),
        right: scan("b"),
    }))
    .unwrap();
    let timed = apply_lifecycle_timings(
        &join,
        &LifecycleAssignment::default_maintained(),
        &mut TimingMemo::new(),
    )
    .unwrap();
    let mut dag = compile_physical_asap_dag(&timed).unwrap();
    dag.validate().unwrap();
    for edge in &mut dag.edges {
        edge.role = match edge.role {
            EdgeRole::Left => EdgeRole::Right,
            EdgeRole::Right => EdgeRole::Left,
            other => other,
        };
    }
    assert_eq!(
        dag.validate(),
        Err(PhysicalASAPDAGValidationError::EdgeRoleMismatch {
            consumer: dag.roots[0]
        })
    );
}

/// An export that fails validation is an error, not a panic.
#[test]
fn an_empty_batch_is_an_error() {
    assert!(matches!(
        compile_physical_asap_workload(&[]),
        Err(ExecutionDataStateError::InvalidPhysicalDAG(
            PhysicalASAPDAGValidationError::NoRoots
        ))
    ));
}
