//! The memo and each group's candidates
//! $v = (e_v, p^{\log}_v)$ and each candidate's input requirements
//! $r^{\log}_{c,i}$ (concepts doc `memo-groups-properties-requirements.md`,
//! section 3; #1203).

use std::collections::HashSet;

use super::{GroupProperties, MemoGroup, MemoGroupAsInput, MemoGroupId};
use crate::ir::properties::summary_coverage::SummaryCoverage;
use crate::ir::properties::AccuracyMetric;
use crate::ir::schema::{
    FamilyCapability, FieldDataType, GroupingStrategy, HydraKind, HydraParams, SketchAlgorithm,
    SketchParams,
};
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
        &self.groups[id.group.0 as usize].candidates[id.index_in_group]
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
        match (family_of_operator(&expression.operator), &parameters) {
            (Some(_), None) => return Err(LogicalPropertiesError::SketchBuildWithoutParameters),
            (None, Some(_)) => return Err(LogicalPropertiesError::ParametersOnExactOperator),
            (Some(family), Some(parameters)) => {
                if family != family_of_parameters(parameters) {
                    return Err(LogicalPropertiesError::ParametersOfAnotherFamily);
                }
                if !parameter_list(family).contains(parameters) {
                    return Err(LogicalPropertiesError::ParametersNotInList);
                }
            }
            (None, None) => {}
        }
        Ok(Self { parameters })
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
    #[error("a sketch build needs parameters from its family's list")]
    SketchBuildWithoutParameters,
    #[error("the parameters are not in their family's predefined list")]
    ParametersNotInList,
}

/// The family's predefined $\boldsymbol\theta$ list (#1207 step 6), each with
/// its error bound derived through `estimators::local_guarantee` for a given
/// statistic.
///
/// Each list is today's sizing (`asap_accuracy`'s `size_params`) at
/// failure probability 0.01 over the errors ε = 0.1, 0.05, 0.02, 0.01,
/// 0.005, 0.002 and 0.001, weakest first, without repeats; an
/// `asap_accuracy` test keeps them equal. Distinct counts (KMV, and
/// UnivMon's distinct-count shapes) stop at ε = 0.01. A UnivMon's list is
/// its L2-norm shapes, then its distinct-count shapes, then its shape for
/// every other statistic. A heap variant combines each width × depth with
/// each heap size of [`HEAP_SIZES`], by width, then heap size: a top-k
/// consumer's floor is heap_size ≥ k, and the next listed step up is at most
/// 2.5× too large. Empty for a family Pass 1 does not offer (the Hydra
/// families included).
pub fn parameter_list(family: SketchAlgorithm) -> &'static [SketchParams] {
    match family {
        SketchAlgorithm::Kll => &KLL_PARAMETERS,
        SketchAlgorithm::DDSketch => &DDSKETCH_PARAMETERS,
        SketchAlgorithm::Hll => &HLL_PARAMETERS,
        SketchAlgorithm::Kmv => &KMV_PARAMETERS,
        SketchAlgorithm::Cms => &CMS_PARAMETERS,
        SketchAlgorithm::CountSketch => &COUNT_SKETCH_PARAMETERS,
        SketchAlgorithm::CmsWithHeap => &CMS_WITH_HEAP_PARAMETERS,
        SketchAlgorithm::CountSketchWithHeap => &COUNT_SKETCH_WITH_HEAP_PARAMETERS,
        SketchAlgorithm::UnivMon => &UNIVMON_PARAMETERS,
        // Listed, not `_`: a family `asap_sketchlib` adds fails to compile
        // here until it gets a list or is listed as not offered.
        SketchAlgorithm::CountSketchWithL2Norm
        | SketchAlgorithm::CmsWithSpaceSaving
        | SketchAlgorithm::CountSketchWithSpaceSaving
        | SketchAlgorithm::CmsFold
        | SketchAlgorithm::CountSketchFold
        | SketchAlgorithm::HllHip
        | SketchAlgorithm::KllDynamic
        | SketchAlgorithm::SpaceSaving
        | SketchAlgorithm::Elastic
        | SketchAlgorithm::Coco
        | SketchAlgorithm::Bloom
        | SketchAlgorithm::SetAggregator
        | SketchAlgorithm::UnivMonTwoTier
        | SketchAlgorithm::UnivMonQ
        | SketchAlgorithm::HydraCms
        | SketchAlgorithm::HydraCountSketch
        | SketchAlgorithm::HydraKll
        | SketchAlgorithm::HydraHll
        | SketchAlgorithm::HydraUnivMon => &[],
    }
}

const KLL_PARAMETERS: [SketchParams; 7] = [
    kll(26),
    kll(52),
    kll(132),
    kll(269),
    kll(547),
    kll(1404),
    kll(2863),
];

const DDSKETCH_PARAMETERS: [SketchParams; 7] = [
    SketchParams::DDSketch { alpha: 0.1 },
    SketchParams::DDSketch { alpha: 0.05 },
    SketchParams::DDSketch { alpha: 0.02 },
    SketchParams::DDSketch { alpha: 0.01 },
    SketchParams::DDSketch { alpha: 0.005 },
    SketchParams::DDSketch { alpha: 0.002 },
    SketchParams::DDSketch { alpha: 0.001 },
];

/// HLL's precision saturates at 18 from ε = 0.01 on.
const HLL_PARAMETERS: [SketchParams; 4] =
    [hll(13, 0.1), hll(15, 0.05), hll(17, 0.02), hll(18, 0.01)];

const KMV_PARAMETERS: [SketchParams; 4] = [
    kmv(1_118, 0.1),
    kmv(4_323, 0.05),
    kmv(26_656, 0.02),
    kmv(106_263, 0.01),
];

const CMS_PARAMETERS: [SketchParams; 7] = [
    SketchParams::Cms {
        width: 28,
        depth: 5,
    },
    SketchParams::Cms {
        width: 55,
        depth: 5,
    },
    SketchParams::Cms {
        width: 136,
        depth: 5,
    },
    SketchParams::Cms {
        width: 272,
        depth: 5,
    },
    SketchParams::Cms {
        width: 544,
        depth: 5,
    },
    SketchParams::Cms {
        width: 1_360,
        depth: 5,
    },
    SketchParams::Cms {
        width: 2_719,
        depth: 5,
    },
];

const COUNT_SKETCH_PARAMETERS: [SketchParams; 7] = [
    SketchParams::CountSketch {
        width: 300,
        depth: 47,
    },
    SketchParams::CountSketch {
        width: 1_200,
        depth: 47,
    },
    SketchParams::CountSketch {
        width: 7_500,
        depth: 47,
    },
    SketchParams::CountSketch {
        width: 30_000,
        depth: 47,
    },
    SketchParams::CountSketch {
        width: 120_000,
        depth: 47,
    },
    SketchParams::CountSketch {
        width: 750_000,
        depth: 47,
    },
    SketchParams::CountSketch {
        width: 3_000_000,
        depth: 47,
    },
];

/// The heap sizes of a heap variant's list: the 1-2-5 series from 10 to
/// 10 000, so a top-k with k ≤ 10 000 has a listed heap.
const HEAP_SIZES: [u32; 10] = [10, 20, 50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000];

const CMS_WITH_HEAP_PARAMETERS: [SketchParams; 70] = with_heap_sizes(&CMS_PARAMETERS);

const COUNT_SKETCH_WITH_HEAP_PARAMETERS: [SketchParams; 70] =
    with_heap_sizes(&COUNT_SKETCH_PARAMETERS);

/// Each `Cms` (or `CountSketch`) shape of `shapes` with each heap size of
/// [`HEAP_SIZES`], as `CmsWithHeap` (or `CountSketchWithHeap`).
const fn with_heap_sizes(shapes: &[SketchParams; 7]) -> [SketchParams; 70] {
    const UNSET: SketchParams = SketchParams::Cms { width: 0, depth: 0 };
    let mut list = [UNSET; 70];
    let mut shape = 0;
    while shape < shapes.len() {
        let mut heap = 0;
        while heap < HEAP_SIZES.len() {
            let heap_size = HEAP_SIZES[heap];
            list[shape * HEAP_SIZES.len() + heap] = match shapes[shape] {
                SketchParams::Cms { width, depth } => SketchParams::CmsWithHeap {
                    width,
                    depth,
                    heap_size,
                },
                SketchParams::CountSketch { width, depth } => SketchParams::CountSketchWithHeap {
                    width,
                    depth,
                    heap_size,
                },
                _ => panic!("a heap variant extends a Cms or CountSketch shape"),
            };
            heap += 1;
        }
        shape += 1;
    }
    list
}

const UNIVMON_PARAMETERS: [SketchParams; 12] = [
    univmon_l2(624),
    univmon_l2(2_367),
    univmon_l2(14_349),
    univmon_l2(56_817),
    univmon_l2(226_130),
    univmon_l2(1_409_067),
    univmon_l2(5_630_630),
    univmon_distinct(13_500),
    univmon_distinct(54_000),
    univmon_distinct(337_500),
    univmon_distinct(1_350_000),
    SketchParams::UnivMon {
        heap_size: 256,
        sketch_rows: 5,
        sketch_cols: 1_024,
        layers: 16,
        distinct_failure_probability: None,
    },
];

const fn kll(k: u32) -> SketchParams {
    SketchParams::Kll {
        k,
        failure_probability: None,
    }
}

const fn hll(precision: u8, relative_error: f64) -> SketchParams {
    SketchParams::Hll {
        precision,
        relative_error: Some(relative_error),
    }
}

const fn kmv(k: u32, relative_error: f64) -> SketchParams {
    SketchParams::Kmv {
        k,
        relative_error: Some(relative_error),
    }
}

const fn univmon_l2(sketch_cols: u32) -> SketchParams {
    SketchParams::UnivMon {
        heap_size: 1,
        sketch_rows: 5,
        sketch_cols,
        layers: 1,
        distinct_failure_probability: None,
    }
}

const fn univmon_distinct(heap_size: u32) -> SketchParams {
    SketchParams::UnivMon {
        heap_size,
        sketch_rows: 1,
        sketch_cols: 1,
        layers: 19,
        distinct_failure_probability: Some(0.01),
    }
}

/// The sketch family an operator builds: a `SummaryAgg`'s or a
/// `SummaryTumblingWindowChain`'s sketch family (a Hydra one's Hydra
/// family); `None` for every other operator, an exact accumulator build
/// included.
fn family_of_operator(operator: &Operator<MemoGroupId>) -> Option<SketchAlgorithm> {
    let (Operator::ASAP(ASAPOp::SummaryAgg { family, .. })
    | Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { family, .. })) = operator
    else {
        return None;
    };
    match family {
        FieldDataType::Sketch(algorithm, GroupingStrategy::PerSubpopulationInstance) => {
            Some(*algorithm)
        }
        FieldDataType::Sketch(_, GroupingStrategy::SharedMultiSubpopulation { kind }) => {
            Some(match kind {
                HydraKind::HydraKll => SketchAlgorithm::HydraKll,
                HydraKind::HydraCms => SketchAlgorithm::HydraCms,
                HydraKind::HydraCountSketch => SketchAlgorithm::HydraCountSketch,
            })
        }
        FieldDataType::Plain(_)
        | FieldDataType::ExactAggregate(..)
        | FieldDataType::SummarySample(..)
        | FieldDataType::Wavelet(..)
        | FieldDataType::StatModel(..) => None,
    }
}

/// The family `parameters` size.
fn family_of_parameters(parameters: &SketchParams) -> SketchAlgorithm {
    match parameters {
        SketchParams::UnivMon { .. } => SketchAlgorithm::UnivMon,
        SketchParams::Kll { .. } => SketchAlgorithm::Kll,
        SketchParams::Cms { .. } => SketchAlgorithm::Cms,
        SketchParams::Hll { .. } => SketchAlgorithm::Hll,
        SketchParams::DDSketch { .. } => SketchAlgorithm::DDSketch,
        SketchParams::CmsWithHeap { .. } => SketchAlgorithm::CmsWithHeap,
        SketchParams::Kmv { .. } => SketchAlgorithm::Kmv,
        SketchParams::CountSketch { .. } => SketchAlgorithm::CountSketch,
        SketchParams::CountSketchWithHeap { .. } => SketchAlgorithm::CountSketchWithHeap,
        SketchParams::Hydra(HydraParams::HydraKll { .. }) => SketchAlgorithm::HydraKll,
        SketchParams::Hydra(HydraParams::HydraCms { .. }) => SketchAlgorithm::HydraCms,
        SketchParams::Hydra(HydraParams::HydraCountSketch { .. }) => {
            SketchAlgorithm::HydraCountSketch
        }
    }
}

/// Whether a tumbling window layout fits the operator: a `SummaryAgg` whose
/// states merge, so that its windows' states make the whole range's.
fn can_be_windowed(operator: &Operator<MemoGroupId>) -> bool {
    match operator {
        Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) => {
            FamilyCapability::of(family).merge_inputs.is_some()
        }
        Operator::ASAP(_) | Operator::NonASAP(_) => false,
    }
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
    match (
        producer.parameters(),
        requirement.parameter_floor_for_required_accuracy(),
    ) {
        (_, None) | (None, Some(_)) => true,
        (Some(parameters), Some(floor)) => dominates(parameters, floor),
    }
}

/// $\boldsymbol\theta \succeq \boldsymbol\theta^{\min}$: every parameter
/// that sets $\varepsilon$ is at least the floor's (at most, for DDSketch's
/// `alpha`). The recorded failure probabilities and relative errors are not
/// compared: every listed configuration of a family records the same
/// failure probability, 0.01. Parameters of different families never
/// dominate.
fn dominates(parameters: &SketchParams, floor: &SketchParams) -> bool {
    use SketchParams as P;
    match (parameters, floor) {
        (P::Kll { k, .. }, P::Kll { k: floor_k, .. })
        | (P::Kmv { k, .. }, P::Kmv { k: floor_k, .. }) => k >= floor_k,
        (P::DDSketch { alpha }, P::DDSketch { alpha: floor_alpha }) => alpha <= floor_alpha,
        (
            P::Hll { precision, .. },
            P::Hll {
                precision: floor_precision,
                ..
            },
        ) => precision >= floor_precision,
        (P::Cms { width, depth }, P::Cms { width: w, depth: d })
        | (P::CountSketch { width, depth }, P::CountSketch { width: w, depth: d }) => {
            width >= w && depth >= d
        }
        (
            P::CmsWithHeap {
                width,
                depth,
                heap_size,
            },
            P::CmsWithHeap {
                width: w,
                depth: d,
                heap_size: h,
            },
        )
        | (
            P::CountSketchWithHeap {
                width,
                depth,
                heap_size,
            },
            P::CountSketchWithHeap {
                width: w,
                depth: d,
                heap_size: h,
            },
        ) => width >= w && depth >= d && heap_size >= h,
        (
            P::UnivMon {
                heap_size,
                sketch_rows,
                sketch_cols,
                layers,
                ..
            },
            P::UnivMon {
                heap_size: h,
                sketch_rows: r,
                sketch_cols: c,
                layers: l,
                ..
            },
        ) => heap_size >= h && sketch_rows >= r && sketch_cols >= c && layers >= l,
        (
            P::Hydra(HydraParams::HydraKll { k, shared_buckets }),
            P::Hydra(HydraParams::HydraKll {
                k: floor_k,
                shared_buckets: b,
            }),
        ) => k >= floor_k && shared_buckets >= b,
        (
            P::Hydra(HydraParams::HydraCms {
                width,
                depth,
                shared_rows,
                shared_columns,
            }),
            P::Hydra(HydraParams::HydraCms {
                width: w,
                depth: d,
                shared_rows: r,
                shared_columns: c,
            }),
        )
        | (
            P::Hydra(HydraParams::HydraCountSketch {
                width,
                depth,
                shared_rows,
                shared_columns,
            }),
            P::Hydra(HydraParams::HydraCountSketch {
                width: w,
                depth: d,
                shared_rows: r,
                shared_columns: c,
            }),
        ) => width >= w && depth >= d && shared_rows >= r && shared_columns >= c,
        // Different families. Listed by `parameters`' variant, not `_`, so a
        // new variant fails to compile here until it is compared above.
        (P::Kll { .. }, _)
        | (P::Kmv { .. }, _)
        | (P::DDSketch { .. }, _)
        | (P::Hll { .. }, _)
        | (P::Cms { .. }, _)
        | (P::CountSketch { .. }, _)
        | (P::CmsWithHeap { .. }, _)
        | (P::CountSketchWithHeap { .. }, _)
        | (P::UnivMon { .. }, _)
        | (P::Hydra(_), _) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::operator::operator_properties::Reduction;
    use crate::ir::scalar::expr_ir::ColumnRef;
    use crate::ir::schema::{ExactKind, SummaryUpdate};

    /// `SummaryAgg` over group 0 building a state of `family`.
    fn summary_agg(family: FieldDataType) -> CandidateExpression {
        CandidateExpression {
            operator: Operator::ASAP(ASAPOp::SummaryAgg {
                child: MemoGroupId(0),
                family,
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
                grouping: GroupingStrategy::default(),
                filter: None,
            }),
        }
    }

    fn sketch(algorithm: SketchAlgorithm, grouping: GroupingStrategy) -> FieldDataType {
        FieldDataType::Sketch(algorithm, grouping)
    }

    const KLL_269: SketchParams = kll(269);
    const CMS: SketchParams = SketchParams::Cms {
        width: 272,
        depth: 5,
    };
    const HYDRA_CMS: SketchParams = SketchParams::Hydra(HydraParams::HydraCms {
        width: 272,
        depth: 5,
        shared_rows: 3,
        shared_columns: 1_024,
    });

    /// `LogicalProperties::new` over every operator kind, parameters and
    /// window layout: a sketch build needs parameters of its family from its
    /// list; parameters are rejected on an operator without a sketch family;
    /// a window is rejected at width 0 or on an operator other than a
    /// `SummaryAgg` whose states merge. Parameters are checked first.
    #[test]
    fn logical_properties_new_over_every_operator_parameters_and_window() {
        use LogicalPropertiesError as Invalid;
        let per_instance = GroupingStrategy::PerSubpopulationInstance;
        let hydra = GroupingStrategy::SharedMultiSubpopulation {
            kind: HydraKind::HydraCms,
        };
        let kll_chain = CandidateExpression {
            operator: Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                child: MemoGroupId(0),
                family: sketch(SketchAlgorithm::Kll, per_instance.clone()),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
                grouping: per_instance.clone(),
                filter: None,
                tumbling_window_width_ms: 5_000,
                tumbling_window_count: 12,
            }),
        };
        let readout = CandidateExpression {
            operator: Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: MemoGroupId(0),
                query: crate::ir::schema::SketchStatistic::Quantile { q: 0.5 },
            }),
        };
        // KLL_270 is of KLL's type but not in its list.
        let parameters = [
            None,
            Some(KLL_269),
            Some(CMS),
            Some(HYDRA_CMS),
            Some(kll(270)),
        ];
        let windows = [
            None,
            Some(WindowLayout::Tumbling { width_ms: 5_000 }),
            Some(WindowLayout::Tumbling { width_ms: 0 }),
        ];
        let no_parameters = Err(Invalid::ParametersOnExactOperator);
        let needs_parameters = Err(Invalid::SketchBuildWithoutParameters);
        let not_in_list = Err(Invalid::ParametersNotInList);
        let another_family = Err(Invalid::ParametersOfAnotherFamily);
        let cannot_be_windowed = Err(Invalid::WindowOnOperatorThatCannotBeWindowed);
        let zero_width = Err(Invalid::ZeroWindowWidth);
        // Per operator: the result for each of `parameters`, then for each of
        // `windows`; a combination fails with the parameters' error first.
        let table = [
            (
                summary_agg(sketch(SketchAlgorithm::Kll, per_instance.clone())),
                [
                    needs_parameters,
                    Ok(()),
                    another_family,
                    another_family,
                    not_in_list,
                ],
                [Ok(()), Ok(()), zero_width],
            ),
            (
                // No Hydra θ is listed yet, so no Hydra build is valid.
                summary_agg(sketch(SketchAlgorithm::Cms, hydra)),
                [
                    needs_parameters,
                    another_family,
                    another_family,
                    not_in_list,
                    another_family,
                ],
                [Ok(()), Ok(()), zero_width],
            ),
            (
                summary_agg(FieldDataType::exact(ExactKind::Sum)),
                [
                    Ok(()),
                    no_parameters,
                    no_parameters,
                    no_parameters,
                    no_parameters,
                ],
                [Ok(()), Ok(()), zero_width],
            ),
            // IRate's states do not merge across a window boundary.
            (
                summary_agg(FieldDataType::exact(ExactKind::IRate)),
                [
                    Ok(()),
                    no_parameters,
                    no_parameters,
                    no_parameters,
                    no_parameters,
                ],
                [Ok(()), cannot_be_windowed, zero_width],
            ),
            (
                kll_chain,
                [
                    needs_parameters,
                    Ok(()),
                    another_family,
                    another_family,
                    not_in_list,
                ],
                [Ok(()), cannot_be_windowed, zero_width],
            ),
            (
                readout,
                [
                    Ok(()),
                    no_parameters,
                    no_parameters,
                    no_parameters,
                    no_parameters,
                ],
                [Ok(()), cannot_be_windowed, zero_width],
            ),
        ];
        for (expression, by_parameters, by_window) in table {
            for (parameters, parameters_result) in parameters.iter().zip(by_parameters) {
                for (window, window_result) in windows.iter().zip(by_window) {
                    let expected = parameters_result.and(window_result);
                    let properties =
                        LogicalProperties::new(&expression, parameters.clone(), *window);
                    assert_eq!(
                        properties.as_ref().map(|_| ()).map_err(|e| *e),
                        expected,
                        "{:?} {parameters:?} {window:?}",
                        expression.operator.kind_name()
                    );
                    if let Ok(properties) = properties {
                        assert_eq!(properties.parameters(), parameters.as_ref());
                        assert_eq!(properties.window_layout(), window.as_ref());
                    }
                }
            }
        }
    }

    /// Each predefined list holds only its family's parameters, without
    /// repeats, and is empty exactly for the families Pass 1 does not offer.
    #[test]
    fn parameter_list_holds_its_family_without_repeats() {
        let offered = [
            SketchAlgorithm::Kll,
            SketchAlgorithm::DDSketch,
            SketchAlgorithm::Hll,
            SketchAlgorithm::Kmv,
            SketchAlgorithm::Cms,
            SketchAlgorithm::CountSketch,
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap,
            SketchAlgorithm::UnivMon,
        ];
        for family in offered.iter().copied().chain([
            SketchAlgorithm::HydraCms,
            SketchAlgorithm::HydraCountSketch,
            SketchAlgorithm::HydraKll,
            SketchAlgorithm::HllHip,
        ]) {
            let list = parameter_list(family);
            assert_eq!(list.is_empty(), !offered.contains(&family), "{family:?}");
            for (i, parameters) in list.iter().enumerate() {
                assert_eq!(family_of_parameters(parameters), family);
                assert!(!list[..i].contains(parameters), "{parameters:?} repeats");
            }
        }
    }

    /// `LogicalRequirements::new` accepts no floor and any value of a family's
    /// parameter type, listed or not (e.g. heap_size ≥ k exactly).
    #[test]
    fn logical_requirement_new_accepts_any_floor() {
        assert_eq!(
            LogicalRequirements::new(None).parameter_floor_for_required_accuracy(),
            None
        );
        for floor in [
            KLL_269,
            kll(270),
            SketchParams::DDSketch { alpha: 0.03 },
            SketchParams::CmsWithHeap {
                width: 272,
                depth: 5,
                heap_size: 37,
            },
            HYDRA_CMS,
        ] {
            let requirement = LogicalRequirements::new(Some(floor.clone()));
            assert_eq!(
                requirement.parameter_floor_for_required_accuracy(),
                Some(&floor)
            );
        }
    }

    /// `producer_satisfies_logical_requirements`: no floor or an exact producer always satisfies;
    /// otherwise every parameter that sets ε is at least the floor's (at
    /// most, for DDSketch's `alpha`), and another family never satisfies.
    #[test]
    fn producer_satisfies_logical_requirements_compares_the_parameters_that_set_the_error() {
        let list = |family| parameter_list(family);
        let [kll_26, _, _, kll_269, kll_547, ..] = list(SketchAlgorithm::Kll) else {
            unreachable!()
        };
        let [dd_10, _, _, dd_1, ..] = list(SketchAlgorithm::DDSketch) else {
            unreachable!()
        };
        let [hll_13, _, _, hll_18] = list(SketchAlgorithm::Hll) else {
            unreachable!()
        };
        let [kmv_1118, kmv_4323, ..] = list(SketchAlgorithm::Kmv) else {
            unreachable!()
        };
        let [cms_28, cms_55, ..] = list(SketchAlgorithm::Cms) else {
            unreachable!()
        };
        let [cs_300, cs_1200, ..] = list(SketchAlgorithm::CountSketch) else {
            unreachable!()
        };
        let [cms_heap_10, cms_heap_20, ..] = list(SketchAlgorithm::CmsWithHeap) else {
            unreachable!()
        };
        let [cs_heap_10, cs_heap_20, ..] = list(SketchAlgorithm::CountSketchWithHeap) else {
            unreachable!()
        };
        let univmon = list(SketchAlgorithm::UnivMon);
        let (l2_624, l2_2367, distinct_13500, univmon_other) =
            (&univmon[0], &univmon[1], &univmon[7], &univmon[11]);
        let build = |parameters: &SketchParams| {
            let family = family_of_parameters(parameters);
            LogicalProperties::new(
                &summary_agg(sketch(family, GroupingStrategy::default())),
                Some(parameters.clone()),
                None,
            )
            .unwrap()
        };
        let floor = |parameters: &SketchParams| LogicalRequirements::new(Some(parameters.clone()));
        let exact = LogicalProperties::new(
            &summary_agg(FieldDataType::exact(ExactKind::Sum)),
            None,
            None,
        )
        .unwrap();
        let no_floor = LogicalRequirements::new(None);
        assert!(producer_satisfies_logical_requirements(
            &build(kll_26),
            &no_floor
        ));
        assert!(producer_satisfies_logical_requirements(&exact, &no_floor));
        assert!(producer_satisfies_logical_requirements(
            &exact,
            &floor(kll_547)
        ));
        // A top-37 needs heap_size ≥ 37: the next listed step, 50, satisfies.
        let top_37 = SketchParams::CmsWithHeap {
            width: 28,
            depth: 5,
            heap_size: 37,
        };
        let heap = |heap_size| SketchParams::CmsWithHeap {
            width: 28,
            depth: 5,
            heap_size,
        };
        assert!(producer_satisfies_logical_requirements(
            &build(&heap(50)),
            &floor(&top_37)
        ));
        assert!(!producer_satisfies_logical_requirements(
            &build(&heap(20)),
            &floor(&top_37)
        ));
        for (producer, required, satisfies) in [
            (kll_547, kll_269, true),
            (kll_269, kll_269, true),
            (kll_269, kll_547, false),
            (dd_1, dd_10, true),
            (dd_10, dd_1, false),
            (hll_18, hll_13, true),
            (hll_13, hll_18, false),
            (kmv_4323, kmv_1118, true),
            (kmv_1118, kmv_4323, false),
            (cms_55, cms_28, true),
            (cms_28, cms_55, false),
            (cs_1200, cs_300, true),
            (cs_300, cs_1200, false),
            (cms_heap_20, cms_heap_10, true),
            (cms_heap_10, cms_heap_20, false),
            (cs_heap_20, cs_heap_10, true),
            (cs_heap_10, cs_heap_20, false),
            (l2_2367, l2_624, true),
            (l2_624, l2_2367, false),
            // A wider, deeper and heavier UnivMon dominates each L2 shape it
            // covers, but no shape dominates one with a larger heap.
            (univmon_other, l2_624, true),
            (univmon_other, l2_2367, false),
            (univmon_other, distinct_13500, false),
            (distinct_13500, univmon_other, false),
            // Another family.
            (kll_547, dd_10, false),
            (cms_55, cs_300, false),
            (cms_heap_20, cms_28, false),
        ] {
            assert_eq!(
                producer_satisfies_logical_requirements(&build(producer), &floor(required)),
                satisfies,
                "{producer:?} ⪰ {required:?}"
            );
        }
    }

    /// Hydra configurations dominate per kind on every size, and never across
    /// kinds (no Hydra floor is listed yet, so this checks `dominates` alone).
    #[test]
    fn hydra_dominance_compares_every_size_of_one_kind() {
        let hydra_cms = |width, shared_columns| {
            SketchParams::Hydra(HydraParams::HydraCms {
                width,
                depth: 5,
                shared_rows: 3,
                shared_columns,
            })
        };
        let hydra_count_sketch = SketchParams::Hydra(HydraParams::HydraCountSketch {
            width: 300,
            depth: 47,
            shared_rows: 3,
            shared_columns: 1_024,
        });
        let hydra_kll =
            |k, shared_buckets| SketchParams::Hydra(HydraParams::HydraKll { k, shared_buckets });
        for (parameters, floor, dominates_floor) in [
            (hydra_cms(272, 1_024), hydra_cms(272, 1_024), true),
            (hydra_cms(544, 2_048), hydra_cms(272, 1_024), true),
            (hydra_cms(544, 512), hydra_cms(272, 1_024), false),
            (hydra_kll(269, 64), hydra_kll(132, 64), true),
            (hydra_kll(269, 32), hydra_kll(132, 64), false),
            (hydra_count_sketch.clone(), hydra_cms(28, 16), false),
            (hydra_cms(28, 16), CMS, false),
        ] {
            assert_eq!(
                dominates(&parameters, &floor),
                dominates_floor,
                "{parameters:?} ⪰ {floor:?}"
            );
        }
    }
}
