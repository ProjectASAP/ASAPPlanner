//! One evaluated physical properties assignment rebuilt as a physical DAG.
use std::collections::BTreeMap;

use crate::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
use crate::ir::QueryRoot;

use super::Lifetime;

/// One CandidatePhysicalASAPDAG (Stage 2 output), derived from exactly one
/// CandidateLogicalASAPDAG (Stage 1 output). It holds planning data only;
/// display labels are derived from it by the devtools.
#[derive(Debug, Clone)]
pub struct PhysicalWorkloadPlan {
    /// `P<n>` (nothing materialized) or `P<n>-m<k>`, set by the caller.
    pub id: String,
    /// The id of the logical candidate it implements.
    pub from_logical: String,
    /// The timed query roots, one per query in workload order, that `dag`
    /// exports.
    pub roots: Vec<QueryRoot>,
    pub dag: PhysicalASAPDAG,
    /// Its physical properties assignment: the lifetime chosen for each summary
    /// build (`SummaryAgg` or `SummaryTumblingWindowChain`) of `dag`, by node id, including
    /// the builds not kept (`Lifetime::None`), and each other node kept
    /// (a shared node built once per evaluation). A build one query reads
    /// at ingestion time and another at query time appears once per copy;
    /// each copy's timing is its `dag` node's.
    pub lifetime: BTreeMap<PhysicalASAPNodeId, Lifetime>,
}

impl PhysicalWorkloadPlan {
    /// Whether any node is materialized: a summary build, or a shared
    /// non-summary node built once per evaluation.
    #[cfg(any(test, feature = "test-support"))]
    pub fn materializes_any(&self) -> bool {
        self.lifetime.values().any(|lifetime| lifetime.is_kept())
    }
}
