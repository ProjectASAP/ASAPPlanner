//! Stage 2's physical candidates: implement each memo candidate's
//! operators, enumerate its timing and materialization, record which
//! candidates can produce each input, and prune what cannot be used
//! ([`generate_physical_asap_memo_candidates`]); and realize one selected
//! assignment as a physical plan (#1047). The output type,
//! `PhysicalASAPMemoCandidates`, lives in `asap_types::stages`; Stage 3
//! selects from it.
mod configurations;
mod factoring;
mod inventory;
mod pruning;
mod realize;
mod reconstruction;

pub use asap_types::stages::{PhysicalCandidateError, RealizedPlan};
pub use configurations::ConfigurationError;
pub use inventory::CandidateInventoryError;
pub use realize::{materialize_selected_candidates, Stage2AssignmentRealizer};
pub use reconstruction::{reconstruct_selected_candidates, ReconstructedCandidates};

use asap_types::stages::{
    LogicalASAPMemoCandidates, PhysicalASAPMemoCandidates, PhysicalCandidateId, ReaderConstraint,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

/// Why [`generate_physical_asap_memo_candidates`] failed.
#[derive(Debug, thiserror::Error)]
pub enum ErrorGeneratingPhysicalASAPMemoCandidates {
    /// Lowering the memo's candidates to operator nodes failed.
    #[error(transparent)]
    Inventory(#[from] CandidateInventoryError),
    /// Enumerating timings and materializations failed.
    #[error(transparent)]
    Configuration(#[from] ConfigurationError),
    /// Structural pruning found the candidates malformed.
    #[error("{0}")]
    Pruning(String),
}

impl ErrorGeneratingPhysicalASAPMemoCandidates {
    /// Whether generation stopped at `deadline`, leaving no usable output.
    pub fn is_deadline_exceeded(&self) -> bool {
        matches!(
            self,
            Self::Inventory(CandidateInventoryError::DeadlineExceeded(_))
                | Self::Configuration(ConfigurationError::DeadlineExceeded(_))
        )
    }
}

/// Stage 2: implement every operator node of every candidate in `logical`'s
/// memo, enumerate the timings and materializations the data and deployment
/// allow (`query_time_results_retained`; no query-time raw scans unless
/// `raw_data_retained`; no exact per-entity aggregate the executor cannot
/// run), record which candidates may produce each input, and prune what
/// cannot be used. Every configuration not generated and every candidate
/// pruned is recorded in the output's `pruned_candidates` with its
/// `PruneReason`. Prices nothing.
pub fn generate_physical_asap_memo_candidates(
    logical: &LogicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    query_time_results_retained: bool,
    raw_data_retained: bool,
    deadline: Option<std::time::Instant>,
) -> Result<PhysicalASAPMemoCandidates, ErrorGeneratingPhysicalASAPMemoCandidates> {
    let memo = &logical.memo;
    let output_groups: Vec<_> = asap_types::stages::output_references(&memo.query_roots)
        .into_iter()
        .map(|(_, group)| group)
        .collect();
    let inventory = inventory::build_candidate_inventory_before(memo, &output_groups, deadline)?;
    let configured = configurations::enumerate_candidate_configurations_before(
        &inventory,
        &memo.query_roots,
        entries,
        data,
        query_time_results_retained,
        raw_data_retained,
        deadline,
    )?;
    pruning::prune(configured).map_err(ErrorGeneratingPhysicalASAPMemoCandidates::Pruning)
}

/// Each output reference's candidates ([`PhysicalASAPMemoCandidates::query_output_candidates`]).
fn query_output_candidates(physical: &PhysicalASAPMemoCandidates) -> Vec<Vec<PhysicalCandidateId>> {
    (0..physical.output_reference_count())
        .map(|output| physical.query_output_candidates(output).collect())
        .collect()
}

/// `ids` name existing candidates (of `count`), each once.
fn validate_ids(ids: &[PhysicalCandidateId], count: usize, context: &str) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        if id.0 >= count {
            return Err(format!("{context} references missing candidate {}", id.0));
        }
        if !seen.insert(*id) {
            return Err(format!("{context} repeats candidate {}", id.0));
        }
    }
    Ok(())
}

/// Every producer, operator node and reader constraint of `physical` names an
/// existing candidate or output reference, and each query reader constraint
/// partitions the output references.
fn validate_candidates(physical: &PhysicalASAPMemoCandidates) -> Result<(), String> {
    let count = physical.candidates.len();
    let outputs_count = physical.output_reference_count();
    let invalid = |message: &str| format!("invalid physical ASAP memo candidates: {message}");
    for constraint in &physical.reader_constraints {
        let outputs = match constraint {
            ReaderConstraint::MinimumQueryReaders { query_outputs, .. }
            | ReaderConstraint::HighestSharedQueryMaterialization { query_outputs, .. } => {
                let outputs: Vec<_> = query_outputs.iter().flatten().copied().collect();
                let unique: std::collections::HashSet<_> = outputs.iter().copied().collect();
                if outputs.len() != outputs_count || unique.len() != outputs.len() {
                    return Err(invalid(
                        "query reader buckets must partition every output exactly once",
                    ));
                }
                outputs
            }
            ReaderConstraint::AllowedOutputs { outputs, .. } => outputs.clone(),
            ReaderConstraint::IncompatibleOutputs { left, right, .. } => vec![*left, *right],
        };
        if constraint.candidate().0 >= count
            || outputs.iter().any(|&output| output >= outputs_count)
        {
            return Err(invalid(
                "reader constraint references missing candidate or output",
            ));
        }
    }
    for (index, candidate) in physical.candidates.iter().enumerate() {
        if candidate.operator_node.0 >= physical.operator_nodes_of_candidates_for_memo_groups.len()
        {
            return Err(invalid(&format!(
                "candidate {index} references missing operator node"
            )));
        }
        for (input, producer_candidates) in
            candidate.producer_candidates_per_input.iter().enumerate()
        {
            validate_ids(
                producer_candidates,
                count,
                &format!("candidate {index}, requirement {input}"),
            )
            .map_err(|message| invalid(&message))?;
        }
    }
    Ok(())
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

/// Catalog internals that plan selection's unit tests exercise together with
/// its solver.
#[cfg(feature = "test-support")]
pub mod test_support {
    pub use super::configurations::{
        compatible_timing, enumerate_candidate_configurations_before,
        may_keep_across_evaluations_per_sole_reader_query,
    };
    pub use super::factoring::{
        factor_terminal_readers, MemoGroupOutputKey, MemoGroupOutputToCompatibleProducersCache,
    };
    pub use super::inventory::{
        build_candidate_inventory_before, build_with_compatible_producers_cache, CandidateInventory,
    };
    pub use super::pruning::{prune, usable_candidates};
    pub use super::realize::{ordered_output_references, restore_query_roots};
    pub use super::reconstruction::{
        ordered_inputs, output_contract_satisfied, paired_node_identities,
    };
}
