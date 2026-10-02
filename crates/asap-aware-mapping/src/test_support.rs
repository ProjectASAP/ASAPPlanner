// Shared fixture helpers; not every test module uses every helper.
#![allow(dead_code)]

use std::rc::Rc;

use asap_types::ir::OperatorNode;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence, PlanningWorkload,
    Predictability, Query, QueryLanguage, QueryRequirements, QueryWorkload, TimeSelection,
};

pub(crate) fn lower_promql(query: &str, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(accuracy),
                    ..Default::default()
                },
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
                time_selection: TimeSelection::default(),
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(1_000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    asap_frontend_promql::lower_promql_workload(&workload, 0)
        .unwrap()
        .pop()
        .unwrap()
}

// ── Shared pre-ASAP fixture builders ─────────────────────────────────────
//
// Every builder returns an `Rc<OperatorNode>` whose schema is derived by
// `OperatorNode::non_asap_node`, so a fixture is exactly what a front end
// would hand the planner. Added by the test migration; only add here, never
// rename or remove (several test modules share these).

use std::time::Duration;

use asap_types::ir::timing::{apply_lifecycle_timings, LifecycleAssignment, TimingMemo};
use asap_types::ir::{NonASAPOp, Predicate, ScalarExpr, TimeRangeKind};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::pre_asap::query_expr::{GroupKeys, Reduction, Source};
use asap_types::pre_asap::schema::{ColumnId, DataType, Field, Schema};

/// A `TimeSeries("m")` scan over `[ts(0), value(1), labels...]`, time index 0,
/// no unique key.
pub(crate) fn metric_scan(labels: &[&str]) -> Rc<OperatorNode> {
    metric_scan_with_keys(labels, vec![])
}

/// [`metric_scan`] with explicit `unique_keys` (a `[[0]]` key makes CSE
/// willing to hoist the scan).
pub(crate) fn metric_scan_with_keys(
    labels: &[&str],
    unique_keys: Vec<Vec<ColumnId>>,
) -> Rc<OperatorNode> {
    let mut columns = vec![
        Field::plain("ts", DataType::Timestamp, false),
        Field::plain("value", DataType::Float64, false),
    ];
    columns.extend(labels.iter().map(|n| Field::plain(*n, DataType::Utf8, true)));
    scan("m", Schema::with_time_index(columns, 0, unique_keys))
}

/// A predicate-free `TimeSeries(metric)` scan with the given schema.
pub(crate) fn scan(metric: &str, schema: Schema) -> Rc<OperatorNode> {
    scan_from(
        Source::TimeSeries {
            metric: metric.into(),
        },
        schema,
    )
}

pub(crate) fn scan_from(source: Source, schema: Schema) -> Rc<OperatorNode> {
    OperatorNode::non_asap_node(NonASAPOp::Scan {
        source,
        predicates: vec![],
        schema,
    })
    .unwrap()
}

/// A general aggregate node.
pub(crate) fn aggregate(
    reduction: Reduction,
    measures: Vec<AggIntent>,
    output_names: Vec<String>,
    having: Option<Predicate>,
    child: Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    OperatorNode::non_asap_node(NonASAPOp::Aggregate {
        reduction,
        measures,
        output_names,
        having,
        child,
    })
    .unwrap()
}

/// `intent by (by)` — a single-measure, `HAVING`-free grouped aggregate.
pub(crate) fn agg(by: Vec<ColumnId>, intent: AggIntent, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    aggregate(Reduction::by(by), vec![intent], vec![], None, child)
}

/// `intent without (excluded)`.
pub(crate) fn without_agg(
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

/// A per-entity (per-series) single-measure aggregate.
pub(crate) fn agg_per_entity(intent: AggIntent, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    aggregate(Reduction::PerEntity, vec![intent], vec![], None, child)
}

pub(crate) fn filter(pred: ScalarExpr, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    OperatorNode::non_asap_node(NonASAPOp::Filter {
        pred: Predicate(pred),
        child,
    })
    .unwrap()
}

pub(crate) fn dedup(cols: Vec<ColumnId>, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    OperatorNode::non_asap_node(NonASAPOp::Dedup { cols, child }).unwrap()
}

/// An explicit range selector `child[range]`.
pub(crate) fn time_range(range: Duration, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    OperatorNode::non_asap_node(NonASAPOp::TimeRange {
        range,
        kind: TimeRangeKind::Range,
        child,
    })
    .unwrap()
}

/// `root` timed under the default (every summary maintained) lifecycle
/// assignment — the shape export and the post-ASAP validators consume.
pub(crate) fn timed(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    apply_lifecycle_timings(
        root,
        &LifecycleAssignment::default_maintained(),
        &mut TimingMemo::new(),
    )
    .expect("default lifecycle timings apply")
}

/// Time `root` under the default lifecycle assignment (which runs every
/// data-state / population-contract check) and export it as a post-ASAP DAG —
/// the replacement for the old one-step `post_asap::compile_post_asap_dag`.
pub(crate) fn time_and_export(
    root: &Rc<OperatorNode>,
) -> Result<
    asap_types::ir::export::PostAsapDag,
    asap_types::post_asap::execution_data_state::ExecutionDataStateError,
> {
    let timed = apply_lifecycle_timings(
        root,
        &LifecycleAssignment::default_maintained(),
        &mut TimingMemo::new(),
    )?;
    asap_types::ir::export::compile_post_asap_dag(&timed)
}
