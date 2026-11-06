//! Scan-label widening for explicit memo state sharing. Reuses the existing
//! Pass 2 widening rule; only its source-chain boundaries are interned here.

use crate::memo::rules::Pass2RuleError;
use asap_types::stages::Memo;

/// Rule 2, scan widening: a group for the widened scan, and in each original scan group a candidate `Project(widened)`.
pub(crate) fn add_scan_label_candidates(memo: &mut Memo) -> Result<(), Pass2RuleError> {
    let _ = memo;
    todo!("#1207 PR 4: rule 2 over groups")
}
