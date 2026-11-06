//! The memo and each group's candidates
//! $v = (e_v, p^{\log}_v)$ and each candidate's input requirements
//! $r^{\log}_{c,i}$ (concepts doc `memo-groups-properties-requirements.md`,
//! section 3; #1203).

use std::collections::{HashMap, HashSet};

use super::hash_consing::{expression_hash, same_expression, same_value};
use super::{GroupProperties, MemoGroup, MemoGroupAsInput, MemoGroupId};
use crate::ir::operator::asap::family_of_operator;
use crate::ir::properties::summary_coverage::SummaryCoverage;
use crate::ir::properties::AccuracyMetric;
use crate::ir::schema::family_capability::sketch_algorithm_of;
use crate::ir::schema::{HydraParams, SketchAlgorithm, SketchParams};
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
    /// The groups [`Self::find_or_add_group`] may return, by
    /// [`expression_hash`] of their expression: every group whose sharing
    /// is legal ([`GroupProperties::shareable_when_equal`]).
    groups_by_expression_hash: HashMap<u64, Vec<MemoGroupId>>,
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
        let query = self.query_roots.len();
        let groups = root.operator_refs().into_iter().copied().collect();
        self.query_roots.push(root);
        self.add_queries_below(groups, &[query]);
        query
    }

    /// Hash-consing: the group whose expression is `expression`
    /// (`same_expression`: operator, arguments and input groups), else a
    /// new group for it, with the `GroupProperties` [`derive_group_properties`]
    /// derives from its input groups, listed as a parent of each input
    /// group. A group whose copies may return different rows (an unordered
    /// `LIMIT`, [`GroupProperties::shareable_when_equal`]) is never returned
    /// for another expression, so such an expression always gets a new
    /// group. Fails, leaving the memo unchanged, when the expression breaks
    /// the node rules (schema derivation, input validation, merge check).
    pub fn find_or_add_group(
        &mut self,
        expression: Operator<MemoGroupId>,
    ) -> Result<MemoGroupId, SchemaDerivationError> {
        let group_properties = derive_group_properties(self, &expression)?;
        let hash = expression_hash(&expression);
        let shareable = group_properties.shareable_when_equal;
        if shareable {
            if let Some(&group) = self
                .groups_by_expression_hash
                .get(&hash)
                .and_then(|groups| {
                    groups.iter().find(|&&group| {
                        same_expression(&self.groups[group.0 as usize].expression, &expression)
                    })
                })
            {
                return Ok(group);
            }
        }
        let id = MemoGroupId(
            u32::try_from(self.groups.len()).expect("a memo has at most u32::MAX groups"),
        );
        for &input in expression.children() {
            add_parent(&mut self.groups[input.0 as usize], id);
        }
        self.groups.push(MemoGroup {
            expression,
            group_properties,
            parents: Vec::new(),
            queries: Vec::new(),
            candidates: Vec::new(),
        });
        if shareable {
            self.groups_by_expression_hash
                .entry(hash)
                .or_default()
                .push(id);
        }
        Ok(id)
    }

    /// Adds `candidate` to `group`'s candidates and returns its id, unless
    /// the group already has a candidate with the same expression
    /// (`same_expression`) and logical properties: then that candidate's
    /// id, and `candidate` is dropped. A new candidate makes `group` a parent
    /// of each of its input groups, and `group`'s queries reach each input
    /// group and every group below it.
    ///
    /// Fails, leaving the memo unchanged, when the candidate's expression
    /// breaks the node rules a group's does ([`derive_group_properties`]),
    /// or when its output schema or kind differs from the group's: then it
    /// does not compute the group's result.
    pub fn add_candidate(
        &mut self,
        group: MemoGroupId,
        candidate: CandidateForMemoGroup,
    ) -> Result<CandidateForMemoGroupId, CandidateError> {
        let output = derive_group_properties(self, &candidate.expression.operator)
            .map_err(CandidateError::Schema)?;
        let group_properties = &self.groups[group.0 as usize].group_properties;
        if output.schema != group_properties.schema
            || output.result_kind != group_properties.result_kind
        {
            return Err(CandidateError::OutputDiffersFromGroup { group });
        }
        let candidates = &mut self.groups[group.0 as usize].candidates;
        if let Some(index_in_group) = candidates
            .iter()
            .position(|known| same_expression_and_logical_properties(known, &candidate))
        {
            return Ok(CandidateForMemoGroupId {
                group,
                index_in_group,
            });
        }
        let input_groups: Vec<MemoGroupId> = candidate
            .expression
            .operator
            .children()
            .into_iter()
            .copied()
            .collect();
        candidates.push(candidate);
        let index_in_group = candidates.len() - 1;
        for &input in &input_groups {
            add_parent(&mut self.groups[input.0 as usize], group);
        }
        let queries = self.groups[group.0 as usize].queries.clone();
        self.add_queries_below(input_groups, &queries);
        Ok(CandidateForMemoGroupId {
            group,
            index_in_group,
        })
    }

    /// Adds `queries` to each of `groups` and every group below them, through
    /// the groups' expressions and their candidates' input groups. A group
    /// that has every query already is not walked: every group below it has
    /// them too.
    fn add_queries_below(&mut self, groups: Vec<MemoGroupId>, queries: &[usize]) {
        let mut to_visit = groups;
        while let Some(group) = to_visit.pop() {
            let group = &mut self.groups[group.0 as usize];
            let mut added = false;
            for &query in queries {
                if let Err(position) = group.queries.binary_search(&query) {
                    group.queries.insert(position, query);
                    added = true;
                }
            }
            if added {
                to_visit.extend(
                    group
                        .expression
                        .children()
                        .into_iter()
                        .chain(
                            group
                                .candidates
                                .iter()
                                .flat_map(|candidate| candidate.expression.operator.children()),
                        )
                        .copied(),
                );
            }
        }
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

/// Lists `parent` among `group`'s parents, once.
fn add_parent(group: &mut MemoGroup, parent: MemoGroupId) {
    if !group.parents.contains(&parent) {
        group.parents.push(parent);
    }
}

/// Whether `left` and `right` are one candidate of a group: the same
/// expression ([`same_expression`]) and logical properties (parameters by
/// [`same_value`], window layout).
fn same_expression_and_logical_properties(
    left: &CandidateForMemoGroup,
    right: &CandidateForMemoGroup,
) -> bool {
    same_expression(&left.expression.operator, &right.expression.operator)
        && same_value(
            &left.logical_properties.parameters,
            &right.logical_properties.parameters,
        )
        && left.logical_properties.window_layout == right.logical_properties.window_layout
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
        let family = family_of_operator(&expression.operator).and_then(sketch_algorithm_of);
        match (family, &parameters) {
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

/// $\boldsymbol\theta \succeq \boldsymbol\theta^{\min}$: `parameters` is
/// the [`strongest`] of itself and `floor`, so every parameter that sets
/// $\varepsilon$ or holds items is at least the floor's (at most, for
/// DDSketch's `alpha`). Parameters of different families never dominate.
fn dominates(parameters: &SketchParams, floor: &SketchParams) -> bool {
    strongest(parameters, floor).as_ref() == Some(parameters)
}

/// The per-parameter maximum of two configurations of one family (the
/// smaller DDSketch `alpha`): what satisfies both. `None` for two
/// families.
pub fn strongest(left: &SketchParams, right: &SketchParams) -> Option<SketchParams> {
    combine(left, right, true)
}

/// The per-parameter minimum of two configurations of one family (the
/// larger DDSketch `alpha`): a floor both satisfy. `None` for two families.
pub fn weakest(left: &SketchParams, right: &SketchParams) -> Option<SketchParams> {
    combine(left, right, false)
}

/// [`strongest`] (`stronger`) or [`weakest`]: the one per-family table of
/// the parameters that set $\varepsilon$ or hold items, each combined. The
/// recorded error and failure probability are `left`'s: every listed
/// configuration of a family records the same failure probability, 0.01.
fn combine(left: &SketchParams, right: &SketchParams, stronger: bool) -> Option<SketchParams> {
    use SketchParams as P;
    /// The larger of `a` and `b` when `larger`, else the smaller.
    fn pick<T: PartialOrd>(larger: bool, a: T, b: T) -> T {
        if (b > a) == larger {
            b
        } else {
            a
        }
    }
    Some(match (left, right) {
        (
            P::Kll {
                k,
                failure_probability,
            },
            P::Kll { k: other, .. },
        ) => P::Kll {
            k: pick(stronger, *k, *other),
            failure_probability: *failure_probability,
        },
        (P::Kmv { k, relative_error }, P::Kmv { k: other, .. }) => P::Kmv {
            k: pick(stronger, *k, *other),
            relative_error: *relative_error,
        },
        // A smaller `alpha` is stronger.
        (P::DDSketch { alpha }, P::DDSketch { alpha: other }) => P::DDSketch {
            alpha: pick(!stronger, *alpha, *other),
        },
        (
            P::Hll {
                precision,
                relative_error,
            },
            P::Hll {
                precision: other, ..
            },
        ) => P::Hll {
            precision: pick(stronger, *precision, *other),
            relative_error: *relative_error,
        },
        (P::Cms { width, depth }, P::Cms { width: w, depth: d }) => P::Cms {
            width: pick(stronger, *width, *w),
            depth: pick(stronger, *depth, *d),
        },
        (P::CountSketch { width, depth }, P::CountSketch { width: w, depth: d }) => {
            P::CountSketch {
                width: pick(stronger, *width, *w),
                depth: pick(stronger, *depth, *d),
            }
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
        ) => P::CmsWithHeap {
            width: pick(stronger, *width, *w),
            depth: pick(stronger, *depth, *d),
            heap_size: pick(stronger, *heap_size, *h),
        },
        (
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
        ) => P::CountSketchWithHeap {
            width: pick(stronger, *width, *w),
            depth: pick(stronger, *depth, *d),
            heap_size: pick(stronger, *heap_size, *h),
        },
        (
            P::UnivMon {
                heap_size,
                sketch_rows,
                sketch_cols,
                layers,
                distinct_failure_probability,
            },
            P::UnivMon {
                heap_size: h,
                sketch_rows: r,
                sketch_cols: c,
                layers: l,
                ..
            },
        ) => P::UnivMon {
            heap_size: pick(stronger, *heap_size, *h),
            sketch_rows: pick(stronger, *sketch_rows, *r),
            sketch_cols: pick(stronger, *sketch_cols, *c),
            layers: pick(stronger, *layers, *l),
            distinct_failure_probability: *distinct_failure_probability,
        },
        (
            P::Hydra(HydraParams::HydraKll { k, shared_buckets }),
            P::Hydra(HydraParams::HydraKll {
                k: other,
                shared_buckets: b,
            }),
        ) => P::Hydra(HydraParams::HydraKll {
            k: pick(stronger, *k, *other),
            shared_buckets: pick(stronger, *shared_buckets, *b),
        }),
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
        ) => P::Hydra(HydraParams::HydraCms {
            width: pick(stronger, *width, *w),
            depth: pick(stronger, *depth, *d),
            shared_rows: pick(stronger, *shared_rows, *r),
            shared_columns: pick(stronger, *shared_columns, *c),
        }),
        (
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
        ) => P::Hydra(HydraParams::HydraCountSketch {
            width: pick(stronger, *width, *w),
            depth: pick(stronger, *depth, *d),
            shared_rows: pick(stronger, *shared_rows, *r),
            shared_columns: pick(stronger, *shared_columns, *c),
        }),
        // Different families. Listed by `left`'s variant, not `_`, so a new
        // variant fails to compile here until it is combined above.
        (P::Kll { .. }, _)
        | (P::Kmv { .. }, _)
        | (P::DDSketch { .. }, _)
        | (P::Hll { .. }, _)
        | (P::Cms { .. }, _)
        | (P::CountSketch { .. }, _)
        | (P::CmsWithHeap { .. }, _)
        | (P::CountSketchWithHeap { .. }, _)
        | (P::UnivMon { .. }, _)
        | (P::Hydra(_), _) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::operator::operator_properties::Reduction;
    use crate::ir::scalar::expr_ir::ColumnRef;
    use crate::ir::schema::{ExactKind, FieldDataType, GroupingStrategy, HydraKind, SummaryUpdate};

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

    /// `LogicalProperties::new` over every operator kind and parameters: a
    /// sketch build (a chain included) needs parameters of its family from
    /// its list; parameters are rejected on an operator without a sketch
    /// family.
    #[test]
    fn logical_properties_new_over_every_operator_and_parameters() {
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
            }),
        };
        let readout = CandidateExpression {
            operator: Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: MemoGroupId(0),
                query: crate::ir::schema::SketchStatistic::Quantile { q: 0.5 },
            }),
        };
        // kll(270) is of KLL's type but not in its list.
        let parameters = [
            None,
            Some(KLL_269),
            Some(CMS),
            Some(HYDRA_CMS),
            Some(kll(270)),
        ];
        let no_parameters = Err(Invalid::ParametersOnExactOperator);
        let needs_parameters = Err(Invalid::SketchBuildWithoutParameters);
        let not_in_list = Err(Invalid::ParametersNotInList);
        let another_family = Err(Invalid::ParametersOfAnotherFamily);
        let exact = [
            Ok(()),
            no_parameters,
            no_parameters,
            no_parameters,
            no_parameters,
        ];
        let kll_build = [
            needs_parameters,
            Ok(()),
            another_family,
            another_family,
            not_in_list,
        ];
        // Per operator: the result for each of `parameters`.
        let table = [
            (
                summary_agg(sketch(SketchAlgorithm::Kll, per_instance.clone())),
                kll_build,
            ),
            // No Hydra θ is listed yet, so no Hydra build is valid.
            (
                summary_agg(sketch(SketchAlgorithm::Cms, hydra)),
                [
                    needs_parameters,
                    another_family,
                    another_family,
                    not_in_list,
                    another_family,
                ],
            ),
            (summary_agg(FieldDataType::exact(ExactKind::Sum)), exact),
            (kll_chain, kll_build),
            (readout, exact),
        ];
        for (expression, by_parameters) in table {
            for (parameters, expected) in parameters.iter().zip(by_parameters) {
                let properties = LogicalProperties::new(&expression, parameters.clone());
                assert_eq!(
                    properties.as_ref().map(|_| ()).map_err(|e| *e),
                    expected,
                    "{:?} {parameters:?}",
                    expression.operator.kind_name()
                );
                if let Ok(properties) = properties {
                    assert_eq!(properties.parameters(), parameters.as_ref());
                }
            }
        }
    }

    /// `family_of_operator` reads the family of a `SummaryAgg` and of a
    /// chain, and of no other operator.
    #[test]
    fn family_of_operator_reads_the_builds_family() {
        let kll = sketch(SketchAlgorithm::Kll, GroupingStrategy::default());
        let chain = Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: MemoGroupId(0),
            family: kll.clone(),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            grouping: GroupingStrategy::default(),
            filter: None,
            tumbling_window_width_ms: 5_000,
        });
        let merge = Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![MemoGroupId(0)],
            group_by: None,
        });
        assert_eq!(
            family_of_operator(&summary_agg(kll.clone()).operator),
            Some(&kll)
        );
        assert_eq!(family_of_operator(&chain), Some(&kll));
        assert_eq!(family_of_operator(&merge), None);
    }

    /// `strongest` and `weakest` take the per-parameter maximum and minimum
    /// of two configurations of one family (DDSketch's smaller `alpha` is the
    /// stronger), keep the left one's recorded fields, and give `None` for
    /// two families.
    #[test]
    fn strongest_and_weakest_combine_one_family_per_parameter() {
        let cms_heap = |width, depth, heap_size| SketchParams::CmsWithHeap {
            width,
            depth,
            heap_size,
        };
        let dd = |alpha| SketchParams::DDSketch { alpha };
        let hll = |precision, relative_error| SketchParams::Hll {
            precision,
            relative_error: Some(relative_error),
        };
        for (left, right, strong, weak) in [
            (kll(52), KLL_269, kll(269), kll(52)),
            (dd(0.05), dd(0.01), dd(0.01), dd(0.05)),
            (hll(13, 0.1), hll(18, 0.01), hll(18, 0.1), hll(13, 0.1)),
            (
                cms_heap(272, 3, 50),
                cms_heap(55, 5, 10),
                cms_heap(272, 5, 50),
                cms_heap(55, 3, 10),
            ),
        ] {
            assert_eq!(strongest(&left, &right), Some(strong), "{left:?} {right:?}");
            assert_eq!(weakest(&left, &right), Some(weak), "{left:?} {right:?}");
        }
        assert_eq!(strongest(&KLL_269, &CMS), None);
        assert_eq!(weakest(&CMS, &HYDRA_CMS), None);
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
            )
            .unwrap()
        };
        let floor = |parameters: &SketchParams| LogicalRequirements::new(Some(parameters.clone()));
        let exact =
            LogicalProperties::new(&summary_agg(FieldDataType::exact(ExactKind::Sum)), None)
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

/// The `Memo` methods: hash-consing groups ([`Memo::find_or_add_group`]) and
/// storing candidates ([`Memo::add_candidate`]), on the concepts doc's KLL
/// example (section 1: G1 the latency scan, G2 its KLL state).
#[cfg(test)]
mod memo_tests {
    use super::*;
    use crate::ir::operator::operator_properties::{GroupKeys, Reduction, Source};
    use crate::ir::scalar::expr_ir::ColumnRef;
    use crate::ir::schema::{DataType, Field, Schema, SketchStatistic, SummaryUpdate};
    use crate::ir::NonASAPOp;

    /// `Scan metric[1m]`: `[ts, value]`.
    fn scan(metric: &str) -> Operator<MemoGroupId> {
        Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries {
                metric: metric.into(),
            },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        })
    }

    /// `SummaryAgg[KLL](input)`: no θ in the expression.
    fn kll_state(input: MemoGroupId) -> Operator<MemoGroupId> {
        Operator::ASAP(ASAPOp::SummaryAgg {
            child: input,
            family: FieldDataType::Sketch(SketchAlgorithm::Kll, GroupingStrategy::default()),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            grouping: GroupingStrategy::default(),
            filter: None,
        })
    }

    /// `SummaryEstimate[q](state)`.
    fn quantile_readout(state: MemoGroupId, q: f64) -> Operator<MemoGroupId> {
        Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: state,
            query: SketchStatistic::Quantile { q },
        })
    }

    /// An unordered `LIMIT 10` over `input`, whose copies may return
    /// different rows.
    fn unordered_limit(input: MemoGroupId) -> Operator<MemoGroupId> {
        Operator::NonASAP(NonASAPOp::Limit {
            n: Some(10),
            offset: 0,
            partition_by: GroupKeys::default(),
            child: input,
        })
    }

    /// `Project(input)` keeping its first column.
    fn project_first_column(input: MemoGroupId) -> Operator<MemoGroupId> {
        Operator::NonASAP(NonASAPOp::Project {
            cols: vec![crate::ir::ProjectItem {
                alias: None,
                expr: crate::ir::ScalarExpr::Column(0),
            }],
            qualifier: None,
            child: input,
        })
    }

    /// A candidate computing `operator` with `parameters` and no window.
    fn candidate(
        operator: Operator<MemoGroupId>,
        parameters: Option<SketchParams>,
    ) -> CandidateForMemoGroup {
        let input_requirements = vec![LogicalRequirements::new(None); operator.children().len()];
        CandidateForMemoGroup {
            // Built directly: the doc example's k = 200 and k = 345 are not
            // in today's KLL list, which `LogicalProperties::new` checks.
            logical_properties: LogicalProperties {
                parameters,
                window_layout: None,
            },
            expression: CandidateExpression { operator },
            input_requirements,
            fused_groups: vec![],
            origin: OriginOfCandidateForMemoGroup::Pass1,
        }
    }

    /// KLL `k` (no recorded failure probability).
    fn kll_k(k: u32) -> Option<SketchParams> {
        Some(SketchParams::Kll {
            k,
            failure_probability: None,
        })
    }

    /// An equal expression over the same input groups is found, not added.
    #[test]
    fn find_or_add_group_returns_the_group_of_an_equal_expression() {
        let mut memo = Memo::default();
        let g1 = memo.find_or_add_group(scan("latency")).unwrap();
        let g2 = memo.find_or_add_group(kll_state(g1)).unwrap();
        assert_eq!(memo.find_or_add_group(scan("latency")).unwrap(), g1);
        assert_eq!(memo.find_or_add_group(kll_state(g1)).unwrap(), g2);
        assert_ne!(memo.find_or_add_group(scan("errors")).unwrap(), g1);
        assert_eq!(memo.groups().len(), 3);
        assert_eq!(
            memo.groups()[g2.0 as usize].expression.children(),
            vec![&g1]
        );
    }

    /// The same operator over different input groups is a different group.
    #[test]
    fn find_or_add_group_separates_equal_operators_over_different_inputs() {
        let mut memo = Memo::default();
        let latency = memo.find_or_add_group(scan("latency")).unwrap();
        let errors = memo.find_or_add_group(scan("errors")).unwrap();
        assert_ne!(
            memo.find_or_add_group(kll_state(latency)).unwrap(),
            memo.find_or_add_group(kll_state(errors)).unwrap()
        );
    }

    /// An expression whose copies may return different rows (an unordered
    /// `LIMIT`) gets a new group every time.
    #[test]
    fn find_or_add_group_never_returns_an_unshareable_group() {
        let mut memo = Memo::default();
        let g1 = memo.find_or_add_group(scan("latency")).unwrap();
        assert_ne!(
            memo.find_or_add_group(unordered_limit(g1)).unwrap(),
            memo.find_or_add_group(unordered_limit(g1)).unwrap()
        );
    }

    /// A new group is a parent of each of its input groups, listed once.
    #[test]
    fn find_or_add_group_lists_the_new_group_among_its_inputs_parents() {
        let mut memo = Memo::default();
        let g1 = memo.find_or_add_group(scan("latency")).unwrap();
        let g2 = memo.find_or_add_group(kll_state(g1)).unwrap();
        memo.find_or_add_group(kll_state(g1)).unwrap();
        assert_eq!(memo.groups()[g1.0 as usize].parent_groups(), &[g2]);
        assert!(memo.groups()[g2.0 as usize].parent_groups().is_empty());
    }

    /// KLL builds with k = 200 and k = 345 are two candidates of one group
    /// G2: θ is a logical property, not part of the expression.
    #[test]
    fn kll_builds_with_k_200_and_k_345_share_one_group() {
        let mut memo = Memo::default();
        let g1 = memo.find_or_add_group(scan("latency")).unwrap();
        let k200_group = memo.find_or_add_group(kll_state(g1)).unwrap();
        let k200 = memo
            .add_candidate(k200_group, candidate(kll_state(g1), kll_k(200)))
            .unwrap();
        let k345_group = memo.find_or_add_group(kll_state(g1)).unwrap();
        let k345 = memo
            .add_candidate(k345_group, candidate(kll_state(g1), kll_k(345)))
            .unwrap();
        assert_eq!(k200_group, k345_group);
        assert_ne!(k200, k345);
        assert_eq!(memo.groups()[k200_group.0 as usize].candidates.len(), 2);
        assert_eq!(
            memo.candidate(k345).logical_properties.parameters(),
            kll_k(345).as_ref()
        );
    }

    /// A candidate with the same expression and logical properties as one
    /// in the group is not added again; its id is returned.
    #[test]
    fn add_candidate_returns_the_existing_candidate_for_a_duplicate() {
        let mut memo = Memo::default();
        let g1 = memo.find_or_add_group(scan("latency")).unwrap();
        let g2 = memo.find_or_add_group(kll_state(g1)).unwrap();
        let first = memo
            .add_candidate(g2, candidate(kll_state(g1), kll_k(200)))
            .unwrap();
        let again = memo
            .add_candidate(g2, candidate(kll_state(g1), kll_k(200)))
            .unwrap();
        assert_eq!(first, again);
        assert_eq!(
            first,
            CandidateForMemoGroupId {
                group: g2,
                index_in_group: 0
            }
        );
        let mut windowed = candidate(kll_state(g1), kll_k(200));
        windowed.logical_properties.window_layout =
            Some(WindowLayout::Tumbling { width_ms: 10_000 });
        assert_ne!(memo.add_candidate(g2, windowed).unwrap(), first);
        assert_eq!(memo.groups()[g2.0 as usize].candidates.len(), 2);
    }

    /// A new candidate makes its group a parent of each group it reads, and
    /// the group's queries reach those groups and every group below them.
    #[test]
    fn add_candidate_maintains_parents_and_queries() {
        // Query 0 is G3 = SummaryEstimate[q=0.95](G2) over G2 = KLL(G1).
        let mut memo = Memo::default();
        let g1 = memo.find_or_add_group(scan("latency")).unwrap();
        let g2 = memo.find_or_add_group(kll_state(g1)).unwrap();
        let g3 = memo.find_or_add_group(quantile_readout(g2, 0.95)).unwrap();
        assert_eq!(memo.add_query(QueryRoot::Operator(g3)), 0);
        // A rule adds a state group G5 over another scan G4: no query
        // reaches them yet.
        let g4 = memo.find_or_add_group(scan("latency_copy")).unwrap();
        let g5 = memo.find_or_add_group(kll_state(g4)).unwrap();
        assert!(memo.groups()[g5.0 as usize].reached_by_queries().is_empty());
        // G3 gains a candidate reading G5: G5 and G4 now serve query 0.
        memo.add_candidate(g3, candidate(quantile_readout(g5, 0.95), None))
            .unwrap();
        assert_eq!(memo.groups()[g5.0 as usize].parent_groups(), &[g3]);
        assert_eq!(memo.groups()[g4.0 as usize].parent_groups(), &[g5]);
        assert_eq!(memo.groups()[g5.0 as usize].reached_by_queries(), &[0]);
        assert_eq!(memo.groups()[g4.0 as usize].reached_by_queries(), &[0]);
        // Query 1 is G6 = Project(G3): it reaches G3 and everything below
        // it, through expressions and candidates, in ascending order.
        let g6 = memo.find_or_add_group(project_first_column(g3)).unwrap();
        assert_eq!(memo.add_query(QueryRoot::Operator(g6)), 1);
        for group in [g1, g2, g3, g4, g5] {
            assert_eq!(
                memo.groups()[group.0 as usize].reached_by_queries(),
                &[0, 1],
                "group {group:?}"
            );
        }
        assert_eq!(memo.groups()[g3.0 as usize].parent_groups(), &[g6]);
    }

    /// A candidate that computes another result than its group's (a
    /// readout offered for a state group) is rejected and changes nothing.
    #[test]
    fn add_candidate_rejects_a_candidate_whose_output_differs_from_its_group() {
        let mut memo = Memo::default();
        let g1 = memo.find_or_add_group(scan("latency")).unwrap();
        let g2 = memo.find_or_add_group(kll_state(g1)).unwrap();
        let before = format!("{memo:?}");
        assert!(matches!(
            memo.add_candidate(g2, candidate(quantile_readout(g2, 0.5), None)),
            Err(CandidateError::OutputDiffersFromGroup { group }) if group == g2
        ));
        assert_eq!(
            format!("{memo:?}"),
            before,
            "a rejected candidate changes nothing"
        );
    }

    /// A candidate whose expression fails input validation (a readout of
    /// raw rows instead of summary state) is rejected and changes nothing.
    #[test]
    fn add_candidate_rejects_a_candidate_whose_inputs_fail_validation() {
        let mut memo = Memo::default();
        let g1 = memo.find_or_add_group(scan("latency")).unwrap();
        let g2 = memo.find_or_add_group(kll_state(g1)).unwrap();
        let g3 = memo.find_or_add_group(quantile_readout(g2, 0.95)).unwrap();
        let over_raw_rows = quantile_readout(g1, 0.95).map_children(|&group_id| {
            MemoGroupAsInput {
                memo: &memo,
                group_id,
            }
        });
        assert!(
            over_raw_rows.output_schema().is_ok(),
            "fixture: the schema derives; only input validation rejects it"
        );
        let before = format!("{memo:?}");
        assert!(matches!(
            memo.add_candidate(g3, candidate(quantile_readout(g1, 0.95), None)),
            Err(CandidateError::Schema(_))
        ));
        assert_eq!(
            format!("{memo:?}"),
            before,
            "a rejected candidate changes nothing"
        );
    }

    /// An expression with no output schema (a group-by column out of range)
    /// is an error, and no group is added.
    #[test]
    fn find_or_add_group_returns_an_error_for_an_expression_with_no_schema() {
        let mut memo = Memo::default();
        let g1 = memo.find_or_add_group(scan("latency")).unwrap();
        let out_of_range = Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![9]),
            measures: vec![crate::ir::operator::agg_intent::AggIntent::Sum { col: Some(1) }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: g1,
        });
        assert!(memo.find_or_add_group(out_of_range).is_err());
        assert_eq!(memo.groups().len(), 1);
        assert!(memo.groups()[g1.0 as usize].parent_groups().is_empty());
    }
}
