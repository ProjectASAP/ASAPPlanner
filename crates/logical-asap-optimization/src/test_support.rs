// Shared fixture helpers for the stage crates' tests (feature
// `test-support`); not every test module uses every helper.
#![allow(dead_code)]

use std::rc::Rc;

use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::OperatorNode;
use asap_types::workload::{
    BatchEntry, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload,
};

/// The statistic classic HLL's failure probability at `precision` and
/// `relative_error` over `source` is left as, for Stage 3 tests that bind
/// it. The estimator keeps the format crate-private.
pub fn hll_classic_failure_statistic(precision: u8, relative_error: f64, source: &str) -> String {
    crate::accuracy::estimators::hll::classic_failure_statistic(precision, relative_error, source)
}

pub fn lower_promql(query: &str, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy,
                    ..Default::default()
                },
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload::sampled_every(
            asap_frontend_promql::promql_metric_names(query).unwrap_or_default(),
            DurationMs(1_000),
        )),
    };
    asap_frontend_promql::lower_promql_workload(&workload)
        .unwrap()
        .pop()
        .unwrap()
}

// ── Shared pre-ASAP fixture builders ─────────────────────────────────────
//
// Every builder returns an `Rc<OperatorNode>` whose schema is derived by
// `OperatorNode::new_shared`, so a fixture is exactly what a front end
// would hand the planner. Added by the test migration; only add here, never
// rename or remove (several test modules share these).

use std::time::Duration;

use asap_types::ir::operator::agg_intent::AggIntent;
use asap_types::ir::operator::operator_properties::{GroupKeys, Reduction, Source};
use asap_types::ir::properties::timing::{
    apply_materialization_timings, MaterializationAssignment, TimingMemo,
};
use asap_types::ir::schema::{ColumnId, DataType, Field, Schema};
use asap_types::ir::{NonASAPOp, Predicate, ScalarExpr, TimeRangeKind};
use asap_types::physical::{Materialization, Medium};

/// A `TimeSeries("m")` scan over `[ts(0), value(1), labels...]`, time index 0,
/// no unique key.
pub fn metric_scan(labels: &[&str]) -> Rc<OperatorNode> {
    metric_scan_with_keys(labels, vec![])
}

/// [`metric_scan`] with explicit `unique_keys` (a `[[0]]` key makes CSE
/// willing to hoist the scan).
pub fn metric_scan_with_keys(labels: &[&str], unique_keys: Vec<Vec<ColumnId>>) -> Rc<OperatorNode> {
    let mut columns = vec![
        Field::plain("ts", DataType::Timestamp, false),
        Field::plain("value", DataType::Float64, false),
    ];
    columns.extend(
        labels
            .iter()
            .map(|n| Field::plain(*n, DataType::Utf8, true)),
    );
    scan("m", Schema::with_time_index(columns, 0, unique_keys))
}

/// A predicate-free `TimeSeries(metric)` scan with the given schema.
pub fn scan(metric: &str, schema: Schema) -> Rc<OperatorNode> {
    scan_from(
        Source::TimeSeries {
            metric: metric.into(),
        },
        schema,
    )
}

pub fn scan_from(source: Source, schema: Schema) -> Rc<OperatorNode> {
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
        source,
        predicates: vec![],
        schema,
    }))
    .unwrap()
}

/// A general aggregate node.
pub fn aggregate(
    reduction: Reduction,
    measures: Vec<AggIntent>,
    output_names: Vec<String>,
    having: Option<Predicate>,
    child: Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Aggregate {
        reduction,
        measures,
        output_names,
        filters: vec![],
        having,
        child,
    }))
    .unwrap()
}

/// `intent by (by)` — a single-measure, `HAVING`-free grouped aggregate.
pub fn agg(by: Vec<ColumnId>, intent: AggIntent, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    aggregate(Reduction::by(by), vec![intent], vec![], None, child)
}

/// `intent without (excluded)`.
pub fn without_agg(
    excluded: Vec<ColumnId>,
    intent: AggIntent,
    child: Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    aggregate(
        Reduction::Reduce(GroupKeys::without(excluded)),
        vec![intent],
        vec![],
        None,
        child,
    )
}

pub fn filter(pred: ScalarExpr, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Filter {
        pred: Predicate(pred),
        child,
    }))
    .unwrap()
}

pub fn dedup(cols: Vec<ColumnId>, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Dedup {
        cols,
        child,
    }))
    .unwrap()
}

/// An explicit range selector `child[range]`.
pub fn time_range(range: Duration, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::TimeRange {
        range,
        kind: TimeRangeKind::Range,
        child,
    }))
    .unwrap()
}

/// `root` timed under the default (every sub-DAG `NotMaterialized`)
/// assignment — the shape export and the post-ASAP validators consume.
pub fn timed(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    apply_materialization_timings(
        root,
        &MaterializationAssignment::default(),
        &mut TimingMemo::new(),
    )
    .expect("default materialization timings apply")
}

/// `root` timed with every sub-DAG `MatAtIngestion` where the DAG allows it.
pub fn at_ingestion(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    apply_materialization_timings(
        root,
        &MaterializationAssignment::with_default(Materialization::MatAtIngestion(Medium::Memory)),
        &mut TimingMemo::new(),
    )
    .expect("at-ingestion materialization timings apply")
}

/// The Count-Min summary of a Stage 1 candidate of `query`.
pub fn cms_summary(query: &str) -> OperatorNode {
    use crate::deployment_capabilities::DeploymentCapabilities;
    use crate::per_computation_candidates::generate_per_computation_candidates;
    use crate::workload_candidates::stage1_logical_asap_optimization;
    use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
    use asap_types::ir::{ASAPOp, QueryRoot};
    let target = AccuracyTarget::max_error(0.01).failure_probability(0.01);
    let root = lower_promql(query, target);
    // The Stage 1 candidates the planner pipeline enumerates for `query`
    // with the built-in deployment and an unknown recurrence.
    let per_computation = generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))])
        .unwrap()
        .without_unsupported_summaries(&DeploymentCapabilities::default());
    let summary = stage1_logical_asap_optimization(
        &per_computation,
        &[asap_types::workload::QueryRecurrence::Unknown],
    )
    .filter_map(|candidate| candidate.roots.ok())
    .flat_map(|roots| match &roots[0] {
        QueryRoot::Operator(node) => OperatorNode::reachable(node),
        QueryRoot::Scalar(_) => vec![],
    })
    .find(|node| {
        matches!(
            node.asap(),
            Some(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _),
                ..
            }) if *kind.algorithm() == SketchAlgorithm::Cms
        )
    })
    .map(|node| (*node).clone())
    .unwrap_or_else(|| panic!("{query}: a Count-Min summary"));
    summary
}

/// `summary` with its update weight replaced by `weight`.
pub fn weighted(
    mut summary: OperatorNode,
    weight: asap_types::ir::schema::SummaryInputExpr,
) -> Rc<OperatorNode> {
    use asap_types::ir::{ASAPOp, Operator};
    if let Operator::ASAP(ASAPOp::SummaryAgg { input, .. }) = &mut summary.operator {
        input.weight = weight;
    }
    Rc::new(summary)
}

/// A point-count estimate over `summary` carrying `guarantee`.
pub fn estimate_over(
    summary: Rc<OperatorNode>,
    guarantee: asap_types::ir::properties::AccuracyGuarantee,
) -> Rc<OperatorNode> {
    use asap_types::ir::scalar::ColumnRef;
    use asap_types::ir::schema::SketchStatistic;
    use asap_types::ir::{ASAPOp, Operator};
    Rc::new(
        OperatorNode::new(Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: summary,
            query: SketchStatistic::PointCount {
                key: ColumnRef::SampleValue,
                value: None,
            },
        }))
        .unwrap()
        .with_guarantee(Some(guarantee)),
    )
}

/// The workload entry of a query evaluated as `recurrence` says, exact and
/// with no latency bound.
pub fn entry(
    recurrence: asap_types::workload::QueryRecurrence,
) -> asap_types::workload::QueryWorkloadEntry {
    entry_with(AccuracyTarget::exact(), recurrence)
}

/// The workload entry of a query with accuracy target `accuracy`, evaluated
/// as `recurrence` says, with no latency bound.
pub fn entry_with(
    accuracy: AccuracyTarget,
    recurrence: asap_types::workload::QueryRecurrence,
) -> asap_types::workload::QueryWorkloadEntry {
    asap_types::workload::QueryWorkloadEntry {
        requirements: QueryRequirements {
            accuracy,
            max_latency_ms: None,
        },
        recurrence,
        ..Default::default()
    }
}
