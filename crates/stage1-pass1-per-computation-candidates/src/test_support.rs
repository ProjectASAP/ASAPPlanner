//! Test helpers: the Pass 1 internals and the PromQL lowering other crates'
//! tests call (feature `test-support`), and the fixtures this crate's own
//! tests share (`cfg(test)`).

use std::rc::Rc;

// Pass 1 internals other crates' tests call: Pass 1's working form.
pub use crate::computation_targets::generate_per_computation_candidates;

use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::OperatorNode;
use asap_types::workload::{
    BatchEntry, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload,
};

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
