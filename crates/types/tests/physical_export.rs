//! A timed plan exports as a PhysicalASAPDAG that keeps timing.
use asap_types::ir::operator::{Reduction, Source};
use asap_types::ir::physical_export::{
    compile_physical_asap_dag, compile_physical_asap_workload, PhysicalASAPDAG,
    PhysicalASAPDAGValidationError,
};
use asap_types::ir::properties::{ExecutionDataStateError, ExecutionTiming};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::schema::{ExactKind, ExactParams, SummaryUpdate};
use asap_types::ir::{
    apply_materialization_timings, ASAPOp, MaterializationAssignment, NodeToTimedNodeCache,
    NonASAPOp, Operator, OperatorNode, QueryRoot,
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
    apply_materialization_timings(&root, &assignment, &mut NodeToTimedNodeCache::new()).unwrap()
}

/// A summary materialized at ingestion time is read at query time, and the
/// exported DAG says where to store it.
#[test]
fn timed_plan_exports_with_timing() {
    let timed = timed_at_ingestion();
    let dag = compile_physical_asap_dag(&timed).unwrap();
    dag.validate().unwrap();

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

    let decoded: PhysicalASAPDAG =
        serde_json::from_str(&serde_json::to_string(&dag).unwrap()).unwrap();
    assert_eq!(decoded, dag);
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
    let mut cache = NodeToTimedNodeCache::new();
    let timed: Vec<_> = [first, second]
        .iter()
        .map(|root| apply_materialization_timings(root, &assignment, &mut cache).unwrap())
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
        &mut NodeToTimedNodeCache::new(),
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
    let decoded: PhysicalASAPDAG =
        serde_json::from_str(&serde_json::to_string(&dag).unwrap()).unwrap();
    assert_eq!(decoded, dag);

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
        &mut NodeToTimedNodeCache::new(),
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
        &mut NodeToTimedNodeCache::new(),
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
    use asap_types::ir::physical_export::{GroupingEdgeCompatibility, WindowEdgeCompatibility};
    use asap_types::ir::properties::ExecutionDataState;
    use asap_types::test_support::compile_physical_asap_dag_with_node_ids;
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
    let mut cache = NodeToTimedNodeCache::new();
    let at_ingestion =
        MaterializationAssignment::with_default(Materialization::MatAtIngestion(Medium::Memory));
    let timed = apply_materialization_timings(&root, &at_ingestion, &mut cache).unwrap();
    let compiled = compile_physical_asap_dag_with_node_ids(&timed).unwrap();
    assert_eq!(compiled.node_ids.node_id(&timed), Some(3));
    assert_eq!(
        compiled.node_ids.node_id(cache.timed(&inner).unwrap()),
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

    let decoded: PhysicalASAPDAG =
        serde_json::from_str(&serde_json::to_string(&dag).unwrap()).unwrap();
    assert_eq!(decoded, dag);
    decoded.validate().unwrap();
    let mut unknown = serde_json::to_value(&dag).unwrap();
    unknown["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<PhysicalASAPDAG>(unknown).is_err());
}

/// `retention_ms` is accepted on a materialized view (a node kept across
/// evaluations), survives a round trip, and is rejected on a node that is
/// neither a view nor a query-time scan (here an ingestion-time node).
#[test]
fn retention_is_only_on_materialized_views_and_query_time_scans() {
    let mut dag = compile_physical_asap_dag(&timed_at_ingestion()).unwrap();
    let view = dag
        .nodes
        .iter()
        .position(|node| node.materialization.is_materialized_view())
        .expect("a node built at ingestion");
    let other = dag
        .nodes
        .iter()
        .position(|node| {
            !node.materialization.is_materialized_view()
                && node.output_state.timing == ExecutionTiming::IngestionTime
        })
        .expect("an ingestion-time node that is not a view");
    dag.nodes[view].retention_ms = Some(60_000);
    dag.validate().unwrap();
    let decoded: PhysicalASAPDAG =
        serde_json::from_str(&serde_json::to_string(&dag).unwrap()).unwrap();
    assert_eq!(decoded, dag);
    dag.nodes[other].retention_ms = Some(60_000);
    assert_eq!(
        dag.validate(),
        Err(PhysicalASAPDAGValidationError::UnexpectedRetention(
            dag.nodes[other].id
        ))
    );
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

/// The plan with every node at query time is the same operators with
/// nothing materialized: what the default materialization exports. The
/// shared summary stays one node, and the scalar root that reads it keeps
/// its batch position.
#[test]
fn every_node_at_query_time_keeps_the_operators_and_stores_nothing() {
    use asap_types::ir::scalar::ArithmeticOpKind;
    use asap_types::ir::{ExprSemantics, ScalarExpr};
    let workload = |root: Rc<OperatorNode>| {
        let scalar = ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Add,
            left: Box::new(ScalarExpr::PromqlScalarFromVector(Rc::clone(&root))),
            right: Box::new(ScalarExpr::literal_f64(1.0)),
            semantics: ExprSemantics::Promql,
        };
        compile_physical_asap_workload(&[
            QueryRoot::Operator(Rc::clone(&root)),
            QueryRoot::Scalar(scalar),
            QueryRoot::Operator(root),
        ])
        .unwrap()
    };
    let materialized = workload(timed_at_ingestion());
    let at_query_time = apply_materialization_timings(
        &plan(),
        &MaterializationAssignment::default(),
        &mut NodeToTimedNodeCache::new(),
    )
    .unwrap();
    assert_eq!(
        materialized.with_every_node_at_query_time().unwrap(),
        workload(at_query_time)
    );
}

/// A window 600,000 ms wide ending 300,000 ms before T = 3,600,000 covers
/// `(2,700,000, 3,300,000]`, and reaches 900,000 ms back.
#[test]
fn a_raw_data_window_ends_its_offset_before_the_evaluation() {
    let window = asap_types::ir::materialized_view::RawDataWindow {
        source: Source::TimeSeries {
            metric: "latency".into(),
        },
        width_ms: 600_000,
        end_offset_ms: 300_000,
    };
    assert_eq!(window.bounds_ms(3_600_000), (2_700_000, 3_300_000));
    assert_eq!(window.lookback_ms(), 900_000);
}

/// `TimeRange(Range, width_ms)` over `shift` (if any) over `scan`, untimed.
fn window_over(
    scan: &Rc<OperatorNode>,
    width_ms: u64,
    shift: Option<asap_types::ir::operator::TimeShift>,
) -> Rc<OperatorNode> {
    let child = match shift {
        Some(shift) => OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
            shift,
            child: Rc::clone(scan),
        }))
        .unwrap(),
        None => Rc::clone(scan),
    };
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: std::time::Duration::from_millis(width_ms),
        kind: asap_types::ir::TimeRangeKind::Range,
        child,
    }))
    .unwrap()
}

/// The exported DAG of `roots`, all at query time, and its one `Scan`'s id.
fn exported_with_scan(
    roots: Vec<Rc<OperatorNode>>,
) -> (
    asap_types::ir::physical_export::PhysicalASAPDAG,
    asap_types::ir::physical_export::PhysicalASAPNodeId,
) {
    // One timing cache, so the roots keep sharing their `Scan`.
    let mut cache = NodeToTimedNodeCache::new();
    let timed: Vec<_> = roots
        .into_iter()
        .map(|root| {
            apply_materialization_timings(&root, &MaterializationAssignment::default(), &mut cache)
                .unwrap()
        })
        .map(QueryRoot::Operator)
        .collect();
    let dag = compile_physical_asap_workload(&timed).unwrap();
    let scans: Vec<_> = dag
        .nodes
        .iter()
        .filter(|node| matches!(node.payload, Operator::NonASAP(NonASAPOp::Scan { .. })))
        .map(|node| node.id)
        .collect();
    assert_eq!(scans.len(), 1, "one shared Scan");
    (dag, scans[0])
}

/// A time-series `Scan` of `m` with a time index.
fn series_scan() -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
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
    .unwrap()
}

// A `Scan` read by `m[5m]` and by `m[1h] offset 10m` is read over `(T - 70m,
// T]`, the union of `(T - 5m, T]` and `(T - 70m, T - 10m]`.
#[test]
fn a_scan_window_is_the_union_of_its_readers_windows() {
    use asap_types::ir::physical_export::ScanWindow;
    let minute_ms = 60_000;
    let scan = series_scan();
    let offset = asap_types::ir::operator::TimeShift {
        offset_ms: 10 * minute_ms as i64,
        at: None,
    };
    let (dag, scan_id) = exported_with_scan(vec![
        window_over(&scan, 5 * minute_ms, None),
        window_over(&scan, 60 * minute_ms, Some(offset)),
    ]);
    let window = dag.scan_window(scan_id, &Default::default());
    assert_eq!(
        window,
        ScanWindow::Within {
            end_offset_ms: 0,
            width_ms: 70 * minute_ms as i64,
        }
    );
    assert_eq!(window.lookback_ms(), Some(70 * minute_ms));
    assert_eq!(
        window.bounds_ms(100 * minute_ms as i64),
        Some((30 * 60_000, 100 * 60_000))
    );
}

// A node above `m[5m]` and `m[1h] offset 10m` summarizes `(T - 70m, T]`,
// the union of the windows below it; a node with no window below it, the
// `Scan` itself, summarizes an unbounded window.
#[test]
fn a_view_window_is_the_union_of_the_windows_below_it() {
    use asap_types::ir::physical_export::ScanWindow;
    let minute_ms = 60_000;
    let scan = series_scan();
    let offset = asap_types::ir::operator::TimeShift {
        offset_ms: 10 * minute_ms as i64,
        at: None,
    };
    let both = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Concat {
        children: vec![
            window_over(&scan, 5 * minute_ms, None),
            window_over(&scan, 60 * minute_ms, Some(offset)),
        ],
        discriminator_unique_key: None,
    }))
    .unwrap();
    let (dag, scan_id) = exported_with_scan(vec![both]);
    assert_eq!(
        dag.view_window(dag.roots[0]),
        ScanWindow::Within {
            end_offset_ms: 0,
            width_ms: 70 * minute_ms as i64,
        }
    );
    assert_eq!(dag.view_window(scan_id), ScanWindow::Unbounded);
}

// A window under an `@` anchor is not a fixed span back from the evaluation
// time, so the `Scan` it reads is read over an unbounded window.
#[test]
fn a_scan_read_under_an_at_anchor_has_an_unbounded_window() {
    use asap_types::ir::physical_export::ScanWindow;
    let scan = series_scan();
    let anchored = asap_types::ir::operator::TimeShift {
        offset_ms: 0,
        at: Some(asap_types::ir::operator::AtModifier::Timestamp(1_000)),
    };
    let (dag, scan_id) = exported_with_scan(vec![
        window_over(&scan, 300_000, None),
        window_over(&scan, 300_000, Some(anchored)),
    ]);
    assert_eq!(
        dag.scan_window(scan_id, &Default::default()),
        ScanWindow::Unbounded
    );
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
            materialization: Materialization::NotMaterialized,
            output_schema: schema.clone(),
            guarantee: None,
            retention_ms: None,
        })
        .collect();
    let inputs: Vec<_> = (0..count).chain([0, 0]).collect();
    nodes.push(PhysicalASAPDAGNode {
        id: count,
        payload: Operator::NonASAP(NonASAPOp::Concat {
            children: inputs.clone(),
            discriminator_unique_key: None,
        }),
        output_state: ExecutionDataState::QUERY_ROWS,
        materialization: Materialization::NotMaterialized,
        output_schema: schema.clone(),
        guarantee: None,
        retention_ms: None,
    });
    let mut dag = PhysicalASAPDAG {
        nodes,
        roots: vec![count],
        scalar_roots: vec![],
        edges: inputs
            .into_iter()
            .map(|producer| PhysicalASAPDAGEdge {
                producer,
                consumer: count,
                role: EdgeRole::Input,
                intermediate_schema: schema.clone(),
                data_state: ExecutionDataState::QUERY_ROWS,
                grouping: GroupingEdgeCompatibility::NotApplicable,
                window: WindowEdgeCompatibility::NotApplicable,
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

#[test]
fn physical_export_deadline_is_typed_and_preserves_unbounded_behavior() {
    use asap_types::ir::physical_export::PhysicalASAPCompilationError;
    use asap_types::test_support::compile_physical_asap_workload_with_node_ids_before;
    use std::time::Instant;
    let timed = timed_at_ingestion();
    let roots = [QueryRoot::Operator(Rc::clone(&timed))];
    let bounded = compile_physical_asap_workload_with_node_ids_before(
        &roots,
        Some(Instant::now() + std::time::Duration::from_secs(1)),
    )
    .unwrap();
    assert_eq!(bounded.dag, compile_physical_asap_dag(&timed).unwrap());
    assert!(matches!(
        compile_physical_asap_workload_with_node_ids_before(&roots, Some(Instant::now())),
        Err(PhysicalASAPCompilationError::DeadlineExceeded)
    ));
    let untimed = [QueryRoot::Operator(plan())];
    assert!(matches!(
        compile_physical_asap_workload_with_node_ids_before(&untimed, None),
        Err(PhysicalASAPCompilationError::Invalid(_))
    ));
}
