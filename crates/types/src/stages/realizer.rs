//! The Stage 3 → Stage 2 callback (#1054 X1): Stage 3 proposes a DAG of
//! selected candidates, and Stage 2 realizes it. Stage 3 receives a `&dyn AssignmentRealizer` from the facade,
//! so it never depends on the Stage 2 crate.
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::ir::physical_export::PhysicalASAPNodeId;
use crate::ir::OperatorNode;
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
/// plan Stage 2 built for it, how long each kept pane chain of that plan
/// keeps its panes, and the operator node each selected node became.
#[derive(Debug, Clone)]
pub struct RealizedPlan {
    /// The selection rebuilt as one physical plan, one operator node per
    /// selected node, each with its candidate's timing and materialization.
    pub plan: PhysicalWorkloadPlan,
    /// For each tumbling-pane chain of `plan` kept beyond one evaluation
    /// (maintained at ingestion time, or built at query time and kept
    /// across evaluations), by node id in `plan.dag`: how far back from the
    /// newest evaluation the runtime must keep its completed panes, in
    /// milliseconds. It is how far its panes reach back: the offset its
    /// newest pane ends before the evaluation (`[60m] offset 10m`: 10 min),
    /// plus the chain's window, plus the longest range-evaluation span of
    /// its readers, so the earliest step of the next request still finds
    /// its panes; without an offset or range evaluations, just the window.
    /// Empty when the plan keeps no panes.
    pub pane_retention_ms: BTreeMap<PhysicalASAPNodeId, u64>,
    /// The operator node of `plan` each selected node became, indexed by
    /// [`SelectedNodeId`](super::SelectedNodeId).
    pub operator_node_of_selected_node: Vec<Rc<OperatorNode>>,
}

/// Realizes a [`DagOfSelectedCandidates`] through Stage 2, one plan node
/// per selected node. Implemented by the
/// Stage 2 crate, which holds the planning context (the workload's
/// original query roots, its entries and data, and whether query-time
/// results are retained); the facade builds it, passes it to Stage 3, and
/// calls the same method to build the selected plan.
///
/// Guarantees: a returned result has one operator node per selected node, each
/// rebuilt over its selected producers with its identity kept, guarantees
/// recomposed, and every output contract (result kind and schema) met; a
/// realized assignment also passes Stage 2's timing and dependency rules
/// and the physical DAG validator, with every node keeping its
/// candidate's timing and materialization.
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
