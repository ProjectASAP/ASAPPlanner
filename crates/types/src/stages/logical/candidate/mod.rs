//! Candidate scope: each group's candidates $v = (e_v, p^{\log}_v)$
//! (concepts doc `memo-groups-properties-requirements.md`, section 3).

use crate::ir::operator::asap::summary_type_of_operator;
use crate::ir::schema::FieldDataType;
use crate::ir::Operator;
use crate::stages::{LogicalRequirements, MemoGroupError, MemoGroupId};
use crate::summary_type::{parameter_list, SketchAlgorithm, SketchParams};

/// One candidate: position `index_in_group` in group `group`'s candidates
/// ([`MemoGroup::candidates`](crate::stages::MemoGroup::candidates)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct CandidateForMemoGroupId {
    pub group: MemoGroupId,
    pub index_in_group: usize,
}

/// $v = (e_v, p^{\log}_v)$: one candidate of a memo group.
#[derive(Debug, Clone)]
pub struct CandidateForMemoGroup {
    /// $e_v$.
    pub expression: CandidateExpression,
    /// $p^{\log}_v$.
    pub logical_properties: LogicalProperties,
    /// $r^{\log}_{c,i}$, one per input group: per child of
    /// `expression.operator`, in [`Operator::children`] order. Changed only
    /// through [`Memo::set_candidate_input_requirements`](crate::stages::Memo::set_candidate_input_requirements).
    pub input_requirements: Vec<LogicalRequirements>,
    /// A fused candidate computes, as one operator, a sub-DAG pattern of
    /// several operators in one appearance of the sub-expression (a top-k
    /// over an aggregate as one heap summary; a sum over a sum as one
    /// aggregate): the groups of the operators it replaces. Empty otherwise.
    /// Reuse across appearances is Pass 2's, not fusion (concepts doc §3,
    /// "Fused groups").
    pub fused_groups: Vec<MemoGroupId>,
    /// Which rule added it. Not in the formulation: kept for
    /// `stage_pipeline --text` and the viewer (approved 2026-11-05).
    pub origin: OriginOfCandidateForMemoGroup,
}

impl CandidateForMemoGroup {
    /// The candidate computing `operator` with θ `parameters`, added by
    /// rule `origin`, fusing `fused_groups`. Its inputs have no requirement
    /// yet: Pass 2 sets each readout's
    /// (`Memo::set_candidate_input_requirements`).
    pub fn new(
        operator: Operator<MemoGroupId>,
        parameters: Option<SketchParams>,
        origin: OriginOfCandidateForMemoGroup,
        fused_groups: Vec<MemoGroupId>,
    ) -> Self {
        let input_requirements = vec![LogicalRequirements::default(); operator.children().len()];
        Self {
            expression: CandidateExpression { operator },
            logical_properties: LogicalProperties::new(parameters),
            input_requirements,
            fused_groups,
            origin,
        }
    }
}

/// The θ of each build candidate of `build`: every listed θ
/// (`parameter_list`) of a sketch build, `[None]` for any other operator
/// (an exact build). A group gets one build candidate per entry.
pub fn listed_build_parameters(build: &Operator<MemoGroupId>) -> Vec<Option<SketchParams>> {
    match summary_type_of_operator(build) {
        Some(FieldDataType::Sketch(algorithm)) => parameter_list(*algorithm)
            .iter()
            .cloned()
            .map(Some)
            .collect(),
        _ => vec![None],
    }
}

/// Which rule added a candidate. Nothing in planning reads it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum OriginOfCandidateForMemoGroup {
    /// Pass 1: exact, exact accumulator, or a summary type.
    Pass1,
    /// A tumbling-window chain, or a merge reading one, of a summary state group.
    TumblingWindows,
    /// A coarse group rolled up from a finer one, the compensating `Filter`
    /// it reads, or a build of a finest common group.
    Rollup,
    /// A top-k read from another group's ranking.
    TopKReuse,
}

/// $e_v = \operatorname{op}_v(G_{v,1}, \dots, G_{v,n})$: one operator, with
/// its arguments, whose children are groups. The input groups $G_{v,i}$ are
/// [`Operator::children`], in that order.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateExpression {
    pub operator: Operator<MemoGroupId>,
}

/// A candidate [`Memo::add_candidate`](crate::stages::Memo::add_candidate) rejects; the memo is unchanged.
#[derive(Debug, thiserror::Error)]
pub enum CandidateError {
    /// The candidate's expression is not valid over its input groups (its
    /// output schema does not derive, an input fails validation, or a merge
    /// reads states it may not merge): a bug in the rule that built it.
    #[error("the candidate's expression is not valid over its input groups: {0}")]
    NotValidOverInputGroups(MemoGroupError),
    /// Its output schema or output kind differs from the group's: it does
    /// not compute the group's result (a bug in the rule that built it).
    #[error("the candidate's output differs from group {group:?}'s")]
    OutputDiffersFromGroup { group: MemoGroupId },
    /// Its θ does not fit the group's output (a bug in the rule that built
    /// it).
    #[error("the candidate's parameters do not fit its group's output: {0}")]
    LogicalProperties(LogicalPropertiesError),
}

/// $p^{\log}_v = \boldsymbol\theta_v$. The summary type is the operator's
/// argument, not a property, and a tumbling-window chain's width is the
/// chain operator's argument. A candidate passing states on without building
/// them (`SummaryMerge`, and a `Filter`, `TimeRange` or `TimeShift` over
/// states) carries the θ of the states it reads.
/// [`Memo::add_candidate`](crate::stages::Memo::add_candidate) checks θ
/// against the candidate's group's output ([`parameters_fit`]).
#[derive(Debug, Clone)]
pub struct LogicalProperties {
    /// $\boldsymbol\theta_v$, from the algorithm's predefined list; `None` for
    /// an exact operator.
    pub(super) parameters: Option<SketchParams>,
}

impl LogicalProperties {
    pub fn new(parameters: Option<SketchParams>) -> Self {
        Self { parameters }
    }

    /// $\boldsymbol\theta_v$.
    pub fn parameters(&self) -> Option<&SketchParams> {
        self.parameters.as_ref()
    }
}

/// Whether θ `parameters` fit an output of sketch `algorithm` (`None`: no
/// sketch state): from its list, and present exactly for a sketch.
pub(super) fn parameters_fit(
    algorithm: Option<SketchAlgorithm>,
    parameters: Option<&SketchParams>,
) -> Result<(), LogicalPropertiesError> {
    match (algorithm, parameters) {
        (Some(_), None) => Err(LogicalPropertiesError::SketchBuildWithoutParameters),
        (None, Some(_)) => Err(LogicalPropertiesError::ParametersOnExactOperator),
        (Some(algorithm), Some(parameters)) if !parameter_list(algorithm).contains(parameters) => {
            Err(LogicalPropertiesError::ParametersNotInList)
        }
        _ => Ok(()),
    }
}

/// θ that does not fit the output of a candidate's group, as
/// [`Memo::add_candidate`](crate::stages::Memo::add_candidate) rejects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LogicalPropertiesError {
    #[error("an exact operator has no summary parameters")]
    ParametersOnExactOperator,
    #[error("a sketch build, or a candidate passing sketch states on, needs parameters from its algorithm's list")]
    SketchBuildWithoutParameters,
    #[error("the parameters are not in their algorithm's predefined list")]
    ParametersNotInList,
}

#[cfg(test)]
mod tests;
