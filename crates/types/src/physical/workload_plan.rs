//! One realized selection as a physical DAG.
use crate::ir::physical_export::PhysicalASAPDAG;
use crate::ir::QueryRoot;
#[cfg(any(test, feature = "test-support"))]
use crate::physical::Lifetime;

/// One CandidatePhysicalASAPDAG (Stage 2 output), derived from exactly one
/// CandidateLogicalASAPDAG (Stage 1 output). It holds planning data only;
/// display labels are derived from it by the devtools.
#[derive(Debug, Clone)]
pub struct PhysicalWorkloadPlan {
    /// `P<n>` (nothing materialized) or `P<n>-m<k>`, set by the caller.
    pub id: String,
    /// The id of the logical candidate it implements.
    pub from_logical: String,
    /// The query roots, one per query in workload order, that `dag`
    /// exports.
    pub roots: Vec<QueryRoot>,
    pub dag: PhysicalASAPDAG,
}

impl PhysicalWorkloadPlan {
    /// Whether any node of `dag` is materialized: a summary build, or a shared
    /// non-summary node built once per evaluation.
    #[cfg(any(test, feature = "test-support"))]
    pub fn materializes_any(&self) -> bool {
        self.dag
            .nodes
            .iter()
            .any(|node| node.lifetime != Lifetime::None)
    }
}
