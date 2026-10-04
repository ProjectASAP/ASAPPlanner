//! SQL front end: parse + plan (via DataFusion) → the name-based
//! [`UnresolvedOp`](asap_frontend_common::UnresolvedOp) tree, built directly
//! (issue #179) → [`resolve_root`].
//!
//! Emits the shared front-end tree (`UnresolvedOp` / `UnresolvedScalar`,
//! name-based [`ColumnRef`](asap_types::ir::scalar::ColumnRef)s) directly, rather
//! than a separate per-language relational tree; `resolve_root` binds it into
//! the unified [`OperatorNode`] IR, deriving every schema on the way.
//! Depends on DataFusion only — never on the PromQL parser.

pub mod error;
pub mod frequency;
pub mod sql;

use std::rc::Rc;

use asap_frontend_common::resolve_root;
use asap_types::ir::OperatorNode;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{QueryLanguage, QueryWorkload, SqlDialect};

pub use error::SqlError;
pub use sql::{SqlCatalog, SqlLowerer};

/// Lower a single SQL query string to the resolved, canonical operator DAG,
/// parsed as `SqlDialect::DataFusionSQL`.
///
/// The `catalog` supplies table schemas (used both to plan the SQL with
/// DataFusion and to carry positional column identity into the resolved
/// DAG). `accuracy` is threaded onto every approximate intent as it's built.
pub async fn lower_sql(
    query: &str,
    catalog: &SqlCatalog,
    accuracy: AccuracyTarget,
) -> Result<Rc<OperatorNode>, SqlError> {
    lower_sql_dialect(query, catalog, SqlDialect::DataFusionSQL, accuracy).await
}

/// Lower a single SQL query string under an explicit [`SqlDialect`].
///
/// `ClickhouseSQL` parses via sqlparser's vendored `ClickHouseDialect`
/// (array-lambda syntax, `arr[-1]` indexing). It also teaches DataFusion's
/// planner the ClickHouse-only builtin functions listed in
/// `asap_sql_function_catalog::CLICKHOUSE_BUILTINS` (`uniqExact`, `countIf`)
/// — every other ClickHouse-only builtin still fails to plan.
/// `ElasticSQL` has no vendored parser and always returns `UnsupportedDialect`.
pub async fn lower_sql_dialect(
    query: &str,
    catalog: &SqlCatalog,
    dialect: SqlDialect,
    accuracy: AccuracyTarget,
) -> Result<Rc<OperatorNode>, SqlError> {
    let unresolved = SqlLowerer::with_dialect(catalog, dialect)
        .lower(query, &accuracy)
        .await?;
    // Binding resolves names and derives every node's schema; result-type
    // checks (such as temporal subtraction, whose duration unit the IR cannot
    // represent) surface here as `ResolveDAGError::Schema`.
    let root = resolve_root(&unresolved)?;
    // #509 Example 2: the L2 and entropy idioms become frequency intents.
    Ok(frequency::recognize_frequency_idioms(&root).unwrap_or(root))
}

/// Lower every SQL batch entry in `workload` to an operator DAG.
///
/// One `Result` per entry — errors are per-query, not fatal for the batch.
/// Returns `WrongLanguage` for every entry if the workload is not SQL, and
/// `UnsupportedDialect` for `ElasticSQL` (no vendored parser).
pub async fn lower_sql_batch(
    workload: &QueryWorkload,
    catalog: &SqlCatalog,
) -> Vec<Result<Rc<OperatorNode>, SqlError>> {
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
    let dialect = match &workload.language {
        QueryLanguage::SQL(d) => d.clone(),
        _ => SqlDialect::DataFusionSQL,
    };
    if matches!(dialect, SqlDialect::ElasticSQL) {
        return entries
            .iter()
            .map(|_| Err(SqlError::UnsupportedDialect("ElasticSQL".into())))
            .collect();
    }

    let mut results = Vec::with_capacity(entries.len());
    for entry in entries {
        let accuracy = entry.requirements.accuracy.target();
        results.push(lower_sql_dialect(&entry.query.0, catalog, dialect.clone(), accuracy).await);
    }
    results
}
