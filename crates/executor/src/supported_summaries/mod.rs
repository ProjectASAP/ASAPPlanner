//! The summary families this executor builds, with the readouts it answers
//! from each: what a deployment running it declares as
//! `DeploymentCapabilities::supported_summaries`, so the Planner offers no
//! summary the executor would reject when it binds the plan.

use crate::capability::EVALUATED_READOUTS;
use planner_types::summary_type::SupportedSummary;

/// Every sketch family this executor builds from rows and the statistics it
/// answers from one state of it (`EVALUATED_READOUTS`). Exact accumulators
/// and compressed samples are always supported and are not listed.
pub fn supported_summary_families() -> Vec<SupportedSummary> {
    EVALUATED_READOUTS
        .iter()
        .map(|&(summary_type, readouts)| SupportedSummary {
            summary_type,
            readouts: readouts.to_vec(),
        })
        .collect()
}

/// Whether this executor reads a compressed-samples chain's open window,
/// what a deployment running it declares as
/// `DeploymentCapabilities::unsealed_window_reads`: it does, through its
/// store's `DataStore::read_open_window` (a store that keeps no open window
/// refuses the read, and its deployment declares `false`).
pub const UNSEALED_WINDOW_READS: bool = true;

#[cfg(test)]
mod tests;
