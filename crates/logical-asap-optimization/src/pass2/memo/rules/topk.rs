//! Exact ranking reuse with explicit exact-input groups. No global selection
//! flags are required: only exact implementations can supply these inputs.

use crate::pass2::memo::rules::Pass2RuleError;
use asap_types::stages::Memo;

/// Rule 4, top-k reuse: a candidate of each top-k group reading a larger-`k` group, with an exact-input requirement.
pub(crate) fn add_topk_candidates(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let _ = memo;
    todo!("#1207 PR 4: rule 4 over groups")
}
