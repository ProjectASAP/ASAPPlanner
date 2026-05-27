//! L1→L3 lowering passes for the ASAP controller core.
//!
//! Both front ends end at the canonical intent algebra via the same L2→L3
//! [`convert_root`]: PromQL parses with `promql-parser`, SQL parses + plans with
//! DataFusion. Each emits the per-language
//! [`relational::QueryExpr`](asap_control_core::intent_algebra::relational); the
//! shared converter runs the [`Binder`](asap_control_core::intent_algebra::Binder)
//! for positional name resolution and folds single-statistic sketchable
//! aggregates into canonical shapes.

pub mod error;
pub mod promql;
pub mod sql;

use asap_control_core::intent_algebra::{convert_root, QueryExpr};
use asap_control_core::types::AccuracyTarget;
use asap_control_core::workload::{QueryLanguage, QueryWorkload, SqlDialect};

pub use error::LoweringError;
pub use promql::PromqlLowerer;
pub use sql::{SqlCatalog, SqlLowerer};

/// Lower a single PromQL query string to the canonical L3 `QueryExpr`.
///
/// `accuracy` is threaded onto every approximate intent (`Count`, `Quantile`,
/// `Cardinality`, `TopK`). The returned tree carries a self-contained `Schema`
/// on its `Scan`; call [`QueryExpr::output_schema`] for any node's schema.
pub fn lower_promql(query: &str, accuracy: AccuracyTarget) -> Result<QueryExpr, LoweringError> {
    let l2 = PromqlLowerer::lower(query)?;
    let l3 = convert_root(&l2, &accuracy)?;
    Ok(l3)
}

/// Lower every PromQL batch entry in `workload` to a `QueryExpr`.
///
/// One `Result` per entry — errors are per-query, not fatal for the batch.
/// Returns an empty `Vec` if `workload.query_batch` is absent or empty, and a
/// `WrongLanguage` error for every entry if the workload language is not PromQL.
pub fn lower_promql_batch(workload: &QueryWorkload) -> Vec<Result<QueryExpr, LoweringError>> {
    let entries = match &workload.query_batch {
        Some(e) if !e.is_empty() => e,
        _ => return vec![],
    };

    if !matches!(workload.language, QueryLanguage::PromQL) {
        let lang = format!("{:?}", workload.language);
        return entries
            .iter()
            .map(|_| Err(LoweringError::WrongLanguage(lang.clone())))
            .collect();
    }

    entries
        .iter()
        .map(|entry| {
            let accuracy = entry
                .requirements
                .as_ref()
                .and_then(|r| r.accuracy.clone())
                .unwrap_or(AccuracyTarget::Exact);
            lower_promql(&entry.query.0, accuracy)
        })
        .collect()
}

/// Lower a single SQL query string to the canonical L3 `QueryExpr`.
///
/// The `catalog` supplies table schemas (used both to plan the SQL with
/// DataFusion and to carry positional column identity into L3). `accuracy` is
/// threaded onto every approximate intent by the shared converter.
pub async fn lower_sql(
    query: &str,
    catalog: &SqlCatalog,
    accuracy: AccuracyTarget,
) -> Result<QueryExpr, LoweringError> {
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
) -> Vec<Result<QueryExpr, LoweringError>> {
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
            .map(|_| Err(LoweringError::WrongLanguage(lang.clone())))
            .collect();
    }
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
        results.push(lower_sql(&entry.query.0, catalog, accuracy).await);
    }
    results
}
