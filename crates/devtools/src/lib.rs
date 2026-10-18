//! `asap-lower` — thin facade over the two query front ends.
//!
//! Re-exports both language paths so a caller can depend on a single crate for
//! PromQL *and* SQL. Both front ends end at the canonical intent algebra via
//! the same unified operator IR ([`asap_types::ir::OperatorNode`]).
//!
//! ## Dependency isolation
//!
//! Depending on this facade pulls **both** parsers (`promql-parser` and
//! DataFusion). A caller that needs only one language should depend on the
//! matching front-end crate directly — [`asap_frontend_promql`] (PromQL parser
//! only) or [`asap_frontend_sql`] (DataFusion only) — so it never compiles the
//! other's parser.
//!
//! [`planning_stages`] and [`planner_layering_examples`] hold the one
//! pipeline and the #509 example workloads that `stage_pipeline` and the
//! planner-layering acceptance tests share; [`candidate_labels`] names the
//! candidates for the viewer document. [`rqe_atomic_costs`] converts
//! sketch-bench's measured per-operation costs into plan selection's
//! measured table.

pub mod candidate_labels;
pub mod planner_layering_examples;
pub mod planning_stages;
pub mod rqe_atomic_costs;

pub use asap_frontend_promql::{
    lower_promql_workload, lower_promql_workload_with_histograms, PromqlError,
};
pub use asap_frontend_sql::{lower_sql, lower_sql_batch, SqlCatalog, SqlError, SqlLowerer};

/// Lower one developer-supplied PromQL query whose metrics are each sampled
/// every `interval_ms`, declared on each metric's source facts. Tools
/// intentionally require the sample interval rather than choosing a
/// default.
pub fn lower_promql_with_sample_interval(
    query: &str,
    accuracy: asap_types::ir::properties::AccuracyTarget,
    interval_ms: u64,
) -> Result<std::rc::Rc<asap_types::ir::OperatorNode>, PromqlError> {
    use asap_types::workload::{
        BatchEntry, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query,
        QueryRequirements, QueryWorkload,
    };

    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: asap_types::workload::QueryLanguage::PromQL,
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
            asap_frontend_promql::promql_metric_names(query)?,
            DurationMs(interval_ms),
        )),
    };
    // One workload entry lowers to one root.
    Ok(lower_promql_workload(&workload)?.remove(0))
}
