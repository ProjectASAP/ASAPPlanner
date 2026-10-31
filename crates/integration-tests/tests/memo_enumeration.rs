//! The memo the production planner builds, measured on the planner-layering
//! doc's example (`avg by (job) (quantile_over_time(0.5, data[5m]) /
//! quantile_over_time(0.5, data[5m]) + quantile_over_time(0.9, data[5m]))`,
//! the stage-pipeline workload
//! `tools/dag-viewer/memo-examples/promql/ratio-of-quantiles-5m.json`).
//!
//! Lowering is production's: the PromQL frontend, then each root with the
//! series-identity column, as `Stage1ToStage3Pass` does. The memo is read from
//! the production run's `PlanningRun::per_computation_candidates` (after
//! Pass 1) and `PlanningRun::memo_steps` (the other steps).
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
//! (`accuracy_budget_parts` 1) and the plans `compose_logical_candidate` builds from
//! the Pass 1 inventory's assignments are not equal sets:
//! - A whole-workload plan is a memo DAG exactly when targets that compute
//!   the same thing take the same choice and it takes no KLL. With series
//!   identity the two `quantile 0.5` are one memo group, which takes one
//!   candidate; the inventory keeps a target for each and may give them
//!   different summaries. The inventory sizes a KLL per value; Pass 1 sizes
//!   every memo KLL for the whole result (a failure probability below the
//!   query's, divided over the source's series), so no KLL plan is in both.
//! - A whole-budget memo DAG is a whole-workload plan exactly when it takes
//!   no KLL sized for the whole result, that is, no KLL.
//!
//! The memo's other choices size an operand's summary for a share of the
//! budget (a ratio's operands); the whole-workload path has no such plans.
use asap_logical_asap_optimization::generate_dag_of_logical_memo_groups;
use std::rc::Rc;

use asap_devtools::planning_stages::lower_promql_roots;
use asap_integration_tests::planning::expanded_memo;
use asap_logical_asap_optimization::test_support::{
    compose_logical_candidate, generate_per_computation_candidates, PerComputationCandidates,
};
use asap_logical_asap_optimization::{
    build_logical_asap_dags_from_chosen_candidates, enumerate, ChosenCandidatePerMemoGroup,
};
use asap_planner::{run_planning, MemoSteps, PlanSearchSettings, PlanningModels, PlanningRun};
use asap_types::ir::schema::{FieldDataType, SketchParams};
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};
use asap_types::stages::{queries_rooted_at, MemoGroupId};
use asap_types::stages::{CandidateForMemoGroup, MemoCandidates, OriginOfCandidateForMemoGroup};
use asap_types::workload::{PlanningWorkload, Query, RepeatedDemand, RepetitionInterval};

const RATIO_OF_QUANTILES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tools/dag-viewer/memo-examples/promql/ratio-of-quantiles-5m.json"
);

/// The example's failure probability, as in its workload file.
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
/// `Stage1ToStage3Pass` and `stage_pipeline` lower a PromQL workload.
fn production_roots(workload: &PlanningWorkload) -> Vec<QueryRoot> {
    lower_promql_roots(workload).unwrap()
}

/// The production run of Stages 1 to 3 over `workload`.
fn production_run(workload: &PlanningWorkload, search: PlanSearchSettings) -> PlanningRun {
    production_run_with(workload, search, PlanningModels::builtin())
}

/// [`production_run`] with `models`, keeping every stage's memo.
fn production_run_with(
    workload: &PlanningWorkload,
    search: PlanSearchSettings,
    models: PlanningModels<'_>,
) -> PlanningRun {
    let search = PlanSearchSettings {
        inspect: true,
        ..search
    };
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    let roots = production_roots(workload);
    run_planning(roots, &entries, &data, models, search).unwrap()
}

/// A deployment model that may accept an output whatever its guarantee, so
/// Pass 1 prunes no summary for its target, and accepts none: Stage 3
/// validates no plan, and the run records its last round's memo.
struct AcceptsNoOutput;

impl asap_accuracy::AccuracyModel for AcceptsNoOutput {
    fn satisfies(
        &self,
        _guarantee: &asap_types::ir::properties::AccuracyGuarantee,
        _target: &asap_types::ir::properties::AccuracyTarget,
    ) -> bool {
        false
    }
}

/// Rounds up to the complete pane-width domain, with a time limit a slow
/// machine's Pass 2 does not reach. Stage 3 may use the whole budget: when
/// it validates no plan, it searches until the deadline; the memo steps are
/// recorded either way.
fn complete_domain_round() -> PlanSearchSettings {
    PlanSearchSettings {
        time_limit_ms: Some(90_000),
        ..PlanSearchSettings::default()
    }
}

fn memo_steps(run: &PlanningRun) -> &MemoSteps {
    run.memo_steps
        .as_ref()
        .expect("a planning round recorded its memo within the time budget")
}

/// The memo after Pass 1.
fn pass1_memo(run: &PlanningRun) -> &MemoCandidates {
    &run.per_computation_candidates
        .as_ref()
        .expect("Pass 1's output")
        .memo
}

/// Pass 1's target list for `workload`, pruned as the facade's Pass 1
/// prunes it under `models`.
fn pass1_inventory(
    workload: &PlanningWorkload,
    models: &PlanningModels<'_>,
) -> PerComputationCandidates {
    let roots = production_roots(workload);
    let inventory =
        generate_per_computation_candidates(&generate_dag_of_logical_memo_groups(&roots))
            .unwrap()
            .without_unsupported_summaries(
                models
                    .deployment_capabilities
                    .supported_summaries
                    .as_deref(),
            );
    let accuracy = models.accuracy_check_inputs();
    if accuracy.model.may_accept_regardless_of_guarantee()
        || accuracy.evidence.may_provide_evidence()
    {
        return inventory;
    }
    let targets: Vec<_> = workload
        .query_workload
        .entries()
        .map(|entry| entry.requirements.accuracy.clone())
        .collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    inventory.without_summaries_no_query_can_use(&targets, &data, accuracy.model)
}

fn candidate_count(memo: &MemoCandidates) -> usize {
    memo.candidates.iter().map(Vec::len).sum()
}

/// Every plan the whole-workload path composes from the inventory: one per
/// assignment of each target's choices, with the assignment.
fn whole_workload_plans(inventory: &PerComputationCandidates) -> Vec<(Vec<usize>, Plan)> {
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
                .map(|root| match root {
                    QueryRoot::Operator(node) => node,
                    QueryRoot::Scalar(_) => panic!("operator roots only"),
                })
                .collect();
            Some((choice, plan))
        })
        .collect()
}

/// `choice`'s DAG, one root per query.
fn composed(memo: &MemoCandidates, choice: &ChosenCandidatePerMemoGroup) -> Plan {
    let mut roots = build_logical_asap_dags_from_chosen_candidates(memo, choice)
        .expect("an enumerated choice composes");
    roots.sort_by_key(|(group, _)| {
        queries_rooted_at(&memo.query_roots, *group)
            .first()
            .copied()
    });
    roots.into_iter().map(|(_, root)| root).collect()
}

fn chosen<'a>(
    memo: &'a MemoCandidates,
    choice: &'a ChosenCandidatePerMemoGroup,
) -> impl Iterator<Item = &'a CandidateForMemoGroup> {
    choice
        .iter()
        .map(|(group, &index)| &memo.candidates[group.0 as usize][index])
}

/// The KLL parameters of the summaries `candidate` builds.
fn kll(candidate: &CandidateForMemoGroup) -> Vec<(u32, Option<f64>)> {
    OperatorNode::reachable(&candidate.root_node)
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

/// A KLL Pass 1 sized for the whole result, at the query's failure
/// probability divided over the source's series (1e-7 here), far below what
/// a per-value KLL is sized for (the query's, or half of it).
fn sized_for_whole_result(candidate: &CandidateForMemoGroup) -> bool {
    kll(candidate)
        .iter()
        .any(|(_, p)| p.is_some_and(|p| p < FAILURE_PROBABILITY / 10.0))
}

/// Checks the relation of the module doc between the Pass 1 memo's
/// whole-budget DAGs and the whole-workload plans; returns (memo choices,
/// whole-budget ones, whole-workload plans, plans in both).
fn assert_pass1_memo_and_whole_workload_plans(
    run: &PlanningRun,
    inventory: &PerComputationCandidates,
) -> (usize, usize, usize, usize) {
    let memo = pass1_memo(run);
    let enumeration = enumerate(memo, 100_000);
    assert!(enumeration.complete);
    let plans: Vec<_> = enumeration
        .all_chosen_candidates
        .iter()
        .map(|choice| composed(memo, choice))
        .collect();
    for (i, plan) in plans.iter().enumerate() {
        assert!(!plans[..i].contains(plan), "each choice is a different DAG");
    }
    let whole_budget: Vec<_> = enumeration
        .all_chosen_candidates
        .iter()
        .zip(&plans)
        .filter(|(choice, _)| chosen(memo, choice).all(|c| c.accuracy_budget_parts == 1))
        .collect();
    let whole = whole_workload_plans(inventory);
    // Targets that compute the same thing.
    let same: Vec<(usize, usize)> = (0..inventory.targets.len())
        .flat_map(|a| (a + 1..inventory.targets.len()).map(move |b| (a, b)))
        .filter(|&(a, b)| {
            let (a, b) = (&inventory.targets[a], &inventory.targets[b]);
            inventory.aggregate(a) == inventory.aggregate(b) && a.measure_index == b.measure_index
        })
        .collect();
    let takes_kll = |plan: &Plan| {
        plan.iter().flat_map(OperatorNode::reachable).any(|node| {
            matches!(node.asap(), Some(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _), ..
            }) if matches!(kind.params(), SketchParams::Kll { .. }))
        })
    };
    for (assignment, plan) in &whole {
        assert_eq!(
            whole_budget.iter().any(|(_, p)| *p == plan),
            same.iter().all(|&(a, b)| assignment[a] == assignment[b]) && !takes_kll(plan),
            "a whole-workload plan is a memo DAG when equal targets choose alike \
             and it takes no (per-value) KLL"
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

/// The example's memo after each production step, and the `accuracy_budget_parts`
/// of the state-configuration rule's rewrites (one test: one planning run).
#[test]
fn ratio_of_quantiles_production_memo() {
    // With the built-in model, Pass 1 prunes every summary of the quantile
    // groups: no query has a guarantee through `/` and `+` over a KLL rank
    // error or a DDSketch relative one. The memo below is planned with a
    // model that prunes none and accepts no output, so its summaries stay
    // and its last round, over the complete pane-width domain, is recorded.
    let enforced = production_run(&ratio_of_quantiles(), PlanSearchSettings::default());
    assert_eq!(candidate_count(pass1_memo(&enforced)), 7);
    let models = PlanningModels::builtin().with_accuracy(&AcceptsNoOutput);
    let run = production_run_with(&ratio_of_quantiles(), complete_domain_round(), models);
    let steps = memo_steps(&run);
    // Stage 0 (Pass 1's groups): series identity makes the two
    // `quantile 0.5` one group.
    // Pass 1: the two quantile groups have 5 candidates each, 3 for the
    // whole budget (exact, DDSketch, KLL sized for the whole result) and 2
    // for half of it; the other 5 groups one each.
    assert_eq!(pass1_memo(&run).groups.len(), 7);
    assert_eq!(candidate_count(pass1_memo(&run)), 15);
    // 5 × 5 choices, 3 × 3 of them for the whole budget. The inventory has
    // 3 × 3 assignments (one target per quantile group and measure, so the
    // two `quantile 0.5` share one, 3 choices each): the 2 × 2 that take no
    // KLL are memo DAGs.
    assert_eq!(
        assert_pass1_memo_and_whole_workload_plans(
            &run,
            &pass1_inventory(&ratio_of_quantiles(), &models)
        ),
        (25, 9, 9, 4)
    );
    // The last step, over the complete pane-width domain.
    let last = &steps.concrete_states;
    assert_eq!(last.groups.len(), 255);
    assert_eq!(candidate_count(last), 1243);
    let enumeration = enumerate(last, 2_000);
    assert!(!enumeration.complete);
    assert_eq!(enumeration.all_chosen_candidates.len(), 2_000);
    for (group, candidates) in last.candidates.iter().enumerate() {
        for index in 0..candidates.len() {
            let choice = [(MemoGroupId(group as u32), index)].into();
            build_logical_asap_dags_from_chosen_candidates(last, &choice)
                .unwrap_or_else(|e| panic!("G{group} candidate {index}: {e}"));
        }
    }
    // The state-configuration rule rewrites a candidate's KLL to the size
    // the readers of a state share; the rewrite is a copy of the candidate
    // (`derive` in `pass2/memo/rules/shared_configuration_readers.rs`) with
    // the `accuracy_budget_parts` its new size was made for. In each quantile
    // group the `accuracy_budget_parts` 1 KLL `k` 567 (sized for the whole
    // result) gets copies at `k` 567 (parts 1) and 1184 (parts 2, the size of
    // the half-budget KLL); the `accuracy_budget_parts` 2 KLL `k` 1184 keeps
    // 2. The `k` 1184 copy composes the same DAG as the original
    // `accuracy_budget_parts` 2 candidate: one sketch, sized for half the
    // budget.
    let pass1 = pass1_memo(&run);
    let quantile_groups: Vec<_> = (0..pass1.groups.len())
        .filter(|&group| pass1.candidates[group].len() == 5)
        .collect();
    assert_eq!(quantile_groups.len(), 2);
    for group in quantile_groups {
        let sizes = |candidates: &[CandidateForMemoGroup]| {
            candidates
                .iter()
                .filter(|c| c.origin == OriginOfCandidateForMemoGroup::Pass1)
                .filter_map(|c| kll(c).first().map(|&(k, _)| (k, c.accuracy_budget_parts)))
                .collect::<Vec<_>>()
        };
        assert_eq!(sizes(&pass1.candidates[group]), [(567, 1), (1184, 2)]);
        let candidates = &last.candidates[group];
        assert_eq!(
            sizes(candidates),
            [(567, 1), (1184, 2), (567, 1), (1184, 2)]
        );
        let at_k: Vec<usize> = candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.origin == OriginOfCandidateForMemoGroup::Pass1
                    && kll(c).first().is_some_and(|&(k, _)| k == 1184)
            })
            .map(|(index, _)| index)
            .collect();
        assert_eq!(at_k.len(), 2);
        let alone = |index: usize| composed(last, &[(MemoGroupId(group as u32), index)].into());
        assert_eq!(alone(at_k[0]), alone(at_k[1]));
    }
}

/// On the three-query workload, the same relation: the memo's 3 × 3
/// whole-budget DAGs and the inventory's 3 × 3 assignments share the 2 × 2
/// that take no KLL (the memo's is sized for the whole result, the
/// inventory's per value). Pass 1 is the same in every round, so the
/// default search will do.
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
    let run = production_run(&workload, PlanSearchSettings::default());
    assert_eq!(
        assert_pass1_memo_and_whole_workload_plans(
            &run,
            &pass1_inventory(&workload, &PlanningModels::builtin())
        ),
        (9, 9, 9, 4)
    );
}

/// Pass 2 rejects a memo whose groups read each other, a group reading
/// itself included, so production's memo (coarse and fine quantiles, whose
/// rollups read the finer grouping) has a dependency order and every choice
/// `enumerate` returns composes. A cycle made by hand, a child candidate
/// reading back the group that reads it, composes no DAG:
/// `build_logical_asap_dags_from_chosen_candidates` says so. The memo is
/// built from production's roots on the seed grids, without the search.
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
    let memo = expanded_memo(production_roots(&workload), &entries);
    assert!(memo.topological_order().is_some());
    for (group, candidates) in memo.candidates.iter().enumerate() {
        for candidate in candidates {
            assert!(!candidate.input_groups.contains(&MemoGroupId(group as u32)));
        }
    }
    let enumeration = enumerate(&memo, 2_000);
    assert!(!enumeration.all_chosen_candidates.is_empty());
    for choice in &enumeration.all_chosen_candidates {
        build_logical_asap_dags_from_chosen_candidates(&memo, choice).unwrap();
    }

    let (group, index, child) = (0..memo.groups.len())
        .flat_map(|group| (0..memo.candidates[group].len()).map(move |index| (group, index)))
        .find_map(|(group, index)| {
            memo.candidates[group][index]
                .input_groups
                .first()
                .map(|&child| (MemoGroupId(group as u32), index, child))
        })
        .expect("a candidate reading another group");
    let mut cyclic = memo.clone();
    let mut back = cyclic.candidates[child.0 as usize][0].clone();
    back.root_node = Rc::clone(&cyclic.groups[group.0 as usize].logical_node);
    back.input_groups = vec![group];
    cyclic.candidates[child.0 as usize].push(back);
    let back_index = cyclic.candidates[child.0 as usize].len() - 1;
    assert_eq!(cyclic.topological_order(), None);
    assert!(matches!(
        build_logical_asap_dags_from_chosen_candidates(
            &cyclic,
            &[(group, index), (child, back_index)].into()
        ),
        Err(asap_logical_asap_optimization::ErrorBuildingLogicalASAPDagsFromChosenCandidates::Cycle { .. })
    ));
}
