//! Stage 1, Pass 2's entry and output: the Pass 1 memo extended with the
//! candidates and groups that share work across computations
//! ([`asap_types::stages::LogicalASAPMemoCandidates`]), which Stage 2 implements.

use std::time::Instant;

use crate::pass2::memo::check_generation_deadline;
use crate::pass2::memo::rules::{
    finer_aggregate, rollup, scan_labels, shared_configuration_readers, summary_inputs,
    summary_results, topk,
};
use crate::pass2::memo::shared_configurations::PaneWidthDomain;
use crate::pass2::memo::state_groups::{
    add_shared_summary_state_groups, check_unexpanded_memo, StateGroupExpansionError,
};
use crate::pass2::memo::window_candidates::extend_local_window_composition;
use asap_types::stages::MemoCandidates;
use asap_types::stages::PerComputationLogicalMemoCandidates;
use asap_types::stages::{CandidateGenerationTimeout, LogicalASAPMemoCandidates};
use asap_types::workload::QueryWorkloadEntry;

/// What Pass 2 keeps between pane-width rounds: the memo after local
/// window composition and before the other rules, and the pane widths it
/// already holds. Each round adds only the newly exposed widths to it,
/// then reruns the other rules on a copy.
#[derive(Debug, Clone)]
pub struct Pass2RoundState {
    unexpanded_memo: MemoCandidates,
    previous_domain: Option<PaneWidthDomain>,
}

impl Pass2RoundState {
    /// The state before the first round: Pass 1's memo, no pane widths.
    pub fn new(pass1: PerComputationLogicalMemoCandidates) -> Self {
        Self {
            unexpanded_memo: pass1.memo,
            previous_domain: None,
        }
    }

    /// The memo after the last round's local window composition, before
    /// the other Pass 2 rules (Pass 1's memo before the first round).
    pub fn unexpanded_memo(&self) -> &MemoCandidates {
        &self.unexpanded_memo
    }
}

/// Why [`generate_logical_asap_memo_candidates`] failed.
#[derive(Debug, thiserror::Error)]
pub enum LogicalASAPMemoCandidatesError {
    /// The deadline passed during local window composition. The
    /// [`Pass2RoundState`] may hold partial work: discard it.
    #[error(transparent)]
    LocalWindowComposition(CandidateGenerationTimeout),
    /// A later rule or the shared summary-state groups failed. The
    /// [`Pass2RoundState`] already holds this round's pane widths and stays
    /// usable for the next round.
    #[error(transparent)]
    Expansion(#[from] StateGroupExpansionError),
}

/// Stage 1, Pass 2 for one pane-width round: adds to `round`'s memo the
/// local pane candidates for the widths of `domain` that earlier rounds did
/// not generate (rule 1), records `domain` in `round`, then runs rules 2–9
/// and `add_shared_summary_state_groups` on a copy and returns it.
/// `entries[i]` is query `i`'s workload entry. Stops once `deadline` passes.
pub fn generate_logical_asap_memo_candidates(
    round: &mut Pass2RoundState,
    entries: &[QueryWorkloadEntry],
    domain: PaneWidthDomain,
    deadline: Option<Instant>,
) -> Result<LogicalASAPMemoCandidates, LogicalASAPMemoCandidatesError> {
    // 1. Local window composition.
    extend_local_window_composition(
        &mut round.unexpanded_memo,
        entries,
        round.previous_domain,
        domain,
        deadline,
    )
    .map_err(LogicalASAPMemoCandidatesError::LocalWindowComposition)?;
    round.previous_domain = Some(domain);
    let mut memo = round.unexpanded_memo.clone();
    apply_sharing_rules(&mut memo, domain, deadline)?;
    // 10. One group per shared summary build, read by its reader groups.
    add_shared_summary_state_groups(&mut memo, deadline)?;
    Ok(LogicalASAPMemoCandidates { memo })
}

/// Pass 2's rules 2–9, in order, on a memo after local window composition.
pub(crate) fn apply_sharing_rules(
    memo: &mut MemoCandidates,
    domain: PaneWidthDomain,
    deadline: Option<Instant>,
) -> Result<(), StateGroupExpansionError> {
    check_generation_deadline(deadline)?;
    check_unexpanded_memo(memo)?;
    // 2. Scan widening.
    scan_labels::add_scan_label_candidates(memo, deadline)?;
    // 3. Summary inputs.
    summary_inputs::factor_summary_inputs(memo, deadline)?;
    // 4. Top-k reuse.
    topk::add_topk_candidates(memo, deadline)?;
    // 5. Finer aggregate.
    finer_aggregate::add_finer_aggregate_candidates(memo, deadline)?;
    // 6. Rollup.
    rollup::add_rollup_candidates(memo, deadline)?;
    // 7. Window rollup.
    rollup::add_window_rollup_candidates(memo, deadline)?;
    // 8. Summary results.
    summary_results::factor_summary_results(memo, deadline)?;
    // 9. Shared configurations.
    shared_configuration_readers::add_state_configuration_candidates_in_domain(
        memo, domain, deadline,
    )?;
    Ok(())
}
