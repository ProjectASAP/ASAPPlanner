//! The selection MILP over small hand-built candidate graphs with given
//! costs: which executions and edges its optimum selects, its retention,
//! and the plan built from it.

use std::time::Duration;

use asap_types::ir::operator::operator_properties::Reduction;
use asap_types::ir::operator::TimeShift;
use asap_types::ir::properties::{AccuracyScope, AccuracyTarget};
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::expr_ir::ColumnRef;
use asap_types::ir::schema::{ExactKind, ExactParams, FieldDataType, SummaryUpdate};
use asap_types::ir::schema::{SketchAlgorithm, SketchParams, SketchStatistic};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, QueryRoot, TimeRangeKind};
use asap_types::ir::{BinaryOperator, ScalarExpr};
use asap_types::physical::{ExecutionTiming, Lifetime, PhysicalProperties, StorageMedium};
use asap_types::stages::{
    output_references, CandidateForMemoGroupId, LogicalASAPMemoCandidates, Memo, MemoGroupId,
    MemoQuery, PhysicalASAPMemoCandidates, PhysicalCandidate, PhysicalCandidateId, PhysicalEdge,
    PhysicalEdgeId,
};
use asap_types::test_support::{
    add_candidate, group_with_own_candidate, series_schema, time_series_scan,
};
use asap_types::workload::{
    Predictability, Query, QueryRecurrence, QueryRequirements, RepeatedDemand, RepetitionInterval,
    TimestampMs,
};

use super::*;
use crate::cost::ExecutionCosts;
use crate::plan::plan_of;

const NONE: Lifetime = Lifetime::None;
const ONE_EVALUATION: Lifetime = Lifetime::OneEvaluation(StorageMedium::Memory);
const ACROSS_EVALUATIONS: Lifetime = Lifetime::AcrossEvaluations(StorageMedium::Memory);

fn scan(memo: &mut Memo) -> MemoGroupId {
    group_with_own_candidate(memo, time_series_scan("m", series_schema(&["job"]), vec![]))
}

fn range(memo: &mut Memo, seconds: u64, child: MemoGroupId) -> MemoGroupId {
    group_with_own_candidate(
        memo,
        Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(seconds),
            kind: TimeRangeKind::Range,
            child,
        }),
    )
}

fn shift(offset_ms: i64, child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::TimeShift {
        shift: TimeShift {
            offset_ms,
            at: None,
        },
        child,
    })
}

/// Stage 2's output over `memo`, with one query-time candidate per
/// (logical candidate, lifetime) of `candidates` and every edge between
/// a candidate of an input group and its consumer admissible.
fn physical(
    memo: Memo,
    candidates: &[(CandidateForMemoGroupId, Lifetime)],
) -> PhysicalASAPMemoCandidates {
    let physical_candidates: Vec<PhysicalCandidate> = candidates
        .iter()
        .map(|&(logical_candidate_id, lifetime)| PhysicalCandidate {
            logical_candidate_id,
            physical_properties: PhysicalProperties::new(ExecutionTiming::QueryTime, lifetime)
                .unwrap(),
        })
        .collect();
    let mut edges = Vec::new();
    for (c, consumer) in physical_candidates.iter().enumerate() {
        let inputs = memo
            .candidate(consumer.logical_candidate_id)
            .expression
            .operator
            .children()
            .into_iter()
            .copied()
            .collect::<Vec<_>>();
        for (i, group) in inputs.into_iter().enumerate() {
            for (u, producer) in physical_candidates.iter().enumerate() {
                if producer.logical_candidate_id.group == group {
                    edges.push(PhysicalEdge {
                        producer_candidate_id: PhysicalCandidateId(u),
                        consumer_candidate_id: PhysicalCandidateId(c),
                        input_index_of_consumer: i,
                        satisfies_logical_requirement: true,
                        satisfies_physical_requirement: true,
                    });
                }
            }
        }
    }
    let groups = memo.groups().len();
    PhysicalASAPMemoCandidates::new(
        LogicalASAPMemoCandidates {
            memo,
            edges: vec![],
            pruned: vec![],
            evaluation_grid_of_group: vec![None; groups],
        },
        physical_candidates,
        edges,
        vec![],
    )
}

fn query(memo: &mut Memo, group: MemoGroupId) {
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(group),
        accuracy_target: None,
        values_per_evaluation: None,
    });
}

/// The id of the edge from `u` to `c`.
fn edge(physical: &PhysicalASAPMemoCandidates, u: usize, c: usize) -> PhysicalEdgeId {
    physical
        .edges()
        .find(|(_, edge)| edge.producer_candidate_id.0 == u && edge.consumer_candidate_id.0 == c)
        .unwrap()
        .0
}

/// Every query on a 10 s grid, `count` of them.
fn every_10s(count: usize) -> Vec<QueryWorkloadEntry> {
    (0..count).map(|_| every(10_000)).collect()
}

/// A query every `interval_ms` at phase 0.
fn every(interval_ms: u32) -> QueryWorkloadEntry {
    QueryWorkloadEntry {
        query: Query("q".into()),
        requirements: QueryRequirements::default(),
        predictability: Predictability::default(),
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
            interval: RepetitionInterval(interval_ms),
            evaluation_phase: TimestampMs(0),
        }),
        range_evaluation: None,
    }
}

/// Each query output reference of `physical`'s memo: a query's operator
/// root, or each operator a scalar root reads.
fn outputs(physical: &PhysicalASAPMemoCandidates) -> Vec<(QueryId, MemoGroupId)> {
    let roots: Vec<QueryRoot<MemoGroupId>> = physical
        .logical_candidates
        .memo
        .queries
        .iter()
        .map(|query| query.root.clone())
        .collect();
    output_references(&roots)
        .into_iter()
        .map(|(query, group)| (QueryId(query), group))
        .collect()
}

/// Solves over `physical` for `entries` with every execution of candidate
/// `v` costing `w[v]`, every edge from `u` to `c` `edge_costs[(u, c)]` (0
/// when missing) and candidate `v` retaining at `retention[v]` per ms;
/// returns the solution and the contexts it is over.
fn solve(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    w: &[f64],
    edge_costs: &[((usize, usize), f64)],
    retention: &[(usize, f64)],
) -> (
    Result<SolvedSelection, SelectionMilpError>,
    ExecutionContexts,
) {
    let (contexts, query_outputs) = ExecutionContexts::new(physical, entries, &outputs(physical));
    let costs = ExecutionCosts {
        per_execution: contexts
            .executions()
            .iter()
            .map(|execution| Ok(w[execution.candidate.0]))
            .collect(),
        per_edge: contexts
            .edges()
            .iter()
            .map(|&(_, id, _)| {
                let edge = physical.edge(id);
                let key = (edge.producer_candidate_id.0, edge.consumer_candidate_id.0);
                Some(
                    edge_costs
                        .iter()
                        .find(|(pair, _)| *pair == key)
                        .map_or(0.0, |(_, cost)| *cost),
                )
            })
            .collect(),
        retention_cost_per_second: retention
            .iter()
            .map(|&(v, per_ms)| (PhysicalCandidateId(v), per_ms))
            .collect(),
    };
    let solved = solve_selection(&SelectionInputs {
        physical,
        entries,
        contexts: &contexts,
        costs: &costs,
        query_outputs: &query_outputs,
    });
    (solved, contexts)
}

/// The candidates of `solved`'s executions, one per execution, ascending.
fn candidates(solved: &SolvedSelection) -> Vec<usize> {
    let mut candidates: Vec<usize> = solved
        .executions
        .iter()
        .map(|execution| execution.candidate.0)
        .collect();
    candidates.sort_unstable();
    candidates
}

fn first(group: MemoGroupId) -> CandidateForMemoGroupId {
    CandidateForMemoGroupId {
        group,
        index_in_group: 0,
    }
}

/// `m[5m]`, its scan with two physical candidates. Through v0 the read
/// costs 1 + 5, through v1 2 + 0: the optimum takes v1, and the plan built
/// from it is the scan v1 below the range v2, the scan retaining the 5
/// minutes the range reads (R).
#[test]
fn selects_the_producer_whose_own_and_edge_costs_are_lowest() {
    let mut memo = Memo::default();
    let m = scan(&mut memo);
    let last_5m = range(&mut memo, 300, m);
    query(&mut memo, last_5m);
    let physical = physical(
        memo,
        &[
            (first(m), ONE_EVALUATION),
            (first(m), NONE),
            (first(last_5m), NONE),
        ],
    );
    let (solved, _) = solve(
        &physical,
        &every_10s(1),
        &[1.0, 2.0, 1.0],
        &[((0, 2), 5.0)],
        &[(1, 1e-9)],
    );
    let solved = solved.unwrap();
    assert_eq!(candidates(&solved), vec![1, 2]);
    assert_eq!(solved.edges.len(), 1);
    assert_eq!(solved.edges[0].1, edge(&physical, 1, 2));
    assert_eq!(solved.outputs[0].candidate, PhysicalCandidateId(2));

    let plan = plan_of(&physical, &solved, &outputs(&physical)).unwrap();
    assert_eq!(plan.roots, vec![1]);
    assert_eq!(plan.edges.len(), 1);
    assert_eq!((plan.edges[0].producer, plan.edges[0].consumer), (0, 1));
    let selected: Vec<_> = plan
        .nodes
        .iter()
        .map(|node| node.selected_physical_candidate)
        .collect();
    assert_eq!(
        selected,
        vec![Some(PhysicalCandidateId(1)), Some(PhysicalCandidateId(2))]
    );
    assert_eq!(plan.nodes[0].retention_ms, Some(300_000));
    assert_eq!(plan.nodes[1].retention_ms, None);
}

/// `m[5m]` and `m[1m]` both read the scan, both queries every 10 s: one
/// context. Its candidate that is not materialized (cost 1) may have one
/// consumer there, so the optimum reads the scan's `OneEvaluation`
/// candidate (cost 10) twice rather than both; without it no plan exists.
#[test]
fn a_query_time_result_that_is_not_materialized_has_one_consumer_per_context() {
    let mut memo = Memo::default();
    let m = scan(&mut memo);
    let last_5m = range(&mut memo, 300, m);
    let last_1m = range(&mut memo, 60, m);
    query(&mut memo, last_5m);
    query(&mut memo, last_1m);
    let physical = physical(
        memo,
        &[
            (first(m), NONE),
            (first(last_5m), NONE),
            (first(last_1m), NONE),
            (first(m), ONE_EVALUATION),
        ],
    );
    let (solved, _) = solve(&physical, &every_10s(2), &[1.0, 1.0, 1.0, 10.0], &[], &[]);
    assert_eq!(candidates(&solved.unwrap()), vec![1, 2, 3]);

    let without_one_evaluation = PhysicalASAPMemoCandidates::new(
        physical.logical_candidates.clone(),
        physical.physical_candidates[..3].to_vec(),
        physical
            .edges()
            .map(|(_, edge)| edge)
            .filter(|edge| edge.producer_candidate_id.0 < 3)
            .collect(),
        vec![],
    );
    let (solved, _) = solve(
        &without_one_evaluation,
        &every_10s(2),
        &[0.0, 1.0, 1.0],
        &[],
        &[],
    );
    assert_eq!(solved.unwrap_err(), SelectionMilpError::Infeasible);
}

/// The same two queries every 10 s and every 15 s: two contexts, each
/// running the scan that is not materialized once for its own query
/// (cost 1 each), cheaper than one `OneEvaluation` run (cost 10), which
/// could not be shared across contexts anyway.
#[test]
fn a_result_that_is_not_materialized_runs_once_per_context_that_reads_it() {
    let mut memo = Memo::default();
    let m = scan(&mut memo);
    let last_5m = range(&mut memo, 300, m);
    let last_1m = range(&mut memo, 60, m);
    query(&mut memo, last_5m);
    query(&mut memo, last_1m);
    let physical = physical(
        memo,
        &[
            (first(m), NONE),
            (first(last_5m), NONE),
            (first(last_1m), NONE),
            (first(m), ONE_EVALUATION),
        ],
    );
    let (solved, contexts) = solve(
        &physical,
        &[every(10_000), every(15_000)],
        &[1.0, 1.0, 1.0, 10.0],
        &[],
        &[],
    );
    let solved = solved.unwrap();
    assert_eq!(candidates(&solved), vec![0, 0, 1, 2]);
    let scan_contexts: Vec<_> = solved
        .executions
        .iter()
        .filter(|execution| execution.candidate.0 == 0)
        .map(|execution| execution.context)
        .collect();
    assert_ne!(scan_contexts[0], scan_contexts[1]);
    let plan = plan_of(&physical, &solved, &outputs(&physical)).unwrap();
    assert_eq!(plan.nodes.len(), 4);
    assert_eq!(contexts.executions().len(), 6);
}

/// `m[1m]` every 10 s and `m[5m]` every 20 s, both over a chain of 10 s
/// windows of the scan, materialized across evaluations: the chain runs
/// once, in its own context, and delivers on both grids; it retains the
/// longest range its selected consumers read, 5 minutes (R).
#[test]
fn a_materialized_chain_runs_once_and_retains_what_its_consumers_read_back() {
    let mut memo = Memo::default();
    let m = scan(&mut memo);
    let chain = group_with_own_candidate(
        &mut memo,
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: m,
            summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
            tumbling_window_width_ms: 10_000,
        }),
    );
    let last_1m = range(&mut memo, 60, chain);
    let last_5m = range(&mut memo, 300, chain);
    query(&mut memo, last_1m);
    query(&mut memo, last_5m);
    let physical = physical(
        memo,
        &[
            (first(m), NONE),
            (first(chain), ACROSS_EVALUATIONS),
            (first(last_1m), NONE),
            (first(last_5m), NONE),
        ],
    );
    let (solved, _) = solve(
        &physical,
        &[every(10_000), every(20_000)],
        &[1.0, 1.0, 1.0, 1.0],
        &[],
        &[(1, 1e-6)],
    );
    let solved = solved.unwrap();
    assert_eq!(candidates(&solved), vec![0, 1, 2, 3]);
    assert_eq!(
        solved.retention_ms.get(&PhysicalCandidateId(1)),
        Some(&300_000)
    );
    let plan = plan_of(&physical, &solved, &outputs(&physical)).unwrap();
    let chain_node = plan
        .nodes
        .iter()
        .find(|node| node.selected_physical_candidate == Some(PhysicalCandidateId(1)))
        .unwrap();
    assert_eq!(chain_node.retention_ms, Some(300_000));
}

/// Ga = `m offset 1s` and Gb = `m offset 2s`, each also computed from the
/// other (Ga② = Gb shifted by −1 s, Gb② = Ga shifted by 1 s). Reading each
/// other costs 1 and each own expression 10; without (E) both would read
/// the other and neither be computed. The optimum computes one from the
/// scan and the other from it.
#[test]
fn acyclicity_forbids_two_groups_reading_each_other() {
    let mut memo = Memo::default();
    let m = scan(&mut memo);
    let a = group_with_own_candidate(&mut memo, shift(1_000, m));
    let b = group_with_own_candidate(&mut memo, shift(2_000, m));
    let a_from_b = add_candidate(&mut memo, a, shift(-1_000, b), None).unwrap();
    let b_from_a = add_candidate(&mut memo, b, shift(1_000, a), None).unwrap();
    query(&mut memo, a);
    query(&mut memo, b);
    // v0 scan; v1 Ga①, v2 Gb①, v3 Ga②, v4 Gb②.
    let physical = physical(
        memo,
        &[
            (first(m), ONE_EVALUATION),
            (first(a), ONE_EVALUATION),
            (first(b), ONE_EVALUATION),
            (a_from_b, ONE_EVALUATION),
            (b_from_a, ONE_EVALUATION),
        ],
    );
    let (solved, _) = solve(
        &physical,
        &every_10s(2),
        &[0.0, 10.0, 10.0, 1.0, 1.0],
        &[],
        &[],
    );
    let solved = solved.unwrap();
    let selected = candidates(&solved);
    assert!(
        selected == vec![0, 1, 4] || selected == vec![0, 2, 3],
        "{selected:?}"
    );
    plan_of(&physical, &solved, &outputs(&physical)).unwrap();
}

/// `m[5m]` over a chain of 10 s windows of the scan: a chain materialized
/// across evaluations builds only its newest window from the scan (10 s
/// read back); one rebuilt in each evaluation builds every window the range
/// over it reads (5 minutes).
#[test]
fn a_chain_rebuilt_in_each_evaluation_reads_back_what_its_consumers_read() {
    let mut memo = Memo::default();
    let m = scan(&mut memo);
    let chain = group_with_own_candidate(
        &mut memo,
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: m,
            summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
            tumbling_window_width_ms: 10_000,
        }),
    );
    let last_5m = range(&mut memo, 300, chain);
    query(&mut memo, last_5m);
    let physical = physical(
        memo,
        &[
            (first(m), NONE),
            (first(chain), ACROSS_EVALUATIONS),
            (first(chain), NONE),
            (first(last_5m), NONE),
        ],
    );
    let need_ms = |chain: usize| {
        crate::cost::edge_read_back_ms(&physical, &every_10s(1), edge(&physical, 0, chain), true)
    };
    assert_eq!(need_ms(1), Some(10_000));
    assert_eq!(need_ms(2), Some(300_000));
}

// Constraint (K).

/// A DDSketch state over `m[1m]`, one build per `alpha`.
fn ddsketch_state(memo: &mut Memo, alphas: &[f64]) -> (MemoGroupId, Vec<CandidateForMemoGroupId>) {
    let m = scan(memo);
    let window = range(memo, 60, m);
    let operator = Operator::ASAP(ASAPOp::SummaryAgg {
        child: window,
        summary_type: FieldDataType::Sketch(SketchAlgorithm::DDSketch),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
    });
    let state = memo.find_or_add_group(operator.clone()).unwrap();
    let builds = alphas
        .iter()
        .map(|&alpha| {
            add_candidate(
                memo,
                state,
                operator.clone(),
                Some(SketchParams::DDSketch { alpha }),
            )
            .unwrap()
        })
        .collect();
    (state, builds)
}

/// The readout group of quantile `q` over `state`, one readout per
/// requirement of `levels` (relative error, failure probability); `None`
/// adds a readout without a requirement.
fn readouts(
    memo: &mut Memo,
    state: MemoGroupId,
    q: f64,
    levels: &[Option<(f64, f64)>],
) -> (MemoGroupId, Vec<CandidateForMemoGroupId>) {
    let operator = Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: state,
        query: SketchStatistic::Quantile { q },
    });
    let group = memo.find_or_add_group(operator.clone()).unwrap();
    let readouts = levels
        .iter()
        .map(|level| {
            let id = add_candidate(memo, group, operator.clone(), None).unwrap();
            if let Some((max_error, failure_probability)) = *level {
                memo.set_input_requirements(
                    id,
                    vec![LogicalRequirements {
                        required_accuracy: Some(RequiredAccuracy {
                            metric: AccuracyMetric::Relative,
                            max_error,
                            failure_probability,
                        }),
                    }],
                );
            }
            id
        })
        .collect();
    (group, readouts)
}

fn targeted_query(
    memo: &mut Memo,
    root: QueryRoot<MemoGroupId>,
    target: AccuracyTarget,
    values_per_evaluation: Option<u64>,
) {
    memo.add_query(MemoQuery {
        root,
        accuracy_target: Some(target),
        values_per_evaluation,
    });
}

/// Every candidate of `memo` at query time, with `lifetime`.
fn every_candidate(memo: &Memo, lifetime: Lifetime) -> Vec<(CandidateForMemoGroupId, Lifetime)> {
    memo.groups()
        .iter()
        .enumerate()
        .flat_map(|(group, memo_group)| {
            (0..memo_group.candidates.len()).map(move |index_in_group| {
                (
                    CandidateForMemoGroupId {
                        group: MemoGroupId(group as u32),
                        index_in_group,
                    },
                    lifetime,
                )
            })
        })
        .collect()
}

/// The physical candidate of logical candidate `id` in `physical`.
fn physical_of(physical: &PhysicalASAPMemoCandidates, id: CandidateForMemoGroupId) -> usize {
    physical
        .physical_candidates
        .iter()
        .position(|candidate| candidate.logical_candidate_id == id)
        .unwrap()
}

/// #1207's query B, p99 / p50 at relative error 0.03 (concepts doc,
/// section 4, "Example: allocation in a ratio"): each operand alone may be
/// off by 0.02, and those readouts cost least, but their composed bound
/// 1.111 (0.02 + 0.02) = 0.044 is above 0.03, and so is 1.111 (0.02 +
/// 0.01) = 0.033; (K) selects both readouts at 0.01 (0.022).
#[test]
fn k_allocates_a_ratio_s_error_among_its_readouts() {
    let mut memo = Memo::default();
    let (state, _) = ddsketch_state(&mut memo, &[0.01]);
    let levels = [Some((0.02, 1.0)), Some((0.01, 1.0))];
    let (p99, p99_readouts) = readouts(&mut memo, state, 0.99, &levels);
    let (p50, p50_readouts) = readouts(&mut memo, state, 0.5, &levels);
    let ratio = group_with_own_candidate(
        &mut memo,
        Operator::NonASAP(NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                checked_relative_division: false,
                checked_finite_division: false,
                kind: asap_types::ir::operator::operator_properties::BinaryOpKind::Arithmetic(
                    ArithmeticOpKind::Div,
                ),
                vector_match: None,
            },
            return_bool: false,
            lhs: p99,
            rhs: p50,
        }),
    );
    targeted_query(
        &mut memo,
        QueryRoot::Operator(ratio),
        AccuracyTarget::max_error(0.03).accuracy_metric(AccuracyMetric::Relative),
        Some(1),
    );
    let physical = physical(memo.clone(), &every_candidate(&memo, ONE_EVALUATION));
    // The 0.02 readouts cost 1, the 0.01 ones 2, everything else 0.
    let mut w = vec![0.0; physical.physical_candidates.len()];
    for readouts in [&p99_readouts, &p50_readouts] {
        w[physical_of(&physical, readouts[0])] = 1.0;
        w[physical_of(&physical, readouts[1])] = 2.0;
    }
    let (solved, _) = solve(&physical, &every_10s(1), &w, &[], &[]);
    let selected = candidates(&solved.unwrap());
    for readouts in [&p99_readouts, &p50_readouts] {
        assert!(selected.contains(&physical_of(&physical, readouts[1])));
        assert!(!selected.contains(&physical_of(&physical, readouts[0])));
    }
}

/// A query reading one readout group: whether a plan exists for `target`
/// with `values_per_evaluation`, the group's readouts having `levels`
/// (relative error) and the root being `root` over the group.
fn plans(
    target: Option<AccuracyTarget>,
    levels: &[Option<(f64, f64)>],
    values_per_evaluation: Option<u64>,
    root: fn(MemoGroupId) -> QueryRoot<MemoGroupId>,
) -> bool {
    let mut memo = Memo::default();
    let (state, _) = ddsketch_state(&mut memo, &[0.01]);
    let (readout_group, _) = readouts(&mut memo, state, 0.5, levels);
    memo.add_query(MemoQuery {
        root: root(readout_group),
        accuracy_target: target,
        values_per_evaluation,
    });
    let physical = physical(memo.clone(), &every_candidate(&memo, ONE_EVALUATION));
    let w = vec![1.0; physical.physical_candidates.len()];
    match solve(&physical, &every_10s(1), &w, &[], &[]).0 {
        Ok(_) => true,
        Err(SelectionMilpError::Infeasible) => false,
        Err(other) => panic!("{other:?}"),
    }
}

fn relative(max_error: f64) -> AccuracyTarget {
    AccuracyTarget::max_error(max_error).accuracy_metric(AccuracyMetric::Relative)
}

/// The root accepts error only in the target's metric: a readout off by
/// relative 0.01 meets relative 0.01, not rank 0.01; a target naming no
/// metric accepts any.
#[test]
fn k_accepts_error_only_in_the_target_s_metric() {
    let level = [Some((0.01, 1.0))];
    let operator = QueryRoot::Operator;
    assert!(plans(Some(relative(0.01)), &level, Some(1), operator));
    assert!(!plans(Some(relative(0.005)), &level, Some(1), operator));
    let rank = AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Rank);
    assert!(!plans(Some(rank), &level, Some(1), operator));
    assert!(plans(
        Some(AccuracyTarget::max_error(0.01)),
        &level,
        Some(1),
        operator
    ));
}

/// A whole-result failure probability 0.01 over 10 values is 1e-3 per
/// value, which a readout failing with probability 1e-3 meets; over 100
/// values (1e-4 per value) it does not. With the number unknown no
/// per-value δ follows: Pass 2 prunes the readouts
/// (`ValuesPerEvaluationUnknown`), so (K) never sees that query.
#[test]
fn k_divides_a_whole_result_failure_probability_over_the_values() {
    let level = [Some((0.01, 1e-3))];
    let target = relative(0.01)
        .failure_probability(0.01)
        .accuracy_scope(AccuracyScope::WholeResult);
    let operator = QueryRoot::Operator;
    assert!(plans(Some(target.clone()), &level, Some(10), operator));
    assert!(!plans(Some(target), &level, Some(100), operator));
}

/// A scalar root's operands get no requirement from Pass 2, so a query
/// with a target over a scalar root reads them exactly.
#[test]
fn a_scalar_root_with_a_target_reads_its_operands_exactly() {
    let level = [Some((0.01, 1.0))];
    let scalar = |group| QueryRoot::Scalar(ScalarExpr::PromqlScalarFromVector(group));
    assert!(!plans(Some(relative(0.01)), &level, Some(1), scalar));
    assert!(plans(None, &level, Some(1), scalar));
}
