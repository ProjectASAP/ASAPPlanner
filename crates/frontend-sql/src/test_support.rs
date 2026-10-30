//! Lowering steps only tests call, exposed under the `test-support` feature.
use asap_frontend_common::UnresolvedOp;
use asap_types::ir::properties::AccuracyTarget;

use crate::sql::{SqlCatalog, SqlLowerer};
use crate::SqlError;

/// `query` lowered to the name-based tree, parsed as
/// `SqlDialect::DataFusionSQL`, before `resolve_root` binds it and before
/// frequency recognition.
pub async fn lower_unresolved(
    query: &str,
    catalog: &SqlCatalog,
    accuracy: &AccuracyTarget,
) -> Result<UnresolvedOp, SqlError> {
    SqlLowerer::new(catalog).lower(query, accuracy).await
}
