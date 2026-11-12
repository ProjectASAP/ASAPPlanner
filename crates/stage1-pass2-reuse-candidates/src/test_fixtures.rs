//! Fixtures this crate's own tests share. Each builds its input memo
//! through `Memo::find_or_add_group`, `Memo::add_candidate` and
//! `Memo::add_query`, with the candidates Pass 1 would add, so no test
//! calls another stage crate. The expressions come from
//! `asap_types::test_support::memo_fixtures`.
use std::collections::BTreeSet;

use asap_accuracy::NoMeasuredGuarantees;
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::operator::arguments::{AtModifier, BinaryOpKind, Reduction};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{CompareOpKind, ScalarValue};
use asap_types::ir::schema::series_identity::with_promql_series_identity;
use asap_types::ir::schema::{DataType, FieldDataType};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, OperatorNode, Predicate,
    ProjectItem, QueryRoot, ScalarExpr,
};
use asap_types::stages::{
    CandidateError, CandidateForMemoGroup, CandidateForMemoGroupId, LogicalASAPMemoCandidates,
    Memo, MemoGroupId, MemoQuery, OriginOfCandidateForMemoGroup, PrunedLogicalCandidate,
    RequiredAccuracy,
};
use asap_types::summary_type::parameter_list;
use asap_types::summary_type::{ExactKind, SketchAlgorithm, SketchParams, SketchStatistic};
use asap_types::workload::{
    DataWorkload, Predictability, Query, QueryRecurrence, QueryRequirements, QueryWorkloadEntry,
    RepeatedDemand, RepetitionInterval, SourceFacts, TimestampMs,
};

use asap_types::test_support::memo_fixtures;

use crate::generate_logical_asap_memo_candidates;

/// A data workload declaring only that `metric` holds `series_count`
/// series.
pub fn with_series(metric: &str, series_count: u64) -> DataWorkload {
    DataWorkload {
        sources: [(
            metric.to_string(),
            SourceFacts {
                series_count: Some(series_count),
                ..SourceFacts::default()
            },
        )]
        .into(),
        ..DataWorkload::default()
    }
}

/// Adds `operator` with `parameters` to `group`, as Pass 1 does.
pub fn try_candidate(
    memo: &mut Memo,
    group: MemoGroupId,
    operator: Operator<MemoGroupId>,
    parameters: Option<SketchParams>,
) -> Result<CandidateForMemoGroupId, CandidateError> {
    memo.add_candidate(
        group,
        CandidateForMemoGroup::new(
            operator,
            parameters,
            OriginOfCandidateForMemoGroup::Pass1,
            Vec::new(),
        ),
    )
}

pub fn candidate(
    memo: &mut Memo,
    group: MemoGroupId,
    operator: Operator<MemoGroupId>,
    parameters: Option<SketchParams>,
) -> CandidateForMemoGroupId {
    try_candidate(memo, group, operator, parameters).unwrap()
}

/// The group of `operator`, with `operator` as its exact candidate.
pub fn group(memo: &mut Memo, operator: Operator<MemoGroupId>) -> MemoGroupId {
    let group = memo.find_or_add_group(operator.clone()).unwrap();
    candidate(memo, group, operator, None);
    group
}

/// A time-series scan of `metric` listing `labels`.
pub fn scan(memo: &mut Memo, metric: &str, labels: &[&str]) -> MemoGroupId {
    scan_where(memo, metric, labels, vec![])
}

/// `column = value`, PromQL semantics.
pub fn equals(column: usize, value: &str) -> ScalarExpr<MemoGroupId> {
    ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(column)),
        op: CompareOpKind::Eq,
        right: Box::new(ScalarExpr::Literal(ScalarValue::Utf8(value.into()))),
        semantics: ExprSemantics::Promql,
    }
}

/// A scan of `metric` listing `labels` whose label matchers are
/// `matchers`, its series identity realized as the planner realizes it
/// before Stage 0 (`with_promql_series_identity`).
pub fn scan_where(
    memo: &mut Memo,
    metric: &str,
    labels: &[&str],
    matchers: Vec<Predicate<MemoGroupId>>,
) -> MemoGroupId {
    let labels: Vec<(&str, DataType)> = labels.iter().map(|l| (*l, DataType::Utf8)).collect();
    let matchers = matchers.into_iter().map(|matcher| matcher.0).collect();
    let mut operator = memo_fixtures::scan(metric, &labels, matchers);
    let open =
        OperatorNode::new_shared(operator.map_children(|_| unreachable!("a scan has no input")))
            .unwrap();
    let realized = with_promql_series_identity(&open).unwrap().schema.clone();
    if let Operator::NonASAP(NonASAPOp::Scan { schema, .. }) = &mut operator {
        *schema = realized;
    }
    group(memo, operator)
}

/// `child[seconds]`.
pub fn range(memo: &mut Memo, child: MemoGroupId, seconds: u64) -> MemoGroupId {
    group(memo, memo_fixtures::range(seconds as i64 * 1000, child))
}

/// `child offset <seconds>`, or `child @ at`.
pub fn shift(
    memo: &mut Memo,
    child: MemoGroupId,
    seconds: i64,
    at: Option<AtModifier>,
) -> MemoGroupId {
    group(memo, memo_fixtures::shift(seconds * 1000, at, child))
}

/// The state group of a sketch build, one candidate per listed θ.
pub fn sketch_state(
    memo: &mut Memo,
    child: MemoGroupId,
    algorithm: SketchAlgorithm,
    reduction: Reduction,
) -> MemoGroupId {
    let operator =
        memo_fixtures::state_grouped_by(FieldDataType::Sketch(algorithm), reduction, child);
    let state = memo.find_or_add_group(operator.clone()).unwrap();
    for parameters in parameter_list(algorithm) {
        candidate(memo, state, operator.clone(), Some(parameters.clone()));
    }
    state
}

/// An exact accumulator state of `kind` over `window` grouped by
/// `reduction`, read by a `FinalizeExactAccumulator` group (the query
/// root): what Pass 1 offers for `sum_over_time` (per series) or for a
/// `sum by (keys)` of `count_over_time` (the count by keys).
pub fn exact_state(
    memo: &mut Memo,
    window: MemoGroupId,
    kind: ExactKind,
    reduction: Reduction,
) -> (MemoGroupId, MemoGroupId) {
    let state = group(
        memo,
        memo_fixtures::state_grouped_by(FieldDataType::exact(kind), reduction, window),
    );
    let finalized = group(
        memo,
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: state }),
    );
    (state, finalized)
}

/// A readout group of `statistic` over `state`.
pub fn estimate(memo: &mut Memo, state: MemoGroupId, statistic: SketchStatistic) -> MemoGroupId {
    group(memo, memo_fixtures::readout(statistic, state))
}

pub fn arithmetic(
    memo: &mut Memo,
    kind: ArithmeticOpKind,
    lhs: MemoGroupId,
    rhs: MemoGroupId,
) -> MemoGroupId {
    group(
        memo,
        Operator::NonASAP(NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                checked_relative_division: false,
                checked_finite_division: false,
                kind: BinaryOpKind::Arithmetic(kind),
                vector_match: None,
            },
            return_bool: false,
            lhs,
            rhs,
        }),
    )
}

/// Query `root` with `target` (`None`: accuracy not enforced).
pub fn query(memo: &mut Memo, root: MemoGroupId, target: Option<AccuracyTarget>) -> usize {
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(root),
        accuracy_target: target,
        number_of_values_per_query_evaluation: Some(1),
    })
}

/// A query repeating every `interval_ms` at phase 0 (`FixedIntervalAt`).
pub fn entry(interval_ms: u32) -> QueryWorkloadEntry {
    QueryWorkloadEntry {
        query: Query("q".into()),
        requirements: QueryRequirements::default(),
        predictability: Predictability::Predictable { known_at: None },
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
            interval: RepetitionInterval(interval_ms),
            evaluation_phase: TimestampMs(0),
        }),
        range_evaluation: None,
    }
}

pub fn pass2(memo: Memo, entries: &[QueryWorkloadEntry]) -> LogicalASAPMemoCandidates {
    generate_logical_asap_memo_candidates(memo, entries, &NoMeasuredGuarantees).unwrap()
}

pub fn candidates_of(memo: &Memo, group: MemoGroupId) -> &[CandidateForMemoGroup] {
    &memo.groups()[group.0 as usize].candidates
}

/// The edges into input `input` of `consumer`: each producer and α^log.
pub fn edges_into(
    logical: &LogicalASAPMemoCandidates,
    consumer: CandidateForMemoGroupId,
    input: usize,
) -> Vec<(CandidateForMemoGroupId, bool)> {
    logical
        .edges
        .iter()
        .filter(|edge| {
            edge.consumer_candidate_id == consumer && edge.input_index_of_consumer == input
        })
        .map(|edge| {
            (
                edge.producer_candidate_id,
                edge.satisfies_logical_requirement,
            )
        })
        .collect()
}

pub fn own_candidate(group: MemoGroupId) -> CandidateForMemoGroupId {
    CandidateForMemoGroupId {
        group,
        index_in_group: 0,
    }
}

/// The pruned entry of `candidate`, if Pass 2 eliminated it.
pub fn pruned_entry(
    logical: &LogicalASAPMemoCandidates,
    candidate: CandidateForMemoGroupId,
) -> Option<&PrunedLogicalCandidate> {
    logical
        .pruned
        .iter()
        .find(|pruned| pruned.candidate == candidate)
}

/// The required accuracy of each readout candidate of `group` (its only
/// input's).
pub fn requirements_of(memo: &Memo, group: MemoGroupId) -> Vec<Option<RequiredAccuracy>> {
    candidates_of(memo, group)
        .iter()
        .filter(|c| {
            matches!(
                c.expression.operator,
                Operator::ASAP(ASAPOp::SummaryEstimate { .. })
            )
        })
        .map(|c| c.input_requirements[0].required_accuracy)
        .collect()
}

/// `quantile_over_time(q, m[seconds])` per series through a sketch state
/// of `algorithm`: the range group, the state group and the readout group.
pub fn quantile_over_time(
    memo: &mut Memo,
    algorithm: SketchAlgorithm,
    q: f64,
    seconds: u64,
) -> (MemoGroupId, MemoGroupId, MemoGroupId) {
    let input = scan(memo, "m", &["job"]);
    let window = range(memo, input, seconds);
    let state = sketch_state(memo, window, algorithm, Reduction::PerEntity);
    let readout = estimate(memo, state, SketchStatistic::Quantile { q });
    (window, state, readout)
}

/// The tumbling-window widths of the chains whose merges state group
/// `state` has as candidates.
pub fn chain_widths_serving(memo: &Memo, state: MemoGroupId) -> BTreeSet<u64> {
    candidates_of(memo, state)
        .iter()
        .filter_map(|c| match &c.expression.operator {
            Operator::ASAP(ASAPOp::SummaryMerge {
                children,
                group_by: None,
            }) => chain_width_below(memo, children[0]),
            _ => None,
        })
        .collect()
}

/// The width of the tumbling-window chain `group` reads through
/// `TimeRange` and `TimeShift` groups, if any.
pub fn chain_width_below(memo: &Memo, mut group: MemoGroupId) -> Option<u64> {
    loop {
        match &memo.groups()[group.0 as usize].expression {
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => return Some(*tumbling_window_width_ms),
            Operator::NonASAP(
                NonASAPOp::TimeRange { child, .. } | NonASAPOp::TimeShift { child, .. },
            ) => group = *child,
            _ => return None,
        }
    }
}

/// The groups whose expression matches `pattern`.
pub fn groups_where(
    memo: &Memo,
    pattern: impl Fn(&Operator<MemoGroupId>) -> bool,
) -> Vec<MemoGroupId> {
    memo.groups()
        .iter()
        .enumerate()
        .filter(|(_, group)| pattern(&group.expression))
        .map(|(index, _)| MemoGroupId(index as u32))
        .collect()
}

/// The group whose expression is `operator`, if the memo has one.
pub fn group_of(memo: &Memo, operator: &Operator<MemoGroupId>) -> Option<MemoGroupId> {
    groups_where(memo, |expression| expression == operator)
        .first()
        .copied()
}

/// The readouts Pass 1 offers one quantile group, per summary type.
pub struct QuantileReadouts {
    /// The `Aggregate` group `quantile_over_time(q, window)` per series.
    pub result: MemoGroupId,
    /// Per algorithm: its state group and the readout candidate of `result`.
    pub by_algorithm: Vec<(SketchAlgorithm, MemoGroupId, CandidateForMemoGroupId)>,
}

/// `quantile_over_time(q, window)` per series as Pass 1 leaves it: the
/// aggregate group with its exact candidate and, per algorithm, a state
/// group (one build per listed θ) and a candidate of the aggregate group
/// reading it (through a renaming `Project` over a readout group when the
/// column names differ, as Pass 1's `add_output_candidate` does).
pub fn quantile_over_time_readouts(
    memo: &mut Memo,
    window: MemoGroupId,
    q: f64,
    algorithms: &[SketchAlgorithm],
) -> QuantileReadouts {
    let result = group(
        memo,
        Operator::NonASAP(NonASAPOp::Aggregate {
            child: window,
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Quantile { col: None, q }],
            output_names: vec![String::new()],
            filters: vec![None],
            having: None,
        }),
    );
    let mut by_algorithm = Vec::new();
    for &algorithm in algorithms {
        let state = sketch_state(memo, window, algorithm, Reduction::PerEntity);
        let direct = Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: state,
            query: SketchStatistic::Quantile { q },
        });
        let id = match try_candidate(memo, result, direct, None) {
            Ok(id) => id,
            Err(CandidateError::OutputDiffersFromGroup { .. }) => {
                let readout = estimate(memo, state, SketchStatistic::Quantile { q });
                let to = &memo.groups()[result.0 as usize].group_properties.schema;
                let renamed = Operator::NonASAP(NonASAPOp::Project {
                    cols: to
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(column, field)| ProjectItem {
                            alias: Some(field.name.clone()),
                            expr: ScalarExpr::Column(column),
                        })
                        .collect(),
                    child: readout,
                });
                candidate(memo, result, renamed, None)
            }
            Err(error) => panic!("{error}"),
        };
        by_algorithm.push((algorithm, state, id));
    }
    QuantileReadouts {
        result,
        by_algorithm,
    }
}

/// The readout of `state` a candidate `id` of a quantile group is or reads.
pub fn readout_of(memo: &Memo, id: CandidateForMemoGroupId) -> CandidateForMemoGroupId {
    match &memo.candidate(id).expression.operator {
        Operator::NonASAP(NonASAPOp::Project { child, .. }) => own_candidate(*child),
        _ => id,
    }
}
