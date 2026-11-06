//! Pass 2's rules over groups (concepts doc, section 3): each reads the
//! memo and adds groups (`Memo::find_or_add_group`) or candidates
//! (`Memo::add_candidate`); none copies a consumer.
pub(crate) mod finer_aggregate;
pub(crate) mod rollup;
pub(crate) mod scan_labels;
pub(crate) mod summary_inputs;
pub(crate) mod topk;

/// Why a Pass 2 rule failed on a memo.
#[derive(Debug, thiserror::Error)]
pub enum Pass2RuleError {
    #[error("memo contains an out-of-range group reference")]
    InvalidGroupReference,
    #[error("memo group capacity exceeds the group identifier range")]
    TooManyGroups,
    #[error("memo dependencies contain a cycle")]
    CyclicDependencies,
}
