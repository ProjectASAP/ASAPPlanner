//! `asap-stage1-pass2-reuse-candidates` — #509 Stage 1, Pass 2: the reuse
//! rules over memo groups, the readouts' required accuracy, dead-candidate
//! elimination and the logical edges. Its entry,
//! [`generate_logical_asap_memo_candidates`], extends Pass 1's memo into
//! the [`LogicalASAPMemoCandidates`](asap_types::stages::LogicalASAPMemoCandidates)
//! Stage 2 expands, e.g. `sum by (job)` and `sum by (job, instance)` of one
//! count reading one state.
//!
//! It depends on `asap-types`, where its input and output are, and on
//! `asap-accuracy` for $\alpha^{\log}$. It depends on no other stage crate,
//! and the planner facade calls it after Pass 1.

/// Steps 1, 2 and 5: the rules that add groups and candidates.
mod rules;

use asap_accuracy::AccuracyEvidence;
use asap_types::stages::{LogicalASAPMemoCandidates, Memo};
use asap_types::workload::QueryWorkloadEntry;
pub use rules::Pass2RuleError;
use rules::{rollup, topk};

/// Stage 1, Pass 2 (concepts doc, section 3, "Stage 1, Pass 2"), in order:
/// 1. rollup from the finest common group;
/// 2. top-k reuse;
/// 3. reach, caps and readout requirements;
/// 4. dead-candidate elimination;
/// 5. chain groups for the surviving state groups, then dead-candidate
///    elimination again over the memo with them;
/// 6. the logical edges between the surviving candidates, with
///    $\alpha^{\log}$ from `evidence` or the theoretical guarantee.
///
/// `entries[i]` is query `i`'s workload entry (its evaluation grid).
pub fn generate_logical_asap_memo_candidates(
    pass1: Memo,
    entries: &[QueryWorkloadEntry],
    evidence: &dyn AccuracyEvidence,
) -> Result<LogicalASAPMemoCandidates, Pass2RuleError> {
    let mut memo = pass1;
    rollup::add_rollup_candidates(&mut memo)?;
    topk::add_topk_candidates(&mut memo)?;
    let _ = (memo, entries, evidence);
    todo!(
        "#1207 PR 4: steps 3-6: readout requirements, dead-candidate elimination, \
         chain groups (`rules::chains::add_chain_groups`) and the logical edges"
    )
}
