//! The selection MILP over small hand-built candidate graphs with given
//! costs: which executions and edges its optimum selects, its retention,
//! and the plan built from it.

use std::collections::{BTreeSet, HashMap};

use asap_types::accuracy::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::operator::Reduction;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::scalar::ExprSemantics;
use asap_types::ir::scalar::ScalarValue;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::ir::{QueryRoot, ScalarExpr};
use asap_types::stages::{
    CandidateForMemoGroup, CandidateForMemoGroupId, LogicalASAPMemoCandidates, LogicalRequirements,
    Memo, MemoGroupId, MemoQuery, OriginOfCandidateForMemoGroup, PhysicalASAPMemoCandidates,
    PhysicalCandidate, PhysicalCandidateId, PhysicalEdge, PhysicalEdgeId, RequiredAccuracy,
};
use asap_types::stages::{Lifetime, PhysicalProperties, StorageMedium};
use asap_types::summary_type::{
    ExactKind, ExactParams, SketchAlgorithm, SketchStatistic, SummaryUpdate,
};
use asap_types::test_support::memo_builders::{
    group, own_candidate, quantile_over_time_readouts, query, scan_group, sketch_state,
    try_candidate,
};
use asap_types::test_support::memo_fixtures;
use asap_types::test_support::memo_fixtures::arithmetic;
use asap_types::workload::DataWorkload;
use asap_types::workload::{
    Predictability, Query, QueryRecurrence, QueryRequirements, RepeatedDemand, RepetitionInterval,
    TimestampMs,
};

use super::*;
use crate::cost::ExecutionCosts;
use crate::milp::accuracy::{AccuracyRules, AccuracyVariables, ErrorModel};
use crate::plan::plan_of;
use crate::retention::{edge_retention_need_ms, least_retention_ms_per_execution};
use crate::selectable::{QueryId, SelectableEdge};

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

/// What the MILP is built over for `physical` and `entries`, with every
/// execution of candidate `v` costing `w[v]`, every edge from `u` to `c`
/// `edge_costs[(u, c)]` (0 when missing) and candidate `v` retaining at
/// `retention[v]` per second retained; each query output answered, as
/// `crate::select_plan` does, by the executions its query may read (X).
fn selection_inputs(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    w: &[f64],
    edge_costs: &[((usize, usize), f64)],
    retention: &[(usize, f64)],
) -> (SelectableExecutions, ExecutionCosts) {
    let mut selectable = SelectableExecutions::new(physical, entries, &output_groups(physical));
    selectable.retain_answering(|query, _, execution| {
        !physical.logical_candidates.query_must_not_read(
            query.0,
            physical.physical_candidates[execution.candidate.0].logical_candidate_id,
        )
    });
    let retention_cost_per_retained_second: HashMap<PhysicalCandidateId, f64> = retention
        .iter()
        .map(|&(v, per_retained_second)| (PhysicalCandidateId(v), per_retained_second))
        .collect();
    let edge_of = |edge: &SelectableEdge| physical.edge(edge.edge);
    let mut costs = ExecutionCosts {
        per_execution: selectable
            .executions()
            .iter()
            .map(|execution| Ok(w[execution.candidate.0]))
            .collect(),
        per_edge: selectable
            .edges()
            .iter()
            .map(|selectable_edge| {
                let edge = edge_of(selectable_edge);
                let key = (edge.producer_candidate_id.0, edge.consumer_candidate_id.0);
                Some(
                    edge_costs
                        .iter()
                        .find(|(pair, _)| *pair == key)
                        .map_or(0.0, |(_, cost)| *cost),
                )
            })
            .collect(),
        retention_need_ms_per_edge: selectable
            .edges()
            .iter()
            .map(|selectable_edge| {
                let edge = edge_of(selectable_edge);
                retention_cost_per_retained_second
                    .contains_key(&edge.producer_candidate_id)
                    .then(|| edge_retention_need_ms(physical, entries, selectable_edge.edge))
                    .flatten()
            })
            .collect(),
        retention_cost_per_retained_second,
        newest_output_retention_ms: HashMap::new(),
        least_retention_ms_per_execution: Vec::new(),
        raw_data_source: HashMap::new(),
        unbounded_raw_data_on_disk_cost_per_second: HashMap::new(),
    };
    costs.least_retention_ms_per_execution =
        least_retention_ms_per_execution(&selectable, &costs, entries);
    (selectable, costs)
}

/// Solves over [`selection_inputs`]; returns the solution and the
/// executions it is over.
fn solve(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    w: &[f64],
    edge_costs: &[((usize, usize), f64)],
    retention: &[(usize, f64)],
) -> (
    Result<SolvedSelection, SelectionMilpError>,
    SelectableExecutions,
) {
    let (selectable, costs) = selection_inputs(physical, entries, w, edge_costs, retention);
    let solved = solve_selection(&SelectionInputs {
        physical,
        entries,
        selectable: &selectable,
        costs: &costs,
    });
    (solved, selectable)
}

/// Each query output reference of `physical`'s memo (`Memo::output_groups`).
fn output_groups(physical: &PhysicalASAPMemoCandidates) -> Vec<(QueryId, MemoGroupId)> {
    physical
        .logical_candidates
        .memo
        .output_groups()
        .into_iter()
        .map(|(query, group)| (QueryId(query), group))
        .collect()
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
        &[(1, 1e-6)],
    );
    let solved = solved.unwrap();
    assert_eq!(candidates(&solved), vec![1, 2]);
    assert_eq!(solved.edges.len(), 1);
    assert_eq!(solved.edges[0].edge, edge(&physical, 1, 2));
    assert_eq!(solved.outputs[0].candidate, PhysicalCandidateId(2));

    let plan = plan_of(&physical, &solved, &output_groups(&physical)).unwrap();
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
    let plan = plan_of(&physical, &solved, &output_groups(&physical)).unwrap();
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
        &[(1, 1e-3)],
    );
    let solved = solved.unwrap();
    assert_eq!(candidates(&solved), vec![0, 1, 2, 3]);
    assert_eq!(
        solved.retention_ms.get(&PhysicalCandidateId(1)),
        Some(&300_000)
    );
    let plan = plan_of(&physical, &solved, &output_groups(&physical)).unwrap();
    let chain_node = plan
        .nodes
        .iter()
        .find(|node| node.selected_physical_candidate == Some(PhysicalCandidateId(1)))
        .unwrap();
    assert_eq!(chain_node.retention_ms, Some(300_000));
}

/// A chain of 10 s windows materialized across evaluations, read every
/// 10 s at its newest output only (no range above it): no read needs a
/// retention, but a store always keeps the newest output, so the chain
/// retains one window, 10 s, and pays for it (1 + 1 + 1e-3 · 10 = 2.01),
/// as `plan_cost` charges one retained output. Without the newest output
/// it would retain 0 and pay 2.
#[test]
fn a_stored_result_read_at_its_newest_output_retains_one_output() {
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
    query(&mut memo, chain, None);
    let physical = physical(
        memo,
        &[
            (own_candidate(m), NONE),
            (own_candidate(chain), ACROSS_EVALUATIONS),
        ],
    );
    let entries = every_10s(1);
    let solved_with = |newest_output_ms: Option<u64>| {
        let (selectable, mut costs) =
            selection_inputs(&physical, &entries, &[1.0, 1.0], &[], &[(1, 1e-3)]);
        if let Some(newest_output_ms) = newest_output_ms {
            costs
                .newest_output_retention_ms
                .insert(PhysicalCandidateId(1), newest_output_ms);
            costs.least_retention_ms_per_execution =
                least_retention_ms_per_execution(&selectable, &costs, &entries);
        }
        solve_selection(&SelectionInputs {
            physical: &physical,
            entries: &entries,
            selectable: &selectable,
            costs: &costs,
        })
        .unwrap()
    };
    let solved = solved_with(Some(10_000));
    assert_eq!(
        solved.retention_ms.get(&PhysicalCandidateId(1)),
        Some(&10_000)
    );
    assert!((solved.cost_per_second_without_request_overhead - 2.01).abs() < 1e-9);
    let without_newest_output = solved_with(None);
    assert_eq!(
        without_newest_output
            .retention_ms
            .get(&PhysicalCandidateId(1)),
        Some(&0)
    );
    assert!((without_newest_output.cost_per_second_without_request_overhead - 2.0).abs() < 1e-9);
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
    plan_of(&physical, &solved, &output_groups(&physical)).unwrap();
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
    // The unused candidate also retains rows at 2.55e6/s per second
    // retained (z05's unused UnivMon chain): it has no h_v once its
    // executions are left out, so its retention cost does not scale the
    // objective either.
    let (solved, _) = solve(&physical, &every_10s(1), &w, &[], &[(9, w[9])]);
    assert_eq!(candidates(&solved.unwrap()), vec![3, 4, 5, 6, 7, 8]);
}

/// `quantile_over_time(0.9, m[5m])` read by Q0 every 10 s and Q1 every
/// 30 s, two contexts: its KLL readout (cost 1 per execution, through the
/// state group) is cheaper than the exact quantile (cost 100). Q1 may read
/// its exact-only groups only exactly (a relative target, which the KLL
/// readout's rank error does not meet). With Pass 2's list (the quantile
/// and every group below it) Q1's output is the exact quantile, constraint
/// (X) at the output; with only the state group listed it is too, as the
/// readout would read the state for Q1 (X along the edge); with no list Q1
/// reads the readout. Q0, with no list, reads the readout throughout.
#[test]
fn a_query_reads_its_exact_only_groups_through_exact_candidates_only() {
    let mut memo = empty_memo();
    let m = scan(&mut memo);
    let window = range(&mut memo, 300, m);
    let quantile = quantile_over_time_readouts(&mut memo, window, 0.9, &[SketchAlgorithm::Kll]);
    let state = quantile.by_algorithm[0].state;
    query(&mut memo, quantile.result, None);
    query(&mut memo, quantile.result, None);
    let logical_candidates = every_candidate_one_theta_of(&memo, state);
    let exact_quantile = logical_candidates
        .iter()
        .position(|&(candidate, _)| candidate == own_candidate(quantile.result))
        .unwrap();
    let costs: Vec<f64> = (0..logical_candidates.len())
        .map(|v| if v == exact_quantile { 100.0 } else { 1.0 })
        .collect();
    let mut physical = physical(memo, &logical_candidates);
    // Every group: the quantile and every group below it.
    let below_quantile: BTreeSet<MemoGroupId> =
        (0..physical.logical_candidates.memo.groups().len())
            .map(|group| MemoGroupId(group as u32))
            .collect();
    for (q1_exact_only, q1_reads_exactly) in [
        (below_quantile, true),
        (BTreeSet::from([state]), true),
        (BTreeSet::new(), false),
    ] {
        physical.logical_candidates.exact_only_groups_of_query =
            vec![BTreeSet::new(), q1_exact_only];
        let (solved, _) = solve(&physical, &[every(10_000), every(30_000)], &costs, &[], &[]);
        let outputs: Vec<usize> = solved
            .unwrap()
            .outputs
            .iter()
            .map(|execution| execution.candidate.0)
            .collect();
        assert_ne!(outputs[0], exact_quantile);
        assert_eq!(
            outputs[1] == exact_quantile,
            q1_reads_exactly,
            "{outputs:?}"
        );
    }
}

/// Every candidate of `memo`'s groups, `OneEvaluation`, with one θ of
/// `state`'s (its first candidate only).
fn every_candidate_one_theta_of(
    memo: &Memo,
    state: MemoGroupId,
) -> Vec<(CandidateForMemoGroupId, Lifetime)> {
    (0..memo.groups().len())
        .map(|group| MemoGroupId(group as u32))
        .flat_map(|group| {
            let count = match group == state {
                true => 1,
                false => memo.groups()[group.0 as usize].candidates.len(),
            };
            (0..count).map(move |index_in_group| {
                (
                    CandidateForMemoGroupId {
                        group,
                        index_in_group,
                    },
                    ONE_EVALUATION,
                )
            })
        })
        .collect()
}

/// The shape of [`a_query_reads_its_exact_only_groups_through_exact_candidates_only`]
/// with Q0 every 10 s, which may read every group, and Q1 every 20 s,
/// which must not read the KLL state; the exact quantile's candidate costs
/// `exact_quantile_cost`, every other 1, and every cost in the 20 s
/// context is half the 10 s one's. The quantile's own candidate is also offered
/// `AcrossEvaluations` at `materialized_cost` (stored on its group's 10 s
/// grid, so it delivers on both; its inputs deliver on no `Materialized`
/// context, so it is never selected), listed first: each query's first
/// answering execution runs in `Materialized(v)`, not in the query's
/// evaluation context. Returns the candidates, the queries, the selection's
/// inputs and the exact quantile's `OneEvaluation` candidate.
fn queries_differing_in_exact_only_groups_with_a_materialized_answer(
    materialized_cost: f64,
    exact_quantile_cost: f64,
) -> (
    PhysicalASAPMemoCandidates,
    [QueryWorkloadEntry; 2],
    SelectableExecutions,
    ExecutionCosts,
    usize,
) {
    let mut memo = empty_memo();
    let m = scan(&mut memo);
    let window = range(&mut memo, 300, m);
    let quantile = quantile_over_time_readouts(&mut memo, window, 0.9, &[SketchAlgorithm::Kll]);
    let state = quantile.by_algorithm[0].state;
    query(&mut memo, quantile.result, None);
    query(&mut memo, quantile.result, None);
    let mut candidates = vec![(own_candidate(quantile.result), ACROSS_EVALUATIONS)];
    candidates.extend(every_candidate_one_theta_of(&memo, state));
    let exact_quantile = candidates
        .iter()
        .position(|&candidate| candidate == (own_candidate(quantile.result), ONE_EVALUATION))
        .unwrap();
    let w: Vec<f64> = (0..candidates.len())
        .map(|v| match v {
            0 => materialized_cost,
            _ if v == exact_quantile => exact_quantile_cost,
            _ => 1.0,
        })
        .collect();
    let mut physical = physical(memo, &candidates);
    physical.logical_candidates.evaluation_grid_of_group[quantile.result.0 as usize] =
        Some(asap_types::stages::EvaluationGrid {
            period_ms: 10_000,
            phase_ms: 0,
        });
    physical.logical_candidates.exact_only_groups_of_query =
        vec![BTreeSet::new(), BTreeSet::from([state])];
    let entries = [every(10_000), every(20_000)];
    let (selectable, mut costs) = selection_inputs(&physical, &entries, &w, &[], &[]);
    let every_20s = |index: usize| {
        matches!(
            selectable.context(selectable.executions()[index].context),
            asap_types::stages::ExecutionContext::Evaluation { grid } if grid.period_ms == 20_000
        )
    };
    for (index, cost) in costs.per_execution.iter_mut().enumerate() {
        if every_20s(index) {
            *cost = Ok(cost.clone().unwrap() / 2.0);
        }
    }
    for (position, edge) in selectable.edges().iter().enumerate() {
        if every_20s(edge.consumer) {
            costs.per_edge[position] = costs.per_edge[position].map(|cost| cost / 2.0);
        }
    }
    (physical, entries, selectable, costs, exact_quantile)
}

/// [`queries_differing_in_exact_only_groups_with_a_materialized_answer`]:
/// the two evaluation contexts have the same executions at half the cost,
/// but their queries read different groups, so they are not copies
/// ([`copies_in_equivalent_contexts`]): each query is in its own
/// evaluation context (`SelectableExecutions::evaluation_context`), not
/// in that of its first answering execution, `Materialized(v)`.
#[test]
fn contexts_whose_queries_read_different_groups_are_not_copies() {
    let (physical, entries, selectable, costs, _) =
        queries_differing_in_exact_only_groups_with_a_materialized_answer(1.0, 100.0);
    let first_answering = selectable.executions()[selectable.answering()[0].1[0]];
    assert!(matches!(
        selectable.context(first_answering.context),
        asap_types::stages::ExecutionContext::Materialized(_)
    ));
    let inputs = SelectionInputs {
        physical: &physical,
        entries: &entries,
        selectable: &selectable,
        costs: &costs,
    };
    let shared = copies_in_equivalent_contexts(&inputs, &vec![true; selectable.executions().len()]);
    assert!(shared
        .original_of_execution
        .iter()
        .enumerate()
        .all(|(index, &original)| original == index));
    assert_eq!(shared.original_of_query, vec![QueryId(0), QueryId(1)]);
}

/// [`queries_differing_in_exact_only_groups_with_a_materialized_answer`]
/// with the exact quantile costing 4 (2 in the 20 s context), every other
/// candidate 1: the optimum reads the KLL readout for Q0 (its five
/// candidates, 5) and the exact quantile for Q1 (0.5 + 0.5 + 2), 8.
/// Sharing the two contexts' variables would make Q0 read the exact
/// quantile too, at 9.
#[test]
fn queries_reading_different_groups_select_differently_in_contexts_alike() {
    let (physical, entries, selectable, costs, exact_quantile) =
        queries_differing_in_exact_only_groups_with_a_materialized_answer(1.0, 4.0);
    let solved = solve_selection(&SelectionInputs {
        physical: &physical,
        entries: &entries,
        selectable: &selectable,
        costs: &costs,
    })
    .unwrap();
    let outputs: Vec<usize> = solved
        .outputs
        .iter()
        .map(|execution| execution.candidate.0)
        .collect();
    assert_ne!(outputs[0], exact_quantile, "{outputs:?}");
    assert_eq!(outputs[1], exact_quantile, "{outputs:?}");
    assert!(
        (solved.cost_per_second_without_request_overhead - 8.0).abs() < 1e-9,
        "{}",
        solved.cost_per_second_without_request_overhead
    );
}

/// x07's shape: `quantile_over_time(0.9, m[5m])` read by Q0 every 10 s,
/// whose only paths read the KLL state (its exact quantile cannot be
/// costed), and by Q1 every 30 s, which may read every group only exactly.
/// The greedy bound leaves out of each query's trees only what that query
/// must not read, so it finds a plan (Q0 through the state, Q1 through the
/// exact quantile) and leaves out an unused scan costing 1e6/s; leaving
/// out of every query's trees what some query must not read found none.
#[test]
fn the_bound_leaves_out_of_each_query_only_what_it_must_not_read() {
    let mut memo = empty_memo();
    let m = scan(&mut memo);
    let window = range(&mut memo, 300, m);
    let quantile = quantile_over_time_readouts(&mut memo, window, 0.9, &[SketchAlgorithm::Kll]);
    let state = quantile.by_algorithm[0].state;
    query(&mut memo, quantile.result, None);
    query(&mut memo, quantile.result, None);
    let mut candidates = every_candidate_one_theta_of(&memo, state);
    let exact_quantile = candidates
        .iter()
        .position(|&(candidate, _)| candidate == own_candidate(quantile.result))
        .unwrap();
    let unused_scan = candidates.len();
    candidates.push((own_candidate(m), NONE));
    let w: Vec<f64> = (0..candidates.len())
        .map(|v| match v {
            _ if v == unused_scan => 1e6,
            _ if v == exact_quantile => 100.0,
            _ => 1.0,
        })
        .collect();
    let mut physical = physical(memo, &candidates);
    physical.logical_candidates.exact_only_groups_of_query = vec![
        BTreeSet::new(),
        (0..physical.logical_candidates.memo.groups().len())
            .map(|group| MemoGroupId(group as u32))
            .collect(),
    ];
    let entries = [every(10_000), every(30_000)];
    let (mut selectable, mut costs) = selection_inputs(&physical, &entries, &w, &[], &[]);
    let q0_context = selectable.executions()[selectable.answering()[0].1[0]].context;
    for (index, execution) in selectable.executions().iter().enumerate() {
        if execution.candidate.0 == exact_quantile && execution.context == q0_context {
            costs.per_execution[index] = Err("not costable".into());
        }
    }
    selectable.retain_answering(|_, index, _| costs.per_execution[index].is_ok());
    let inputs = SelectionInputs {
        physical: &physical,
        entries: &entries,
        selectable: &selectable,
        costs: &costs,
    };
    let left_out = dearer_than_a_feasible_plan(&inputs, None);
    let unused: Vec<bool> = selectable
        .executions()
        .iter()
        .zip(&left_out)
        .filter(|(execution, _)| execution.candidate.0 == unused_scan)
        .map(|(_, &left_out)| left_out)
        .collect();
    assert_eq!(unused, vec![true, true]);
    assert!(solve_selection(&inputs).is_ok());
}

/// `m[5m]` every 10 s over a scan that is not materialized (cost 1) or a
/// `OneEvaluation` one (cost 2, retaining at 1e6/s per second retained).
/// The greedy plan reads the first, at 2/s. The second's own cost alone is
/// no dearer than that, but (R) makes it retain the 5 minutes its one
/// consumer reads, 3e8/s: the bound leaves it out.
#[test]
fn the_bound_counts_the_least_retention_an_execution_pays() {
    let mut memo = empty_memo();
    let m = scan(&mut memo);
    let last_5m = range(&mut memo, 300, m);
    query(&mut memo, last_5m, None);
    let physical = physical(
        memo,
        &[
            (own_candidate(m), NONE),
            (own_candidate(m), ONE_EVALUATION),
            (own_candidate(last_5m), NONE),
        ],
    );
    let entries = every_10s(1);
    let (selectable, costs) =
        selection_inputs(&physical, &entries, &[1.0, 2.0, 1.0], &[], &[(1, 1e6)]);
    let left_out = dearer_than_a_feasible_plan(
        &SelectionInputs {
            physical: &physical,
            entries: &entries,
            selectable: &selectable,
            costs: &costs,
        },
        None,
    );
    let left_out: Vec<usize> = selectable
        .executions()
        .iter()
        .zip(&left_out)
        .filter(|(_, &left_out)| left_out)
        .map(|(execution, _)| execution.candidate.0)
        .collect();
    assert_eq!(left_out, vec![1]);
}

/// `m[5m]` every 10 s over a scan offered with ℓ = None (v0) and
/// `OneEvaluation` (v1). At equal cost and with the same consumer, v1
/// dominates v0, which is left out; v0 is kept when v1 costs more, or when
/// v1 cannot be read by v0's consumer (its edge is not costed).
#[test]
fn a_none_execution_its_one_evaluation_twin_dominates_is_left_out() {
    let mut memo = empty_memo();
    let m = scan(&mut memo);
    let last_5m = range(&mut memo, 300, m);
    query(&mut memo, last_5m, None);
    let physical = physical(
        memo,
        &[
            (own_candidate(m), NONE),
            (own_candidate(m), ONE_EVALUATION),
            (own_candidate(last_5m), NONE),
        ],
    );
    let entries = every_10s(1);
    let dominated = |w: &[f64], twin_edge_costed: bool| {
        let (selectable, mut costs) = selection_inputs(&physical, &entries, w, &[], &[]);
        for (position, &SelectableEdge { edge, .. }) in selectable.edges().iter().enumerate() {
            if physical.edge(edge).producer_candidate_id.0 == 1 && !twin_edge_costed {
                costs.per_edge[position] = None;
            }
        }
        let inputs = SelectionInputs {
            physical: &physical,
            entries: &entries,
            selectable: &selectable,
            costs: &costs,
        };
        let candidates: Vec<usize> = selectable
            .executions()
            .iter()
            .zip(dominated_by_one_evaluation_twin(&inputs))
            .filter(|(_, dominated)| *dominated)
            .map(|(execution, _)| execution.candidate.0)
            .collect();
        let solved = solve_selection(&inputs).unwrap();
        (candidates, self::candidates(&solved))
    };
    assert_eq!(dominated(&[1.0, 1.0, 1.0], true), (vec![0], vec![1, 2]));
    assert_eq!(dominated(&[1.0, 2.0, 1.0], true), (vec![], vec![0, 2]));
    assert_eq!(dominated(&[1.0, 1.0, 1.0], false), (vec![], vec![0, 2]));
}

/// The same query `m[5m]` every 10 s and every 20 s: two evaluation
/// contexts with the same executions and edges, each cost in the 20 s one
/// half the 10 s one's (half the runs). The 20 s context is a copy of the
/// 10 s one ([`copies_in_equivalent_contexts`]): its executions, edges and
/// query share their variables, and the optimum reads the scan's
/// `OneEvaluation` candidate in both (1.5 + 0 against 1 + 2), costing
/// 2.5 + 1.25. With one cost of the 20 s context not half, they are not
/// copies; nor are they when the queries' accuracy targets differ (rank
/// 0.01 every 10 s, rank 0.05 every 20 s), which the accuracy constraint
/// (K) reads per query.
#[test]
fn a_context_equivalent_to_another_shares_its_variables() {
    let mut memo = empty_memo();
    let m = scan(&mut memo);
    let last_5m = range(&mut memo, 300, m);
    query(&mut memo, last_5m, None);
    query(&mut memo, last_5m, None);
    let physical = physical(
        memo,
        &[
            (own_candidate(m), NONE),
            (own_candidate(m), ONE_EVALUATION),
            (own_candidate(last_5m), NONE),
        ],
    );
    let entries = [every(10_000), every(20_000)];
    let (selectable, mut costs) =
        selection_inputs(&physical, &entries, &[1.0, 1.5, 1.0], &[((0, 2), 2.0)], &[]);
    let executions = selectable.executions();
    let every_20s = |index: usize| {
        matches!(
            selectable.context(executions[index].context),
            asap_types::stages::ExecutionContext::Evaluation { grid } if grid.period_ms == 20_000
        )
    };
    for (index, cost) in costs.per_execution.iter_mut().enumerate() {
        if every_20s(index) {
            *cost = Ok(cost.clone().unwrap() / 2.0);
        }
    }
    for (position, edge) in selectable.edges().iter().enumerate() {
        if every_20s(edge.consumer) {
            costs.per_edge[position] = costs.per_edge[position].map(|cost| cost / 2.0);
        }
    }
    let kept = vec![true; executions.len()];
    let copies = |costs: &ExecutionCosts| {
        let inputs = SelectionInputs {
            physical: &physical,
            entries: &entries,
            selectable: &selectable,
            costs,
        };
        copies_in_equivalent_contexts(&inputs, &kept)
    };
    let SharedVariables {
        original_of_execution,
        original_of_edge,
        original_of_query,
    } = copies(&costs);
    for (index, &original) in original_of_execution.iter().enumerate() {
        match every_20s(index) {
            true => {
                assert!(!every_20s(original));
                assert_eq!(executions[original].candidate, executions[index].candidate);
            }
            false => assert_eq!(original, index),
        }
    }
    for (position, &original) in original_of_edge.iter().enumerate() {
        let edge = selectable.edges()[position];
        assert_eq!(original != position, every_20s(edge.consumer));
        assert_eq!(selectable.edges()[original].edge, edge.edge);
    }
    assert_eq!(original_of_query, vec![QueryId(0), QueryId(0)]);

    let solved = solve_selection(&SelectionInputs {
        physical: &physical,
        entries: &entries,
        selectable: &selectable,
        costs: &costs,
    })
    .unwrap();
    assert_eq!(candidates(&solved), vec![1, 1, 2, 2]);
    assert!((solved.cost_per_second_without_request_overhead - 3.75).abs() < 1e-9);
    assert_eq!(solved.variable_count, 3 + 2 + 1);

    let first_of_20s = (0..executions.len())
        .find(|&index| every_20s(index))
        .unwrap();
    let rank =
        |max_error| AccuracyTarget::max_error(max_error).accuracy_metric(AccuracyMetric::Rank);
    let mut differing_targets = entries.clone();
    differing_targets[0].requirements.accuracy = rank(0.01);
    differing_targets[1].requirements.accuracy = rank(0.05);
    let shared = copies_in_equivalent_contexts(
        &SelectionInputs {
            physical: &physical,
            entries: &differing_targets,
            selectable: &selectable,
            costs: &costs,
        },
        &kept,
    );
    assert_eq!(shared.original_of_query, vec![QueryId(0), QueryId(1)]);
    assert!(shared
        .original_of_execution
        .iter()
        .enumerate()
        .all(|(index, &original)| original == index));

    costs.per_execution[first_of_20s] = Ok(10.0);
    assert!(copies(&costs)
        .original_of_execution
        .iter()
        .enumerate()
        .all(|(index, &original)| original == index));
}

/// The same query `m[5m]` every 60 s and every 300 s, each cost in the
/// 300 s context a fifth of the 60 s one's, except the scan-to-range
/// edge: a difference of the range's cost through the scan and its own,
/// as RollupHeavy-8users' flows component computes it,
/// 2.811002367077395e-9 − 2.8110047226529567e-9 (−2.3556e-15) in the 60 s
/// context and 5.622004734154791e-10 − 5.622009445305915e-10 (−4.7112e-16)
/// in the 300 s one. The two differences are in the ratio 0.200000000026,
/// off by more than 1e-12 of themselves but not of the contexts' costs,
/// so the contexts are copies.
#[test]
fn contexts_whose_small_edge_costs_differ_by_rounding_are_copies() {
    let mut memo = empty_memo();
    let m = scan(&mut memo);
    let last_5m = range(&mut memo, 300, m);
    query(&mut memo, last_5m, None);
    query(&mut memo, last_5m, None);
    let physical = physical(
        memo,
        &[
            (own_candidate(m), NONE),
            (own_candidate(m), ONE_EVALUATION),
            (own_candidate(last_5m), NONE),
        ],
    );
    let entries = [every(60_000), every(300_000)];
    let (selectable, mut costs) = selection_inputs(&physical, &entries, &[1.0, 1.5, 1.0], &[], &[]);
    let executions = selectable.executions();
    let every_300s = |index: usize| {
        matches!(
            selectable.context(executions[index].context),
            asap_types::stages::ExecutionContext::Evaluation { grid } if grid.period_ms == 300_000
        )
    };
    for (index, cost) in costs.per_execution.iter_mut().enumerate() {
        if every_300s(index) {
            *cost = Ok(cost.clone().unwrap() / 5.0);
        }
    }
    let scan_to_range = edge(&physical, 0, 2);
    for (position, selectable_edge) in selectable.edges().iter().enumerate() {
        if selectable_edge.edge == scan_to_range {
            costs.per_edge[position] = Some(match every_300s(selectable_edge.consumer) {
                true => 5.622004734154791e-10 - 5.622009445305915e-10,
                false => 2.811002367077395e-9 - 2.8110047226529567e-9,
            });
        }
    }
    let shared = copies_in_equivalent_contexts(
        &SelectionInputs {
            physical: &physical,
            entries: &entries,
            selectable: &selectable,
            costs: &costs,
        },
        &vec![true; executions.len()],
    );
    assert_eq!(shared.original_of_query, vec![QueryId(0), QueryId(0)]);
    for (index, &original) in shared.original_of_execution.iter().enumerate() {
        assert_eq!(original != index, every_300s(index));
    }
}

/// The same query `m[5m]` every 60 s and every 300 s over a scan with
/// ℓ = None (s0) or `OneEvaluation` (s1), read by the range (r) or by a
/// `OneEvaluation` range costing 1e12 (2e11 every 300 s), which the bound
/// leaves out. Every 60 s, r, s0 and s1 cost 1, 1 and 1.1; every 300 s
/// 0.2, 0.3 and 0.22, and the edge s1 → r 1e-3 (so s1 does not dominate
/// s0). The 300 s costs are not one multiple of the 60 s ones (s0 would
/// be 0.2), so the contexts are not copies, and the optimum reads s0
/// every 60 s and s1 every 300 s: 2 + 0.421 = 2.421. Compared by every
/// cost, the left-out range's 2e11 sets the tolerance to 0.2, the
/// contexts share their variables, and the optimum is 2.5.
#[test]
fn a_left_out_execution_does_not_set_the_tolerance_of_context_copies() {
    let mut memo = empty_memo();
    let m = scan(&mut memo);
    let last_5m = range(&mut memo, 300, m);
    query(&mut memo, last_5m, None);
    query(&mut memo, last_5m, None);
    let physical = physical(
        memo,
        &[
            (own_candidate(m), NONE),
            (own_candidate(m), ONE_EVALUATION),
            (own_candidate(last_5m), NONE),
            (own_candidate(last_5m), ONE_EVALUATION),
        ],
    );
    let entries = [every(60_000), every(300_000)];
    let (selectable, mut costs) =
        selection_inputs(&physical, &entries, &[1.0, 1.1, 1.0, 1e12], &[], &[]);
    let executions = selectable.executions();
    let every_300s = |index: usize| {
        matches!(
            selectable.context(executions[index].context),
            asap_types::stages::ExecutionContext::Evaluation { grid } if grid.period_ms == 300_000
        )
    };
    for (index, cost) in costs.per_execution.iter_mut().enumerate() {
        if every_300s(index) {
            *cost = Ok([0.3, 0.22, 0.2, 2e11][executions[index].candidate.0]);
        }
    }
    let s1_to_range = edge(&physical, 1, 2);
    for (position, selectable_edge) in selectable.edges().iter().enumerate() {
        if selectable_edge.edge == s1_to_range && every_300s(selectable_edge.consumer) {
            costs.per_edge[position] = Some(1e-3);
        }
    }
    let solved = solve_selection(&SelectionInputs {
        physical: &physical,
        entries: &entries,
        selectable: &selectable,
        costs: &costs,
    })
    .unwrap();
    assert!(
        (solved.cost_per_second_without_request_overhead - 2.421).abs() < 1e-9,
        "{}",
        solved.cost_per_second_without_request_overhead
    );
    assert_eq!(candidates(&solved), vec![0, 1, 2, 2]);
}

// Constraint (K).

/// Quantile `q` over a DDSketch state of `m[1m]` (one build per listed α):
/// the readout group, one readout per relative-error requirement of
/// `levels` (ε, δ).
fn ddsketch_readouts(
    memo: &mut Memo,
    q: f64,
    levels: &[(f64, f64)],
) -> (MemoGroupId, Vec<CandidateForMemoGroupId>) {
    let m = scan(memo);
    let window = range(memo, 60, m);
    let state = sketch_state(
        memo,
        window,
        SketchAlgorithm::DDSketch,
        Reduction::PerEntity,
    );
    let operator = memo_fixtures::readout(SketchStatistic::Quantile { q }, state);
    let group = memo.find_or_add_group(operator.clone()).unwrap();
    let readouts = levels
        .iter()
        .map(|&(max_error, failure_probability)| {
            let mut readout = CandidateForMemoGroup::new(
                operator.clone(),
                None,
                OriginOfCandidateForMemoGroup::Pass1,
                Vec::new(),
            );
            readout.input_requirements = vec![LogicalRequirements {
                required_accuracy: Some(RequiredAccuracy {
                    metric: AccuracyMetric::Relative,
                    max_error,
                    failure_probability,
                }),
            }];
            memo.add_candidate(group, readout).unwrap()
        })
        .collect();
    (group, readouts)
}

/// Every candidate of `memo` at query time, `OneEvaluation`.
fn every_candidate(memo: &Memo) -> Vec<(CandidateForMemoGroupId, Lifetime)> {
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
                    ONE_EVALUATION,
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

fn relative(max_error: f64) -> AccuracyTarget {
    AccuracyTarget::max_error(max_error).accuracy_metric(AccuracyMetric::Relative)
}

/// #1207's query B, p99 / p50 at relative 0.03 (concepts doc, section 4,
/// "Example: allocation in a ratio"), over readouts required at relative
/// 0.02 (cost 1) or 0.01 (cost 3) each: 1.111 (0.02 + 0.02) = 0.044 and
/// 1.111 (0.02 + 0.01) = 0.033 are above 0.03, so (K) selects both 0.01
/// readouts (0.022). The greedy plan reads the two 0.02 readouts (cost 2)
/// and misses the target, so it bounds nothing: as a bound it would leave
/// out each 0.01 readout (cost 3 > 2). The same ratio every 10 s and every
/// 20 s is one context's copy, whose (K) has no rows of its own and
/// selects alike.
#[test]
fn k_allocates_a_ratio_s_error_among_its_readouts() {
    for entries in [vec![every(10_000)], vec![every(10_000), every(20_000)]] {
        let mut memo = empty_memo();
        let levels = [(0.02, 1.0), (0.01, 1.0)];
        let (p99, p99_readouts) = ddsketch_readouts(&mut memo, 0.99, &levels);
        let (p50, p50_readouts) = ddsketch_readouts(&mut memo, 0.5, &levels);
        let ratio = group(&mut memo, arithmetic(ArithmeticOpKind::Div, p99, p50));
        for _ in &entries {
            query(&mut memo, ratio, Some(relative(0.03)));
        }
        let physical = physical(memo.clone(), &every_candidate(&memo));
        let mut w = vec![0.0; physical.physical_candidates.len()];
        for readouts in [&p99_readouts, &p50_readouts] {
            w[physical_of(&physical, readouts[0])] = 1.0;
            w[physical_of(&physical, readouts[1])] = 3.0;
        }
        let (selectable, mut costs) = selection_inputs(&physical, &entries, &w, &[], &[]);
        // Each cost in the 20 s context is half the 10 s one's.
        for (index, cost) in costs.per_execution.iter_mut().enumerate() {
            if let asap_types::stages::ExecutionContext::Evaluation { grid } =
                selectable.context(selectable.executions()[index].context)
            {
                if grid.period_ms == 20_000 {
                    *cost = Ok(cost.clone().unwrap() / 2.0);
                }
            }
        }
        let inputs = SelectionInputs {
            physical: &physical,
            entries: &entries,
            selectable: &selectable,
            costs: &costs,
        };
        let solved = solve_selection(&inputs).unwrap();
        let selected = candidates(&solved);
        for readouts in [&p99_readouts, &p50_readouts] {
            let chosen = physical_of(&physical, readouts[1]);
            let not_chosen = physical_of(&physical, readouts[0]);
            assert_eq!(
                selected.iter().filter(|&&v| v == chosen).count(),
                entries.len(),
                "{selected:?}"
            );
            assert!(!selected.contains(&not_chosen), "{selected:?}");
        }
        // (K)'s E and D exist only in the 10 s context (the 20 s one is
        // its copy), and only for the ratio: a readout's error is fixed.
        let kept = vec![true; selectable.executions().len()];
        let shared = copies_in_equivalent_contexts(&inputs, &kept);
        let mut problem = ProblemVariables::new();
        let variables = selection_variables(
            &inputs,
            &kept,
            &shared,
            &Components::of(&inputs),
            &mut problem,
        );
        let rules = AccuracyRules::of(&inputs);
        let model = ErrorModel::new(&inputs, rules.as_ref(), &variables.y, &shared);
        let accuracy = AccuracyVariables::new(&model, &mut problem);
        let one_context = 2;
        assert_eq!(accuracy.variable_count(), one_context, "{entries:?}");
    }
}

/// A query reading one readout group of quantile 0.5 whose readouts have
/// `levels`, rooted at `root` over it, with `target` and $n_q$
/// `values_per_evaluation`: whether some plan meets (K).
fn plans(
    target: Option<AccuracyTarget>,
    levels: &[(f64, f64)],
    values_per_evaluation: Option<u64>,
    root: fn(MemoGroupId) -> QueryRoot<MemoGroupId>,
) -> bool {
    let mut memo = empty_memo();
    let (readout_group, _) = ddsketch_readouts(&mut memo, 0.5, levels);
    memo.add_query(MemoQuery {
        root: root(readout_group),
        accuracy_target: target,
        number_of_values_per_query_evaluation: values_per_evaluation,
    });
    let physical = physical(memo.clone(), &every_candidate(&memo));
    let w = vec![1.0; physical.physical_candidates.len()];
    match solve(&physical, &every_10s(1), &w, &[], &[]).0 {
        Ok(_) => true,
        Err(SelectionMilpError::Infeasible) => false,
        Err(other) => panic!("{other:?}"),
    }
}

/// The root accepts error only in the target's metric: a readout off by
/// relative 0.01 meets relative 0.01, not relative 0.005 nor rank 0.01; a
/// target naming no metric accepts any.
#[test]
fn k_accepts_error_only_in_the_target_s_metric() {
    let level = [(0.01, 1.0)];
    let operator = QueryRoot::Operator;
    assert!(plans(Some(relative(0.01)), &level, Some(1), operator));
    assert!(!plans(Some(relative(0.005)), &level, Some(1), operator));
    let rank = AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Rank);
    assert!(!plans(Some(rank), &level, Some(1), operator));
    let any = AccuracyTarget::max_error(0.01);
    assert!(plans(Some(any), &level, Some(1), operator));
}

/// A whole-result failure probability 0.01 over 10 values is 1e-3 per
/// value, which a readout failing with probability 1e-3 meets; over 100
/// values (1e-4 per value) it does not. With the count unknown no
/// per-value δ follows, and (K) has no row for it (Pass 2 gives such a
/// query's readouts no level).
#[test]
fn k_divides_a_whole_result_failure_probability_over_the_values() {
    let level = [(0.01, 1e-3)];
    let target = relative(0.01).failure_probability(0.01);
    let operator = QueryRoot::Operator;
    assert!(plans(Some(target.clone()), &level, Some(10), operator));
    assert!(!plans(Some(target.clone()), &level, Some(100), operator));
    assert!(plans(Some(target), &level, None, operator));
}

/// `scalar(v)` returns the value it reads, so its root accepts the
/// target's metric; `scalar(v) * 2`'s operand is read exactly, as no error
/// contract covers the scalar expression; without a target either reads
/// the readout.
#[test]
fn a_scalar_root_accepts_the_target_s_metric_only_where_it_returns_its_operand() {
    let level = [(0.01, 1.0)];
    let scalar = |group| QueryRoot::Scalar(ScalarExpr::PromqlScalarFromVector(group));
    let doubled = |group| {
        QueryRoot::Scalar(ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Mul,
            left: Box::new(ScalarExpr::PromqlScalarFromVector(group)),
            right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(2.0))),
            semantics: ExprSemantics::Promql,
        })
    };
    assert!(plans(Some(relative(0.01)), &level, Some(1), scalar));
    assert!(!plans(Some(relative(0.01)), &level, Some(1), doubled));
    assert!(plans(None, &level, Some(1), doubled));
}

/// A ratio's linear bound holds while its denominator is off by at most
/// 0.1 (`ErrorContract::linear_bound_holds_up_to_per_input`): a
/// denominator readout at relative 0.2 cannot be read under any target,
/// one at 0.05 can.
#[test]
fn k_caps_a_ratio_s_denominator_at_its_validity_cap() {
    let ratio_plans = |denominator_error: f64| {
        let mut memo = empty_memo();
        let (p99, _) = ddsketch_readouts(&mut memo, 0.99, &[(0.01, 1.0)]);
        let (p50, _) = ddsketch_readouts(&mut memo, 0.5, &[(denominator_error, 1.0)]);
        let ratio = group(&mut memo, arithmetic(ArithmeticOpKind::Div, p99, p50));
        query(&mut memo, ratio, Some(relative(1.0)));
        let physical = physical(memo.clone(), &every_candidate(&memo));
        let w = vec![1.0; physical.physical_candidates.len()];
        match solve(&physical, &every_10s(1), &w, &[], &[]).0 {
            Ok(_) => true,
            Err(SelectionMilpError::Infeasible) => false,
            Err(other) => panic!("{other:?}"),
        }
    };
    assert!(ratio_plans(0.05));
    assert!(!ratio_plans(0.2));
}

/// The same readout every 10 s and every 20 s at relative 0.01 with δ =
/// 0.01 over the whole result, returning 10 values every 10 s and 100
/// every 20 s: the readout failing with 1e-3 (cost 1) meets 1e-3 per
/// value, not 1e-4, where the one failing with 1e-5 (cost 2) is needed.
/// The contexts differ in what (K) reads ($n_q$), so the 20 s one is not a
/// copy: each selects its own readout.
#[test]
fn contexts_whose_queries_split_their_failure_probability_differently_are_not_copies() {
    let mut memo = empty_memo();
    let (readout_group, readouts) =
        ddsketch_readouts(&mut memo, 0.5, &[(0.01, 1e-3), (0.01, 1e-5)]);
    for values in [10, 100] {
        memo.add_query(MemoQuery {
            root: QueryRoot::Operator(readout_group),
            accuracy_target: Some(relative(0.01).failure_probability(0.01)),
            number_of_values_per_query_evaluation: Some(values),
        });
    }
    let physical = physical(memo.clone(), &every_candidate(&memo));
    // The scan costs 10, so neither readout is dearer than a feasible plan.
    let mut w = vec![0.0; physical.physical_candidates.len()];
    w[0] = 10.0;
    w[physical_of(&physical, readouts[0])] = 1.0;
    w[physical_of(&physical, readouts[1])] = 2.0;
    let entries = [every(10_000), every(20_000)];
    let (selectable, mut costs) = selection_inputs(&physical, &entries, &w, &[], &[]);
    for (index, cost) in costs.per_execution.iter_mut().enumerate() {
        if let asap_types::stages::ExecutionContext::Evaluation { grid } =
            selectable.context(selectable.executions()[index].context)
        {
            if grid.period_ms == 20_000 {
                *cost = Ok(cost.clone().unwrap() / 2.0);
            }
        }
    }
    let solved = solve_selection(&SelectionInputs {
        physical: &physical,
        entries: &entries,
        selectable: &selectable,
        costs: &costs,
    })
    .unwrap();
    let selected = candidates(&solved);
    assert!(
        selected.contains(&physical_of(&physical, readouts[0])),
        "{selected:?}"
    );
    assert!(
        selected.contains(&physical_of(&physical, readouts[1])),
        "{selected:?}"
    );
}

/// The same readout group as `q` every 10 s and as `scalar(q) * 2` every
/// 20 s, both at relative 0.01, with only a readout at relative 0.01: the
/// operator root accepts it, the scalar expression's operand only exact
/// values (no error contract covers it). The contexts differ in the
/// metrics their outputs allow, so the 20 s one is not a copy whose root
/// rows (K) could leave out: no plan meets both.
#[test]
fn contexts_whose_roots_allow_different_metrics_are_not_copies() {
    let mut memo = empty_memo();
    let (readout_group, _) = ddsketch_readouts(&mut memo, 0.5, &[(0.01, 1.0)]);
    let doubled = QueryRoot::Scalar(ScalarExpr::Arithmetic {
        op: ArithmeticOpKind::Mul,
        left: Box::new(ScalarExpr::PromqlScalarFromVector(readout_group)),
        right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(2.0))),
        semantics: ExprSemantics::Promql,
    });
    for root in [QueryRoot::Operator(readout_group), doubled] {
        memo.add_query(MemoQuery {
            root,
            accuracy_target: Some(relative(0.01)),
            number_of_values_per_query_evaluation: Some(1),
        });
    }
    let physical = physical(memo.clone(), &every_candidate(&memo));
    let w = vec![1.0; physical.physical_candidates.len()];
    let (solved, _) = solve(&physical, &[every(10_000), every(20_000)], &w, &[], &[]);
    assert_eq!(solved.unwrap_err(), SelectionMilpError::Infeasible);
}
