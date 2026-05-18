pub mod error;
pub mod schema_pass;
pub mod sql;

use asap_control_core::intent_algebra::expr::QueryExpr;
use asap_control_core::intent_algebra::schema::SchemaCatalog;
use asap_control_core::types::AccuracyTarget;
use asap_control_core::workload::{QueryLanguage, QueryWorkload, SqlDialect};

pub use error::LoweringError;
pub use schema_pass::populate_schemas;
pub use sql::SqlLowerer;

/// Lower every SQL batch entry in `workload` to a `QueryExpr`.
///
/// One `Result` per entry — errors are per-query, not fatal for the batch.
/// Returns an empty `Vec` if `workload.query_batch` is absent or empty.
/// Returns `WrongLanguage` for every entry if the workload language is not SQL.
pub async fn lower_batch(
    workload: &QueryWorkload,
    catalog: &SchemaCatalog,
) -> Vec<Result<QueryExpr, LoweringError>> {
    let entries = match &workload.query_batch {
        Some(e) if !e.is_empty() => e,
        _ => return vec![],
    };

    // Guard: only SQL languages are handled by this lowerer.
    if !matches!(workload.language, QueryLanguage::SQL(_) | QueryLanguage::DataFusion) {
        let lang = format!("{:?}", workload.language);
        return entries
            .iter()
            .map(|_| Err(LoweringError::WrongLanguage(lang.clone())))
            .collect();
    }

    // Guard: only DataFusionSQL is implemented; other SQL dialects need their
    // own parser and plan-conversion layer before reaching SqlLowerer.
    if let QueryLanguage::SQL(dialect) = &workload.language {
        if !matches!(dialect, SqlDialect::DataFusionSQL) {
            let d = format!("{dialect:?}");
            return entries
                .iter()
                .map(|_| Err(LoweringError::UnsupportedDialect(d.clone())))
                .collect();
        }
    }

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
