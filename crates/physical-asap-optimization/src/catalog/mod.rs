//! Stage 2's physical candidate catalog: implement each memo candidate's
//! operators, enumerate its timing and materialization, record which
//! candidates can supply each input, and prune what cannot be used; and
//! realize one selected assignment as a physical plan (#1047). The catalog
//! types live in `asap_types::stages`; Stage 3 searches over them.
mod configurations;
mod factoring;
mod inventory;
mod pruning;
mod realize;
mod reconstruction;
mod scopes;

pub use configurations::{enumerate_candidate_configurations_before, ConfigurationError};
pub use inventory::{build_candidate_inventory_before, CandidateInventoryError};
pub use pruning::{prune, PruningMetrics};
pub use realize::{
    materialize_selected_candidates, PhysicalCandidateError, SelectedPhysicalCandidate,
};
pub use reconstruction::{reconstruct_selected_candidates, ReconstructedCandidates};

/// Catalog internals that plan selection's unit tests exercise together with
/// its solver.
#[cfg(feature = "test-support")]
pub mod test_support {
    pub use super::configurations::{compatible_timing, may_keep_with_sole_reader};
    pub use super::factoring::{
        factor_terminal_readers, MemoGroupOutputKey, MemoGroupOutputToCompatibleProducersCache,
    };
    pub use super::inventory::build_with_compatible_producers_cache;
    pub use super::pruning::usable_candidates;
    pub use super::realize::{ordered_output_references, restore_query_roots};
    pub use super::reconstruction::{
        ordered_inputs, output_contract_satisfied, paired_node_identities,
    };
    pub use super::scopes::expand_execution_scopes_before;
}

/// Discard partially built catalogs on timeout; they cannot certify coverage.
fn check_deadline(
    deadline: Option<std::time::Instant>,
) -> Result<(), asap_types::stages::CandidateGenerationTimeout> {
    if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
        Err(asap_types::stages::CandidateGenerationTimeout)
    } else {
        Ok(())
    }
}
