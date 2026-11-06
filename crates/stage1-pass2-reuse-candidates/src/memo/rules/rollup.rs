//! Producer-level exact and sketch rollups, using the existing capability rules.

use crate::memo::rules::Pass2RuleError;
use asap_types::stages::Memo;

/// Rule 6, rollup: a candidate of a coarse aggregate or summary group reading
/// a finer group whose state rolls up (exact or sketch merge).
pub(crate) fn add_rollup_candidates(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let _ = memo;
    todo!("#1207 PR 4: rule 6 over groups")
}

/// Rule 7, window rollup: a candidate of a longer-range group merging a
/// shorter-range group's windows.
pub(crate) fn add_window_rollup_candidates(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let _ = memo;
    todo!("#1207 PR 4: rule 7 over groups")
}
