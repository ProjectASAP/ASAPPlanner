//! Pass 2's rules over groups (concepts doc, section 3, "Stage 1, Pass 2"):
//! each reads the memo and adds groups (`Memo::find_or_add_group`) or
//! candidates (`Memo::add_candidate`); none copies a consumer, so every
//! consumer of a group sees what a rule adds to it.
pub(crate) mod chains;
pub(crate) mod rollup;
pub(crate) mod topk;

use crate::Pass2RuleError;
use asap_types::ir::Operator;
use asap_types::stages::{
    CandidateError, CandidateForMemoGroup, CandidateForMemoGroupId, Memo, MemoGroupId,
    OriginOfCandidateForMemoGroup,
};
use asap_types::summary_type::SketchParams;

/// Adds the candidate `operator` with $\boldsymbol\theta$ `parameters` to
/// `group`, its inputs without a requirement (step 3 sets the readouts').
/// `None` when its output is not the group's
/// (`CandidateError::OutputDiffersFromGroup`): the rules match shapes and
/// let the memo decide whether a match computes the group's result. Any
/// other rejection is a bug in the rule and an error.
pub(crate) fn add_candidate_if_output_matches(
    memo: &mut Memo,
    group: MemoGroupId,
    operator: Operator<MemoGroupId>,
    parameters: Option<SketchParams>,
    origin: OriginOfCandidateForMemoGroup,
) -> Result<Option<CandidateForMemoGroupId>, Pass2RuleError> {
    match memo.add_candidate(
        group,
        CandidateForMemoGroup::new(operator, parameters, origin, Vec::new()),
    ) {
        Ok(id) => Ok(Some(id)),
        Err(CandidateError::OutputDiffersFromGroup { .. }) => Ok(None),
        Err(error) => Err(Pass2RuleError::Candidate(error)),
    }
}

/// The distinct θ of `candidates`, in order.
pub(crate) fn distinct_parameters<'a>(
    candidates: impl IntoIterator<Item = &'a CandidateForMemoGroup>,
) -> Vec<Option<SketchParams>> {
    let mut parameters = Vec::new();
    for candidate in candidates {
        let theta = candidate.logical_properties.parameters().cloned();
        if !parameters.contains(&theta) {
            parameters.push(theta);
        }
    }
    parameters
}

/// Adds `operator`, which passes on the states or rows of its one input
/// group (a `SummaryMerge`, or a `Filter`, `TimeRange` or `TimeShift`), to
/// `group`: one candidate per θ among the input group's candidates (concepts
/// doc, section 2: such a candidate carries the θ of the states it reads),
/// one without a θ over rows or exact states.
pub(crate) fn add_candidates_per_input_parameters(
    memo: &mut Memo,
    group: MemoGroupId,
    operator: Operator<MemoGroupId>,
    origin: OriginOfCandidateForMemoGroup,
) -> Result<(), Pass2RuleError> {
    let input = *operator.children()[0];
    for theta in distinct_parameters(&memo.groups()[input.0 as usize].candidates) {
        add_candidate_if_output_matches(memo, group, operator.clone(), theta, origin.clone())?;
    }
    Ok(())
}

/// The group of `operator`, with its own expression as its candidates
/// ([`add_candidates_per_input_parameters`]): a group a rule adds between
/// others, a compensating `Filter`, a `TimeRange` or `TimeShift` over a
/// chain.
pub(crate) fn add_group_computing_itself(
    memo: &mut Memo,
    operator: Operator<MemoGroupId>,
    origin: OriginOfCandidateForMemoGroup,
) -> Result<MemoGroupId, Pass2RuleError> {
    let group = memo.find_or_add_group(operator.clone())?;
    add_candidates_per_input_parameters(memo, group, operator, origin)?;
    Ok(group)
}
