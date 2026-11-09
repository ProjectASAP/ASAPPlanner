//! The baseline MILP over small hand-built candidate graphs with given
//! costs: which candidates and edges its optimum selects, and the plan
//! built from it.

use std::collections::HashMap;
use std::time::Duration;

use asap_types::ir::operator::TimeShift;
use asap_types::ir::{NonASAPOp, Operator, QueryRoot, TimeRangeKind};
use asap_types::physical::{ExecutionTiming, Lifetime, PhysicalProperties, StorageMedium};
use asap_types::stages::{
    CandidateForMemoGroupId, LogicalASAPMemoCandidates, Memo, MemoGroupId, MemoQuery,
    PhysicalASAPMemoCandidates, PhysicalCandidate, PhysicalCandidateId, PhysicalEdge,
    PhysicalEdgeId,
};
use asap_types::test_support::{
    add_candidate, group_with_own_candidate, series_schema, time_series_scan,
};

use super::*;
use crate::cost::SelectionCosts;
use crate::plan::plan_of;

const NONE: Lifetime = Lifetime::None;
const ONE_EVALUATION: Lifetime = Lifetime::OneEvaluation(StorageMedium::Memory);

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

/// Costs `w` per candidate and `edge_costs` per (producer, consumer), 0 for
/// every other edge.
fn costs(
    physical: &PhysicalASAPMemoCandidates,
    w: &[f64],
    edge_costs: &[((usize, usize), f64)],
) -> SelectionCosts {
    let mut edge_cost: HashMap<PhysicalEdgeId, f64> =
        physical.edges().map(|(id, _)| (id, 0.0)).collect();
    for &((u, c), cost) in edge_costs {
        edge_cost.insert(edge(physical, u, c), cost);
    }
    SelectionCosts {
        candidate: w.iter().map(|&w| Ok(w)).collect(),
        edge: edge_cost,
    }
}

/// Each query output reference of `physical`'s memo with every candidate of
/// its group.
fn outputs(physical: &PhysicalASAPMemoCandidates) -> Vec<(QueryId, Vec<PhysicalCandidateId>)> {
    physical
        .logical_candidates
        .memo
        .queries
        .iter()
        .enumerate()
        .map(|(query, memo_query)| {
            let QueryRoot::Operator(group) = memo_query.root else {
                unreachable!("operator roots only")
            };
            let candidates = (0..physical.physical_candidates.len())
                .filter(|&v| physical.physical_candidates[v].logical_candidate_id.group == group)
                .map(PhysicalCandidateId)
                .collect();
            (QueryId(query), candidates)
        })
        .collect()
}

fn solve(
    physical: &PhysicalASAPMemoCandidates,
    costs: &SelectionCosts,
) -> Result<SolvedSelection, SelectionMilpError> {
    solve_selection(&SelectionInputs {
        physical,
        costs,
        query_outputs: &outputs(physical),
    })
}

fn ids(ids: &[usize]) -> Vec<PhysicalCandidateId> {
    ids.iter().copied().map(PhysicalCandidateId).collect()
}

/// `m[5m]`, its scan with two physical candidates. Through v0 the read
/// costs 1 + 5, through v1 2 + 0: the optimum takes v1, and the plan built
/// from it is the scan v1 below the range v2, the scan retaining the 5
/// minutes the range reads.
#[test]
fn selects_the_producer_whose_own_and_edge_costs_are_lowest() {
    let mut memo = Memo::default();
    let m = scan(&mut memo);
    let last_5m = range(&mut memo, 300, m);
    query(&mut memo, last_5m);
    let first = |group: MemoGroupId| CandidateForMemoGroupId {
        group,
        index_in_group: 0,
    };
    let physical = physical(
        memo,
        &[
            (first(m), ONE_EVALUATION),
            (first(m), NONE),
            (first(last_5m), NONE),
        ],
    );
    let costs = costs(&physical, &[1.0, 2.0, 1.0], &[((0, 2), 5.0)]);
    let solved = solve(&physical, &costs).unwrap();
    assert_eq!(solved.candidates, ids(&[1, 2]));
    assert_eq!(solved.edges, vec![edge(&physical, 1, 2)]);
    assert_eq!(solved.outputs, ids(&[2]));

    let outputs = vec![(QueryId(0), last_5m)];
    let plan = plan_of(&physical, &[], &solved, &outputs).unwrap();
    assert_eq!(plan.roots, vec![1]);
    assert_eq!(plan.edges.len(), 1);
    assert_eq!((plan.edges[0].producer, plan.edges[0].consumer), (0, 1));
    let candidates: Vec<_> = plan
        .nodes
        .iter()
        .map(|node| node.selected_physical_candidate)
        .collect();
    assert_eq!(
        candidates,
        vec![Some(PhysicalCandidateId(1)), Some(PhysicalCandidateId(2))]
    );
    assert_eq!(plan.nodes[0].retention_ms, Some(300_000));
    assert_eq!(plan.nodes[1].retention_ms, None);
}

/// `m[5m]` and `m[1m]` both read the scan. Its candidate that is not
/// materialized (cost 1) may have one consumer, so the optimum reads the
/// scan's `OneEvaluation` candidate (cost 10) twice rather than both.
#[test]
fn a_query_time_result_that_is_not_materialized_has_one_consumer() {
    let mut memo = Memo::default();
    let m = scan(&mut memo);
    let last_5m = range(&mut memo, 300, m);
    let last_1m = range(&mut memo, 60, m);
    query(&mut memo, last_5m);
    query(&mut memo, last_1m);
    let first = |group: MemoGroupId| CandidateForMemoGroupId {
        group,
        index_in_group: 0,
    };
    let physical = physical(
        memo,
        &[
            (first(m), NONE),
            (first(last_5m), NONE),
            (first(last_1m), NONE),
            (first(m), ONE_EVALUATION),
        ],
    );
    let with_one_evaluation = costs(&physical, &[1.0, 1.0, 1.0, 10.0], &[]);
    let solved = solve(&physical, &with_one_evaluation).unwrap();
    assert_eq!(solved.candidates, ids(&[1, 2, 3]));

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
    let only_unshared = costs(&without_one_evaluation, &[0.0, 1.0, 1.0], &[]);
    assert_eq!(
        solve(&without_one_evaluation, &only_unshared).unwrap_err(),
        SelectionMilpError::Infeasible
    );
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
    let first = |group: MemoGroupId| CandidateForMemoGroupId {
        group,
        index_in_group: 0,
    };
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
    let costs = costs(&physical, &[0.0, 10.0, 10.0, 1.0, 1.0], &[]);
    let solved = solve(&physical, &costs).unwrap();
    assert!(
        solved.candidates == ids(&[0, 1, 4]) || solved.candidates == ids(&[0, 2, 3]),
        "{:?}",
        solved.candidates
    );
    let outputs = vec![(QueryId(0), a), (QueryId(1), b)];
    plan_of(&physical, &[], &solved, &outputs).unwrap();
}
