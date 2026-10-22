//! Sharing clusters and the exact joint choice in each (steps 4–6).

use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::{ClusterSelection, GroupCandidate, MemoGroup};
use crate::{PlanningModels, SelectionError};

/// The clusters of groups whose kept sites can meet, each selected exactly.
pub(crate) fn select_clusters(
    _groups: &[MemoGroup],
    _candidates: &[Vec<GroupCandidate>],
    _entries: &[QueryWorkloadEntry],
    _data: &DataWorkload,
    _models: PlanningModels<'_>,
) -> Result<Vec<ClusterSelection>, SelectionError> {
    todo!("integrator")
}
