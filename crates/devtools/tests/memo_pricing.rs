//! Pricing a memo's enumerated choices: on small workloads every choice is
//! priced, and none that is valid is cheaper than the plan the search
//! selected.
use asap_devtools::memo_pricing::{price_choices, PricedChoice, SelectedBaseline};
use asap_devtools::planner_layering_examples::example1;
use asap_devtools::planning_stages::{lower_promql_roots, run_default_search_over};
use asap_logical_asap_optimization::deployment_capabilities::DeploymentCapabilities;
use asap_logical_asap_optimization::memo::candidates::{try_populate_pass1, CandidateMemo};
use asap_logical_asap_optimization::memo::enumeration::enumerate;
use asap_logical_asap_optimization::memo::LogicalMemo;
use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
use asap_plan_selection::{PlanSelectionSettings, PlanningModels};
use asap_planner::PlannerExecutorCompileCheck;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::QueryRoot;
use asap_types::workload::{
    DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload, QueryWorkloadEntry, RepeatedDemand, RepeatingEntry,
    RepetitionInterval,
};

const RATIO_OF_QUANTILES: &str = "avg by (job) (quantile_over_time(0.5, data[5m]) \
     / quantile_over_time(0.5, data[5m]) + quantile_over_time(0.9, data[5m]))";

/// One query evaluated every minute at max_error 0.01, failure probability
/// 0.001, over 1,000 series of `data`, each sampled every 15 s.
fn ratio_of_quantiles() -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![RepeatingEntry {
                query: Query(RATIO_OF_QUANTILES.into()),
                demand: RepeatedDemand::FixedInterval(RepetitionInterval(60_000)),
                requirements: QueryRequirements {
                    accuracy: AccuracyTarget::max_error(0.01).failure_probability(0.001),
                    max_latency_ms: None,
                },
                predictability: Predictability::Unknown,
                range_evaluation: None,
            }]),
        },
        data_workload: Some({
            let mut data = DataWorkload::sampled_every(["data".to_string()], DurationMs(15_000));
            for facts in data.sources.values_mut() {
                facts.series_count = Some(1_000);
            }
            data
        }),
    }
}

/// The workload's roots, entries and data, and its Pass 1 memo.
struct Planned {
    roots: Vec<QueryRoot>,
    entries: Vec<QueryWorkloadEntry>,
    data: DataWorkload,
    memo: CandidateMemo,
}

/// `stage_pipeline` lowers each series' identity as a column
/// (`lower_promql_roots`); without it (`series_identity` false, as the
/// memo-enumeration tests lower), the ratio's two `quantile 0.5` have no
/// provable unique key and stay separate groups.
fn planned(workload: &PlanningWorkload, series_identity: bool) -> Planned {
    let roots = if series_identity {
        lower_promql_roots(workload).unwrap()
    } else {
        asap_frontend_promql::lower_promql_query_workload(workload)
            .unwrap()
            .into_iter()
            .collect()
    };
    let indexed: Vec<_> = roots.iter().cloned().enumerate().collect();
    let initial = LogicalMemo::from_roots(&indexed);
    let inventory = generate_per_computation_candidates(indexed).unwrap();
    Planned {
        roots,
        entries: workload.query_workload.entries().collect(),
        data: workload.data_workload.clone().unwrap(),
        memo: try_populate_pass1(initial, &inventory).unwrap(),
    }
}

/// `stage_pipeline`'s models: the executor's summary families and compile
/// check, `settings` (accuracy guarantees ignored unless enforced).
fn models<'a>(
    settings: &'a PlanSelectionSettings,
    deployment: &'a DeploymentCapabilities,
) -> PlanningModels<'a> {
    PlanningModels::builtin()
        .with_plan_selection(settings)
        .with_deployment_capabilities(deployment)
        .with_executor_compile_check(&PlannerExecutorCompileCheck)
}

fn deployment() -> DeploymentCapabilities {
    DeploymentCapabilities {
        supported_summaries: Some(asap_executor::supported_summary_families()),
        ..DeploymentCapabilities::default()
    }
}

fn settings(enforce_accuracy_guarantees: bool) -> PlanSelectionSettings {
    PlanSelectionSettings {
        ignore_accuracy_guarantees: !enforce_accuracy_guarantees,
        ..PlanSelectionSettings::default()
    }
}

/// The default search's selection of `planned`'s workload, as
/// `stage_pipeline` runs it.
fn selected(planned: &Planned, models: PlanningModels<'_>) -> SelectedBaseline {
    let run = run_default_search_over(
        planned.roots.clone(),
        &planned.entries,
        &planned.data,
        models,
    )
    .unwrap();
    SelectedBaseline::of(run.stage3_plan_selection.as_ref().expect("a selection"))
}

/// Every choice of `planned`'s Pass 1 memo (the enumeration is complete),
/// priced against `baseline`.
fn price_every_choice(
    planned: &Planned,
    models: PlanningModels<'_>,
    baseline: SelectedBaseline,
) -> Vec<PricedChoice> {
    let enumeration = enumerate(&planned.memo, 100_000);
    assert!(enumeration.complete);
    price_choices(
        &planned.memo,
        &enumeration.choices,
        &planned.roots,
        &planned.entries,
        &planned.data,
        models,
        Some(baseline),
    )
}

fn assert_none_cheaper(priced: &[PricedChoice], baseline: SelectedBaseline) {
    let cheaper: Vec<_> = priced
        .iter()
        .enumerate()
        .filter(|(_, p)| p.cheaper_than_selected)
        .map(|(i, p)| (format!("E{}", i + 1), p.cost_per_second))
        .collect();
    assert!(
        cheaper.is_empty(),
        "selected {:?}; cheaper: {cheaper:?}",
        baseline.cost_per_second
    );
    assert!(priced.iter().any(|p| p.valid));
}

/// With accuracy guarantees ignored (`stage_pipeline`'s default) and
/// enforced, no Pass 1 choice of `workload` is cheaper than the plan the
/// default search selects; returns the choices priced each way.
fn assert_selection_is_cheapest(planned: &Planned) -> [Vec<PricedChoice>; 2] {
    let deployment = deployment();
    [false, true].map(|enforce| {
        let settings = settings(enforce);
        let models = models(&settings, &deployment);
        let baseline = selected(planned, models);
        let priced = price_every_choice(planned, models, baseline);
        assert_none_cheaper(&priced, baseline);
        priced
    })
}

/// Example 1's Pass 1 memo stands for 3 DAGs (Q2's three candidates); none
/// is cheaper than the selection.
#[test]
fn example1_no_pass1_choice_is_cheaper_than_the_selected_plan() {
    let [ignored, enforced] = assert_selection_is_cheapest(&planned(&example1(), true));
    assert_eq!(ignored.len(), 3);
    // Q2's summary reads miss its 100 ms latency bound or, enforced, have no
    // accuracy guarantee.
    assert!(ignored.iter().any(|p| !p.valid));
    assert_eq!(enforced.iter().filter(|p| p.valid).count(), 1);
}

/// As `stage_pipeline` lowers it (series identity), the ratio's two
/// `quantile 0.5` share one group: the Pass 1 memo stands for 5 × 5 DAGs,
/// and none is cheaper than the selection.
#[test]
fn ratio_of_quantiles_no_pass1_choice_is_cheaper_than_the_selected_plan() {
    let [ignored, _] = assert_selection_is_cheapest(&planned(&ratio_of_quantiles(), true));
    assert_eq!(ignored.len(), 25);
}

/// Lowered without series identity, the two `quantile 0.5` stay separate
/// groups: 7 × 7 × 7 = 343 DAGs, 27 of them sized for the whole budget. The
/// executor compiles none of their per-series summaries without the series
/// identity, so only the exact DAG is valid, and it is not cheaper than the
/// plan selected for the same query as `stage_pipeline` lowers it. (The
/// default search finds no plan for this lowering in its budget, so the
/// selection comes from the series-identity lowering.)
#[test]
fn ratio_of_quantiles_343_pass1_choices_none_cheaper_than_the_selected_plan() {
    let without_identity = planned(&ratio_of_quantiles(), false);
    let enumeration = enumerate(&without_identity.memo, 100_000);
    let whole_budget = enumeration
        .choices
        .iter()
        .filter(|choice| {
            choice.iter().all(|(group, &index)| {
                without_identity.memo.candidates[group.0 as usize][index].budget_share == 1
            })
        })
        .count();
    assert_eq!((enumeration.choices.len(), whole_budget), (343, 27));
    let with_identity = planned(&ratio_of_quantiles(), true);
    let deployment = deployment();
    for enforce in [false, true] {
        let settings = settings(enforce);
        let models = models(&settings, &deployment);
        let baseline = selected(&with_identity, models);
        let started = std::time::Instant::now();
        let priced = price_every_choice(&without_identity, models, baseline);
        eprintln!(
            "priced {} choices in {:.2?}",
            priced.len(),
            started.elapsed()
        );
        assert_none_cheaper(&priced, baseline);
        assert_eq!(priced.iter().filter(|p| p.valid).count(), 1);
    }
}

/// A selection one step worse than the cheapest valid choice (the
/// next-cheapest cost) makes exactly the cheaper choices
/// `cheaper_than_selected`.
#[test]
fn a_worse_selection_is_caught_by_cheaper_than_selected() {
    let planned = planned(&example1(), true);
    let (settings, deployment) = (settings(false), deployment());
    let models = models(&settings, &deployment);
    let choices = enumerate(&planned.memo, 100).choices;
    let price = |selected| {
        price_choices(
            &planned.memo,
            &choices,
            &planned.roots,
            &planned.entries,
            &planned.data,
            models,
            selected,
        )
    };
    let unpriced = price(None);
    assert!(unpriced.iter().all(|p| !p.cheaper_than_selected));
    let mut costs: Vec<f64> = unpriced.iter().filter_map(|p| p.cost_per_second).collect();
    costs.sort_by(f64::total_cmp);
    costs.dedup();
    assert!(costs.len() >= 2, "two valid choices of different cost");
    let worse = SelectedBaseline {
        cost_per_second: Some(costs[1]),
        calibration_factor: None,
    };
    let priced = price(Some(worse));
    for (before, after) in unpriced.iter().zip(&priced) {
        assert_eq!(before.cost_per_second, after.cost_per_second);
        assert_eq!(
            after.cheaper_than_selected,
            after.cost_per_second == Some(costs[0])
        );
    }
    assert!(priced.iter().any(|p| p.cheaper_than_selected));
}
