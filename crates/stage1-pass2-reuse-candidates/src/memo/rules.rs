//! Pass 2's rules over groups (concepts doc, section 3): each reads the
//! memo and adds groups (`Memo::find_or_add_group`) or candidates
//! (`Memo::add_candidate`); none copies a consumer, so every consumer of a
//! group sees what a rule adds to it.
pub(crate) mod finer_aggregate;
pub(crate) mod rollup;
pub(crate) mod scan_labels;
pub(crate) mod topk;

use asap_types::ir::schema::SketchParams;
use asap_types::ir::Operator;
use asap_types::stages::{
    CandidateExpression, CandidateForMemoGroup, CandidateForMemoGroupId, LogicalProperties,
    LogicalPropertiesError, LogicalRequirements, Memo, MemoGroupId, OriginOfCandidateForMemoGroup,
    WindowLayout,
};

/// Why a Pass 2 rule failed on a memo.
#[derive(Debug, thiserror::Error)]
pub enum Pass2RuleError {
    /// A group, or one of its candidates, reads itself through other groups.
    #[error("memo dependencies contain a cycle")]
    CyclicDependencies,
    /// A rule gave a candidate logical properties its expression cannot
    /// have: a bug in the rule.
    #[error("a rule built invalid logical properties: {0}")]
    LogicalProperties(#[from] LogicalPropertiesError),
}

/// Adds the candidate `operator` with $\boldsymbol\theta$ `parameters` and
/// $\omega$ `window_layout` to `group`, reading each input group without a
/// parameter floor unless `input_requirements` gives one per input (Pass 2
/// sets every floor after the rules). `None` when the memo rejects it: its
/// expression breaks the node rules or its output is not the group's, which
/// is how a rule's shape check ends (the candidate does not compute the
/// group's result).
pub(crate) fn add_candidate(
    memo: &mut Memo,
    group: MemoGroupId,
    operator: Operator<MemoGroupId>,
    parameters: Option<SketchParams>,
    window_layout: Option<WindowLayout>,
    input_requirements: Option<Vec<LogicalRequirements>>,
    origin: OriginOfCandidateForMemoGroup,
) -> Result<Option<CandidateForMemoGroupId>, Pass2RuleError> {
    let expression = CandidateExpression { operator };
    let logical_properties = LogicalProperties::new(&expression, parameters, window_layout)?;
    let input_requirements = input_requirements.unwrap_or_else(|| {
        vec![LogicalRequirements::new(None); expression.operator.children().len()]
    });
    Ok(memo
        .add_candidate(
            group,
            CandidateForMemoGroup {
                expression,
                logical_properties,
                input_requirements,
                fused_groups: Vec::new(),
                origin,
            },
        )
        .ok())
}

/// The group `group`'s expression.
pub(crate) fn expression(memo: &Memo, group: MemoGroupId) -> &Operator<MemoGroupId> {
    &memo.groups()[group.0 as usize].expression
}
