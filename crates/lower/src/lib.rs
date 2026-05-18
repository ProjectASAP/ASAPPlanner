pub mod error;
pub mod sql;

use asap_control_core::intent_algebra::expr::QueryExpr;
use asap_control_core::intent_algebra::schema::SchemaCatalog;
use asap_control_core::types::AccuracyTarget;
use asap_control_core::workload::QueryWorkload;

pub use error::LoweringError;
pub use sql::SqlLowerer;

/// Lower every SQL batch entry in `workload` to a `QueryExpr`.
///
/// One `Result` per entry — errors are per-query, not fatal for the batch.
/// Returns an empty `Vec` if `workload.query_batch` is absent or empty.
pub async fn lower_batch(
    workload: &QueryWorkload,
    catalog: &SchemaCatalog,
) -> Vec<Result<QueryExpr, LoweringError>> {
    let entries = match &workload.query_batch {
        Some(e) if !e.is_empty() => e,
        _ => return vec![],
    };
    let mut results = Vec::with_capacity(entries.len());
    for entry in entries {
        let accuracy = entry
            .requirements
            .as_ref()
            .and_then(|r| r.accuracy.clone())
            .unwrap_or(AccuracyTarget::Exact);
        let lowerer = SqlLowerer::new(catalog, accuracy);
        results.push(lowerer.lower(&entry.query.0).await);
    }
    results
}
