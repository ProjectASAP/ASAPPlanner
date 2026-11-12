//! Memo builders the memo's unit tests and the stage and accuracy crates'
//! tests share: each adds groups with the candidates Pass 1 would give
//! them (`Memo::find_or_add_group`, `Memo::add_candidate`) and queries
//! (`Memo::add_query`), so no test calls a stage crate. The expressions
//! come from [`memo_fixtures`](super::memo_fixtures).

use crate::accuracy::AccuracyTarget;
use crate::ir::operator::arguments::Reduction;
use crate::ir::operator::AggIntent;
use crate::ir::schema::series_identity::with_promql_series_identity;
use crate::ir::schema::{DataType, FieldDataType};
use crate::ir::{NonASAPOp, Operator, OperatorNode, ProjectItem, QueryRoot, ScalarExpr};
use crate::stages::{
    listed_build_parameters, CandidateError, CandidateForMemoGroup, CandidateForMemoGroupId, Memo,
    MemoGroupId, MemoQuery, OriginOfCandidateForMemoGroup,
};
use crate::summary_type::{SketchAlgorithm, SketchParams, SketchStatistic};
use crate::workload::{DataWorkload, SourceFacts};

use super::memo_fixtures::{range, readout, scan, state_grouped_by};

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

/// [`try_candidate`], which must succeed.
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

/// The [`group`] of a `TimeSeries(metric)` scan listing Utf8 `labels`
/// whose label matchers are `matchers`, its series identity realized as
/// the planner realizes it before Stage 0 (`with_promql_series_identity`):
/// a closed schema whose last column is `$promql_series_identity`, keyed by
/// it and the timestamp.
pub fn scan_group(
    memo: &mut Memo,
    metric: &str,
    labels: &[&str],
    matchers: Vec<ScalarExpr<MemoGroupId>>,
) -> MemoGroupId {
    let labels: Vec<(&str, DataType)> = labels.iter().map(|l| (*l, DataType::Utf8)).collect();
    let mut operator = scan(metric, &labels, matchers);
    let open =
        OperatorNode::new_shared(operator.map_children(|_| unreachable!("a scan has no input")))
            .unwrap();
    let realized = with_promql_series_identity(&open).unwrap().schema.clone();
    if let Operator::NonASAP(NonASAPOp::Scan { schema, .. }) = &mut operator {
        *schema = realized;
    }
    group(memo, operator)
}

/// Adds the query rooted at group `root` with `target` (`None`: accuracy
/// not enforced), returning one value per evaluation.
pub fn query(memo: &mut Memo, root: MemoGroupId, target: Option<AccuracyTarget>) -> usize {
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(root),
        accuracy_target: target,
        number_of_values_per_query_evaluation: Some(1),
    })
}

/// Candidate 0 of `group`: the exact candidate [`group`] adds first.
pub fn own_candidate(group: MemoGroupId) -> CandidateForMemoGroupId {
    CandidateForMemoGroupId {
        group,
        index_in_group: 0,
    }
}

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

/// The state group of a sketch build of `algorithm` over `child` grouped
/// by `reduction`, one build candidate per listed θ
/// (`listed_build_parameters`).
pub fn sketch_state(
    memo: &mut Memo,
    child: MemoGroupId,
    algorithm: SketchAlgorithm,
    reduction: Reduction,
) -> MemoGroupId {
    let operator = state_grouped_by(FieldDataType::Sketch(algorithm), reduction, child);
    let state = memo.find_or_add_group(operator.clone()).unwrap();
    for parameters in listed_build_parameters(&operator) {
        candidate(memo, state, operator.clone(), parameters);
    }
    state
}

/// The [`group`] of a readout of `statistic` over `state`.
pub fn estimate(memo: &mut Memo, state: MemoGroupId, statistic: SketchStatistic) -> MemoGroupId {
    group(memo, readout(statistic, state))
}

/// `quantile_over_time(q, m[seconds])` per series of `m` by `job` through
/// a sketch state of `algorithm`: the range group, the state group and the
/// readout group.
pub fn quantile_over_time(
    memo: &mut Memo,
    algorithm: SketchAlgorithm,
    q: f64,
    seconds: u64,
) -> (MemoGroupId, MemoGroupId, MemoGroupId) {
    let input = scan_group(memo, "m", &["job"], vec![]);
    let window = group(memo, range(seconds as i64 * 1000, input));
    let state = sketch_state(memo, window, algorithm, Reduction::PerEntity);
    let readout = estimate(memo, state, SketchStatistic::Quantile { q });
    (window, state, readout)
}

/// The readouts Pass 1 offers one quantile group, per summary type.
pub struct QuantileReadouts {
    /// The `Aggregate` group `quantile_over_time(q, window)` per series.
    pub result: MemoGroupId,
    /// One per algorithm.
    pub by_algorithm: Vec<QuantileReadout>,
}

/// One summary type's readout of a quantile group ([`QuantileReadouts`]).
pub struct QuantileReadout {
    pub algorithm: SketchAlgorithm,
    /// The state group, one build per listed θ.
    pub state: MemoGroupId,
    /// The candidate of the quantile group reading the state: the readout
    /// itself, or a renaming `Project` over the readout group.
    pub candidate_of_result: CandidateForMemoGroupId,
    /// The readout (`SummaryEstimate`) candidate.
    pub readout: CandidateForMemoGroupId,
}

/// `quantile_over_time(q, window)` per series as Pass 1 leaves it: the
/// aggregate group with its exact candidate and, per algorithm, a state
/// group (one build per listed θ) and a candidate of the aggregate group
/// reading it (through a renaming `Project` over a readout group when the
/// column names differ, as Pass 1 does).
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
        let direct = readout(SketchStatistic::Quantile { q }, state);
        let (candidate_of_result, readout) = match try_candidate(memo, result, direct, None) {
            Ok(id) => (id, id),
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
                (
                    candidate(memo, result, renamed, None),
                    own_candidate(readout),
                )
            }
            Err(error) => panic!("{error}"),
        };
        by_algorithm.push(QuantileReadout {
            algorithm,
            state,
            candidate_of_result,
            readout,
        });
    }
    QuantileReadouts {
        result,
        by_algorithm,
    }
}

/// The candidates of `group`.
pub fn candidates_of(memo: &Memo, group: MemoGroupId) -> &[CandidateForMemoGroup] {
    &memo.groups()[group.0 as usize].candidates
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
