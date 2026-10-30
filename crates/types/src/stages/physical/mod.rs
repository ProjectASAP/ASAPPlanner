//! The Stage 2 → Stage 3 boundary: the physical candidate catalog Stage 2
//! builds from the memo and Stage 3 searches (#1047, #1054).

mod configurations;
mod graph;
mod inventory;
mod suppliers;

pub use configurations::{CandidateConfiguration, CandidateConfigurationInventory};
pub use graph::{
    validate_selected_graph, CandidateDependencyGraph, CandidateSelectionError, PhysicalCandidate,
    PhysicalCandidateId, ProducerConsumerConnection, ProducerRequirement, QueryRootAssignment,
    QueryRootCandidates, ReaderConstraint, ResourceBudget, SelectedCandidateGraph,
    MINIMUM_RELATIVE_COEFFICIENT,
};
pub use inventory::{CandidateInventory, CandidateOperatorTemplates};
pub use suppliers::{
    supplier_storage_metrics, SupplierCandidates, SupplierSetInterner, SupplierStorageMetrics,
};
