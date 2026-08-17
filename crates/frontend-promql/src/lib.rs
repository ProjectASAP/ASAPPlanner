//! PromQL front end: L1 (parse via `promql-parser`) → the canonical L2 shape,
//! built directly (issue #179) → [`resolve_root`].
//!
//! Emits [`L2QueryExpr`](asap_types::pre_asap::L2QueryExpr) itself — the
//! canonical `QueryExpr`, generic over an unresolved
//! [`ColumnRef`](asap_types::pre_asap::ColumnRef) — directly, rather than a
//! separate per-language relational tree; `resolve_root` runs the
//! [`Binder`](asap_types::pre_asap::Binder) for positional name resolution.
//! Depends on the PromQL parser only — never on the SQL / DataFusion stack.

pub mod error;
pub mod histogram;
pub mod promql;

use asap_types::pre_asap::resolve_root;
use asap_types::pre_asap::QueryExpr;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{QueryLanguage, QueryWorkload};

pub use error::PromqlError;
pub use histogram::{HistogramCatalog, HistogramKind};
pub use promql::PromqlLowerer;

/// Lower a single PromQL query string to the canonical L3 `QueryExpr`.
///
/// `accuracy` is threaded onto every approximate intent (`Count`, `Quantile`,
/// `Cardinality`, `TopK`). The returned tree carries a self-contained `Schema`
/// on its `Scan`; call [`QueryExpr::output_schema`] for any node's schema.
///
/// `histogram_quantile` discrimination uses the structural heuristic; to drive
/// it from declared sample types instead, use [`lower_promql_with_histograms`].
pub fn lower_promql(query: &str, accuracy: AccuracyTarget) -> Result<QueryExpr, PromqlError> {
    let l2 = PromqlLowerer::lower(query, &accuracy)?;
    let l3 = resolve_root(&l2)?;
    Ok(l3)
}

/// Like [`lower_promql`], but consults `histograms` to decide whether a
/// `histogram_quantile` argument is sketch-able (generic `Quantile`) or a
/// classic-bucket interpolation (`HistogramQuantile`) — a type-driven decision
/// instead of the structural heuristic (issue #79). Metrics absent from the
/// catalog still fall back to the heuristic.
pub fn lower_promql_with_histograms(
    query: &str,
    accuracy: AccuracyTarget,
    histograms: HistogramCatalog,
) -> Result<QueryExpr, PromqlError> {
    let _guard = histogram::CatalogGuard::install(histograms);
    lower_promql(query, accuracy)
}

/// Lower every PromQL batch entry in `workload` to a `QueryExpr`.
///
/// One `Result` per entry — errors are per-query, not fatal for the batch.
/// Returns an empty `Vec` if `workload.query_batch` is absent or empty, and a
/// `WrongLanguage` error for every entry if the workload language is not PromQL.
pub fn lower_promql_batch(workload: &QueryWorkload) -> Vec<Result<QueryExpr, PromqlError>> {
    let entries = match &workload.query_batch {
        Some(e) if !e.is_empty() => e,
        _ => return vec![],
    };

    if !matches!(workload.language, QueryLanguage::PromQL) {
        let lang = format!("{:?}", workload.language);
        return entries
            .iter()
            .map(|_| Err(PromqlError::WrongLanguage(lang.clone())))
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
