//! A plan with its nodes' physical properties exports as a PhysicalASAPDAG
//! that keeps them.
use asap_types::ir::operator::{Reduction, Source};
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPDAGValidationError};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::schema::{ExactKind, ExactParams, SummaryUpdate};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::physical::{Lifetime, PhysicalPropertiesError, StorageMedium};
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
        summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
        input: SummaryUpdate::column(ColumnRef::Named("value".into())),
        reduction: Reduction::by(vec![0]),
        filter: None,
    }))
    .unwrap();
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
        child: Rc::new(state),
    }))
    .unwrap()
}

/// Two queries reading one summary state export once, with one root per query.
#[test]
fn batch_shares_the_summary_and_keeps_one_root_per_query() {
    let first = plan();
    let state = first.children()[0].clone();
    let second = OperatorNode::new_shared(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
        child: state,
    }))
    .unwrap();
    let roots: Vec<QueryRoot> = [first, second]
        .into_iter()
        .map(QueryRoot::Operator)
        .collect();
    let dag = asap_types::test_support::physical_asap_dag_at_query_time_with_node_ids(&roots)
        .map(|(dag, _)| dag)
        .unwrap();
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
    let read = plan();
    let scalar = ScalarExpr::Arithmetic {
        op: ArithmeticOpKind::Add,
        left: Box::new(ScalarExpr::PromqlScalarFromVector(Rc::clone(&read))),
        right: Box::new(ScalarExpr::literal_f64(1.0)),
        semantics: ExprSemantics::Promql,
    };
    let dag = asap_types::test_support::physical_asap_dag_at_query_time_with_node_ids(&[
        QueryRoot::Scalar(scalar),
        QueryRoot::Operator(Rc::clone(&read)),
    ])
    .map(|(dag, _)| dag)
    .unwrap();
    assert_eq!(dag.nodes.len(), 3, "the read plan is exported once");
    assert_eq!(dag.roots.len(), 1);
    assert_eq!(dag.scalar_roots.len(), 1);
    let (position, expr) = &dag.scalar_roots[0];
    assert_eq!(*position, 0);
    assert_eq!(expr.operator_refs(), vec![&dag.roots[0]]);
    let decoded: PhysicalASAPDAG =
        serde_json::from_str(&serde_json::to_string(&dag).unwrap()).unwrap();
    assert_eq!(decoded, dag);

    // A scalar-only batch is valid: its referenced nodes are reachable.
    let mut scalar_only = dag.clone();
    scalar_only.roots.clear();
    scalar_only.validate().unwrap();
    let valid_scalar_only = scalar_only.clone();
    // Physical properties are checked on the nodes a scalar reads, too.
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
    scalar_only.nodes[state].output_state.timing = ExecutionTiming::IngestionTime;
    scalar_only.nodes[state].lifetime = Lifetime::OneEvaluation(StorageMedium::Disk);
    assert_eq!(
        scalar_only.validate(),
        Err(PhysicalASAPDAGValidationError::PhysicalProperties {
            node: scalar_only.nodes[state].id,
            reason: PhysicalPropertiesError::OneEvaluationAtIngestion,
        })
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
        child: series("a"),
    }))
    .unwrap();
    let dag = asap_types::test_support::physical_asap_dag_at_query_time(&project).unwrap();
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
    let mut dag = asap_types::test_support::physical_asap_dag_at_query_time(&join).unwrap();
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
        asap_types::test_support::physical_asap_dag_at_query_time_with_node_ids(&[])
            .map(|(dag, _)| dag),
        Err(PhysicalASAPDAGValidationError::NoRoots)
    ));
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

/// A window 600,000 ms wide ending 300,000 ms before T = 3,600,000 covers
/// `(2,700,000, 3,300,000]`.
#[test]
fn a_raw_data_window_ends_its_offset_before_the_evaluation() {
    let window = asap_types::ir::materialized_view::RawDataWindow::new(
        Source::TimeSeries {
            metric: "latency".into(),
        },
        300_000,
        600_000,
    );
    assert_eq!(window.bounds_ms(3_600_000), (2_700_000, 3_300_000));
}

/// High-fan-in merge/concat validation is independent of wire edge order and
/// preserves repeated input occurrences, rather than treating edges as a set.
#[test]
fn high_fan_in_edges_preserve_multiplicity_without_order_dependence() {
    use asap_types::ir::physical_export::{
        EdgeRole, GroupingEdgeCompatibility, PhysicalASAPDAG, PhysicalASAPDAGEdge,
        PhysicalASAPDAGNode, WindowEdgeCompatibility,
    };
    use asap_types::ir::properties::ExecutionDataState;
    let schema = Schema::new(vec![Field::plain("value", DataType::Float64, false)]);
    let count = 20_000;
    let mut nodes: Vec<_> = (0..count)
        .map(|id| PhysicalASAPDAGNode {
            id,
            payload: Operator::NonASAP(NonASAPOp::Scan {
                source: Source::Table {
                    table_ref: format!("source-{id}"),
                },
                predicates: vec![],
                schema: schema.clone(),
            }),
            output_state: ExecutionDataState::QUERY_ROWS,
            lifetime: Lifetime::None,
            parameters: None,
            output_schema: schema.clone(),
            guarantee: None,
            retention_ms: None,
            summary_coverage: None,
            selected_physical_candidate: None,
        })
        .collect();
    let inputs: Vec<_> = (0..count).chain([0, 0]).collect();
    nodes.push(PhysicalASAPDAGNode {
        id: count,
        payload: Operator::NonASAP(NonASAPOp::Concat {
            children: inputs.clone(),
        }),
        output_state: ExecutionDataState::QUERY_ROWS,
        lifetime: Lifetime::None,
        parameters: None,
        output_schema: schema.clone(),
        guarantee: None,
        retention_ms: None,
        summary_coverage: None,
        selected_physical_candidate: None,
    });
    let mut dag = PhysicalASAPDAG {
        nodes,
        roots: vec![count],
        scalar_roots: vec![],
        edges: inputs
            .into_iter()
            .map(|producer| {
                PhysicalASAPDAGEdge::new(
                    producer,
                    count,
                    EdgeRole::Input,
                    schema.clone(),
                    ExecutionDataState::QUERY_ROWS,
                    GroupingEdgeCompatibility::NotApplicable,
                    WindowEdgeCompatibility::NotApplicable,
                )
            })
            .collect(),
    };
    dag.edges.reverse();
    dag.validate()
        .expect("reordered input multiset remains valid");
    let missing = dag.edges.pop().unwrap();
    assert!(matches!(
        dag.validate(),
        Err(PhysicalASAPDAGValidationError::EdgeRoleMismatch { .. })
    ));
    dag.edges.push(missing);
    dag.edges.push(dag.edges[0].clone());
    assert!(matches!(
        dag.validate(),
        Err(PhysicalASAPDAGValidationError::EdgeRoleMismatch { .. })
    ));
}
