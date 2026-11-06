//! Fixtures this crate's own tests share. Every builder returns an
//! `Rc<OperatorNode>` whose schema is derived by `OperatorNode::new_shared`,
//! so a fixture is exactly what a front end would hand the planner.

use std::rc::Rc;

use asap_types::ir::operator::operator_properties::Source;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::schema::{ColumnId, DataType, Field, Schema};
use asap_types::ir::{NonASAPOp, OperatorNode};
use asap_types::workload::{
    BatchEntry, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload,
};

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

/// `query` lowered by the PromQL frontend, with accuracy target `accuracy`.
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
                range_evaluation: None,
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload::sampled_every(
            asap_frontend_promql::promql_metric_names(query).unwrap_or_default(),
            DurationMs(1_000),
        )),
    };
    asap_frontend_promql::lower_promql_query_workload(&workload)
        .unwrap()
        .remove(0)
        .as_operator()
        .cloned()
        .expect("operator root")
}
