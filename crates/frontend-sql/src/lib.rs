//! SQL front end: L1 (parse + plan via DataFusion) → L2 relational, then the
//! shared L2→L3 [`convert_root`](asap_l2::convert_root).
//!
//! Emits the per-language
//! [`relational::QueryExpr`](asap_l2::relational); the shared
//! converter runs the [`Binder`](asap_l2::Binder) for
//! positional name resolution. Depends on DataFusion only — never on the PromQL
//! parser.

pub mod error;
pub mod sql;

use asap_ir::intent_algebra::QueryExpr;
use asap_ir::types::AccuracyTarget;
use asap_ir::workload::{QueryLanguage, QueryWorkload, SqlDialect};
use asap_l2::convert_root;

pub use error::SqlError;
pub use sql::{SqlCatalog, SqlLowerer};

/// Lower a single SQL query string to the canonical L3 `QueryExpr`.
///
/// The `catalog` supplies table schemas (used both to plan the SQL with
/// DataFusion and to carry positional column identity into L3). `accuracy` is
/// threaded onto every approximate intent by the shared converter.
pub async fn lower_sql(
    query: &str,
    catalog: &SqlCatalog,
    accuracy: AccuracyTarget,
) -> Result<QueryExpr, SqlError> {
    let l2 = SqlLowerer::new(catalog).lower(query).await?;
    let l3 = convert_root(&l2, &accuracy)?;
    Ok(l3)
}

/// Lower every SQL batch entry in `workload` to a `QueryExpr`.
///
/// One `Result` per entry — errors are per-query, not fatal for the batch.
/// Returns `WrongLanguage` for every entry if the workload is not SQL, and
/// `UnsupportedDialect` for non-DataFusion SQL dialects.
pub async fn lower_sql_batch(
    workload: &QueryWorkload,
    catalog: &SqlCatalog,
) -> Vec<Result<QueryExpr, SqlError>> {
    let entries = match &workload.query_batch {
        Some(e) if !e.is_empty() => e,
        _ => return vec![],
    };

    // `DataFusion` is a legacy alias for `SQL(DataFusionSQL)`; accept both.
    if !matches!(
        workload.language,
        QueryLanguage::SQL(_) | QueryLanguage::DataFusion
    ) {
        let lang = format!("{:?}", workload.language);
        return entries
            .iter()
            .map(|_| Err(SqlError::WrongLanguage(lang.clone())))
            .collect();
    }
    if let QueryLanguage::SQL(dialect) = &workload.language {
        if !matches!(dialect, SqlDialect::DataFusionSQL) {
            let d = format!("{dialect:?}");
            return entries
                .iter()
                .map(|_| Err(SqlError::UnsupportedDialect(d.clone())))
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
        results.push(lower_sql(&entry.query.0, catalog, accuracy).await);
    }
    results
}
