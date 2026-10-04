// Fixture helpers for this crate's tests: the subset of
// `asap-logical-optimizer`'s `test_support` that Stage 2 tests use.

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
