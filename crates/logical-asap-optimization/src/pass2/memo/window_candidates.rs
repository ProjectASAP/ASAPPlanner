//! Rule 1, local window composition: tumbling-window layouts $\omega_v$ as
//! candidates of a summary state group (#1160).

use crate::pass2::memo::rules::Pass2RuleError;
use asap_types::stages::Memo;
use asap_types::workload::QueryWorkloadEntry;

/// Rule 1: for each summary state group, a candidate per tumbling width $w$
/// from the gcd closure of the ranges and steps of the queries that reach it,
/// offered only when $w$ divides the group's query range. `entries[i]` is
/// query `i`'s workload entry.
pub(crate) fn add_local_window_composition(
    memo: &mut Memo,
    entries: &[QueryWorkloadEntry],
) -> Result<(), Pass2RuleError> {
    let _ = (memo, entries);
    todo!("#1207 PR 4: rule 1, tumbling layouts as candidates of the state group")
}
