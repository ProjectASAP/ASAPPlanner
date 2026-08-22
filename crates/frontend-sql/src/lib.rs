//! SQL front end: parse + plan (via DataFusion) → the canonical, unresolved
//! shape, built directly (issue #179) → [`resolve_root`].
//!
//! Emits [`UnresolvedQueryExpr`](asap_types::pre_asap::UnresolvedQueryExpr) itself — the
//! canonical `QueryExpr`, generic over an unresolved
//! [`ColumnRef`](asap_types::pre_asap::ColumnRef) — directly, rather than a
//! separate per-language relational tree; `resolve_root` runs the
//! [`Binder`](asap_types::pre_asap::Binder) for positional name resolution.
//! Depends on DataFusion only — never on the PromQL parser.

pub mod error;
pub mod sql;

use asap_types::pre_asap::resolve_root;
use asap_types::pre_asap::QueryExpr;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{QueryLanguage, QueryWorkload, SqlDialect};

pub use error::SqlError;
pub use sql::{SqlCatalog, SqlLowerer};

/// Lower a single SQL query string to the canonical, resolved `QueryExpr`,
/// parsed as `SqlDialect::DataFusionSQL`.
///
/// The `catalog` supplies table schemas (used both to plan the SQL with
/// DataFusion and to carry positional column identity into the resolved
/// tree). `accuracy` is threaded onto every approximate intent as it's built.
pub async fn lower_sql(
    query: &str,
    catalog: &SqlCatalog,
    accuracy: AccuracyTarget,
) -> Result<QueryExpr, SqlError> {
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
) -> Result<QueryExpr, SqlError> {
    let unresolved = SqlLowerer::with_dialect(catalog, dialect)
        .lower(query, &accuracy)
        .await?;
    let resolved = resolve_root(&unresolved)?;
    Ok(resolved)
}

/// Lower every SQL batch entry in `workload` to a `QueryExpr`.
///
/// One `Result` per entry — errors are per-query, not fatal for the batch.
/// Returns `WrongLanguage` for every entry if the workload is not SQL, and
/// `UnsupportedDialect` for `ElasticSQL` (no vendored parser).
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
        let accuracy = entry
            .requirements
            .as_ref()
            .and_then(|r| r.accuracy.clone())
            .unwrap_or(AccuracyTarget::Exact);
        results.push(lower_sql_dialect(&entry.query.0, catalog, dialect.clone(), accuracy).await);
    }
    results
}
