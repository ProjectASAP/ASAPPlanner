//! Stage 0: the memo groups of the workload's query roots (a [`Memo`]
//! without candidates). Each node of the queries' DAGs becomes the group of
//! its expression, found or added by [`Memo::find_or_add_group`], so
//! identical sub-expressions are one group. Stage 1 starts from these groups.

use asap_types::ir::{QueryRoot, SchemaDerivationError};
use asap_types::stages::Memo;

/// Stage 0: the memo groups of the workload's query roots, query `i`
/// being `roots[i]`, every group without candidates. Fails with the first
/// node that breaks the node rules over its children's groups
/// ([`Memo::find_or_add_group`]).
pub fn memo_of_queries(roots: &[QueryRoot]) -> Result<Memo, SchemaDerivationError> {
    let _ = roots;
    todo!("#1207 PR 2: Stage 0 over Memo::find_or_add_group")
}
