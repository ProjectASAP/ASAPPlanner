//! Pass 2's rules over groups (concepts doc, section 3, "Stage 1, Pass 2"):
//! each reads the memo and adds groups (`Memo::find_or_add_group`) or
//! candidates (`Memo::add_candidate`); none copies a consumer, so every
//! consumer of a group sees what a rule adds to it.
pub(crate) mod chains;
pub(crate) mod rollup;
pub(crate) mod topk;

use asap_types::ir::schema::SketchParams;
use asap_types::ir::{Operator, SchemaDerivationError};
use asap_types::stages::{
    CandidateError, CandidateExpression, CandidateForMemoGroup, CandidateForMemoGroupId,
    LogicalProperties, LogicalPropertiesError, LogicalRequirements, Memo, MemoGroupId,
    OriginOfCandidateForMemoGroup,
};

/// Why Pass 2 failed on a memo: a rule built an expression the node rules
/// reject (a bug in the rule), or the memo's reads form a cycle.
#[derive(Debug, thiserror::Error)]
pub enum Pass2RuleError {
    /// A group, or one of its candidates, reads itself through other groups.
    #[error("memo dependencies contain a cycle")]
    CyclicDependencies,
    /// A rule built a group expression the node rules reject.
    #[error("a rule built a group that breaks the node rules: {0}")]
    Group(#[from] SchemaDerivationError),
    /// A rule built a candidate expression the node rules reject.
    #[error("a rule built a candidate that breaks the node rules: {0}")]
    Candidate(CandidateError),
    /// A rule gave a candidate parameters its expression cannot have.
    #[error("a rule built invalid logical properties: {0}")]
    LogicalProperties(#[from] LogicalPropertiesError),
}

/// Adds the candidate `operator` with $\boldsymbol\theta$ `parameters` to
/// `group`, its inputs without a requirement (step 3 sets the readouts').
/// `None` when its output is not the group's
/// (`CandidateError::OutputDiffersFromGroup`): the rules match shapes and
/// let the memo decide whether a match computes the group's result. Any
/// other rejection is a bug in the rule and an error.
pub(crate) fn add_candidate(
    memo: &mut Memo,
    group: MemoGroupId,
    operator: Operator<MemoGroupId>,
    parameters: Option<SketchParams>,
    origin: OriginOfCandidateForMemoGroup,
) -> Result<Option<CandidateForMemoGroupId>, Pass2RuleError> {
    let expression = CandidateExpression { operator };
    let logical_properties = LogicalProperties::new(&expression, parameters)?;
    let input_requirements =
        vec![LogicalRequirements::default(); expression.operator.children().len()];
    match memo.add_candidate(
        group,
        CandidateForMemoGroup {
            expression,
            logical_properties,
            input_requirements,
            fused_groups: Vec::new(),
            origin,
        },
    ) {
        Ok(id) => Ok(Some(id)),
        Err(CandidateError::OutputDiffersFromGroup { .. }) => Ok(None),
        Err(error) => Err(Pass2RuleError::Candidate(error)),
    }
}

/// The group of `operator`, with its own expression as a candidate (a
/// group a rule adds between others: a compensating `Filter`, a
/// `TimeRange` or `TimeShift` over a chain).
pub(crate) fn add_group_computing_itself(
    memo: &mut Memo,
    operator: Operator<MemoGroupId>,
    origin: OriginOfCandidateForMemoGroup,
) -> Result<MemoGroupId, Pass2RuleError> {
    let group = memo.find_or_add_group(operator.clone())?;
    add_candidate(memo, group, operator, None, origin)?;
    Ok(group)
}
