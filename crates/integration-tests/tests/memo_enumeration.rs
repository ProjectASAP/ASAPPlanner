//! The memo stands for the candidate DAGs the whole-workload path composes:
//! after Pass 1, the memo's choices whose candidates are all sized for the
//! whole accuracy budget (`budget_share` 1) compose exactly the plans
//! `compose_logical_candidate` builds from the assignments of the Pass 1
//! inventory. The memo's other choices size an operand's summary for a share
//! of the budget (a ratio's operands); the whole-workload path has no such
//! plans. (No query here splits its budget among its inputs in the
//! whole-workload path; one that did would match the choices at its share.)
use std::rc::Rc;

use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::expanded_memo;
use asap_logical_asap_optimization::memo::candidates::{try_populate_pass1, CandidateMemo};
use asap_logical_asap_optimization::memo::enumeration::{compose, enumerate};
use asap_logical_asap_optimization::memo::{LogicalMemo, MemoGroupId};
use asap_logical_asap_optimization::per_computation_candidates::{
    compose_logical_candidate, generate_per_computation_candidates, PerComputationCandidates,
};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::workload::{
    QueryRecurrence, QueryRequirements, QueryWorkloadEntry, RepeatedDemand, RepetitionInterval,
};

const RATIO_OF_QUANTILES: &str = "avg by (job) (quantile_over_time(0.5, data[5m]) \
     / quantile_over_time(0.5, data[5m]) + quantile_over_time(0.9, data[5m]))";

/// One root per query.
type Plan = Vec<Rc<OperatorNode>>;

fn approximate() -> AccuracyTarget {
    AccuracyTarget::max_error(0.01).failure_probability(0.001)
}

fn roots(queries: &[&str]) -> Vec<(usize, QueryRoot)> {
    queries
        .iter()
        .enumerate()
        .map(|(i, query)| {
            (
                i,
                QueryRoot::Operator(lower_promql(query, approximate()).unwrap()),
            )
        })
        .collect()
}

fn pass1_memo(roots: Vec<(usize, QueryRoot)>) -> (CandidateMemo, PerComputationCandidates<usize>) {
    let initial = LogicalMemo::from_roots(&roots);
    let inventory = generate_per_computation_candidates(roots).unwrap();
    (try_populate_pass1(initial, &inventory).unwrap(), inventory)
}

/// Every plan the whole-workload path composes from the inventory: one per
/// assignment of each target's choices.
fn whole_workload_plans(inventory: &PerComputationCandidates<usize>) -> Vec<Plan> {
    let counts: Vec<_> = inventory.targets.iter().map(|t| t.choice_count()).collect();
    let total: usize = counts.iter().product();
    (0..total)
        .map(|mut n| {
            counts
                .iter()
                .map(|&count| {
                    let digit = n % count;
                    n /= count;
                    digit
                })
                .collect::<Vec<_>>()
        })
        .filter(|choice| !inventory.absorbed_choice_varies(choice))
        .filter_map(|choice| compose_logical_candidate(inventory, &choice).ok())
        .map(|roots| {
            roots
                .into_iter()
                .map(|(_, root)| match root {
                    QueryRoot::Operator(node) => node,
                    QueryRoot::Scalar(_) => panic!("operator roots only"),
                })
                .collect()
        })
        .collect()
}

/// Each enumerated choice's DAG, one root per query, and whether every
/// candidate it chose is sized for the whole budget.
fn memo_plans(memo: &CandidateMemo, limit: usize) -> (Vec<(Plan, bool)>, bool) {
    let enumeration = enumerate(memo, limit);
    let plans = enumeration
        .choices
        .iter()
        .map(|choice| {
            let roots = compose(memo, choice)
                .expect("an enumerated choice composes")
                .into_iter()
                .map(|(_, root)| root)
                .collect();
            let whole_budget = choice
                .iter()
                .all(|(group, &index)| memo.candidates[group.0 as usize][index].budget_share == 1);
            (roots, whole_budget)
        })
        .collect();
    (plans, enumeration.complete)
}

fn distinct(plans: &[&Plan]) -> usize {
    let mut seen: Vec<&Plan> = Vec::new();
    for &plan in plans {
        if !seen.contains(&plan) {
            seen.push(plan);
        }
    }
    seen.len()
}

/// The memo's DAGs: (sized for the whole budget, sized for a share of it).
fn assert_pass1_memo_stands_for_whole_workload_plans(queries: &[&str]) -> (usize, usize) {
    let (memo, inventory) = pass1_memo(roots(queries));
    let (plans, complete) = memo_plans(&memo, 100_000);
    assert!(complete);
    let all: Vec<_> = plans.iter().map(|(roots, _)| roots).collect();
    assert_eq!(distinct(&all), all.len(), "each choice is a different DAG");
    let whole_budget: Vec<_> = plans
        .iter()
        .filter(|(_, whole)| *whole)
        .map(|(r, _)| r)
        .collect();
    let whole = whole_workload_plans(&inventory);
    assert_eq!(whole_budget.len(), whole.len());
    for plan in &whole {
        assert!(
            whole_budget.contains(&plan),
            "the memo stands for every whole-workload plan"
        );
    }
    for (roots, whole_budget) in &plans {
        assert_eq!(whole.contains(roots), *whole_budget);
    }
    (whole.len(), plans.len() - whole.len())
}

#[test]
fn ratio_of_quantiles_pass1_memo_stands_for_343_plans() {
    let (memo, _) = pass1_memo(roots(&[RATIO_OF_QUANTILES]));
    assert_eq!(memo.groups.len(), 8);
    assert_eq!(enumerate(&memo, 1000).choices.len(), 343);
    // Each quantile: exact, or one of two summary families sized for the
    // whole budget or a half or third of it, 7 candidates; 3 × 3 × 3 of the
    // 7 × 7 × 7 choices use the whole budget only.
    assert_eq!(
        assert_pass1_memo_stands_for_whole_workload_plans(&[RATIO_OF_QUANTILES]),
        (27, 316)
    );
}

#[test]
fn shared_quantiles_pass1_memo_stands_for_whole_workload_plans() {
    assert_pass1_memo_stands_for_whole_workload_plans(&[
        "quantile_over_time(0.5, data[5m])",
        "quantile_over_time(0.9, data[5m])",
        "sum by (job) (rate(data[1m]))",
    ]);
}

/// After Pass 2 and the concrete state groups the memo stands for too many
/// DAGs to list: the limit stops the enumeration. Depth-first, the first
/// choices vary only the last groups popped, so every candidate is also
/// composed once, each in the choice that names only it.
#[test]
fn expanded_memo_enumeration_stops_at_the_limit_and_every_candidate_composes() {
    let entry = QueryWorkloadEntry {
        query: Default::default(),
        requirements: QueryRequirements {
            accuracy: approximate(),
            max_latency_ms: None,
        },
        predictability: Default::default(),
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            60_000,
        ))),
        range_evaluation: None,
    };
    let memo = expanded_memo(roots(&[RATIO_OF_QUANTILES]), &[entry]);
    assert_eq!(memo.groups.len(), 378);
    assert_eq!(memo.candidates.iter().map(Vec::len).sum::<usize>(), 2614);
    let (plans, complete) = memo_plans(&memo, 2_000);
    assert!(!complete);
    assert_eq!(plans.len(), 2_000);
    for (group, candidates) in memo.candidates.iter().enumerate() {
        for index in 0..candidates.len() {
            let choice = [(MemoGroupId(group as u32), index)].into();
            compose(&memo, &choice).unwrap_or_else(|e| panic!("G{group} candidate {index}: {e}"));
        }
    }
}

/// Candidates may read each other: a coarse quantile's rollup reads the finer
/// grouping, whose candidates may read the coarse one. A choice taking both
/// composes no DAG: `compose` says so, and `enumerate` leaves it out.
#[test]
fn choices_whose_candidates_read_each_other_are_left_out() {
    let queries = [
        "quantile_over_time(0.5, data[1m])",
        "quantile_over_time(0.9, data[5m])",
        "quantile by (host, job) (0.5, data)",
        "quantile by (job) (0.5, data)",
    ];
    let entry = QueryWorkloadEntry {
        query: Default::default(),
        requirements: QueryRequirements {
            accuracy: approximate(),
            max_latency_ms: None,
        },
        predictability: Default::default(),
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            10_000,
        ))),
        range_evaluation: None,
    };
    let memo = expanded_memo(roots(&queries), &vec![entry; queries.len()]);
    let mutual = (0..memo.groups.len())
        .flat_map(|group| (0..memo.candidates[group].len()).map(move |index| (group, index)))
        .find_map(|(group, index)| {
            memo.candidates[group][index]
                .children
                .iter()
                .find_map(|&child| {
                    memo.candidates[child.0 as usize]
                        .iter()
                        .position(|c| c.children.contains(&MemoGroupId(group as u32)))
                        .map(|back| (child, back))
                })
                .map(|(child, back)| [(MemoGroupId(group as u32), index), (child, back)])
        })
        .expect("a pair of groups whose candidates read each other");
    assert!(matches!(
        compose(&memo, &mutual.into()),
        Err(asap_logical_asap_optimization::memo::enumeration::ComposeError::Cycle { .. })
    ));
    let enumeration = enumerate(&memo, 2_000);
    assert!(!enumeration.choices.is_empty());
    for choice in &enumeration.choices {
        compose(&memo, choice).unwrap();
    }
}
