use super::*;

/// One per-state window choice within a complete Planner candidate.
#[derive(Debug, Clone)]
pub struct StreamingWindowFrameworkAssignment {
    pub summary: Rc<SummaryNode>,
    /// `None` explicitly means that this state is not window-organized.
    pub framework: Option<SummaryWindowFramework>,
}

/// Cost evidence for one complete abstract window-framework assignment across
/// a summary DAG in Planner search.
///
/// The provider derives this evidence from a concrete downstream
/// implementation under the current data workload. The stable identity is
/// provenance for the chosen implementation, while deployment placement and
/// runtime configuration remain downstream concerns.
#[derive(Debug, Clone)]
pub struct StreamingWindowFrameworkCandidate {
    /// Stable identity of the complete provider implementation whose evidence
    /// is bound to this planner-visible framework assignment.
    pub physical_plan_id: String,
    /// Exactly one assignment for every summary deployment in the DAG.
    pub assignments: Vec<StreamingWindowFrameworkAssignment>,
    pub node_evidence: StreamingNodeEvidence,
}
