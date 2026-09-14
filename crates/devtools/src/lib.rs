//! `asap-lower` — thin facade over the two query front ends.
//!
//! Re-exports both language paths so a caller can depend on a single crate for
//! PromQL *and* SQL. Both front ends end at the canonical intent algebra via
//! the same shared [`resolve_root`](asap_types::pre_asap::resolve_root).
//!
//! ## Dependency isolation
//!
//! Depending on this facade pulls **both** parsers (`promql-parser` and
//! DataFusion). A caller that needs only one language should depend on the
//! matching front-end crate directly — [`asap_frontend_promql`] (PromQL parser
//! only) or [`asap_frontend_sql`] (DataFusion only) — so it never compiles the
//! other's parser.

pub use asap_frontend_promql::{
    lower_promql_workload, lower_promql_workload_with_histograms, PromqlError,
};
pub use asap_frontend_sql::{lower_sql, lower_sql_batch, SqlCatalog, SqlError, SqlLowerer};

/// Lower one developer-supplied PromQL query with an explicit source cadence.
/// Tools intentionally require the cadence rather than choosing a default.
pub fn lower_promql_with_data_ingestion_interval(
    query: &str,
    accuracy: asap_types::types::AccuracyTarget,
    interval_ms: u64,
) -> Result<asap_types::pre_asap::QueryExpr, PromqlError> {
    use asap_types::workload::{
        BatchEntry, DataWorkload, DurationMs, Evidence, Predictability, Query, QueryRequirements,
        QueryWorkload, TimeSelection,
    };

    let workload = QueryWorkload {
        language: asap_types::workload::QueryLanguage::PromQL,
        query_batch: Some(vec![BatchEntry {
            query: Query(query.into()),
            requirements: QueryRequirements {
                accuracy: asap_types::workload::AccuracyRequirement::Explicit(accuracy),
                ..Default::default()
            },
            predictability: Predictability::Unknown,
            invocations: 1,
            execute_at: None,
            time_selection: TimeSelection::default(),
        }]),
        repeating_queries: None,
        data_workload: Some(DataWorkload {
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(interval_ms)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    lower_promql_workload(&workload)?.into_iter().next().ok_or({
        PromqlError::InvalidWorkload(asap_types::workload::WorkloadError::MissingPromqlDataWorkload)
    })
}
