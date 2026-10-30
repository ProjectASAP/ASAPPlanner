//! Pricing a memo's enumerated choices: on small workloads every choice is
//! priced, and none that is valid is cheaper than the plan the search
//! selected.
use asap_cost::CostInputs;
use asap_devtools::memo_pricing::{price_choices, PricedChoice, SelectedBaseline};
use asap_devtools::planner_layering_examples::example1;
use asap_devtools::planning_stages::{lower_promql_roots, run_default_search_over};
use asap_logical_asap_optimization::deployment_capabilities::DeploymentCapabilities;
use asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates;
use asap_logical_asap_optimization::pass2::memo::candidates::try_populate_pass1;
use asap_logical_asap_optimization::pass2::memo::enumeration::enumerate;
use asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups;
use asap_planner::PlanningModels;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::QueryRoot;
use asap_types::stages::CandidateMemo;
use asap_types::workload::{
    DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload, QueryWorkloadEntry, RepeatedDemand, RepeatingEntry,
    RepetitionInterval,
};
use serde_json::{json, Value};
use std::process::Command;

/// The issue's workload file: the query every minute, ε = 0.01, δ = 0.001.
const RATIO_OF_QUANTILES_WORKLOAD: &str =
    "../../tools/dag-viewer/memo-examples/promql/ratio-of-quantiles-5m.json";

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
/// provable unique key, but they are still one group.
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
    let initial = DagOfLogicalMemoGroups::from_roots(&indexed);
    let inventory = generate_per_computation_candidates(
        &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(&indexed),
    )
    .unwrap();
    Planned {
        roots,
        entries: workload.query_workload.entries().collect(),
        data: workload.data_workload.clone().unwrap(),
        memo: try_populate_pass1(initial, &inventory).unwrap(),
    }
}

/// `stage_pipeline`'s models: the executor's summary families,
/// `settings` (accuracy guarantees ignored unless enforced).
fn models<'a>(
    settings: &'a CostInputs,
    deployment: &'a DeploymentCapabilities,
) -> PlanningModels<'a> {
    PlanningModels::builtin()
        .with_plan_selection(settings)
        .with_deployment_capabilities(deployment)
}

fn deployment() -> DeploymentCapabilities {
    DeploymentCapabilities {
        supported_summaries: Some(asap_executor::supported_summary_families()),
        ..DeploymentCapabilities::default()
    }
}

fn settings(enforce_accuracy_guarantees: bool) -> CostInputs {
    CostInputs {
        ignore_accuracy_guarantees: !enforce_accuracy_guarantees,
        ..CostInputs::default()
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
    SelectedBaseline::of(run.run.selection.as_ref().expect("a selection"))
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
        &enumeration.all_chosen_candidates,
        &planned.roots,
        &planned.entries,
        &planned.data,
        models,
        Some(baseline),
    )
}

/// `stage_pipeline --price-enumerated 200` on `args`' workload: the listed
/// choices of the enumeration's step, each priced.
fn priced_document(args: &[&str]) -> Value {
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let out = std::env::temp_dir().join(format!(
        "stage_pipeline_{}_{call}_priced.json",
        std::process::id()
    ));
    let status = Command::new(env!("CARGO_BIN_EXE_stage_pipeline"))
        .args(args)
        .args(["--price-enumerated", "200", "--out"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    let document = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    std::fs::remove_file(out).unwrap();
    document
}

/// Every listed choice is priced; the enumeration of its step is complete,
/// and no valid choice is cheaper than the selected plan, whose cost is
/// the one Stage 3 reports. Returns the choices.
fn assert_no_listed_choice_is_cheaper(document: &Value) -> Vec<Value> {
    let enumeration = &document["memo"]["enumeration"];
    let step = enumeration["step"].as_str().unwrap();
    assert_eq!(enumeration["per_step"][step]["complete"], true);
    let choices = enumeration["choices"].as_array().unwrap().clone();
    assert_eq!(
        enumeration["per_step"][step]["count"].as_u64().unwrap(),
        choices.len() as u64
    );
    let stage3 = &document["stage3_plan_selection"];
    let selected = &stage3["costs"][stage3["selected"].as_str().unwrap()]["cost_per_second"];
    assert_eq!(
        enumeration["priced"],
        json!({"count": choices.len(), "cheaper_than_selected": 0, "selected_cost_per_second": selected})
    );
    for choice in &choices {
        assert_eq!(choice["cheaper_than_selected"], false, "{}", choice["id"]);
        if choice["valid"] == true {
            assert!(choice["cost_per_second"].as_f64().unwrap() >= selected.as_f64().unwrap());
            assert_eq!(choice["rejection"], Value::Null);
        } else {
            assert!(choice["rejection"].is_string(), "{}", choice["id"]);
        }
    }
    assert!(choices.iter().any(|c| c["valid"] == true));
    choices
}

/// #509 Example 1: its Pass 1 memo (the listed step) stands for 3 DAGs
/// (Q2's top-k group: exact, Count-Min with a heap and Count Sketch with a
/// heap);
/// Q2's summary reads miss its 100 ms latency bound, and no valid DAG is
/// cheaper than the selection.
#[test]
fn example1_no_listed_choice_is_cheaper_than_the_selected_plan() {
    let document = priced_document(&["--example", "planner-layering-1"]);
    let choices = assert_no_listed_choice_is_cheaper(&document);
    assert_eq!(document["memo"]["enumeration"]["step"], "pass1");
    assert_eq!(choices.len(), 3);
    assert!(choices
        .iter()
        .any(|c| c["rejection"] == "latency_bound_exceeded"));
}

/// The issue's ratio of quantiles as `stage_pipeline` lowers it (series
/// identity: the two `quantile 0.5` share one group): no listed DAG is
/// cheaper than the selection, with accuracy guarantees ignored (the
/// default) or enforced.
#[test]
fn ratio_of_quantiles_no_listed_choice_is_cheaper_than_the_selected_plan() {
    for enforce in [&[][..], &["--enforce-accuracy-guarantees"][..]] {
        let mut args = vec!["--workload", RATIO_OF_QUANTILES_WORKLOAD];
        args.extend_from_slice(enforce);
        let choices = assert_no_listed_choice_is_cheaper(&priced_document(&args));
        assert!(choices.len() >= 25);
    }
}

/// Lowered without series identity, the two `quantile 0.5` have no unique
/// key but return the same rows, so they are still one group: 5 × 5 = 25
/// DAGs (two distinct approximate inputs), 9 of them sized for the whole
/// budget. The executor compiles none of their per-series summaries without
/// the series identity, so only the exact DAG compiles and is valid. Its price is not
/// compared with the plan selected for the series-identity lowering: its
/// rows carry no identity column, so it prices lower than the same exact
/// DAG with the identity (#1035).
#[test]
fn ratio_of_quantiles_25_pass1_choices_without_identity_only_exact_is_valid() {
    let without_identity = planned(&ratio_of_quantiles(), false);
    let enumeration = enumerate(&without_identity.memo, 100_000);
    let whole_budget = enumeration
        .all_chosen_candidates
        .iter()
        .filter(|choice| {
            choice.iter().all(|(group, &index)| {
                without_identity.memo.candidates[group.0 as usize][index].accuracy_budget_parts == 1
            })
        })
        .count();
    assert_eq!(
        (enumeration.all_chosen_candidates.len(), whole_budget),
        (25, 9)
    );
    let with_identity = planned(&ratio_of_quantiles(), true);
    let deployment = deployment();
    for enforce in [false, true] {
        let settings = settings(enforce);
        let models = models(&settings, &deployment);
        let baseline = selected(&with_identity, models);
        let priced = price_every_choice(&without_identity, models, baseline);
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
    let choices = enumerate(&planned.memo, 100).all_chosen_candidates;
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

/// A query whose root is a scalar expression reading two operator DAGs:
/// each reference is replaced by its own root group's composed DAG, so
/// every choice composes and is priced.
#[test]
fn scalar_root_choices_are_composed_and_priced() {
    let mut workload = ratio_of_quantiles();
    workload.query_workload.repeating_queries.as_mut().unwrap()[0].query = Query(
        "scalar(sum(quantile_over_time(0.5, data[5m]))) + scalar(max(rate(data[1m])))".into(),
    );
    let planned = planned(&workload, true);
    assert!(matches!(planned.roots[0], QueryRoot::Scalar(_)));
    let (settings, deployment) = (settings(false), deployment());
    let enumeration = enumerate(&planned.memo, 100_000);
    assert!(enumeration.complete && enumeration.all_chosen_candidates.len() > 1);
    let priced = price_choices(
        &planned.memo,
        &enumeration.all_chosen_candidates,
        &planned.roots,
        &planned.entries,
        &planned.data,
        models(&settings, &deployment),
        None,
    );
    for p in &priced {
        assert_ne!(p.rejection.as_deref(), Some("compose_failed"), "{p:?}");
    }
    assert!(priced.iter().any(|p| p.valid));
}

/// A choice whose selected plan installation refuses for a reason other
/// than the executor (here a raw-data window under a subquery, which no
/// runtime can place) is `not_installable`, never valid, so it is never
/// flagged cheaper than the selected plan.
#[test]
fn a_choice_installation_refuses_is_not_installable() {
    let mut workload = ratio_of_quantiles();
    workload.query_workload.repeating_queries.as_mut().unwrap()[0].query =
        Query("max_over_time(rate(data[5m])[1h:1m])".into());
    workload
        .query_workload
        .repeating_queries
        .as_mut()
        .unwrap()
        .truncate(1);
    let planned = planned(&workload, true);
    let (settings, deployment) = (settings(false), deployment());
    let enumeration = enumerate(&planned.memo, 100_000);
    let priced = price_choices(
        &planned.memo,
        &enumeration.all_chosen_candidates,
        &planned.roots,
        &planned.entries,
        &planned.data,
        models(&settings, &deployment),
        None,
    );
    assert!(!priced.is_empty());
    for p in &priced {
        assert!(!p.valid && !p.cheaper_than_selected, "{p:?}");
    }
    let refused: Vec<_> = priced
        .iter()
        .filter(|p| p.rejection.as_deref() == Some("not_installable"))
        .collect();
    assert!(!refused.is_empty(), "{priced:?}");
    for p in refused {
        let reason = p.rejection_reason.as_deref().unwrap();
        assert!(
            reason.contains("a subquery or a TimeShift above the range"),
            "{reason}"
        );
    }
}
