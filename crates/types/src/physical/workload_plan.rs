//! One evaluated materialization assignment rebuilt as a physical DAG.
use std::collections::BTreeMap;

use crate::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
use crate::ir::QueryRoot;

use super::Materialization;

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
    /// Its materialization assignment: the option chosen for each summary
    /// build (`SummaryAgg` or `PaneChain`) of `dag`, by node id, including
    /// the builds left `NotMaterialized`, and each other node materialized
    /// (a shared node built once per evaluation). A build one query reads
    /// at ingestion time and another at query time appears once per copy.
    pub materialization: BTreeMap<PhysicalASAPNodeId, Materialization>,
}

impl PhysicalWorkloadPlan {
    /// Whether any node is materialized: a summary build, or a shared
    /// non-summary node built once per evaluation.
    #[cfg(any(test, feature = "test-support"))]
    pub fn materializes_any(&self) -> bool {
        self.materialization
            .values()
            .any(|m| *m != Materialization::NotMaterialized)
    }
}
