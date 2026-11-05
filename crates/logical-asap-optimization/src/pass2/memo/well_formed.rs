//! Pass 2's error, and its check of the memo before its rules.

use asap_types::stages::Memo;

/// Why Pass 2's rules failed on a memo: its precondition check or a rule.
#[derive(Debug, thiserror::Error)]
pub enum Pass2RuleError {
    #[error("memo group and candidate vectors have different lengths")]
    MismatchedGroups,
    #[error("memo contains an out-of-range group reference")]
    InvalidGroupReference,
    #[error("memo group capacity exceeds the group identifier range")]
    TooManyGroups,
    #[error("memo dependencies contain a cycle")]
    CyclicDependencies,
    #[error("scan-label candidate expansion: {0}")]
    ScanLabelWidening(String),
    #[error("producer reuse rule: {0}")]
    ReuseRule(String),
}

/// Rejects a memo the Pass 2 rules cannot extend: mismatched or
/// out-of-range group references, or cyclic dependencies. Pass 2 checks
/// this before its rules.
pub(crate) fn check_group_references_valid_and_acyclic(memo: &Memo) -> Result<(), Pass2RuleError> {
    let count = memo.groups.len();
    if count != memo.candidates.len() {
        return Err(Pass2RuleError::MismatchedGroups);
    }
    if count > u32::MAX as usize {
        return Err(Pass2RuleError::TooManyGroups);
    }
    let out_of_range = |id: &asap_types::stages::MemoGroupId| id.0 as usize >= count;
    if memo
        .groups
        .iter()
        .any(|group| group.child_groups.iter().chain(&group.parents).any(out_of_range))
        || memo.candidates.iter().flatten().any(|candidate| {
            candidate
                .expression
                .operator
                .children()
                .into_iter()
                .chain(&candidate.fused_groups)
                .any(out_of_range)
        })
    {
        return Err(Pass2RuleError::InvalidGroupReference);
    }
    if memo.topological_order().is_none() {
        return Err(Pass2RuleError::CyclicDependencies);
    }
    Ok(())
}
