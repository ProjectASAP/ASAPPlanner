//! Step 1, rollup from the finest common group (concepts doc, section 3):
//! a coarse aggregate group $G_c$ computed from a finer group $G_f$ of the
//! same definition, and per cluster of equal definitions one finest common
//! group $G^*$ every member can roll up from.

use crate::rules::Pass2RuleError;
use asap_types::stages::Memo;

/// Adds, to each aggregate group $G_c$, a candidate computing it from each
/// finer group $G_f$ of its cluster, after synthesizing each cluster's
/// finest common group. A cluster is the summary state groups (`SummaryAgg`
/// of a summary type whose states merge across groups) or exact `Aggregate`
/// groups (of sums, counts, minima and maxima) with equal definitions apart
/// from their keys and equal interval constraints (the time window).
pub(crate) fn add_rollup_candidates(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let _ = memo;
    todo!("#1207 PR 4: step 1, rollup from the finest common group")
}
