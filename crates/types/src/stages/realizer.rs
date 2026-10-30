//! The Stage 3 → Stage 2 callback (#1054 X1): Stage 3 proposes a selection
//! of physical candidates with a materialization per candidate, and Stage 2
//! realizes it. Stage 3 receives a `&dyn AssignmentRealizer` from the facade,
//! so it never depends on the Stage 2 crate.
use std::rc::Rc;

use crate::ir::{OperatorNode, QueryRoot};
use crate::physical::{Materialization, PhysicalWorkloadPlan};
use crate::workload::{DataWorkload, QueryWorkloadEntry};

use super::{PhysicalASAPMemoCandidates, SelectedCandidateGraph};

/// Why a selected assignment could not be realized. Only rejected
/// assignments are safe to exclude from the finite search; adapter or
/// identity failures must stop with diagnostics, not become no-good cuts.
#[derive(Debug, thiserror::Error)]
pub enum PhysicalCandidateError {
    #[error("selected physical assignment is infeasible: {0}")]
    RejectedAssignment(String),
    #[error("invalid or unsupported physical reconstruction: {0}")]
    InvalidOrUnsupported(String),
}

impl From<String> for PhysicalCandidateError {
    fn from(message: String) -> Self {
        Self::InvalidOrUnsupported(message)
    }
}
impl From<&str> for PhysicalCandidateError {
    fn from(message: &str) -> Self {
        Self::InvalidOrUnsupported(message.into())
    }
}

/// A realized selection: the physical plan Stage 2 built for it, and the
/// node of that plan each selected candidate became (indexed by
/// `PhysicalCandidateId`; `None` for candidates not selected).
pub struct SelectedPhysicalCandidate {
    pub physical: PhysicalWorkloadPlan,
    pub nodes_by_candidate: Vec<Option<Rc<OperatorNode>>>,
}

/// Realizes a selection of physical candidates, proposed by Stage 3, through
/// Stage 2. Implemented by the Stage 2 crate and passed to Stage 3 by the
/// facade.
///
/// Guarantees: a returned result has each selected candidate rebuilt over
/// its selected producers with its candidate identity kept, guarantees
/// recomposed, and every output contract (result kind and schema) met; a
/// realized assignment also passes Stage 2's timing and dependency rules
/// and the physical DAG validator, with every candidate keeping the
/// materialization it was assigned.
///
/// It checks structure and execution legality only. It does not check cost,
/// and it does not check accuracy targets: Stage 3 does both.
pub trait AssignmentRealizer {
    /// The output reference of every query root, in `map_operator_refs`
    /// traversal order of the roots in workload order, rebuilt from the
    /// selected candidates over their selected producers.
    fn reconstruct_selected_outputs(
        &self,
        physical: &PhysicalASAPMemoCandidates,
        selected: &SelectedCandidateGraph,
    ) -> Result<Vec<Rc<OperatorNode>>, String>;

    /// The physical plan Stage 2 builds for `selected` with
    /// `materializations` (indexed by `PhysicalCandidateId`, `Some` exactly
    /// for selected candidates), and the node each selected candidate became.
    /// `original_roots` are the workload's query roots, one per entry of
    /// `entries`.
    #[allow(clippy::too_many_arguments)]
    fn realize_selected_assignment(
        &self,
        physical: &PhysicalASAPMemoCandidates,
        selected: &SelectedCandidateGraph,
        materializations: &[Option<Materialization>],
        original_roots: &[QueryRoot],
        entries: &[QueryWorkloadEntry],
        data: &DataWorkload,
        query_time_results_retained: bool,
    ) -> Result<SelectedPhysicalCandidate, PhysicalCandidateError>;
}
