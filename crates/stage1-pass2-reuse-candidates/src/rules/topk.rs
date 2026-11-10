//! Step 2, top-k reuse: an exact top-k reads a larger top-k of the same
//! input and partition, whose ranked rows hold its own.

use crate::rules::Pass2RuleError;
use asap_types::stages::Memo;

/// To each top-k group another top-k group over the same input group and
/// partition with the largest `k` serves, the candidate `TopK(k)` over that
/// group's ranked rows: the `k` highest of the `k_max` highest rows are the
/// `k` highest. Only exact ranked rows give the reuse its answer.
pub(crate) fn add_topk_candidates(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let _ = memo;
    todo!("#1207 PR 4: step 2, top-k reuse")
}
