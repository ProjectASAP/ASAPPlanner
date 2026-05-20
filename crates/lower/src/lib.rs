//! L1→L3 lowering passes for the ASAP controller core.
//!
//! PromQL flows through three layers, all ending at the canonical intent
//! algebra: L1 parse (`promql-parser`), L2 per-language tree
//! ([`relational::QueryExpr`](asap_control_core::intent_algebra::relational)),
//! and the L2→L3 conversion ([`convert_root`]) that runs the
//! [`Binder`](asap_control_core::intent_algebra::Binder) and folds the
//! single-statistic sketchable aggregate into canonical shapes.

pub mod error;
pub mod promql;

use asap_control_core::intent_algebra::{convert_root, QueryExpr};
use asap_control_core::types::AccuracyTarget;
use asap_control_core::workload::{QueryLanguage, QueryWorkload};

pub use error::LoweringError;
pub use promql::PromqlLowerer;

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
