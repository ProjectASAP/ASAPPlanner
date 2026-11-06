//! Realize a selected DAG through Stage 2: each node's physical properties
//! come from its `PhysicalCandidate` (concepts doc, section 3,
//! "Realizer/export"). This validates execution legality, not cost estimates
//! or accuracy targets.

use asap_types::ir::QueryRoot;
use asap_types::stages::{
    AssignmentRealizer, DagOfSelectedCandidates, PhysicalASAPMemoCandidates,
    PhysicalCandidateError, RealizedPlan,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

/// Stage 2's [`AssignmentRealizer`], over the planning context: the
/// workload's original query roots (one per entry of `entries`), its data,
/// and whether the deployment keeps query-time results across
/// evaluations. The facade builds it, passes it to Stage 3, and builds the
/// selected plan with it.
pub struct Stage2AssignmentRealizer<'a> {
    pub original_roots: &'a [QueryRoot],
    pub entries: &'a [QueryWorkloadEntry],
    pub data: &'a DataWorkload,
    pub query_time_results_retained: bool,
}

impl AssignmentRealizer for Stage2AssignmentRealizer<'_> {
    fn realize(
        &self,
        physical: &PhysicalASAPMemoCandidates,
        selected: &DagOfSelectedCandidates,
    ) -> Result<RealizedPlan, PhysicalCandidateError> {
        let _ = (physical, selected);
        todo!("#1207 PR 6: build the selected DAG, each node at its PhysicalCandidate's properties")
    }
}
