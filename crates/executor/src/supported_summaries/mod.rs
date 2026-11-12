//! The summary families this executor builds, with the readouts it answers
//! from each: what a deployment running it declares as
//! `DeploymentCapabilities::supported_summaries`, so the Planner offers no
//! summary the executor would reject when it binds the plan.

use crate::capability::EVALUATED_READOUTS;
use planner_types::summary_type::SupportedSummary;

/// Every sketch family this executor builds from rows and the statistics it
/// answers from one state of it (`EVALUATED_READOUTS`). Exact accumulators
/// are always supported and are not listed.
pub fn supported_summary_families() -> Vec<SupportedSummary> {
    EVALUATED_READOUTS
        .iter()
        .map(|&(summary_type, readouts)| SupportedSummary {
            summary_type,
            readouts: readouts.to_vec(),
        })
        .collect()
}

#[cfg(test)]
mod tests;
