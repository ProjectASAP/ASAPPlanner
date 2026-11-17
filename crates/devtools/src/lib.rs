//! Developer tools over the planner: the `asap_planner` inspection CLI and
//! the other bins, plus the helpers they and the integration tests share.
//!
//! Re-exports both language paths so a caller can depend on a single crate for
//! PromQL *and* SQL. Both front ends end at the canonical intent algebra via
//! the same unified operator IR ([`asap_types::ir::OperatorNode`]).
//!
//! [`planning_stages`] holds the one pipeline that `asap_planner` and the
//! integration tests share; [`candidate_labels`] names the
//! candidates for the viewer document. [`corpora`] splits the checked-in
//! query corpora and holds their SQL catalogs. [`rqe_atomic_costs`] converts
//! sketch-bench's measured per-operation costs into plan selection's
//! measured table. [`planner_fuzz`] is the metamorphic fuzz tester for
//! missed reuse and sharing behind the `planner_fuzz` bin.

pub mod candidate_labels;
pub mod corpora;
pub mod planner_fuzz;
pub mod planning_stages;
pub mod planning_stages_text;
pub mod rqe_atomic_costs;
pub mod stable_output;

use asap_frontend_promql::lower_promql_query_workload;
pub use asap_frontend_promql::PromqlError;
pub use asap_frontend_sql::{lower_sql, SqlCatalog, SqlError};

/// Lower one developer-supplied PromQL query whose metrics are each sampled
/// every `interval_ms`, declared on each metric's source facts. Tools
/// intentionally require the sample interval rather than choosing a
/// default.
pub fn lower_promql_with_sample_interval(
    query: &str,
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
                requirements: QueryRequirements::default(),
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
                range_evaluation: None,
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload::sampled_every(
            asap_frontend_promql::promql_metric_names(query)?,
            DurationMs(interval_ms),
        )),
    };
    // One workload entry lowers to one root.
    lower_promql_query_workload(&workload)?
        .remove(0)
        .as_operator()
        .cloned()
        .ok_or_else(|| {
            PromqlError::UnsupportedFeature("scalar root: use lower_promql_query_workload".into())
        })
}
