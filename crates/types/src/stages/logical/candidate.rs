//! Candidate scope: each group's candidates $v = (e_v, p^{\log}_v)$
//! (concepts doc `memo-groups-properties-requirements.md`, section 3;
//! #1203).

use crate::ir::Operator;
use crate::ir::SchemaDerivationError;
use crate::stages::{LogicalRequirements, MemoGroupId};
use crate::summary_type::SketchParams;

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
    /// The groups a fused computation also implements, beyond the group
    /// whose candidate it is; empty otherwise.
    pub fused_groups: Vec<MemoGroupId>,
    /// Which rule added it. Not in the formulation: kept for
    /// `stage_pipeline --text` and the viewer (approved 2026-11-05).
    pub origin: OriginOfCandidateForMemoGroup,
}

/// Which rule added a candidate. Nothing in planning reads it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum OriginOfCandidateForMemoGroup {
    /// Pass 1: exact, exact accumulator, or a summary type.
    Pass1,
    /// Canonical value preprocessing feeding a summary build.
    SummaryInput,
    /// A tumbling-window chain, or a merge reading one, of a summary state group.
    TumblingWindows,
    /// A coarse group regrouping a finer group.
    Rollup,
    /// A top-k read from another group's ranking.
    TopKReuse,
    /// Its metric's scans widened to a shared label set, read through a
    /// `Project` (scan widening).
    WidenedScans,
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
    /// The candidate's expression fails the node rules (schema derivation,
    /// input validation, merge check): a bug in the rule that built it.
    #[error("the candidate's expression breaks the node rules: {0}")]
    Schema(SchemaDerivationError),
    /// Its output schema or output kind differs from the group's: it does
    /// not compute the group's result (a bug in the rule that built it).
    #[error("the candidate's output differs from group {group:?}'s")]
    OutputDiffersFromGroup { group: MemoGroupId },
}

/// $p^{\log}_v = \boldsymbol\theta_v$. The summary type is the operator's
/// argument, not a property, and a tumbling-window chain's width is the
/// chain operator's argument. [`Self::new`] rejects parameters of another
/// algorithm, not in the algorithm's list, missing on a sketch build (a chain
/// included) or present on an exact operator.
#[derive(Debug, Clone)]
pub struct LogicalProperties {
    /// $\boldsymbol\theta_v$, from the algorithm's predefined list; `None` for
    /// an exact operator.
    parameters: Option<SketchParams>,
}

impl LogicalProperties {
    pub fn new(
        expression: &CandidateExpression,
        parameters: Option<SketchParams>,
    ) -> Result<Self, LogicalPropertiesError> {
        let _ = (expression, parameters);
        todo!("#1207 PR 1: reject invalid parameters")
    }

    /// $\boldsymbol\theta_v$.
    pub fn parameters(&self) -> Option<&SketchParams> {
        self.parameters.as_ref()
    }
}

/// Logical properties that do not fit their operator: bad input to
/// [`LogicalProperties::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LogicalPropertiesError {
    #[error("an exact operator has no summary parameters")]
    ParametersOnExactOperator,
    #[error("a sketch build needs parameters from its algorithm's list")]
    SketchBuildWithoutParameters,
    #[error("the parameters are not in their algorithm's predefined list")]
    ParametersNotInList,
}
