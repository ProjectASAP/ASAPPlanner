//! The memo and each group's candidates
//! $v = (e_v, p^{\log}_v)$ and each candidate's input requirements
//! $r^{\log}_{c,i}$ (concepts doc `memo-groups-properties-requirements.md`,
//! section 3; #1203).

use std::collections::HashSet;

use super::{MemoGroup, MemoGroupId};
use crate::ir::schema::{SketchAlgorithm, SketchParams};
use crate::ir::Operator;
use crate::ir::QueryRoot;

/// One candidate: index `index` in group `group`'s candidates
/// ([`MemoGroup::candidates`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CandidateForMemoGroupId {
    pub group: MemoGroupId,
    pub index: usize,
}

/// The memo, Stage 0's and Stage 1's one type: the memo groups, each with
/// its candidates ([`MemoGroup::candidates`]: empty after Stage 0), and each
/// query's root over them.
#[derive(Debug, Clone, Default)]
pub struct Memo {
    pub groups: Vec<MemoGroup>,
    /// Each query's root over groups, by index in the workload.
    pub query_roots: Vec<QueryRoot<MemoGroupId>>,
}

impl Memo {
    /// Hash-consing: the group with this expression, added if new.
    pub fn find_or_add_group(&mut self, expression: Operator<MemoGroupId>) -> MemoGroupId {
        let _ = expression;
        todo!("#1207 PR 2: hash-consing over groups")
    }

    /// Adds `candidate` to `group`'s candidates.
    pub fn add_candidate(
        &mut self,
        group: MemoGroupId,
        candidate: CandidateForMemoGroup,
    ) -> CandidateForMemoGroupId {
        let _ = (group, candidate);
        todo!("#1207 PR 2")
    }

    /// The candidate `id`.
    pub fn candidate(&self, id: CandidateForMemoGroupId) -> &CandidateForMemoGroup {
        let _ = id;
        todo!("#1207 PR 2")
    }

    /// The groups, each after every group it or one of its candidates
    /// reads, the lower index first among those ready: a dependency order for
    /// candidate traversal, whatever order rules appended groups in. `None`
    /// when those reads form a cycle, a group reading itself included.
    pub fn topological_order(&self) -> Option<Vec<MemoGroupId>> {
        let count = self.groups.len();
        let mut reads: Vec<HashSet<usize>> = vec![HashSet::new(); count];
        for (g, group) in self.groups.iter().enumerate() {
            for child in group.child_groups.iter().chain(
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

/// $v = (e_v, p^{\log}_v)$: one candidate of a memo group.
#[derive(Debug, Clone)]
pub struct CandidateForMemoGroup {
    /// $e_v$.
    pub expression: CandidateExpression,
    /// $p^{\log}_v$.
    pub logical_properties: LogicalProperties,
    /// $r^{\log}_{c,i}$, one per input group: per child of
    /// `expression.operator`, in [`Operator::children`] order.
    pub input_requirements: Vec<LogicalRequirement>,
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
    /// A tumbling-window layout of a summary state group.
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

/// $p^{\log}_v = (\boldsymbol\theta_v, \omega_v)$. The family is the
/// operator's argument, not a property. [`Self::new`] rejects parameters of
/// another family (or on an exact operator) and a window on an operator that
/// cannot be windowed.
#[derive(Debug, Clone)]
pub struct LogicalProperties {
    /// $\boldsymbol\theta_v$, from the family's predefined list; `None` for
    /// an exact operator.
    parameters: Option<SketchParams>,
    /// $\omega_v$.
    window_layout: Option<WindowLayout>,
}

impl LogicalProperties {
    pub fn new(
        expression: &CandidateExpression,
        parameters: Option<SketchParams>,
        window_layout: Option<WindowLayout>,
    ) -> Result<Self, InvalidLogicalProperties> {
        let _ = (expression, parameters, window_layout);
        todo!("#1207 PR 1: reject parameters of another family and invalid windows")
    }

    /// $\boldsymbol\theta_v$.
    pub fn parameters(&self) -> Option<&SketchParams> {
        self.parameters.as_ref()
    }

    /// $\omega_v$.
    pub fn window_layout(&self) -> Option<&WindowLayout> {
        self.window_layout.as_ref()
    }
}

/// $\omega_v$. The window count is the group's query range ÷ width;
/// retention is $h_v$ (Stage 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowLayout {
    Tumbling { width_ms: u64 },
}

/// Logical properties or a logical requirement whose dimensions cannot go
/// together: bad input to [`LogicalProperties::new`] or
/// [`LogicalRequirement::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidLogicalProperties {
    #[error("the parameters belong to another family than the operator's")]
    ParametersOfAnotherFamily,
    #[error("an exact operator has no summary parameters")]
    ParametersOnExactOperator,
    #[error("the operator cannot be windowed")]
    WindowOnOperatorThatCannotBeWindowed,
    #[error("a tumbling window has a positive width")]
    ZeroWindowWidth,
    #[error("the parameter floor is not in its family's predefined list")]
    FloorNotInParameterList,
}

/// The family's predefined $\boldsymbol\theta$ list (#1207 step 6), each with
/// its error bound derived through `estimators::local_guarantee` for a given
/// statistic.
pub fn parameter_list(family: SketchAlgorithm) -> &'static [SketchParams] {
    let _ = family;
    todo!("#1207 PR 1: the predefined θ list per family")
}

/// $r^{\log}_{c,i}$: only what $G_{c,i}$'s identity does not fix.
/// [`Self::new`] rejects a floor that is not in the family's predefined list.
#[derive(Debug, Clone)]
pub struct LogicalRequirement {
    /// $\boldsymbol\theta^{\min}_{c,i}$: the smallest $\boldsymbol\theta$ in
    /// the list whose $\varepsilon$, for the statistic the consumer reads, is
    /// at most $\text{cap}_{c,i}$ = min(validity cap of $g_c$, pruning cap).
    parameter_floor: Option<SketchParams>,
}

impl LogicalRequirement {
    pub fn new(parameter_floor: Option<SketchParams>) -> Result<Self, InvalidLogicalProperties> {
        let _ = parameter_floor;
        todo!("#1207 PR 1: reject a floor not in the family's predefined list")
    }

    /// $\boldsymbol\theta^{\min}_{c,i}$.
    pub fn parameter_floor(&self) -> Option<&SketchParams> {
        self.parameter_floor.as_ref()
    }

    /// $g_c$'s linear bound: $\beta_{c,i}$ and $b_{c,i}$ per input, read
    /// from the operator and its arguments. An operator without an error
    /// contract is reported as unsupported; no factor is guessed.
    pub fn error_propagation(consumer: &Operator<MemoGroupId>) -> ErrorPropagation {
        let _ = consumer;
        todo!("#1207 PR 4: the error contract of each operator")
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
pub fn satisfies_logical(producer: &LogicalProperties, requirement: &LogicalRequirement) -> bool {
    let _ = (producer, requirement);
    todo!("#1207 PR 1: per-family dominance")
}
