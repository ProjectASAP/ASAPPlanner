//! `asap-stage0-memo` — #509 Stage 0: the memo groups of the workload's
//! query roots (a [`Memo`] without candidates). Each node of the queries'
//! DAGs becomes the group of its expression, found or added by
//! [`Memo::find_or_add_group`], so identical sub-expressions are one group.
//! Stage 1 starts from these groups.
//!
//! It depends only on `asap-types`, where its output (`Memo`) lives; no
//! other stage crate depends on it except in their tests, and the planner
//! facade calls it first.

use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::{QueryRoot, SchemaDerivationError};
use asap_types::stages::Memo;
use asap_types::workload::DataWorkload;

/// Stage 0: the memo groups of the workload's query roots, query `i`
/// being `roots[i]` with accuracy target `accuracy_targets[i]` (`None`:
/// none), every group without candidates. Each query is added with its
/// $n_q$, the number of values one evaluation returns, which `data` bounds;
/// a `Scan`'s column signs are `data`'s declared ones. Fails with the first
/// node that breaks the node rules over its children's groups
/// ([`Memo::find_or_add_group`]).
pub fn memo_of_queries(
    roots: &[QueryRoot],
    accuracy_targets: &[Option<AccuracyTarget>],
    data: &DataWorkload,
) -> Result<Memo, SchemaDerivationError> {
    let _ = (roots, accuracy_targets, data);
    todo!("#1207 PR 2: Stage 0 over Memo::find_or_add_group, each query with its target and n_q")
}
