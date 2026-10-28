//! The memo the production planner builds, measured on the planner-layering
//! doc's example (`avg by (job) (quantile_over_time(0.5, data[5m]) /
//! quantile_over_time(0.5, data[5m]) + quantile_over_time(0.9, data[5m]))`,
//! the stage-pipeline workload
//! `tools/dag-viewer/memo-examples/promql/ratio-of-quantiles-5m.json`).
//!
//! Lowering is production's: the PromQL frontend, then each root with the
//! series-identity column, as `Stage1ToStage3Pass` does. The memo is read from
//! the production run's `Stage1ToStage3Run::memo_steps`.
//!
//! **Which round's memo.** The default search plans in rounds of growing
//! pane-width domains and records the memo of the round whose plan it
//! selected, so its last step depends on the time budget (the seed-grid
//! round when a debug build runs out of it; round 5 of the release run that
//! made the doc's document, 354 groups and 1738 candidates). The ratio test
//! runs one round over the complete pane-width domain (a search with fewer
//! than 4 evaluations has no progressive rounds), so its last step is the
//! same on every machine. Stage 0 and Pass 1 are the same in every round.
//!
//! **The memo and the whole-workload plans.** After Pass 1 the memo's
//! choices whose candidates are all sized for the whole accuracy budget
//! (`budget_share` 1) and the plans `compose_logical_candidate` builds from
//! the Pass 1 inventory's assignments are not equal sets:
//! - A whole-workload plan is a memo DAG exactly when targets that compute
//!   the same thing take the same choice. With series identity the two
//!   `quantile 0.5` are one memo group, which takes one candidate; the
//!   inventory keeps a target for each and may give them different
//!   summaries.
//! - A whole-budget memo DAG is a whole-workload plan exactly when it takes
//!   no KLL sized for the whole result (Pass 1's accuracy-scoped candidates,
//!   sized for a failure probability below the query's, which the inventory
//!   does not offer).
//!
//! The memo's other choices size an operand's summary for a share of the
//! budget (a ratio's operands); the whole-workload path has no such plans.
use std::rc::Rc;

use asap_integration_tests::planning::expanded_memo;
use asap_logical_asap_optimization::memo::candidates::{
    CandidateMemo, CandidateOrigin, ImplementationCandidate,
};
use asap_logical_asap_optimization::memo::enumeration::{compose, enumerate, MemoChoice};
use asap_logical_asap_optimization::memo::MemoGroupId;
use asap_logical_asap_optimization::per_computation_candidates::{
    compose_logical_candidate, PerComputationCandidates,
};
use asap_plan_selection::{
    run_stage1_to_stage3_with, PlanningModels, Stage1ToStage3Run, Stage1ToStage3Search,
};
use asap_types::ir::schema::schema_support::with_promql_series_identity;
use asap_types::ir::schema::{FieldDataType, SketchParams};
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};
use asap_types::workload::{PlanningWorkload, Query, RepeatedDemand, RepetitionInterval};

const RATIO_OF_QUANTILES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tools/dag-viewer/memo-examples/promql/ratio-of-quantiles-5m.json"
);

/// The example's failure probability.
const FAILURE_PROBABILITY: f64 = 0.001;

/// One root per query, in query order.
type Plan = Vec<Rc<OperatorNode>>;

fn ratio_of_quantiles() -> PlanningWorkload {
    serde_json::from_str(&std::fs::read_to_string(RATIO_OF_QUANTILES).unwrap()).unwrap()
}

/// The example's workload with `queries` in place of its query, each with
/// its requirements, repeating every `interval_ms`.
fn with_queries(queries: &[&str], interval_ms: u32) -> PlanningWorkload {
    let mut workload = ratio_of_quantiles();
    let repeating = workload.query_workload.repeating_queries.as_mut().unwrap();
    let mut entry = repeating[0].clone();
    entry.demand = RepeatedDemand::FixedInterval(RepetitionInterval(interval_ms));
    *repeating = queries
        .iter()
        .map(|query| {
            let mut entry = entry.clone();
            entry.query = Query((*query).into());
            entry
        })
        .collect();
    workload
}

/// The frontend's roots, each with the series-identity column, as
/// `Stage1ToStage3Pass` lowers a PromQL workload.
fn production_roots(workload: &PlanningWorkload) -> Vec<(usize, QueryRoot)> {
    asap_frontend_promql::lower_promql_query_workload(workload)
        .unwrap()
        .into_iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => {
                QueryRoot::Operator(with_promql_series_identity(&node).unwrap())
            }
            scalar => scalar,
        })
        .enumerate()
        .collect()
}

/// The production run of Stages 1 to 3 over `workload`.
fn production_run(
    workload: &PlanningWorkload,
    search: Stage1ToStage3Search,
) -> Stage1ToStage3Run<usize> {
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    let roots = production_roots(workload);
    run_stage1_to_stage3_with(roots, &entries, &data, PlanningModels::builtin(), search).unwrap()
}

/// One round over the complete pane-width domain, without a time limit that
/// a slow machine could reach.
fn complete_domain_round() -> Stage1ToStage3Search {
    Stage1ToStage3Search {
        max_evaluations: Some(1),
        milp_time_limit_ms: Some(3_600_000),
        ..Stage1ToStage3Search::default()
    }
}

fn candidate_count(memo: &CandidateMemo) -> usize {
    memo.candidates.iter().map(Vec::len).sum()
}

/// Every plan the whole-workload path composes from the inventory: one per
/// assignment of each target's choices, with the assignment.
fn whole_workload_plans(inventory: &PerComputationCandidates<usize>) -> Vec<(Vec<usize>, Plan)> {
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
        .filter_map(|choice| {
            let roots = compose_logical_candidate(inventory, &choice).ok()?;
            let plan = roots
                .into_iter()
                .map(|(_, root)| match root {
                    QueryRoot::Operator(node) => node,
                    QueryRoot::Scalar(_) => panic!("operator roots only"),
                })
                .collect();
            Some((choice, plan))
        })
        .collect()
}

/// `choice`'s DAG, one root per query.
fn composed(memo: &CandidateMemo, choice: &MemoChoice) -> Plan {
    let mut roots = compose(memo, choice).expect("an enumerated choice composes");
    roots.sort_by_key(|(group, _)| memo.groups[group.0 as usize].roots_of.iter().min().copied());
    roots.into_iter().map(|(_, root)| root).collect()
}

fn chosen<'a>(
    memo: &'a CandidateMemo,
    choice: &'a MemoChoice,
) -> impl Iterator<Item = &'a ImplementationCandidate> {
    choice
        .iter()
        .map(|(group, &index)| &memo.candidates[group.0 as usize][index])
}

/// The KLL parameters of the summaries `candidate` builds.
fn kll(candidate: &ImplementationCandidate) -> Vec<(u32, Option<f64>)> {
    OperatorNode::reachable(&candidate.node)
        .iter()
        .filter_map(|node| match node.asap() {
            Some(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _),
                ..
            }) => match kind.params() {
                SketchParams::Kll {
                    k,
                    failure_probability,
                } => Some((*k, *failure_probability)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Pass 1's accuracy-scoped candidate: a KLL sized for the whole result, at
/// a failure probability below the query's.
fn sized_for_whole_result(candidate: &ImplementationCandidate) -> bool {
    kll(candidate)
        .iter()
        .any(|(_, p)| p.is_some_and(|p| p < FAILURE_PROBABILITY / candidate.budget_share as f64))
}

/// Checks the relation of the module doc between the Pass 1 memo's
/// whole-budget DAGs and the whole-workload plans; returns (memo choices,
/// whole-budget ones, whole-workload plans, plans in both).
fn assert_pass1_memo_and_whole_workload_plans(
    run: &Stage1ToStage3Run<usize>,
) -> (usize, usize, usize, usize) {
    let memo = &run.memo_steps.as_ref().unwrap().pass1;
    let enumeration = enumerate(memo, 100_000);
    assert!(enumeration.complete);
    let plans: Vec<_> = enumeration
        .choices
        .iter()
        .map(|choice| composed(memo, choice))
        .collect();
    for (i, plan) in plans.iter().enumerate() {
        assert!(!plans[..i].contains(plan), "each choice is a different DAG");
    }
    let whole_budget: Vec<_> = enumeration
        .choices
        .iter()
        .zip(&plans)
        .filter(|(choice, _)| chosen(memo, choice).all(|c| c.budget_share == 1))
        .collect();
    let inventory = &run.stage1_pass1_per_computation;
    let whole = whole_workload_plans(inventory);
    // Targets that compute the same thing.
    let same: Vec<(usize, usize)> = (0..inventory.targets.len())
        .flat_map(|a| (a + 1..inventory.targets.len()).map(move |b| (a, b)))
        .filter(|&(a, b)| {
            let (a, b) = (&inventory.targets[a], &inventory.targets[b]);
            a.aggregate == b.aggregate && a.measure == b.measure
        })
        .collect();
    for (assignment, plan) in &whole {
        assert_eq!(
            whole_budget.iter().any(|(_, p)| *p == plan),
            same.iter().all(|&(a, b)| assignment[a] == assignment[b]),
            "a whole-workload plan is a memo DAG when equal targets choose alike"
        );
        assert!(
            !plans.iter().any(|p| p == plan) || whole_budget.iter().any(|(_, p)| *p == plan),
            "a whole-workload plan is never a budget-share DAG"
        );
    }
    for (choice, plan) in &whole_budget {
        assert_eq!(
            whole.iter().any(|(_, p)| p == *plan),
            !chosen(memo, choice).any(sized_for_whole_result),
            "a whole-budget memo DAG is a whole-workload plan unless it takes a \
             KLL sized for the whole result"
        );
    }
    let both = whole
        .iter()
        .filter(|(_, plan)| whole_budget.iter().any(|(_, p)| p == &plan))
        .count();
    (plans.len(), whole_budget.len(), whole.len(), both)
}

#[test]
fn ratio_of_quantiles_memo_after_each_production_step() {
    let run = production_run(&ratio_of_quantiles(), complete_domain_round());
    let steps = run.memo_steps.as_ref().unwrap();
    // Stage 0: series identity makes the two `quantile 0.5` one group.
    assert_eq!(steps.stage0.groups.len(), 7);
    assert_eq!(candidate_count(&steps.stage0), 0);
    // Pass 1: the two quantile groups have 7 candidates each, 4 for the
    // whole budget (exact, KLL per value, DDSketch, KLL for the whole
    // result) and 3 for half of it; the other 5 groups one each.
    assert_eq!(steps.pass1.groups.len(), 7);
    assert_eq!(candidate_count(&steps.pass1), 19);
    // 7 × 7 choices, 4 × 4 of them for the whole budget. The inventory has
    // 3 × 3 × 3 assignments (a target per `quantile 0.5`, 3 choices each):
    // the 3 × 3 that give both `quantile 0.5` the same choice are memo DAGs.
    assert_eq!(
        assert_pass1_memo_and_whole_workload_plans(&run),
        (49, 16, 27, 9)
    );
    // The last step, over the complete pane-width domain.
    let last = &steps.concrete_states;
    assert_eq!(last.groups.len(), 438);
    assert_eq!(candidate_count(last), 2158);
    let enumeration = enumerate(last, 2_000);
    assert!(!enumeration.complete);
    assert_eq!(enumeration.choices.len(), 2_000);
    for (group, candidates) in last.candidates.iter().enumerate() {
        for index in 0..candidates.len() {
            let choice = [(MemoGroupId(group as u32), index)].into();
            compose(last, &choice).unwrap_or_else(|e| panic!("G{group} candidate {index}: {e}"));
        }
    }
}

/// The state-size rewrite resizes a candidate's KLL to the size readers of
/// that state share, keeping its `budget_share`. In each quantile group the
/// `budget_share` 1 KLL `k` 567 (sized for the whole result) is rewritten to
/// `k` 1156 and to `k` 1184, the size of the group's `budget_share` 2 KLL.
/// That rewrite has `budget_share` 1 and composes the same DAG as the
/// `budget_share` 2 candidate, which keeps 2: one sketch, sized for half the
/// budget, which also meets the whole budget. The label is sound, not a
/// share-2 candidate relabelled.
#[test]
fn state_size_rewrite_keeps_the_budget_share_of_the_candidate_it_resizes() {
    let run = production_run(&ratio_of_quantiles(), complete_domain_round());
    let steps = run.memo_steps.as_ref().unwrap();
    let (pass1, last) = (&steps.pass1, &steps.concrete_states);
    let quantile_groups: Vec<_> = (0..pass1.groups.len())
        .filter(|&group| pass1.candidates[group].len() == 7)
        .collect();
    assert_eq!(quantile_groups.len(), 2);
    for group in quantile_groups {
        let sizes = |candidates: &[ImplementationCandidate]| {
            candidates
                .iter()
                .filter(|c| c.origin == CandidateOrigin::Pass1)
                .filter_map(|c| kll(c).first().map(|&(k, _)| (k, c.budget_share)))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            sizes(&pass1.candidates[group]),
            [(345, 1), (746, 2), (567, 1), (1184, 2)]
        );
        let candidates = &last.candidates[group];
        assert_eq!(
            sizes(candidates),
            [
                (345, 1),
                (746, 2),
                (567, 1),
                (1184, 2),
                (567, 1),
                (1156, 1),
                (1184, 1)
            ]
        );
        let at = |k: u32, share: usize| {
            candidates
                .iter()
                .position(|c| {
                    c.origin == CandidateOrigin::Pass1
                        && c.budget_share == share
                        && kll(c).first().is_some_and(|&(kk, _)| kk == k)
                })
                .unwrap()
        };
        let alone = |index: usize| composed(last, &[(MemoGroupId(group as u32), index)].into());
        assert_eq!(alone(at(1184, 1)), alone(at(1184, 2)));
    }
}

/// On the three-query workload, the same relation: the inventory's 3 × 3
/// assignments are all memo DAGs; the memo's 4 × 4 whole-budget DAGs add
/// the KLL sized for the whole result. Pass 1 is the same in every round,
/// so the default search will do.
#[test]
fn shared_quantiles_pass1_memo_and_whole_workload_plans() {
    let workload = with_queries(
        &[
            "quantile_over_time(0.5, data[5m])",
            "quantile_over_time(0.9, data[5m])",
            "sum by (job) (rate(data[1m]))",
        ],
        60_000,
    );
    let run = production_run(&workload, Stage1ToStage3Search::default());
    assert_eq!(
        assert_pass1_memo_and_whole_workload_plans(&run),
        (16, 16, 9, 9)
    );
}

/// Candidates may read each other: a coarse quantile's rollup reads the finer
/// grouping, whose candidates may read the coarse one. A choice taking both
/// composes no DAG: `compose` says so, and `enumerate` leaves it out. The
/// memo is built from production's roots on the seed grids, without the
/// search: the pair, not the counts, is what this checks.
#[test]
fn choices_whose_candidates_read_each_other_are_left_out() {
    let workload = with_queries(
        &[
            "quantile_over_time(0.5, data[1m])",
            "quantile_over_time(0.9, data[5m])",
            "quantile by (host, job) (0.5, data)",
            "quantile by (job) (0.5, data)",
        ],
        10_000,
    );
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let memo = &expanded_memo(production_roots(&workload), &entries);
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
        compose(memo, &mutual.into()),
        Err(asap_logical_asap_optimization::memo::enumeration::ComposeError::Cycle { .. })
    ));
    let enumeration = enumerate(memo, 2_000);
    assert!(!enumeration.choices.is_empty());
    for choice in &enumeration.choices {
        compose(memo, choice).unwrap();
    }
}
