//! Memo groups and their candidates (steps 1–2).

use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidates;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::{GroupCandidate, MemoGroup};
use crate::PlanningModels;

/// The memo groups of a workload: identical queries and classes of
/// interchangeable queries are one group each, and so are queries that
/// read a common Pass 1 target.
pub(crate) fn memo_groups<Id: Clone>(
    _per_computation: &PerComputationCandidates<Id>,
    _entries: &[QueryWorkloadEntry],
    _models: &PlanningModels<'_>,
) -> Vec<MemoGroup> {
    todo!("W1")
}

/// Every candidate of `group`, planned alone, unpriced, each with the
/// sharing sites it exposes to the workload's other groups.
pub(crate) fn group_candidates<Id: Clone>(
    _per_computation: &PerComputationCandidates<Id>,
    _groups: &[MemoGroup],
    _group: usize,
    _entries: &[QueryWorkloadEntry],
    _data: &DataWorkload,
    _models: PlanningModels<'_>,
) -> Vec<GroupCandidate> {
    todo!("W1")
}
