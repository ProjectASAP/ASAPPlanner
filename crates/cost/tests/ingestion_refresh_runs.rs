//! How often a not-materialized node is computed for the views refreshed
//! at ingestion above it ([`computations`]): once per refresh window, since
//! a runtime refreshes the views of one refresh window in one ingestion run
//! (issue #1151).

use std::rc::Rc;
use std::time::Duration;

use asap_cost::execution_demand::computations;
use asap_types::ir::operator::operator_properties::{Reduction, Source, TimeShift};
use asap_types::ir::physical_export::{compile_physical_asap_workload, PhysicalASAPDAG};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{
    DataType, Field, FieldDataType, GroupingStrategy, Schema, SketchAlgorithm, SketchKind,
    SketchParams, SummaryUpdate,
};
use asap_types::ir::{
    apply_physical_properties_to_roots, ASAPOp, NonASAPOp, Operator, OperatorNode,
    PhysicalPropertiesAssignment, QueryRoot, TimeRangeKind,
};
use asap_types::physical::PhysicalProperties;
use asap_types::workload::{QueryRecurrence, RepeatedDemand, RepetitionInterval};

/// `m` with columns `ts`, `value` and `job`.
fn scan() -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "m".into() },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
                Field::plain("job", DataType::Utf8, true),
            ],
            0,
            vec![],
        ),
    }))
    .unwrap()
}

/// A quantile sketch of `algorithm` (KLL k = 200 or DDSketch α = 0.01).
fn quantile_sketch(algorithm: SketchAlgorithm) -> FieldDataType {
    let params = match algorithm {
        SketchAlgorithm::Kll => SketchParams::Kll {
            k: 200,
            failure_probability: None,
        },
        _ => SketchParams::DDSketch { alpha: 0.01 },
    };
    FieldDataType::Sketch(SketchKind::new(algorithm, params), Default::default())
}

/// `tumbling_window_count` tumbling windows of `tumbling_window_width_ms` of a quantile sketch of
/// `algorithm` per `job` over `input`.
fn chain(
    input: &Rc<OperatorNode>,
    algorithm: SketchAlgorithm,
    tumbling_window_width_ms: u64,
    tumbling_window_count: u32,
) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: Rc::clone(input),
        family: quantile_sketch(algorithm),
        input: SummaryUpdate::column(ColumnRef::Named("value".into())),
        reduction: Reduction::by(vec![2]),
        grouping: GroupingStrategy::default(),
        filter: None,
        tumbling_window_width_ms,
        tumbling_window_count,
    }))
    .unwrap()
}

/// One query per summary of `summaries`, merging it, with each summary
/// materialized with `materialized` (`None`: not materialized): the DAG Stage 3 costs, chains
/// unexpanded.
fn dag_of(
    summaries: &[Rc<OperatorNode>],
    materialized: Option<PhysicalProperties>,
) -> PhysicalASAPDAG {
    let mut assignment = PhysicalPropertiesAssignment::default();
    let roots: Vec<QueryRoot> = summaries
        .iter()
        .map(|summary| {
            if let Some(materialized) = materialized {
                assignment.set(summary, materialized);
            }
            QueryRoot::Operator(
                OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
                    children: vec![Rc::clone(summary)],
                    newest_tumbling_windows: None,
                    group_by: None,
                }))
                .unwrap(),
            )
        })
        .collect();
    let physical_properties = apply_physical_properties_to_roots(&roots, &assignment).unwrap();
    compile_physical_asap_workload(&roots, &physical_properties).unwrap()
}

/// `(multiplier, per_second)` of `dag`'s `Scan`, each query every 10 s.
fn scan_computations(dag: &PhysicalASAPDAG) -> (u64, f64) {
    computations_of(dag, |op| matches!(op, NonASAPOp::Scan { .. }))
}

/// `(multiplier, per_second)` of the one node of `dag` whose operator
/// `is_node` picks, each query every 10 s.
fn computations_of(
    dag: &PhysicalASAPDAG,
    is_node: impl Fn(&NonASAPOp<usize>) -> bool,
) -> (u64, f64) {
    let every_10_s =
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(10_000)));
    let recurrence = vec![every_10_s; dag.roots.len()];
    let times = computations(dag, &recurrence, &[], 3600.0, None).unwrap();
    let node = dag
        .nodes
        .iter()
        .find(|node| matches!(&node.payload, Operator::NonASAP(op) if is_node(op)))
        .unwrap();
    times[&node.id]
}

/// `child` evaluated `offset_ms` earlier.
fn shifted(child: &Rc<OperatorNode>, offset_ms: i64) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
        shift: TimeShift {
            offset_ms,
            at: None,
        },
        child: Rc::clone(child),
    }))
    .unwrap()
}

/// A KLL chain and a DDSketch chain of 10 s tumbling windows over one `Scan`, both
/// materialized at ingestion, are refreshed in one ingestion run per tumbling window: the
/// `Scan` is computed once per second of arriving data, not twice.
#[test]
fn a_scan_under_two_chains_of_one_tumbling_window_width_is_computed_once() {
    let scan = scan();
    let dag = dag_of(
        &[
            chain(&scan, SketchAlgorithm::Kll, 10_000, 6),
            chain(&scan, SketchAlgorithm::DDSketch, 10_000, 30),
        ],
        Some(PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION),
    );
    assert_eq!(scan_computations(&dag), (1, 1.0));
}

/// With 10 s and 60 s tumbling windows the chains need different runs (the 10 s chain
/// needs five tumbling window ends the 60 s chain does not): the `Scan` is computed
/// once for each.
#[test]
fn a_scan_under_chains_of_two_tumbling_window_widths_is_computed_for_each() {
    let scan = scan();
    let dag = dag_of(
        &[
            chain(&scan, SketchAlgorithm::Kll, 10_000, 6),
            chain(&scan, SketchAlgorithm::DDSketch, 60_000, 5),
        ],
        Some(PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION),
    );
    assert_eq!(scan_computations(&dag), (2, 2.0));
}

/// Query consumers are unchanged: with both chains built at query time
/// (not materialized), each query computes the `Scan` at its own rate.
#[test]
fn query_consumers_still_count_once_each() {
    let scan = scan();
    let range = |width_ms| {
        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_millis(width_ms),
            kind: TimeRangeKind::Range,
            child: Rc::clone(&scan),
        }))
        .unwrap()
    };
    let build = |input: Rc<OperatorNode>, algorithm| {
        OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
            child: input,
            family: quantile_sketch(algorithm),
            input: SummaryUpdate::column(ColumnRef::Named("value".into())),
            reduction: Reduction::by(vec![2]),
            grouping: GroupingStrategy::default(),
            filter: None,
        }))
        .unwrap()
    };
    let dag = dag_of(
        &[
            build(range(60_000), SketchAlgorithm::Kll),
            build(range(60_000), SketchAlgorithm::DDSketch),
        ],
        None,
    );
    assert_eq!(scan_computations(&dag), (2, 0.2));
}

/// Two 10 s chains and a 60 s chain over one `Scan`: two refresh windows,
/// two ingestion runs.
#[test]
fn a_scan_under_chains_of_10_10_and_60_s_is_computed_for_each_width() {
    let scan = scan();
    let dag = dag_of(
        &[
            chain(&scan, SketchAlgorithm::Kll, 10_000, 6),
            chain(&scan, SketchAlgorithm::DDSketch, 10_000, 30),
            chain(&scan, SketchAlgorithm::Kll, 60_000, 5),
        ],
        Some(PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION),
    );
    assert_eq!(scan_computations(&dag), (2, 2.0));
}

/// A 10 s chain over the `Scan` and a 10 s chain over the `Scan` shifted by
/// 5 s end their tumbling windows on different grids (phases 0 and 5 s): two runs.
#[test]
fn chains_of_one_width_and_two_phases_are_computed_apart() {
    let scan = scan();
    let dag = dag_of(
        &[
            chain(&scan, SketchAlgorithm::Kll, 10_000, 6),
            chain(
                &shifted(&scan, 5_000),
                SketchAlgorithm::DDSketch,
                10_000,
                30,
            ),
        ],
        Some(PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION),
    );
    assert_eq!(scan_computations(&dag), (2, 2.0));
}

/// Two 10 s chains over one `TimeShift(10 s)` of the `Scan` (phase 0 for
/// both): the shift and the `Scan` below it run once per run.
#[test]
fn nodes_below_a_shared_intermediate_node_are_computed_once_per_run() {
    let shift = shifted(&scan(), 10_000);
    let dag = dag_of(
        &[
            chain(&shift, SketchAlgorithm::Kll, 10_000, 6),
            chain(&shift, SketchAlgorithm::DDSketch, 10_000, 30),
        ],
        Some(PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION),
    );
    assert_eq!(scan_computations(&dag), (1, 1.0));
    assert_eq!(
        computations_of(&dag, |op| matches!(op, NonASAPOp::TimeShift { .. })),
        (1, 1.0)
    );
}

/// Two summaries materialized at ingestion over rows with no time window
/// (`Values`) have no refresh window: each counts alone.
#[test]
fn consumers_of_unbounded_windows_count_alone() {
    let rows = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Values {
        rows: vec![],
        schema: scan().schema.clone(),
    }))
    .unwrap();
    let build = |algorithm| {
        OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
            child: Rc::clone(&rows),
            family: quantile_sketch(algorithm),
            input: SummaryUpdate::column(ColumnRef::Named("value".into())),
            reduction: Reduction::by(vec![2]),
            grouping: GroupingStrategy::default(),
            filter: None,
        }))
        .unwrap()
    };
    let dag = dag_of(
        &[
            build(SketchAlgorithm::Kll),
            build(SketchAlgorithm::DDSketch),
        ],
        Some(PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION),
    );
    assert_eq!(
        computations_of(&dag, |op| matches!(op, NonASAPOp::Values { .. })),
        (2, 2.0)
    );
}

/// A node whose timing and lifetime cannot go together (materialized for one
/// evaluation at ingestion time) is reported, not skipped.
#[test]
fn an_invalid_node_is_reported_not_skipped() {
    let scan = scan();
    let mut dag = dag_of(
        &[chain(&scan, SketchAlgorithm::Kll, 10_000, 6)],
        Some(PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION),
    );
    let chain = dag
        .nodes
        .iter_mut()
        .find(|node| node.lifetime.is_materialized())
        .unwrap();
    chain.lifetime =
        asap_types::physical::Lifetime::OneEvaluation(asap_types::physical::Medium::Memory);
    let chain = chain.id;
    assert_eq!(
        computations(&dag, &[], &[], 3600.0, None),
        Err(
            asap_cost::analytical::AnalyticalCostError::InvalidPhysicalProperties {
                node: chain,
                reason: asap_types::physical::InvalidPhysicalProperties::OneEvaluationAtIngestion,
            }
        )
    );
}
