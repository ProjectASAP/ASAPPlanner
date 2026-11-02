//! The logical memo's candidates, rules and enumeration, over the memo
//! groups Stage 0 builds (`crate::stage0`).

/// Logical implementation candidates populated after initial identity construction.
pub(crate) mod candidates;
/// The candidate DAGs a memo stands for: choices and their composition.
pub(crate) mod enumeration;
pub(crate) mod groups;
pub(crate) use groups::NodeToStructuralIdCache;
pub(crate) mod rules;
/// Shared finite state configuration enumeration.
pub(crate) mod shared_configurations;
/// Concrete summary-state groups exposed by sharing rules.
pub(crate) mod state_groups;
/// Local tumbling-window candidates with selected-reader grid configurations.
pub(crate) mod window_candidates;

use std::time::Instant;

use asap_types::stages::CandidateGenerationTimeout;

pub(crate) fn check_generation_deadline(
    deadline: Option<Instant>,
) -> Result<(), CandidateGenerationTimeout> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        Err(CandidateGenerationTimeout)
    } else {
        Ok(())
    }
}
