//! The Stage 3 → Stage 2 callback (#1054 X1): Stage 3 proposes a DAG of
//! selected candidates, and Stage 2 realizes it. Stage 3 receives a `&dyn AssignmentRealizer` from the facade,
//! so it never depends on the Stage 2 crate.
use std::collections::BTreeMap;

use crate::ir::physical_export::PhysicalASAPNodeId;
use crate::physical::PhysicalWorkloadPlan;

use super::{DagOfSelectedCandidates, PhysicalASAPMemoCandidates};

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

/// A realized selection (was `SelectedPhysicalCandidate`): the physical
/// plan Stage 2 built for it, how long each materialized tumbling-window chain of that plan
/// retains its tumbling windows.
#[derive(Debug, Clone)]
pub struct RealizedPlan {
    /// The selection rebuilt as one physical plan: one operator node per
    /// selected node (a tumbling-window layout as its chain read through a
    /// merge), each exported with its candidate's timing, lifetime, θ and
    /// its group's summary coverage.
    pub plan: PhysicalWorkloadPlan,
    /// For each tumbling-window chain of `plan` materialized beyond one evaluation
    /// (maintained at ingestion time, or built at query time and materialized
    /// across evaluations), by node id in `plan.dag`: how far back from the
    /// newest evaluation the runtime must keep its completed tumbling windows, in
    /// milliseconds. It is how far its tumbling windows reach back: the offset its
    /// newest tumbling window ends before the evaluation (`[60m] offset 10m`: 10 min),
    /// plus the chain's window, plus the longest range-evaluation span of
    /// its queries, so the earliest step of the next request still finds
    /// its tumbling windows; without an offset or range evaluations, just the window.
    /// Empty when the plan keeps no tumbling windows.
    pub tumbling_window_retention_ms: BTreeMap<PhysicalASAPNodeId, u64>,
}

/// Realizes a [`DagOfSelectedCandidates`] through Stage 2, one plan node
/// per selected node. Implemented by the
/// Stage 2 crate, which holds the planning context (the workload's entries
/// and data); the facade builds it, passes it to Stage 3, and calls the
/// same method to build the selected plan.
///
/// Guarantees: a returned plan has one operator node per selected node,
/// each rebuilt over its selected producers, and passes the physical DAG
/// validator, with every node keeping its candidate's timing and lifetime.
///
/// It checks structure and execution legality only. It does not check cost,
/// and it does not check accuracy targets: Stage 3 does both.
pub trait AssignmentRealizer {
    fn realize(
        &self,
        physical: &PhysicalASAPMemoCandidates,
        selected: &DagOfSelectedCandidates,
    ) -> Result<RealizedPlan, PhysicalCandidateError>;
}
