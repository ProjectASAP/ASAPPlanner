//! The selection MILP over small hand-built candidate graphs with given
//! costs: which executions and edges its optimum selects, its retention,
//! and the plan built from it.

use std::collections::HashMap;

use asap_types::ir::operator::Reduction;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator, QueryRoot};
use asap_types::stages::{
    CandidateForMemoGroupId, LogicalASAPMemoCandidates, Memo, MemoGroupId,
    PhysicalASAPMemoCandidates, PhysicalCandidate, PhysicalCandidateId, PhysicalEdge,
    PhysicalEdgeId,
};
use asap_types::stages::{Lifetime, PhysicalProperties, StorageMedium};
use asap_types::summary_type::{ExactKind, ExactParams, SummaryUpdate};
use asap_types::test_support::memo_builders::{
    group, own_candidate, query, scan_group, try_candidate,
};
use asap_types::test_support::memo_fixtures;
use asap_types::workload::DataWorkload;
use asap_types::workload::{
    Predictability, Query, QueryRecurrence, QueryRequirements, RepeatedDemand, RepetitionInterval,
    TimestampMs,
};

use super::*;
use crate::cost::ExecutionCosts;
use crate::plan::plan_of;
use crate::selectable::ExecutionContexts;

const NONE: Lifetime = Lifetime::None;
const ONE_EVALUATION: Lifetime = Lifetime::OneEvaluation(StorageMedium::Memory);
const ACROSS_EVALUATIONS: Lifetime = Lifetime::AcrossEvaluations(StorageMedium::Memory);

fn scan(memo: &mut Memo) -> MemoGroupId {
    scan_group(memo, "m", &["job"], vec![])
}

fn range(memo: &mut Memo, seconds: i64, child: MemoGroupId) -> MemoGroupId {
    group(memo, memo_fixtures::range(seconds * 1_000, child))
}

fn shift(offset_ms: i64, child: MemoGroupId) -> Operator<MemoGroupId> {
    memo_fixtures::shift(offset_ms, None, child)
}

fn empty_memo() -> Memo {
    Memo::new(&DataWorkload::default())
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
            exact_only_groups_of_query: vec![],
        },
        physical_candidates,
        edges,
        vec![],
    )
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
        .queries()
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
        raw_data_source: HashMap::new(),
        unbounded_raw_data_on_disk_cost_per_second: HashMap::new(),
        request_overhead_cost_per_second: 0.0,
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

/// `m[5m]`, its scan with two physical candidates. Through v0 the read
/// costs 1 + 5, through v1 2 + 0: the optimum takes v1, and the plan built
/// from it is the scan v1 below the range v2, the scan retaining the 5
/// minutes the range reads (R).
#[test]
fn selects_the_producer_whose_own_and_edge_costs_are_lowest() {
    let mut memo = empty_memo();
    let m = scan(&mut memo);
    let last_5m = range(&mut memo, 300, m);
    query(&mut memo, last_5m, None);
    let physical = physical(
        memo,
        &[
            (own_candidate(m), ONE_EVALUATION),
            (own_candidate(m), NONE),
            (own_candidate(last_5m), NONE),
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
    let mut memo = empty_memo();
    let m = scan(&mut memo);
    let last_5m = range(&mut memo, 300, m);
    let last_1m = range(&mut memo, 60, m);
    query(&mut memo, last_5m, None);
    query(&mut memo, last_1m, None);
    let physical = physical(
        memo,
        &[
            (own_candidate(m), NONE),
            (own_candidate(last_5m), NONE),
            (own_candidate(last_1m), NONE),
            (own_candidate(m), ONE_EVALUATION),
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
    let mut memo = empty_memo();
    let m = scan(&mut memo);
    let last_5m = range(&mut memo, 300, m);
    let last_1m = range(&mut memo, 60, m);
    query(&mut memo, last_5m, None);
    query(&mut memo, last_1m, None);
    let physical = physical(
        memo,
        &[
            (own_candidate(m), NONE),
            (own_candidate(last_5m), NONE),
            (own_candidate(last_1m), NONE),
            (own_candidate(m), ONE_EVALUATION),
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
    let mut memo = empty_memo();
    let m = scan(&mut memo);
    let chain = group(
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
    query(&mut memo, last_1m, None);
    query(&mut memo, last_5m, None);
    let physical = physical(
        memo,
        &[
            (own_candidate(m), NONE),
            (own_candidate(chain), ACROSS_EVALUATIONS),
            (own_candidate(last_1m), NONE),
            (own_candidate(last_5m), NONE),
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
    let mut memo = empty_memo();
    let m = scan(&mut memo);
    let a = group(&mut memo, shift(1_000, m));
    let b = group(&mut memo, shift(2_000, m));
    let a_from_b = try_candidate(&mut memo, a, shift(-1_000, b), None).unwrap();
    let b_from_a = try_candidate(&mut memo, b, shift(1_000, a), None).unwrap();
    query(&mut memo, a, None);
    query(&mut memo, b, None);
    // v0 scan; v1 Ga①, v2 Gb①, v3 Ga②, v4 Gb②.
    let physical = physical(
        memo,
        &[
            (own_candidate(m), ONE_EVALUATION),
            (own_candidate(a), ONE_EVALUATION),
            (own_candidate(b), ONE_EVALUATION),
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

/// x08's shape: `max by (job)(count_over_time(m[1m]))`. Path A (scan and
/// range `OneEvaluation`, the root reading the range: 0.0058 + 0.0010 +
/// 0.0020) costs 0.0088/s; path B (each `None`, through three more groups)
/// 0.0078/s. An unused candidate costing 2.55e6/s scaled every coefficient
/// so the 0.001 difference fell under HiGHS's tolerances and the optimum
/// took path A, 12.6% dearer. A candidate dearer than a feasible plan is
/// left out before scaling, so the optimum is path B.
#[test]
fn a_candidate_dearer_than_a_feasible_plan_does_not_hide_a_cheaper_plan() {
    let mut memo = empty_memo();
    let m = scan(&mut memo);
    let r = range(&mut memo, 60, m);
    let a = group(&mut memo, shift(1_000, r));
    let b = group(&mut memo, shift(2_000, a));
    let c = group(&mut memo, shift(3_000, b));
    let root = group(&mut memo, shift(4_000, c));
    let root_from_r = try_candidate(&mut memo, root, shift(10_000, r), None).unwrap();
    query(&mut memo, root, None);
    let first = own_candidate;
    let physical = physical(
        memo,
        &[
            (first(m), ONE_EVALUATION), // v0: path A's scan
            (first(r), ONE_EVALUATION), // v1: path A's range
            (root_from_r, NONE),        // v2: path A's root, reading the range
            (first(m), NONE),           // v3: path B's scan
            (first(r), NONE),           // v4: path B's range
            (first(a), NONE),           // v5
            (first(b), NONE),           // v6
            (first(c), NONE),           // v7
            (first(root), NONE),        // v8: path B's root
            (first(c), ONE_EVALUATION), // v9: unused, 2.55e6/s
        ],
    );
    let w = [
        0.005_802_088,
        0.001_000_36,
        0.002_033_333_5,
        0.005_800_000_034_8,
        0.001_000_000_006,
        0.001_000_007,
        1.666_666_666_666_666_7e-5,
        1.666_678_333_333_333_4e-5,
        1.666_666_666_666_666_7e-5,
        2_553_517.822_748_626_6,
    ];
    let (solved, _) = solve(&physical, &every_10s(1), &w, &[], &[]);
    assert_eq!(candidates(&solved.unwrap()), vec![3, 4, 5, 6, 7, 8]);
}
