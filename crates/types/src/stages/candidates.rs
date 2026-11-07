//! The memo and each group's candidates
//! $v = (e_v, p^{\log}_v)$ and each candidate's input requirements
//! $r^{\log}_{c,i}$ (concepts doc `memo-groups-properties-requirements.md`,
//! section 3; #1203).

use std::collections::HashSet;

use super::{GroupProperties, MemoGroup, MemoGroupAsInput, MemoGroupId};
use crate::ir::properties::summary_coverage::SummaryCoverage;
use crate::ir::properties::AccuracyMetric;
use crate::ir::schema::{SketchAlgorithm, SketchParams};
use crate::ir::ASAPOp;
use crate::ir::Operator;
use crate::ir::QueryRoot;
use crate::ir::SchemaDerivationError;

/// One candidate: position `index_in_group` in group `group`'s candidates
/// ([`MemoGroup::candidates`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CandidateForMemoGroupId {
    pub group: MemoGroupId,
    pub index_in_group: usize,
}

/// The memo, Stage 0's and Stage 1's one type: the memo groups, each with
/// its candidates ([`MemoGroup::candidates`]: empty after Stage 0), and each
/// query's root over them.
#[derive(Debug, Clone, Default)]
pub struct Memo {
    /// The groups, by id ([`Self::groups`]); added only through
    /// [`Self::find_or_add_group`].
    groups: Vec<MemoGroup>,
    /// Each query's root over groups, by index in the workload.
    pub query_roots: Vec<QueryRoot<MemoGroupId>>,
}

impl Memo {
    /// The groups, by id.
    pub fn groups(&self) -> &[MemoGroup] {
        &self.groups
    }

    /// Adds a query whose root is `root` and returns its index: the root's
    /// groups and every group below them list it
    /// ([`MemoGroup::reached_by_queries`]). Stage 0 adds each query once.
    pub fn add_query(&mut self, root: QueryRoot<MemoGroupId>) -> usize {
        let _ = root;
        todo!("#1207 PR 2")
    }

    /// Hash-consing: the group with this expression, added if new, with the
    /// `GroupProperties` [`derive_group_properties`] derives. Fails when the
    /// expression breaks the node rules (schema derivation, input
    /// validation, merge check).
    pub fn find_or_add_group(
        &mut self,
        expression: Operator<MemoGroupId>,
    ) -> Result<MemoGroupId, SchemaDerivationError> {
        let _group_properties = derive_group_properties(self, &expression)?;
        todo!("#1207 PR 2: hash-consing over groups")
    }

    /// Adds `candidate` to `group`'s candidates. Fails when its expression
    /// breaks the node rules ([`derive_group_properties`]) or its output
    /// schema or kind differs from the group's.
    pub fn add_candidate(
        &mut self,
        group: MemoGroupId,
        candidate: CandidateForMemoGroup,
    ) -> Result<CandidateForMemoGroupId, CandidateError> {
        let _output = derive_group_properties(self, &candidate.expression.operator)
            .map_err(CandidateError::Schema)?;
        let _ = group;
        todo!("#1207 PR 2")
    }

    /// The candidate `id`.
    pub fn candidate(&self, id: CandidateForMemoGroupId) -> &CandidateForMemoGroup {
        let _ = id;
        todo!("#1207 PR 2")
    }

    /// Replaces candidate `id`'s input requirements $r^{\log}_{c,i}$, one
    /// per input group. The only way to change a candidate's
    /// $r^{\log}$: Pass 2 sets each parameter floor from the input's caps
    /// and direct needs after the rules have run. Panics when the vector
    /// does not have one entry per input group.
    pub fn set_input_requirements(
        &mut self,
        id: CandidateForMemoGroupId,
        input_requirements: Vec<LogicalRequirements>,
    ) {
        let candidate = &mut self.groups[id.group.0 as usize].candidates[id.index_in_group];
        assert_eq!(
            input_requirements.len(),
            candidate.expression.operator.children().len(),
            "one input requirement per input group of candidate {id:?}"
        );
        candidate.input_requirements = input_requirements;
    }

    /// The groups, each after every group it or one of its candidates
    /// reads, the lower index first among those ready: a dependency order for
    /// candidate traversal, whatever order rules appended groups in. `None`
    /// when those reads form a cycle, a group reading itself included.
    pub fn topological_order(&self) -> Option<Vec<MemoGroupId>> {
        let count = self.groups.len();
        let mut reads: Vec<HashSet<usize>> = vec![HashSet::new(); count];
        for (g, group) in self.groups.iter().enumerate() {
            for child in group.expression.children().into_iter().chain(
                group
                    .candidates
                    .iter()
                    .flat_map(|c| c.expression.operator.children()),
            ) {
                // A group reading itself never becomes ready: a cycle.
                reads[g].insert(child.0 as usize);
            }
        }
        let mut consumers: Vec<Vec<usize>> = vec![Vec::new(); count];
        let mut waiting: Vec<usize> = reads.iter().map(HashSet::len).collect();
        for (g, read) in reads.iter().enumerate() {
            for &child in read {
                consumers[child].push(g);
            }
        }
        let mut ready: std::collections::BinaryHeap<std::cmp::Reverse<usize>> = (0..count)
            .filter(|&g| waiting[g] == 0)
            .map(std::cmp::Reverse)
            .collect();
        let mut order = Vec::with_capacity(count);
        while let Some(std::cmp::Reverse(g)) = ready.pop() {
            order.push(MemoGroupId(g as u32));
            for &consumer in &consumers[g] {
                waiting[consumer] -= 1;
                if waiting[consumer] == 0 {
                    ready.push(std::cmp::Reverse(consumer));
                }
            }
        }
        (order.len() == count).then_some(order)
    }
}

/// The properties of a group, or a candidate, computing `expression` over
/// its input groups: the rules that check operator nodes, reading each
/// input group's `group_properties` and `expression`
/// ([`MemoGroupAsInput`]), never a node tree. Fails when the expression
/// breaks them (schema derivation, input validation, merge check).
fn derive_group_properties(
    memo: &Memo,
    expression: &Operator<MemoGroupId>,
) -> Result<GroupProperties, SchemaDerivationError> {
    let operator = expression.map_children(|&group_id| MemoGroupAsInput { memo, group_id });
    let schema = operator.output_schema()?;
    operator.validate_inputs()?;
    let aggregate_coverage = match SummaryCoverage::derive_from_inputs(&operator) {
        Ok(coverage) => Some(coverage),
        // A merge is valid only over states of one definition it may merge.
        Err(error) if matches!(operator, Operator::ASAP(ASAPOp::SummaryMerge { .. })) => {
            return Err(error.into())
        }
        Err(_) => None,
    };
    Ok(GroupProperties {
        schema,
        result_kind: operator.output_kind(),
        aggregate_coverage,
        shareable_when_equal: operator.is_shareable_when_equal(),
    })
}

/// A candidate [`Memo::add_candidate`] rejects; the memo is unchanged.
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

/// $v = (e_v, p^{\log}_v)$: one candidate of a memo group.
#[derive(Debug, Clone)]
pub struct CandidateForMemoGroup {
    /// $e_v$.
    pub expression: CandidateExpression,
    /// $p^{\log}_v$.
    pub logical_properties: LogicalProperties,
    /// $r^{\log}_{c,i}$, one per input group: per child of
    /// `expression.operator`, in [`Operator::children`] order. Changed only
    /// through [`Memo::set_input_requirements`].
    pub input_requirements: Vec<LogicalRequirements>,
    /// The groups a fused computation also implements, beyond the group
    /// whose candidate it is; empty otherwise.
    pub fused_groups: Vec<MemoGroupId>,
    /// Which rule added it. Not in the formulation: kept for
    /// `stage_pipeline --text` and the viewer (approved 2026-11-05).
    pub origin: OriginOfCandidateForMemoGroup,
}

/// Which rule added a candidate. Nothing in planning reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginOfCandidateForMemoGroup {
    /// Pass 1: exact, exact accumulator, or a summary family.
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

/// $p^{\log}_v = \boldsymbol\theta_v$. The family is the operator's
/// argument, not a property, and a tumbling-window chain's width is the
/// chain operator's argument. [`Self::new`] rejects parameters of another
/// family, not in the family's list, missing on a sketch build (a chain
/// included) or present on an exact operator.
#[derive(Debug, Clone)]
pub struct LogicalProperties {
    /// $\boldsymbol\theta_v$, from the family's predefined list; `None` for
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
    #[error("the parameters belong to another family than the operator's")]
    ParametersOfAnotherFamily,
    #[error("an exact operator has no summary parameters")]
    ParametersOnExactOperator,
    #[error("a sketch build needs parameters from its family's list")]
    SketchBuildWithoutParameters,
    #[error("the parameters are not in their family's predefined list")]
    ParametersNotInList,
}

/// The family's predefined $\boldsymbol\theta$ list (#1207 step 6), each with
/// its error bound derived through `estimators::local_guarantee` for a given
/// statistic.
pub fn parameter_list(family: SketchAlgorithm) -> &'static [SketchParams] {
    let _ = family;
    todo!("#1207 PR 1: the predefined θ list per family")
}

/// $r^{\log}_{c,i}$: only what $G_{c,i}$'s identity does not fix.
#[derive(Debug, Clone)]
pub struct LogicalRequirements {
    /// $\boldsymbol\theta^{\min}_{c,i}$: the weakest parameters whose
    /// $\varepsilon$, for the statistic the consumer reads, is at most
    /// $\text{cap}_{c,i}$ = min(validity cap of $g_c$, pruning cap), or a
    /// direct need such as heap_size ≥ k. Any value of the family's parameter
    /// type; it need not be in the family's list.
    parameter_floor_for_required_accuracy: Option<SketchParams>,
}

impl LogicalRequirements {
    pub fn new(parameter_floor_for_required_accuracy: Option<SketchParams>) -> Self {
        Self {
            parameter_floor_for_required_accuracy,
        }
    }

    /// $\boldsymbol\theta^{\min}_{c,i}$.
    pub fn parameter_floor_for_required_accuracy(&self) -> Option<&SketchParams> {
        self.parameter_floor_for_required_accuracy.as_ref()
    }

    /// $g_c$'s linear bound for errors in `metric`, each input's and the
    /// output's: $\beta_{c,i}$ and $b_{c,i}$ per input, read from the
    /// operator and its arguments. `None` when the consumer has no error
    /// contract for `metric`: an input whose error is in `metric` then gives
    /// no edge (the metric rule). No factor is guessed.
    pub fn error_propagation(
        consumer: &Operator<MemoGroupId>,
        metric: AccuracyMetric,
    ) -> Option<ErrorPropagation> {
        let _ = (consumer, metric);
        todo!("#1207 PR 4: the error contract of each operator, per metric")
    }
}

/// The local error propagation of one consumer: its output error is at
/// most $\sum_i \beta_{c,i}\varepsilon_{c,i}$ while each input's error stays
/// within its validity cap (concepts doc, section 4; approved 2026-11-05).
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorPropagation {
    /// $\beta_{c,i}$ per input, for that input's metric.
    pub factor_per_input: Vec<f64>,
    /// $b_{c,i}$ per input: the cap under which the linear bound holds;
    /// `None` when the bound holds everywhere (e.g. a ratio caps only its
    /// denominator).
    pub validity_cap_per_input: Vec<Option<f64>>,
}

/// $\alpha^{\log}_{u,c,i}$: $\boldsymbol\theta_u \succeq
/// \boldsymbol\theta^{\min}_{c,i}$, compared per family on the parameters
/// that set $\varepsilon$. No floor, or an exact producer, always satisfies.
pub fn producer_satisfies_logical_requirements(
    producer: &LogicalProperties,
    requirement: &LogicalRequirements,
) -> bool {
    let _ = (producer, requirement);
    todo!("#1207 PR 1: per-family dominance")
}
