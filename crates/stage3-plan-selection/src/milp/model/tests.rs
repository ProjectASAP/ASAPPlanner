//! The selection MILP over small hand-built candidate graphs with given
//! costs: which executions and edges its optimum selects, its retention,
//! and the plan built from it.

use std::time::Duration;

use asap_types::ir::operator::operator_properties::Reduction;
use asap_types::ir::operator::TimeShift;
use asap_types::ir::scalar::expr_ir::ColumnRef;
use asap_types::ir::schema::{ExactKind, ExactParams, FieldDataType, SummaryUpdate};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, QueryRoot, TimeRangeKind};
use asap_types::physical::{ExecutionTiming, Lifetime, PhysicalProperties, StorageMedium};
use asap_types::stages::{
    CandidateForMemoGroupId, LogicalASAPMemoCandidates, Memo, MemoGroupId, MemoQuery,
    PhysicalASAPMemoCandidates, PhysicalCandidate, PhysicalCandidateId, PhysicalEdge,
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

/// Each query output reference of `physical`'s memo: query `q` reads its
/// operator root.
fn outputs(physical: &PhysicalASAPMemoCandidates) -> Vec<(QueryId, MemoGroupId)> {
    physical
        .logical_candidates
        .memo
        .queries
        .iter()
        .enumerate()
        .map(|(query, memo_query)| match memo_query.root {
            QueryRoot::Operator(group) => (QueryId(query), group),
            QueryRoot::Scalar(_) => unreachable!("operator roots only"),
        })
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
        crate::cost::edge_retention_need_ms(&physical, &every_10s(1), edge(&physical, 0, chain))
    };
    assert_eq!(need_ms(1), Some(10_000));
    assert_eq!(need_ms(2), Some(300_000));
}
