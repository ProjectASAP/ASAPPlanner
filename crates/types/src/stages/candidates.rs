//! The memo and each group's candidates
//! $v = (e_v, p^{\log}_v)$ and each candidate's input requirements
//! $r^{\log}_{c,i}$ (concepts doc `memo-groups-properties-requirements.md`,
//! section 3; #1203).

use crate::ir::operator::input::reads_only_time_series;
use std::collections::{BTreeMap, HashMap, HashSet};

use super::column_sign::output_column_sign;
use super::hash_consing::{expression_hash, same_expression, same_value};
use super::{GroupProperties, MemoGroup, MemoGroupAsInput, MemoGroupId, ValueSign};
use crate::ir::operator::asap::summary_type_of_operator;
use crate::ir::operator::operator_properties::{AtModifier, SourceColumn, TimeShift};
use crate::ir::properties::summary_coverage::SummaryCoverage;
use crate::ir::properties::{AccuracyMetric, AccuracyTarget};
use crate::ir::schema::{FieldDataType, SketchAlgorithm, SketchParams};
use crate::ir::ASAPOp;
use crate::ir::Operator;
use crate::ir::QueryRoot;
use crate::ir::SchemaDerivationError;
use crate::ir::{NonASAPOp, ScalarExpr};

/// One candidate: position `index_in_group` in group `group`'s candidates
/// ([`MemoGroup::candidates`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
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
    /// The queries, by index in the workload.
    pub queries: Vec<MemoQuery>,
    /// The groups [`Self::find_or_add_group`] may return, by
    /// [`expression_hash`] of their expression: every group whose sharing
    /// is legal ([`GroupProperties::shareable_when_equal`]).
    groups_by_expression_hash: HashMap<u64, Vec<MemoGroupId>>,
    /// The sign the data workload declares for each source column it
    /// declares one for: a `Scan`'s [`GroupProperties::column_sign`].
    declared_column_signs: BTreeMap<SourceColumn, ValueSign>,
}

/// One query of the memo: its root over groups, its accuracy target and
/// $n_q$.
#[derive(Debug, Clone)]
pub struct MemoQuery {
    pub root: QueryRoot<MemoGroupId>,
    /// $A_q$; `None` for a query without one (every query when accuracy is
    /// not enforced).
    pub accuracy_target: Option<AccuracyTarget>,
    /// $n_q$, the number of values one evaluation returns; `None` when the
    /// data workload does not bound it. Pass 2 turns a whole-result failure
    /// probability into a per-value one with it.
    pub values_per_evaluation: Option<u64>,
}

impl Memo {
    /// A memo without groups whose `Scan`s read `declared_column_signs`
    /// (an undeclared source column's sign is `Unknown`).
    pub fn new(declared_column_signs: BTreeMap<SourceColumn, ValueSign>) -> Self {
        Self {
            declared_column_signs,
            ..Self::default()
        }
    }

    /// The groups, by id.
    pub fn groups(&self) -> &[MemoGroup] {
        &self.groups
    }

    /// Adds `query` and returns its index: its root's groups and every
    /// group below them list it ([`MemoGroup::reached_by_queries`]). Stage 0
    /// adds each query once.
    pub fn add_query(&mut self, query: MemoQuery) -> usize {
        let index = self.queries.len();
        let groups = query.root.operator_refs().into_iter().copied().collect();
        self.queries.push(query);
        self.add_queries_below(groups, &[index]);
        index
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
    /// per input group. The only way to change a candidate's $r^{\log}$:
    /// Pass 2 sets a readout's required accuracy after the rules have run.
    /// Panics when the vector does not have one entry per input group.
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
        column_sign: output_column_sign(&operator, &schema, &memo.declared_column_signs),
        depends_on_evaluation_time: depends_on_evaluation_time(&operator),
        schema,
        result_kind: operator.output_kind(),
        aggregate_coverage,
        shareable_when_equal: operator.is_shareable_when_equal(),
        reads_only_time_series: reads_only_time_series(&operator),
    })
}

/// Whether `operator`'s result changes with the evaluation time: a
/// `TimeRange` reads the window before it, and `time()` or `now()` read it
/// directly; `@ T` pins its subtree to T. Any other operator depends on it
/// when an input group does.
fn depends_on_evaluation_time(operator: &Operator<MemoGroupAsInput<'_>>) -> bool {
    fn reads_evaluation_time<C>(expr: &ScalarExpr<C>) -> bool {
        matches!(
            expr,
            ScalarExpr::EvalTimestamp | ScalarExpr::CurrentTimestamp
        ) || expr.children().into_iter().any(reads_evaluation_time)
    }
    match operator {
        Operator::NonASAP(NonASAPOp::TimeRange { .. }) => true,
        Operator::NonASAP(NonASAPOp::TimeShift {
            shift:
                TimeShift {
                    at: Some(AtModifier::Timestamp(_)),
                    ..
                },
            ..
        }) => false,
        Operator::NonASAP(op) => {
            op.scalar_exprs().into_iter().any(reads_evaluation_time)
                || operator.children().into_iter().any(depends_on_it)
        }
        Operator::ASAP(
            ASAPOp::SummaryAgg { filter, .. } | ASAPOp::SummaryTumblingWindowChain { filter, .. },
        ) => {
            filter.iter().any(|pred| reads_evaluation_time(&pred.0))
                || operator.children().into_iter().any(depends_on_it)
        }
        Operator::ASAP(_) => operator.children().into_iter().any(depends_on_it),
    }
}

fn depends_on_it(input: &MemoGroupAsInput<'_>) -> bool {
    input.group_properties().depends_on_evaluation_time
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
/// [`same_value`]).
fn same_expression_and_logical_properties(
    left: &CandidateForMemoGroup,
    right: &CandidateForMemoGroup,
) -> bool {
    same_expression(&left.expression.operator, &right.expression.operator)
        && same_value(
            &left.logical_properties.parameters,
            &right.logical_properties.parameters,
        )
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
        let algorithm = match summary_type_of_operator(&expression.operator) {
            Some(FieldDataType::Sketch(algorithm)) => Some(*algorithm),
            _ => None,
        };
        match (algorithm, &parameters) {
            (Some(_), None) => return Err(LogicalPropertiesError::SketchBuildWithoutParameters),
            (None, Some(_)) => return Err(LogicalPropertiesError::ParametersOnExactOperator),
            (Some(algorithm), Some(parameters)) => {
                if !parameter_list(algorithm).contains(parameters) {
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
    #[error("a sketch build needs parameters from its algorithm's list")]
    SketchBuildWithoutParameters,
    #[error("the parameters are not in their algorithm's predefined list")]
    ParametersNotInList,
}

/// The algorithm's predefined $\boldsymbol\theta$ list: structures only; the
/// error of each, for a statistic at a failure probability, is its guarantee
/// (`asap_accuracy::theoretical_guarantee`).
///
/// The widths, `k`s, precisions, `alpha`s and heap sizes are today's sizing
/// (`asap_accuracy`'s test-only `size_params`) over the errors ε = 0.1,
/// 0.05, 0.02, 0.01, 0.005, 0.002 and 0.001, smallest first, without
/// repeats; an `asap_accuracy` test keeps them equal. Distinct counts (KMV,
/// and UnivMon's distinct-count shapes) stop at ε = 0.01. Where δ is set by
/// a structural parameter, the list covers the δ levels with it: Count-Min
/// lists each width with each depth of [`CMS_DEPTHS`], Count Sketch with
/// each of [`COUNT_SKETCH_DEPTHS`]. A
/// UnivMon's list is its L2-norm shapes, its distinct-count shapes, each L2
/// shape combined with each distinct-count shape, then its shape for every
/// other statistic. A heap variant combines each width × depth with each
/// heap size of [`HEAP_SIZES`]: a top-k needs heap_size ≥ k, and the next
/// listed step up is at most 2.5× too large. Empty for an algorithm without
/// an error model (the Hydra algorithms included), so no build of it is a
/// valid candidate.
pub fn parameter_list(algorithm: SketchAlgorithm) -> &'static [SketchParams] {
    match algorithm {
        SketchAlgorithm::Kll => &KLL_PARAMETERS,
        SketchAlgorithm::DDSketch => &DDSKETCH_PARAMETERS,
        SketchAlgorithm::Hll => &HLL_PARAMETERS,
        SketchAlgorithm::Kmv => &KMV_PARAMETERS,
        SketchAlgorithm::Cms => &CMS_PARAMETERS,
        SketchAlgorithm::CountSketch => &COUNT_SKETCH_PARAMETERS,
        SketchAlgorithm::CmsWithHeap => &CMS_WITH_HEAP_PARAMETERS,
        SketchAlgorithm::CountSketchWithHeap => &COUNT_SKETCH_WITH_HEAP_PARAMETERS,
        SketchAlgorithm::UnivMon => &UNIVMON_PARAMETERS,
        // Listed, not `_`: an algorithm `asap_sketchlib` adds fails to
        // compile here until it gets a list or is listed without one.
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
    SketchParams::Kll { k: 26 },
    SketchParams::Kll { k: 52 },
    SketchParams::Kll { k: 132 },
    SketchParams::Kll { k: 269 },
    SketchParams::Kll { k: 547 },
    SketchParams::Kll { k: 1_404 },
    SketchParams::Kll { k: 2_863 },
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
const HLL_PARAMETERS: [SketchParams; 4] = [
    SketchParams::Hll { precision: 13 },
    SketchParams::Hll { precision: 15 },
    SketchParams::Hll { precision: 17 },
    SketchParams::Hll { precision: 18 },
];

const KMV_PARAMETERS: [SketchParams; 4] = [
    SketchParams::Kmv { k: 1_118 },
    SketchParams::Kmv { k: 4_323 },
    SketchParams::Kmv { k: 26_656 },
    SketchParams::Kmv { k: 106_263 },
];

/// The depths each Count-Min width is listed with: δ = e^−d ≤ 1e-2, 1e-3,
/// 1e-4 and 1e-5.
const CMS_DEPTHS: [u32; 4] = [5, 7, 10, 12];
/// The depths each Count Sketch width is listed with, for the same δ
/// levels: Count Sketch reads the median of its rows, each wrong with
/// probability about 1/3, so δ = P(Binomial(d, 1/3) ≥ (d+1)/2), the
/// smallest odd d at or below each level.
const COUNT_SKETCH_DEPTHS: [u32; 4] = [47, 81, 117, 155];

const CMS_WIDTHS: [u32; 7] = [28, 55, 136, 272, 544, 1_360, 2_719];
const COUNT_SKETCH_WIDTHS: [u32; 7] = [300, 1_200, 7_500, 30_000, 120_000, 750_000, 3_000_000];

const CMS_PARAMETERS: [SketchParams; 28] = with_depths(
    &CMS_WIDTHS,
    &CMS_DEPTHS,
    SketchParams::Cms { width: 0, depth: 0 },
);
const COUNT_SKETCH_PARAMETERS: [SketchParams; 28] = with_depths(
    &COUNT_SKETCH_WIDTHS,
    &COUNT_SKETCH_DEPTHS,
    SketchParams::CountSketch { width: 0, depth: 0 },
);

/// Each of `widths` with each of `depths`, by width, then depth, as
/// `shape`'s variant (`Cms` or `CountSketch`).
const fn with_depths(
    widths: &[u32; 7],
    depths: &[u32; 4],
    shape: SketchParams,
) -> [SketchParams; 28] {
    const UNSET: SketchParams = SketchParams::Cms { width: 0, depth: 0 };
    let mut list = [UNSET; 28];
    let mut width = 0;
    while width < widths.len() {
        let mut depth = 0;
        while depth < depths.len() {
            let (width_value, depth_value) = (widths[width], depths[depth]);
            list[width * depths.len() + depth] = match shape {
                SketchParams::Cms { .. } => SketchParams::Cms {
                    width: width_value,
                    depth: depth_value,
                },
                SketchParams::CountSketch { .. } => SketchParams::CountSketch {
                    width: width_value,
                    depth: depth_value,
                },
                _ => panic!("a Count-Min or Count Sketch shape"),
            };
            depth += 1;
        }
        width += 1;
    }
    list
}

/// The heap sizes of a heap variant's list: the 1-2-5 series from 10 to
/// 10 000, so a top-k with k ≤ 10 000 has a listed heap.
const HEAP_SIZES: [u32; 10] = [10, 20, 50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000];

const CMS_WITH_HEAP_PARAMETERS: [SketchParams; 280] = with_heap_sizes(&CMS_PARAMETERS);
const COUNT_SKETCH_WITH_HEAP_PARAMETERS: [SketchParams; 280] =
    with_heap_sizes(&COUNT_SKETCH_PARAMETERS);

/// Each `Cms` (or `CountSketch`) shape of `shapes` with each heap size of
/// [`HEAP_SIZES`], as `CmsWithHeap` (or `CountSketchWithHeap`).
const fn with_heap_sizes(shapes: &[SketchParams; 28]) -> [SketchParams; 280] {
    const UNSET: SketchParams = SketchParams::Cms { width: 0, depth: 0 };
    let mut list = [UNSET; 280];
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

/// UnivMon's L2-norm column counts and distinct-count heap sizes.
const UNIVMON_L2_COLUMNS: [u32; 7] = [624, 2_367, 14_349, 56_817, 226_130, 1_409_067, 5_630_630];
const UNIVMON_DISTINCT_HEAP_SIZES: [u32; 4] = [13_500, 54_000, 337_500, 1_350_000];

const UNIVMON_PARAMETERS: [SketchParams; 40] = univmon_parameters();

/// The L2-norm shapes, the distinct-count shapes, then each L2 column
/// count × distinct heap size (by column count, then heap size), last the
/// shape for every other statistic.
const fn univmon_parameters() -> [SketchParams; 40] {
    const fn univmon(
        heap_size: u32,
        sketch_rows: u32,
        sketch_cols: u32,
        layers: u8,
    ) -> SketchParams {
        SketchParams::UnivMon {
            heap_size,
            sketch_rows,
            sketch_cols,
            layers,
        }
    }
    const BASELINE: SketchParams = univmon(256, 5, 1_024, 16);
    let mut list = [BASELINE; 40];
    let mut next = 0;
    let mut columns = 0;
    while columns < UNIVMON_L2_COLUMNS.len() {
        list[next] = univmon(1, 5, UNIVMON_L2_COLUMNS[columns], 1);
        next += 1;
        columns += 1;
    }
    let mut heap = 0;
    while heap < UNIVMON_DISTINCT_HEAP_SIZES.len() {
        list[next] = univmon(UNIVMON_DISTINCT_HEAP_SIZES[heap], 1, 1, 19);
        next += 1;
        heap += 1;
    }
    columns = 0;
    while columns < UNIVMON_L2_COLUMNS.len() {
        heap = 0;
        while heap < UNIVMON_DISTINCT_HEAP_SIZES.len() {
            list[next] = univmon(
                UNIVMON_DISTINCT_HEAP_SIZES[heap],
                5,
                UNIVMON_L2_COLUMNS[columns],
                19,
            );
            next += 1;
            heap += 1;
        }
        columns += 1;
    }
    list
}

/// $r^{\log}_{c,i}$: only what $G_{c,i}$'s identity does not fix. Only a
/// readout's input (`SummaryEstimate`, `FinalizeExactAccumulator`,
/// `EvaluatePopulation` over an approximate state) carries one (concepts
/// doc, section 2, "Logical requirements").
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogicalRequirements {
    /// `None` on every input that is not a readout's, and on a readout's
    /// input when no accuracy target reaches it.
    pub required_accuracy: Option<RequiredAccuracy>,
}

/// The accuracy a readout's input needs from its producer: the producer's
/// guarantee for the readout's statistic, evaluated at
/// `failure_probability`, is at most `max_error` in `metric`. It is also
/// the readout's own output error.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct RequiredAccuracy {
    pub metric: AccuracyMetric,
    /// $\varepsilon_r$, in the metric's units.
    pub max_error: f64,
    /// $\delta_r$, per value; 1 when the target sets none.
    pub failure_probability: f64,
}

impl LogicalRequirements {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::operator::operator_properties::Reduction;
    use crate::ir::scalar::expr_ir::ColumnRef;
    use crate::ir::schema::{ExactKind, FieldDataType, HydraParams, SummaryUpdate};

    /// `SummaryAgg` over group 0 building a state of `summary_type`.
    fn summary_agg(summary_type: FieldDataType) -> CandidateExpression {
        CandidateExpression {
            operator: Operator::ASAP(ASAPOp::SummaryAgg {
                child: MemoGroupId(0),
                summary_type,
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
                filter: None,
            }),
        }
    }

    const KLL_269: SketchParams = SketchParams::Kll { k: 269 };
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
    /// sketch build (a chain included) needs parameters of its algorithm from
    /// its list; parameters are rejected on an operator that builds no
    /// sketch.
    #[test]
    fn logical_properties_new_over_every_operator_and_parameters() {
        use LogicalPropertiesError as Invalid;
        let kll = FieldDataType::Sketch(SketchAlgorithm::Kll);
        let kll_chain = CandidateExpression {
            operator: Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                child: MemoGroupId(0),
                summary_type: kll.clone(),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
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
        // k = 270 is KLL's structure but not in its list.
        let parameters = [
            None,
            Some(KLL_269),
            Some(CMS),
            Some(HYDRA_CMS),
            Some(SketchParams::Kll { k: 270 }),
        ];
        let no_parameters = Err(Invalid::ParametersOnExactOperator);
        let needs_parameters = Err(Invalid::SketchBuildWithoutParameters);
        let not_in_list = Err(Invalid::ParametersNotInList);
        let exact = [
            Ok(()),
            no_parameters,
            no_parameters,
            no_parameters,
            no_parameters,
        ];
        // Another algorithm's parameters are not in KLL's list either.
        let kll_build = [
            needs_parameters,
            Ok(()),
            not_in_list,
            not_in_list,
            not_in_list,
        ];
        // Per operator: the result for each of `parameters`.
        let table = [
            (summary_agg(kll), kll_build),
            // No Hydra θ is listed yet, so no Hydra build is valid.
            (
                summary_agg(FieldDataType::Sketch(SketchAlgorithm::HydraCms)),
                [
                    needs_parameters,
                    not_in_list,
                    not_in_list,
                    not_in_list,
                    not_in_list,
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

    /// `summary_type_of_operator` reads the summary type of a `SummaryAgg`
    /// and of a chain, and of no other operator.
    #[test]
    fn summary_type_of_operator_reads_the_builds_summary_type() {
        let kll = FieldDataType::Sketch(SketchAlgorithm::Kll);
        let chain = Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: MemoGroupId(0),
            summary_type: kll.clone(),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
            tumbling_window_width_ms: 5_000,
        });
        let merge = Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![MemoGroupId(0)],
            group_by: None,
        });
        assert_eq!(
            summary_type_of_operator(&summary_agg(kll.clone()).operator),
            Some(&kll)
        );
        assert_eq!(summary_type_of_operator(&chain), Some(&kll));
        assert_eq!(summary_type_of_operator(&merge), None);
    }

    /// Each predefined list holds one parameter type, without repeats, and
    /// is empty exactly for the algorithms without an error model. Count-Min
    /// lists each width with depths 5, 7, 10 and 12, Count Sketch with 47,
    /// 81, 117 and 155, and a
    /// heap variant each of those with every heap size; UnivMon's list has 40
    /// entries.
    #[test]
    fn parameter_list_holds_its_algorithm_without_repeats() {
        let listed = [
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
        for algorithm in listed.iter().copied().chain([
            SketchAlgorithm::HydraCms,
            SketchAlgorithm::HydraCountSketch,
            SketchAlgorithm::HydraKll,
            SketchAlgorithm::HllHip,
        ]) {
            let list = parameter_list(algorithm);
            assert_eq!(
                list.is_empty(),
                !listed.contains(&algorithm),
                "{algorithm:?}"
            );
            for (i, parameters) in list.iter().enumerate() {
                assert_eq!(
                    std::mem::discriminant(parameters),
                    std::mem::discriminant(&list[0])
                );
                assert!(!list[..i].contains(parameters), "{parameters:?} repeats");
            }
        }
        let depths = |algorithm| -> Vec<u32> {
            parameter_list(algorithm)
                .iter()
                .take(4)
                .map(|parameters| match parameters {
                    SketchParams::Cms { depth, .. }
                    | SketchParams::CountSketch { depth, .. }
                    | SketchParams::CmsWithHeap { depth, .. }
                    | SketchParams::CountSketchWithHeap { depth, .. } => *depth,
                    other => panic!("{other:?}"),
                })
                .collect()
        };
        assert_eq!(depths(SketchAlgorithm::Cms), [5, 7, 10, 12]);
        assert_eq!(depths(SketchAlgorithm::CountSketch), [47, 81, 117, 155]);
        assert_eq!(depths(SketchAlgorithm::CmsWithHeap), [5, 5, 5, 5]);
        assert_eq!(parameter_list(SketchAlgorithm::Cms).len(), 28);
        assert_eq!(parameter_list(SketchAlgorithm::CmsWithHeap).len(), 280);
        assert_eq!(parameter_list(SketchAlgorithm::UnivMon).len(), 40);
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
    use crate::ir::schema::{
        DataType, Field, FieldDataType, Schema, SketchStatistic, SummaryUpdate,
    };
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
            summary_type: FieldDataType::Sketch(SketchAlgorithm::Kll),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
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

    /// A candidate computing `operator` with `parameters`.
    fn candidate(
        operator: Operator<MemoGroupId>,
        parameters: Option<SketchParams>,
    ) -> CandidateForMemoGroup {
        let input_requirements = vec![LogicalRequirements::default(); operator.children().len()];
        CandidateForMemoGroup {
            // Built directly: the doc example's k = 200 and k = 345 are not
            // in today's KLL list, which `LogicalProperties::new` checks.
            logical_properties: LogicalProperties { parameters },
            expression: CandidateExpression { operator },
            input_requirements,
            fused_groups: vec![],
            origin: OriginOfCandidateForMemoGroup::Pass1,
        }
    }

    /// A query rooted at `group`, without an accuracy target or $n_q$.
    fn query(group: MemoGroupId) -> MemoQuery {
        MemoQuery {
            root: QueryRoot::Operator(group),
            accuracy_target: None,
            values_per_evaluation: None,
        }
    }

    /// KLL `k`.
    fn kll_k(k: u32) -> Option<SketchParams> {
        Some(SketchParams::Kll { k })
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
        let other_size = candidate(kll_state(g1), kll_k(345));
        assert_ne!(memo.add_candidate(g2, other_size).unwrap(), first);
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
        assert_eq!(memo.add_query(query(g3)), 0);
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
        assert_eq!(memo.add_query(query(g6)), 1);
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
        let over_raw_rows = quantile_readout(g1, 0.95).map_children(|&group_id| MemoGroupAsInput {
            memo: &memo,
            group_id,
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
