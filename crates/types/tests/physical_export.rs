//! A timed plan exports as a PhysicalASAPDAG that keeps timing.
use asap_types::ir::operator::{Reduction, Source};
use asap_types::ir::physical_export::{
    compile_physical_asap_dag, compile_physical_asap_workload, PhysicalASAPDAGDocument,
    PhysicalASAPDAGValidationError,
};
use asap_types::ir::properties::{ExecutionDataStateError, ExecutionTiming};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::schema::{ExactKind, ExactParams, SummaryUpdate};
use asap_types::ir::{
    apply_materialization_timings, ASAPOp, MaterializationAssignment, NonASAPOp, Operator,
    OperatorNode, QueryRoot, TimingMemo,
};
use asap_types::physical::{Materialization, Medium};
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

const ON_DISK: Materialization = Materialization::MatAtIngestion(Medium::Disk);

/// `plan()` timed with its summary `MatAtIngestion` on disk.
fn timed_at_ingestion() -> Rc<OperatorNode> {
    let root = plan();
    let mut assignment = MaterializationAssignment::default();
    assignment.set(root.children()[0], ON_DISK);
    apply_materialization_timings(&root, &assignment, &mut TimingMemo::new()).unwrap()
}

/// A summary materialized at ingestion time is read at query time, and the
/// exported DAG says where to store it.
#[test]
fn timed_plan_exports_with_timing() {
    let timed = timed_at_ingestion();
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
    let materializations: Vec<_> = dag.nodes.iter().map(|n| n.materialization).collect();
    assert_eq!(
        materializations,
        vec![
            Materialization::NotMaterialized,
            ON_DISK,
            Materialization::NotMaterialized
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
    let mut dag = compile_physical_asap_dag(&timed_at_ingestion()).unwrap();
    dag.nodes[0].output_state.timing = ExecutionTiming::QueryTime;
    dag.edges[0].data_state = dag.nodes[0].output_state;
    assert!(matches!(
        dag.validate(),
        Err(PhysicalASAPDAGValidationError::QueryDependencyInIngestion { .. })
    ));
}

/// A summary's timing must be the one its materialization implies: the
/// deployment cannot run a `MatAtIngestion` state at query time.
#[test]
fn materialization_inconsistent_with_timing_is_rejected() {
    let mut dag = compile_physical_asap_dag(&timed_at_ingestion()).unwrap();
    dag.nodes[1].output_state.timing = ExecutionTiming::QueryTime;
    dag.edges[1].data_state = dag.nodes[1].output_state;
    assert_eq!(
        dag.validate(),
        Err(
            PhysicalASAPDAGValidationError::MaterializationTimingMismatch {
                node: 1,
                timing: ExecutionTiming::QueryTime,
                materialization: ON_DISK,
            }
        )
    );
    dag.nodes[1].materialization = Materialization::MatAtQuery(Medium::Disk);
    dag.validate().unwrap();
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
    let assignment = MaterializationAssignment::default();
    let mut memo = TimingMemo::new();
    let timed: Vec<_> = [first, second]
        .iter()
        .map(|root| apply_materialization_timings(root, &assignment, &mut memo).unwrap())
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
    use asap_types::ir::scalar::ArithmeticOpKind;
    use asap_types::ir::{ExprSemantics, ScalarExpr};
    let timed = apply_materialization_timings(
        &plan(),
        &MaterializationAssignment::default(),
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
    let valid_scalar_only = scalar_only.clone();
    // Materialization is checked on the nodes a scalar reads, too.
    let state = scalar_only
        .nodes
        .iter()
        .position(|node| {
            matches!(
                node.payload,
                asap_types::ir::physical_export::PhysicalASAPOperatorPayload::ASAP(
                    ASAPOp::SummaryAgg { .. }
                )
            )
        })
        .unwrap();
    scalar_only.nodes[state].materialization = ON_DISK;
    assert_eq!(
        scalar_only.validate(),
        Err(
            PhysicalASAPDAGValidationError::MaterializationTimingMismatch {
                node: scalar_only.nodes[state].id,
                timing: ExecutionTiming::QueryTime,
                materialization: ON_DISK,
            }
        )
    );

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
        ScalarExpr::Literal(asap_types::ir::scalar::ScalarValue::Float64(2.0));
    let mut duplicate = valid_scalar_only.clone();
    duplicate.scalar_roots.push((0, literal.clone()));
    assert_eq!(
        duplicate.validate(),
        Err(PhysicalASAPDAGValidationError::ScalarRootPosition(0))
    );
    let mut descending = valid_scalar_only;
    descending.scalar_roots[0].0 = 1;
    descending.scalar_roots.push((0, literal));
    assert_eq!(
        descending.validate(),
        Err(PhysicalASAPDAGValidationError::ScalarRootPosition(0))
    );
}

/// A `Project` reading the same node twice from its expressions has one
/// `ScalarRef` edge to it, as cost lowering reads it once.
#[test]
fn a_reference_read_twice_is_one_scalar_ref_edge() {
    use asap_types::ir::physical_export::EdgeRole;
    use asap_types::ir::scalar::ArithmeticOpKind;
    use asap_types::ir::{ExprSemantics, ProjectItem, ScalarExpr};
    let series = |metric: &str| {
        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries {
                metric: metric.into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![Field::plain("value", DataType::Float64, false)]),
        }))
        .unwrap()
    };
    let read = series("b");
    let scalar = || ScalarExpr::PromqlScalarFromVector(Rc::clone(&read));
    let project = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
        cols: vec![ProjectItem {
            alias: Some("value".into()),
            expr: ScalarExpr::Arithmetic {
                op: ArithmeticOpKind::Mul,
                left: Box::new(scalar()),
                right: Box::new(scalar()),
                semantics: ExprSemantics::Promql,
            },
        }],
        qualifier: None,
        child: series("a"),
    }))
    .unwrap();
    let timed = apply_materialization_timings(
        &project,
        &MaterializationAssignment::default(),
        &mut TimingMemo::new(),
    )
    .unwrap();
    let dag = compile_physical_asap_dag(&timed).unwrap();
    dag.validate().unwrap();
    let roles: Vec<_> = dag
        .edges
        .iter()
        .filter(|edge| edge.consumer == dag.roots[0])
        .map(|edge| edge.role)
        .collect();
    assert_eq!(roles, vec![EdgeRole::Input, EdgeRole::ScalarRef]);
}

/// The roles of a binary operator's edges must match its inputs: swapping
/// `Left` and `Right` would reorder the operands of `a - b`.
#[test]
fn swapped_edge_roles_are_rejected() {
    use asap_types::ir::operator::JoinKind;
    use asap_types::ir::physical_export::EdgeRole;
    use asap_types::ir::scalar::ScalarValue;
    use asap_types::ir::{Predicate, ScalarExpr};
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
    let timed = apply_materialization_timings(
        &join,
        &MaterializationAssignment::default(),
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

/// A summary over another summary exports a typed maintenance edge between
/// them, and the document round-trips and rejects unknown fields and versions.
#[test]
fn exports_summary_over_summary_as_typed_precompute_edges() {
    use asap_types::ir::physical_export::{
        compile_physical_asap_dag_with_node_ids, GroupingEdgeCompatibility, WindowEdgeCompatibility,
    };
    use asap_types::ir::properties::ExecutionDataState;
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
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
    }))
    .unwrap();
    let sum = |child, column| {
        OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
            child,
            family: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            input: SummaryUpdate::column(column),
            reduction: Reduction::by(vec![]),
            grouping: Default::default(),
            filter: None,
        }))
        .unwrap()
    };
    let inner = sum(scan, ColumnRef::SampleValue);
    let outer = sum(Rc::clone(&inner), ColumnRef::Named("state".into()));
    let root = OperatorNode::new_shared(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
        child: outer,
    }))
    .unwrap();
    let mut memo = TimingMemo::new();
    let at_ingestion =
        MaterializationAssignment::with_default(Materialization::MatAtIngestion(Medium::Memory));
    let timed = apply_materialization_timings(&root, &at_ingestion, &mut memo).unwrap();
    let compiled = compile_physical_asap_dag_with_node_ids(&timed).unwrap();
    assert_eq!(compiled.node_ids.node_id(&timed), Some(3));
    assert_eq!(
        compiled.node_ids.node_id(memo.timed(&inner).unwrap()),
        Some(1)
    );
    let dag = compiled.dag;
    assert_eq!(dag.roots, vec![3]);
    assert_eq!(
        dag.nodes[1].output_state,
        ExecutionDataState::INGESTION_SUMMARY
    );
    assert_eq!(
        dag.nodes[2].output_state,
        ExecutionDataState::INGESTION_SUMMARY
    );
    let dependency = dag
        .edges
        .iter()
        .find(|e| e.producer == 1 && e.consumer == 2)
        .unwrap();
    assert_eq!(dependency.data_state, ExecutionDataState::INGESTION_SUMMARY);
    assert_eq!(dependency.grouping, GroupingEdgeCompatibility::Identical);
    assert_eq!(
        dependency.window,
        WindowEdgeCompatibility::RequiresAlignedPanePhaseOrExactWindowEdgeResidual
    );
    assert!(matches!(
        dependency.intermediate_schema.fields[0].dtype,
        FieldDataType::ExactAggregate(ExactKind::Sum, _)
    ));
    assert!(matches!(
        dag.nodes[2].payload,
        Operator::ASAP(ASAPOp::SummaryAgg {
            family: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            reduction: Reduction::Reduce(_),
            ..
        })
    ));

    let document = PhysicalASAPDAGDocument::new(dag);
    let decoded: PhysicalASAPDAGDocument =
        serde_json::from_str(&serde_json::to_string(&document).unwrap()).unwrap();
    assert_eq!(decoded, document);
    decoded.validate().unwrap();
    let mut old_version = document.clone();
    old_version.schema_version = 1;
    assert_eq!(
        old_version.validate(),
        Err(PhysicalASAPDAGValidationError::UnsupportedVersion(1))
    );
    let mut unknown = serde_json::to_value(&document).unwrap();
    unknown["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<PhysicalASAPDAGDocument>(unknown).is_err());
}

/// Node-id-keyed bindings serialize in numeric id order.
#[test]
fn physical_node_ids_serialize_in_deterministic_binding_order() {
    use asap_types::ir::physical_export::PhysicalASAPNodeId;
    let mut bindings = std::collections::BTreeMap::<PhysicalASAPNodeId, &str>::new();
    bindings.insert(10, "materialization-10");
    bindings.insert(2, "query-2");
    assert_eq!(
        serde_json::to_string(&bindings).unwrap(),
        r#"{"2":"query-2","10":"materialization-10"}"#
    );
}

/// Expanding a DAG without a pane chain returns the same DAG: each node,
/// its timing and materialization, the shared summary and the scalar root
/// that reads it, in batch order.
#[test]
fn expanding_a_dag_without_pane_chains_keeps_it() {
    use asap_types::ir::scalar::ArithmeticOpKind;
    use asap_types::ir::{ExprSemantics, ScalarExpr};
    let timed = timed_at_ingestion();
    let scalar = ScalarExpr::Arithmetic {
        op: ArithmeticOpKind::Add,
        left: Box::new(ScalarExpr::PromqlScalarFromVector(Rc::clone(&timed))),
        right: Box::new(ScalarExpr::literal_f64(1.0)),
        semantics: ExprSemantics::Promql,
    };
    let dag = compile_physical_asap_workload(&[
        QueryRoot::Operator(Rc::clone(&timed)),
        QueryRoot::Scalar(scalar),
        QueryRoot::Operator(Rc::clone(&timed)),
    ])
    .unwrap();
    assert_eq!(dag.with_pane_chains_expanded().unwrap(), dag);
}

/// The plan with every node at query time is the same operators with
/// nothing materialized: what the default materialization exports.
#[test]
fn every_node_at_query_time_keeps_the_operators_and_stores_nothing() {
    let materialized =
        compile_physical_asap_workload(&[QueryRoot::Operator(timed_at_ingestion())]).unwrap();
    let at_query_time = apply_materialization_timings(
        &plan(),
        &MaterializationAssignment::default(),
        &mut TimingMemo::new(),
    )
    .unwrap();
    assert_eq!(
        materialized.with_every_node_at_query_time().unwrap(),
        compile_physical_asap_workload(&[QueryRoot::Operator(at_query_time)]).unwrap()
    );
}
