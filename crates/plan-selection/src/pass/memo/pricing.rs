//! Each group candidate priced alone, and local dominance per signature
//! (steps 2–3).

use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::{GroupCandidate, MemoGroup};
use crate::PlanningModels;

/// `candidates` of `group`, each priced by Stage 3 with the group alone
/// (`usage` filled), the ones failing a check sharing cannot repair
/// dropped.
pub(crate) fn priced(
    _group: &MemoGroup,
    _candidates: Vec<GroupCandidate>,
    _entries: &[QueryWorkloadEntry],
    _data: &DataWorkload,
    _models: PlanningModels<'_>,
) -> Vec<GroupCandidate> {
    todo!("W2")
}

/// `candidates` (priced) with every one another of the same signature
/// dominates dropped: no smaller on every usage component and larger on
/// one. Ties are kept, in order.
pub(crate) fn keep_undominated(_candidates: Vec<GroupCandidate>) -> Vec<GroupCandidate> {
    todo!("W2")
}
