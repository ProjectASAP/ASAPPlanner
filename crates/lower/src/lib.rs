//! L1→L3 lowering passes for the ASAP controller core.
//!
//! Each query language has one pass that ends at the same intent-algebra
//! [`QueryExpr`](asap_control_core::intent_algebra::expr::QueryExpr): L1 parse
//! (delegated to a language parser crate), L2 per-language tree (the parser's
//! own AST), and L3 lowering to the language- and deployment-independent intent
//! algebra. PromQL lives in [`promql`]; the SQL path (PR #4) lives alongside it.

pub mod error;
pub mod promql;
pub mod schema_pass;

use asap_control_core::intent_algebra::expr::QueryExpr;
use asap_control_core::intent_algebra::schema::SchemaCatalog;
use asap_control_core::types::AccuracyTarget;
use asap_control_core::workload::{QueryLanguage, QueryWorkload};

pub use error::LoweringError;
pub use promql::PromqlLowerer;
pub use schema_pass::populate_schemas;

/// Lower a single PromQL query string to an intent-algebra `QueryExpr`.
///
/// The returned tree has empty schemas on every node; call [`populate_schemas`]
/// before inspecting node schemas or passing the tree to schema-aware stages.
pub fn lower_promql(
    query: &str,
    catalog: &SchemaCatalog,
    accuracy: AccuracyTarget,
) -> Result<QueryExpr, LoweringError> {
    PromqlLowerer::new(catalog, accuracy).lower(query)
}

/// Lower every PromQL batch entry in `workload` to a `QueryExpr`.
///
/// One `Result` per entry — errors are per-query, not fatal for the batch.
/// Returns an empty `Vec` if `workload.query_batch` is absent or empty, and a
/// `WrongLanguage` error for every entry if the workload language is not PromQL.
pub fn lower_promql_batch(
    workload: &QueryWorkload,
    catalog: &SchemaCatalog,
) -> Vec<Result<QueryExpr, LoweringError>> {
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
            lower_promql(&entry.query.0, catalog, accuracy)
        })
        .collect()
}
