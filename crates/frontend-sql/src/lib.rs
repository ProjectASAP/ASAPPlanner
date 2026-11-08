//! SQL front end: parse + plan (via DataFusion) → the name-based
//! [`UnresolvedOp`](asap_frontend_common::UnresolvedOp) tree, built directly
//! (issue #179) → [`resolve_root`].
//!
//! Emits the shared front-end tree (`UnresolvedOp` / `UnresolvedScalar`,
//! name-based [`ColumnRef`](asap_types::ir::scalar::ColumnRef)s) directly, rather
//! than a separate per-language relational tree; `resolve_root` binds it into
//! the unified [`OperatorNode`] IR, deriving every schema on the way.
//! Depends on DataFusion only — never on the PromQL parser.

mod error;
mod frequency;
mod sql;
#[cfg(feature = "test-support")]
pub mod test_support;

use std::rc::Rc;

use asap_frontend_common::resolve_root;
use asap_types::ir::OperatorNode;
use asap_types::workload::SqlDialect;

pub use error::SqlError;
pub use sql::SqlCatalog;
use sql::SqlLowerer;

/// Lower a single SQL query string to the resolved, canonical operator DAG,
/// parsed as `SqlDialect::DataFusionSQL`.
///
/// The `catalog` supplies table schemas (used both to plan the SQL with
/// DataFusion and to carry positional column identity into the resolved
/// DAG).
pub async fn lower_sql(query: &str, catalog: &SqlCatalog) -> Result<Rc<OperatorNode>, SqlError> {
    lower_sql_dialect(query, catalog, SqlDialect::DataFusionSQL).await
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
) -> Result<Rc<OperatorNode>, SqlError> {
    let unresolved = SqlLowerer::with_dialect(catalog, dialect)
        .lower(query)
        .await?;
    // Binding resolves names and derives every node's schema; result-type
    // checks (such as temporal subtraction, whose duration unit the IR cannot
    // represent) surface here as `ResolveDAGError::Schema`.
    let root = resolve_root(&unresolved)?;
    // #509 Example 2: the L2 and entropy idioms become frequency intents.
    Ok(frequency::recognize_frequency_idioms(&root).unwrap_or(root))
}
