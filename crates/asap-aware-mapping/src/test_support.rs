// Fixture helpers for this crate's tests: the subset of
// `asap-logical-optimizer`'s `test_support` that Stage 2/3 tests use.

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
// `OperatorNode::new_shared`, so a fixture is exactly what a front end
// would hand the planner.

use asap_types::ir::operator::agg_intent::AggIntent;
use asap_types::ir::operator::operator_properties::{Reduction, Source};
use asap_types::ir::schema::{ColumnId, DataType, Field, Schema};
use asap_types::ir::{NonASAPOp, Predicate};

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
    columns.extend(
        labels
            .iter()
            .map(|n| Field::plain(*n, DataType::Utf8, true)),
    );
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
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
        source,
        predicates: vec![],
        schema,
    }))
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
pub(crate) fn agg(
    by: Vec<ColumnId>,
    intent: AggIntent,
    child: Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    aggregate(Reduction::by(by), vec![intent], vec![], None, child)
}
