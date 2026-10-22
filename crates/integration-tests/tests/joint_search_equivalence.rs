//! The pruned joint-planning search selects what the exhaustive search
//! selects (`docs/design_docs/proposals/joint-planning-search.md`,
//! "Exactness contract"): the same plan, at the same cost, with the same
//! materialization, on the planner-layering examples and on seeded random
//! workloads small enough to search exhaustively, under every cost model.

mod planner_layering_common;

use asap_devtools::planning_stages::{lower_promql_roots, lower_sql_roots};
use asap_plan_selection::{
    run_stage1_to_stage3, run_stage1_to_stage3_with, AccuracyModel, BuiltinAccuracyModel,
    ClusterLimits, ClusterObjective, CostModel, PlanSelectionSettings, PlanningModels,
    RejectionCause, ResourceUnitCost, SelectionError, Stage1ToStage3Run, Stage1ToStage3Search,
};
use asap_types::ir::flat::flatten;
use asap_types::ir::properties::{AccuracyGuarantee, AccuracyMetric, AccuracyTarget};
use asap_types::ir::QueryRoot;
use asap_types::workload::{
    DataArrival, DataWorkload, DurationMs, MetricType, PlanningWorkload, Predictability, Query,
    QueryLanguage, QueryRequirements, QueryWorkload, QueryWorkloadEntry, RepeatedDemand,
    RepeatingEntry, RepetitionInterval, SourceFacts,
};
use planner_layering_common::*;

/// Every cost model, each objective of cost model 3 included, with
/// `memory_gb` of cluster memory: small enough that some candidates do not
/// fit, so the limits are exercised too.
fn cost_models(memory_gb: u64) -> Vec<PlanSelectionSettings> {
    let cluster = |objective| {
        CostModel::ClusterCapacity(ClusterLimits {
            memory_bytes: memory_gb * 1_000_000_000,
            disk_bytes: 1_000_000_000_000,
            cpu_cores: None,
            objective,
        })
    };
    [
        CostModel::UsageOverTime(ResourceUnitCost::illustrative()),
        CostModel::ProvisionedCapacity(ResourceUnitCost::illustrative()),
        cluster(ClusterObjective::LowestLatency),
        cluster(ClusterObjective::LeastMemory),
        cluster(ClusterObjective::ParetoFront),
    ]
    .into_iter()
    .map(|cost_model| PlanSelectionSettings {
        cost_model,
        ..PlanSelectionSettings::default()
    })
    .collect()
}

/// `roots` planned with the exhaustive search keeping every candidate (the
/// planner before pruning: every candidate priced, then selected at once)
/// and with the default search under `models`: both select the same plan with the same cost, materialization
/// and (under the Pareto front) front, or both fail the same way. Returns
/// the default search's run.
fn assert_equivalent(
    label: &str,
    roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Stage1ToStage3Run<usize> {
    let indexed = || roots.iter().cloned().enumerate().collect::<Vec<_>>();
    let exhaustive = run_stage1_to_stage3_with(
        indexed(),
        entries,
        data,
        models,
        Stage1ToStage3Search::exhaustive(),
    )
    .unwrap_or_else(|e| panic!("{label}: {e}"));
    let pruned = run_stage1_to_stage3(indexed(), entries, data, models)
        .unwrap_or_else(|e| panic!("{label}: {e}"));
    match (
        &exhaustive.stage3_plan_selection,
        &pruned.stage3_plan_selection,
    ) {
        (Ok(all), Ok(searched)) => {
            assert_eq!(flatten(&all.plan), flatten(&searched.plan), "{label}: plan");
            assert_eq!(
                all.costs[&all.selected], searched.costs[&searched.selected],
                "{label}: cost"
            );
            let (all_selected, searched_selected) =
                (exhaustive.selected().unwrap(), pruned.selected().unwrap());
            assert_eq!(
                all_selected.dag, searched_selected.dag,
                "{label}: physical plan"
            );
            assert_eq!(
                all_selected.materialization, searched_selected.materialization,
                "{label}: materialization"
            );
            let front = |s: &asap_plan_selection::Selection| {
                s.front
                    .iter()
                    .map(|id| s.costs[id].clone())
                    .collect::<Vec<_>>()
            };
            assert_eq!(front(all), front(searched), "{label}: Pareto front");
        }
        (Err(SelectionError::NoValidCandidate(_)), Err(SelectionError::NoValidCandidate(_))) => {}
        (all, searched) => panic!("{label}: exhaustive {all:?}, pruned {searched:?}"),
    }
    pruned
}

/// [`assert_equivalent`] for `workload`'s roots under each of `settings`.
fn assert_equivalent_under(
    label: &str,
    workload: &PlanningWorkload,
    roots: &[QueryRoot],
    settings: &[PlanSelectionSettings],
) -> Vec<Stage1ToStage3Run<usize>> {
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap_or_default();
    settings
        .iter()
        .map(|settings| {
            let models = PlanningModels::builtin().with_plan_selection(settings);
            assert_equivalent(
                &format!("{label} under {:?}", settings.cost_model),
                roots,
                &entries,
                &data,
                models,
            )
        })
        .collect()
}

/// The causes for which `run`'s search discarded some query's candidates
/// before composition.
fn discard_causes(run: &Stage1ToStage3Run<usize>) -> Vec<RejectionCause> {
    match &run.stage3_plan_selection {
        Ok(selection) => selection
            .search
            .queries
            .iter()
            .flat_map(|q| q.discarded.iter().map(|d| d.cause))
            .collect(),
        Err(SelectionError::NoValidCandidate(rejected)) => rejected
            .iter()
            .filter(|r| r.id.starts_with('q'))
            .map(|r| r.cause)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Examples 1 and 2 and Example 3's Pattern B under every cost model, and
/// Pattern A as given under cost model 1: the pruned search selects the
/// exhaustive search's plan.
#[test]
fn planner_layering_examples_select_the_exhaustive_plan() {
    let example1 = example1();
    assert_equivalent_under(
        "example 1",
        &example1,
        &lower_promql_roots(&example1).unwrap(),
        &cost_models(64),
    );
    let (example2, catalog) = example2();
    assert_equivalent_under(
        "example 2",
        &example2,
        &lower_sql_roots(&example2, &catalog).unwrap(),
        &cost_models(64),
    );
    let pattern_b = pattern_b();
    // 4 GB rejects Pattern B's ingestion-time panes (6.1 GB).
    assert_equivalent_under(
        "pattern B",
        &pattern_b,
        &lower_promql_roots(&pattern_b).unwrap(),
        &cost_models(4),
    );
    let (label, once) = &pattern_a_variants()[0];
    let roots = lower_promql_roots(once).unwrap();
    assert_equivalent_under(label, once, &roots, &cost_models(64)[..1]);
}

/// Pattern A (as given, and monthly as Example 4 varies it) under every
/// cost model. The monthly variant's exhaustive search takes minutes: run
/// with `--release --ignored`.
#[test]
#[ignore = "exhaustive search takes minutes; run with --release --ignored"]
fn pattern_a_selects_the_exhaustive_plan_under_every_cost_model() {
    for (label, workload) in pattern_a_variants() {
        let roots = lower_promql_roots(&workload).unwrap();
        assert_equivalent_under(label, &workload, &roots, &cost_models(64));
    }
}

/// Pattern A as given (Example 3), then monthly (Example 4). The at-rest
/// variant is left out: its exhaustive search takes hours.
fn pattern_a_variants() -> Vec<(&'static str, PlanningWorkload)> {
    let mixed = DataArrival::Mixed {
        rows_per_second: SHARED_ROWS_PER_SECOND,
    };
    vec![
        (
            "pattern A once",
            pattern_a(PatternARecurrence::OnceAdHoc, mixed),
        ),
        (
            "pattern A monthly",
            pattern_a(PatternARecurrence::MonthlyPredictable, mixed),
        ),
    ]
}

/// A seeded xorshift generator, so each random workload is reproducible
/// from its seed.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// One random PromQL query over one of the gauges `gauges` and the
/// counter `c_total`, some of whose candidates fail a check before
/// composition (a per-series `stddev_over_time`, a top-k over rates).
fn random_query(rng: &mut Rng, gauges: &[&str]) -> String {
    let m = rng.pick(gauges);
    let group = rng.pick(&["job", "job, instance", "instance"]);
    let window = rng.pick(&["1m", "5m", "10m"]);
    let phi = rng.pick(&["0.5", "0.9", "0.99"]);
    let k = rng.pick(&["3", "5"]);
    match rng.below(10) {
        0 => format!("sum by ({group}) (sum_over_time({m}[{window}]))"),
        1 => format!("avg by ({group}) (quantile_over_time({phi}, {m}[{window}]))"),
        2 => format!("quantile_over_time({phi}, {m}[{window}])"),
        3 => format!("topk by ({group}) ({k}, sum_over_time({m}[{window}]))"),
        4 => format!("sum by ({group}) (rate(c_total[{window}]))"),
        5 => format!("max by ({group}) (max_over_time({m}[{window}]))"),
        6 => format!("count_over_time({m}[{window}])"),
        7 => format!("sum by ({group}) (increase(c_total[{window}]))"),
        // The executor has no exact per-series stddev kernel.
        8 => format!("stddev_over_time({m}[{window}])"),
        // A heap sketch over rates, whose weights are not proven non-negative.
        _ => format!("topk({k}, sum by ({group}) (rate(c_total[{window}])))"),
    }
}

/// A random workload of two to four repeating queries with random accuracy
/// targets, over 1,000 series of each metric: the gauges `gauges` and the
/// counter `c_total`.
fn random_workload_over(seed: u64, gauges: &[&str]) -> PlanningWorkload {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let queries = 2 + rng.below(3);
    let entries = (0..queries)
        .map(|_| RepeatingEntry {
            query: Query(random_query(&mut rng, gauges)),
            demand: RepeatedDemand::FixedInterval(RepetitionInterval(
                *rng.pick(&[60_000u32, 300_000]),
            )),
            requirements: QueryRequirements {
                accuracy: rng
                    .pick(&[
                        AccuracyTarget::exact(),
                        AccuracyTarget::max_error(0.05),
                        AccuracyTarget::max_error(0.01).failure_probability(0.01),
                        AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Rank),
                    ])
                    .clone(),
                max_latency_ms: None,
            },
            predictability: Predictability::Predictable { known_at: None },
            range_evaluation: None,
        })
        .collect();
    let source = |metric_type| SourceFacts {
        series_count: Some(1_000),
        metric_type: Some(metric_type),
        sample_interval: Some(DurationMs(15_000)),
        ..SourceFacts::default()
    };
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(entries),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 2_000.0 / 15.0,
            },
            sources: gauges
                .iter()
                .map(|g| (g.to_string(), source(MetricType::Gauge)))
                .chain([("c_total".to_string(), source(MetricType::Counter))])
                .collect(),
        }),
    }
}

/// [`random_workload_over`] the gauge `m`.
fn random_workload(seed: u64) -> PlanningWorkload {
    random_workload_over(seed, &["m"])
}

/// The most composed Pass 1 candidates a random workload may have to be
/// searched exhaustively here; larger ones are skipped.
const MAX_EXHAUSTIVE_COMBINATIONS: usize = 48;

/// Seeded random workloads small enough to search exhaustively, each under
/// one cost model in turn (with 1 GB of cluster memory, which rejects some
/// candidates): the pruned search selects the exhaustive search's plan.
#[test]
fn random_small_workloads_select_the_exhaustive_plan() {
    assert_random_workloads_equivalent(false);
}

/// [`random_small_workloads_select_the_exhaustive_plan`] with every seed
/// under every cost model: minutes in a debug build.
#[test]
#[ignore = "every seed under every cost model takes minutes in a debug build; run with --release --ignored"]
fn random_small_workloads_select_the_exhaustive_plan_under_every_cost_model() {
    assert_random_workloads_equivalent(true);
}

/// Seeds 1 to 60 that are small enough, under every cost model or one per
/// seed in turn.
fn assert_random_workloads_equivalent(every_cost_model: bool) {
    let models = cost_models(1);
    let mut searched = 0;
    let mut causes = std::collections::BTreeSet::new();
    for seed in 1..=60u64 {
        let workload = random_workload(seed);
        let roots = lower_promql_roots(&workload).unwrap();
        let inventory =
            asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates(
                roots.iter().cloned().enumerate().collect(),
            )
            .unwrap();
        if inventory.combinations() > MAX_EXHAUSTIVE_COMBINATIONS {
            continue;
        }
        searched += 1;
        let queries: Vec<_> = workload
            .query_workload
            .entries()
            .map(|e| e.query.0.clone())
            .collect();
        let turn = seed as usize % models.len();
        let settings = match every_cost_model {
            true => &models[..],
            false => &models[turn..=turn],
        };
        let runs = assert_equivalent_under(
            &format!("seed {seed}: {queries:?}"),
            &workload,
            &roots,
            settings,
        );
        causes.extend(
            runs.iter()
                .flat_map(discard_causes)
                .map(|c| format!("{c:?}")),
        );
    }
    assert!(
        searched >= 20,
        "only {searched} workloads were small enough"
    );
    // Every kind of early rejection is exercised: an executor without the
    // kernel, Count-Min weights not proven non-negative, and accuracy (no
    // guarantee, or an unknown bound).
    for cause in [
        "PerEntityAggregateNotExecutable",
        "CountMinWeightsNotProvenNonNegative",
        "NoAccuracyGuarantee",
        "AccuracyTargetMissed",
    ] {
        assert!(causes.contains(cause), "{cause} not in {causes:?}");
    }
}

/// The average of per-series p99s has no accuracy rule: planned alone, the
/// query's summary candidates fail whatever their sizes, so the search
/// discards them before composition and reports why, and Stage 3 checks
/// fewer candidates for the same plan.
#[test]
fn a_query_candidate_without_a_guarantee_is_discarded_before_composition() {
    let mut workload = random_workload(1);
    let entry = |query: &str, accuracy: AccuracyTarget| RepeatingEntry {
        query: Query(query.into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(60_000)),
        requirements: QueryRequirements {
            accuracy,
            max_latency_ms: None,
        },
        predictability: Predictability::Predictable { known_at: None },
        range_evaluation: None,
    };
    workload.query_workload.repeating_queries = Some(vec![
        entry(
            "sum by (job) (sum_over_time(m[5m]))",
            AccuracyTarget::max_error(0.05),
        ),
        entry(
            "avg by (job) (quantile_over_time(0.99, m[5m]))",
            AccuracyTarget::max_error(0.05),
        ),
    ]);
    let roots = lower_promql_roots(&workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let run = assert_equivalent(
        "avg of p99",
        &roots,
        &entries,
        &data,
        PlanningModels::builtin(),
    );
    let search = &run.stage3_plan_selection.as_ref().unwrap().search;
    // q1's candidates all pass; q2's summary candidates are discarded.
    assert!(search.queries[0].discarded.is_empty());
    let discarded = &search.queries[1].discarded;
    assert!(!discarded.is_empty());
    assert!(discarded.len() < search.queries[1].candidates_checked as usize);
    assert!(discarded.iter().all(|d| matches!(
        d.cause,
        RejectionCause::NoAccuracyGuarantee | RejectionCause::AccuracyTargetMissed
    )));
}

/// A deployment model that may accept an output whatever its guarantee
/// says could accept a larger summary of the same plan, which sharing can
/// give it: no accuracy failure is then permanent, and nothing is
/// discarded before composition.
#[test]
fn a_model_that_may_accept_any_output_discards_nothing() {
    struct AcceptsLargeSummaries;
    impl AccuracyModel for AcceptsLargeSummaries {
        fn satisfies(&self, guarantee: &AccuracyGuarantee, target: &AccuracyTarget) -> bool {
            BuiltinAccuracyModel.satisfies(guarantee, target)
        }
    }
    let mut workload = random_workload(1);
    workload.query_workload.repeating_queries = Some(vec![RepeatingEntry {
        query: Query("avg by (job) (quantile_over_time(0.99, m[5m]))".into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(60_000)),
        requirements: QueryRequirements {
            accuracy: AccuracyTarget::max_error(0.05),
            max_latency_ms: None,
        },
        predictability: Predictability::Predictable { known_at: None },
        range_evaluation: None,
    }]);
    let roots = lower_promql_roots(&workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let model = AcceptsLargeSummaries;
    let run = assert_equivalent(
        "may accept",
        &roots,
        &entries,
        &data,
        PlanningModels::builtin().with_accuracy(&model),
    );
    let search = &run.stage3_plan_selection.as_ref().unwrap().search;
    assert!(search.queries[0].discarded.is_empty());
}

/// A summary whose guarantee is known but misses the target (a relative
/// bound for a rank target) might meet it at another size: it is not
/// discarded before composition, and Stage 3 rejects the completed plans
/// that read it.
#[test]
fn a_known_bound_that_misses_is_not_discarded_before_composition() {
    let mut workload = random_workload(1);
    workload.query_workload.repeating_queries = Some(vec![RepeatingEntry {
        query: Query("quantile_over_time(0.99, m[5m])".into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(600_000)),
        requirements: QueryRequirements {
            accuracy: AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Rank),
            max_latency_ms: None,
        },
        predictability: Predictability::Predictable { known_at: None },
        range_evaluation: None,
    }]);
    let roots = lower_promql_roots(&workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let indexed: Vec<_> = roots.iter().cloned().enumerate().collect();
    let run = run_stage1_to_stage3_with(
        indexed,
        &entries,
        &data,
        PlanningModels::builtin(),
        Stage1ToStage3Search {
            keep_every_candidate: true,
            exhaustive: false,
            memo_groups: false,
        },
    )
    .unwrap();
    let selection = run.stage3_plan_selection.as_ref().unwrap();
    assert!(selection.search.queries[0].discarded.is_empty());
    assert!(selection
        .rejected
        .iter()
        .any(|r| r.cause == RejectionCause::AccuracyTargetMissed));
}

/// A query none of whose own candidates the executor can run (the exact
/// per-series `stddev_over_time` has no kernel and no summary alternative)
/// leaves nothing to compose: the selection fails, and its rejections say
/// which query and why, as the exhaustive search's do.
#[test]
fn a_query_with_no_valid_candidate_is_named_in_the_failure() {
    let mut workload = random_workload(1);
    workload.query_workload.repeating_queries = Some(vec![RepeatingEntry {
        query: Query("stddev_over_time(m[5m])".into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(60_000)),
        requirements: QueryRequirements {
            accuracy: AccuracyTarget::exact(),
            max_latency_ms: None,
        },
        predictability: Predictability::Predictable { known_at: None },
        range_evaluation: None,
    }]);
    let roots = lower_promql_roots(&workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let run = assert_equivalent("stddev", &roots, &entries, &data, PlanningModels::builtin());
    let Err(SelectionError::NoValidCandidate(rejected)) = &run.stage3_plan_selection else {
        panic!("{:?}", run.stage3_plan_selection)
    };
    assert!(rejected.iter().any(|r| r.id == "q1"
        && r.cause == RejectionCause::PerEntityAggregateNotExecutable
        && r.reason.starts_with("q1: ")));
}

/// With a measured cost table, the formulas are calibrated by one factor
/// fitted over every candidate the exhaustive search prices: the default
/// search fits it over the same candidates, the discarded ones included,
/// and selects the exhaustive search's plan.
#[test]
fn a_calibrated_cost_model_selects_the_exhaustive_plan() {
    use asap_plan_selection::{MeasuredOperationCosts, OperatorCost, OperatorKind};
    // Scans are priced from the table; every other node is calibrated by
    // the factor fitted over the scans.
    let settings = PlanSelectionSettings {
        measured_operation_costs: MeasuredOperationCosts {
            operators: vec![OperatorCost {
                operator: OperatorKind::Scan,
                phase: None,
                sketch: None,
                exact: None,
                measure: None,
                ns_per_unit: 300.0,
                ns_per_invocation: 0.0,
                memory_bytes_per_state: None,
            }],
            ..MeasuredOperationCosts::default()
        },
        ..PlanSelectionSettings::default()
    };
    let mut searched = 0;
    for seed in 1..=30u64 {
        let workload = random_workload(seed);
        let roots = lower_promql_roots(&workload).unwrap();
        let inventory =
            asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates(
                roots.iter().cloned().enumerate().collect(),
            )
            .unwrap();
        if inventory.combinations() > MAX_EXHAUSTIVE_COMBINATIONS {
            continue;
        }
        searched += 1;
        assert_equivalent_under(
            &format!("seed {seed}"),
            &workload,
            &roots,
            std::slice::from_ref(&settings),
        );
    }
    assert!(
        searched >= 10,
        "only {searched} workloads were small enough"
    );
}

/// Seeded random workloads over two gauges and a counter under cost model
/// 1, whose queries often read disjoint sources: the components are
/// searched separately and combined (design method 5), and the combined
/// plan is the exhaustive search's.
#[test]
fn random_workloads_over_disjoint_sources_combine_their_components() {
    let settings = &cost_models(1)[..1];
    let mut combined = 0;
    for seed in 1..=40u64 {
        let workload = random_workload_over(seed, &["m", "n"]);
        let roots = lower_promql_roots(&workload).unwrap();
        let inventory =
            asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates(
                roots.iter().cloned().enumerate().collect(),
            )
            .unwrap();
        if inventory.combinations() > MAX_EXHAUSTIVE_COMBINATIONS {
            continue;
        }
        let entries: Vec<_> = workload.query_workload.entries().collect();
        let data = workload.data_workload.clone().unwrap();
        let models = PlanningModels::builtin().with_plan_selection(&settings[0]);
        let run = assert_same_cost(&format!("seed {seed}"), &roots, &entries, &data, models);
        if let Ok(selection) = &run.stage3_plan_selection {
            if !selection.search.components.is_empty() {
                combined += 1;
                assert_eq!(selection.search.searched_as_one, None);
                let queries: usize = selection
                    .search
                    .components
                    .iter()
                    .map(|c| c.queries.len())
                    .sum();
                assert_eq!(queries, roots.len());
            }
        }
    }
    assert!(combined >= 10, "only {combined} workloads were combined");
}

/// Queries reading disjoint sources are separate components, and queries
/// linked through a summary build they can share are one, in workload
/// order.
#[test]
fn components_follow_shared_sources() {
    let mut workload = random_workload_over(1, &["m", "n"]);
    let entry = |query: &str| RepeatingEntry {
        query: Query(query.into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(60_000)),
        requirements: QueryRequirements {
            accuracy: AccuracyTarget::max_error(0.05),
            max_latency_ms: None,
        },
        predictability: Predictability::Predictable { known_at: None },
        range_evaluation: None,
    };
    workload.query_workload.repeating_queries = Some(vec![
        entry("sum by (job) (sum_over_time(m[5m]))"),
        entry("sum by (job) (rate(c_total[5m]))"),
        entry("max by (job) (max_over_time(n[5m]))"),
        entry("sum by (job) (sum_over_time(m[5m])) / max by (job) (max_over_time(n[5m]))"),
    ]);
    let roots = lower_promql_roots(&workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let run = assert_equivalent("linked", &roots, &entries, &data, PlanningModels::builtin());
    let search = &run.stage3_plan_selection.as_ref().unwrap().search;
    // q4 can share q1's build over m and q3's over n, linking them; q2
    // alone reads c_total.
    let components: Vec<_> = search
        .components
        .iter()
        .map(|c| c.queries.clone())
        .collect();
    assert_eq!(components, vec![vec![0, 2, 3], vec![1]]);
}

/// A deployment that keeps no raw data, as the MVP benchmark runs: a plan
/// is valid only when every scan runs at ingestion time.
fn no_raw_data() -> asap_logical_asap_optimization::DeploymentCapabilities {
    asap_logical_asap_optimization::DeploymentCapabilities {
        raw_data_retained: false,
        ..asap_logical_asap_optimization::DeploymentCapabilities::default()
    }
}

/// Without raw data, the default search builds no Stage 2 assignment that
/// scans raw data at query time and enumerates no Pass 2 candidate whose
/// every assignment does: seeded random workloads, under every cost model,
/// select the exhaustive search's plan (or both find none), and some
/// assignments and candidates are actually left out.
#[test]
fn without_raw_data_random_workloads_select_the_exhaustive_plan() {
    let deployment = no_raw_data();
    let models = cost_models(1);
    let (mut searched, mut skipped) = (0, 0);
    for seed in 1..=60u64 {
        let workload = random_workload_over(seed, &["m", "n"]);
        let roots = lower_promql_roots(&workload).unwrap();
        let inventory =
            asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates(
                roots.iter().cloned().enumerate().collect(),
            )
            .unwrap();
        if inventory.combinations() > MAX_EXHAUSTIVE_COMBINATIONS {
            continue;
        }
        searched += 1;
        let entries: Vec<_> = workload.query_workload.entries().collect();
        let data = workload.data_workload.clone().unwrap();
        let settings = &models[seed as usize % models.len()];
        let models = PlanningModels::builtin()
            .with_plan_selection(settings)
            .with_deployment_capabilities(&deployment);
        let run = assert_equivalent(&format!("seed {seed}"), &roots, &entries, &data, models);
        if let Ok(selection) = &run.stage3_plan_selection {
            skipped += selection.search.raw_scan_assignments_skipped;
        }
    }
    assert!(
        searched >= 20,
        "only {searched} workloads were small enough"
    );
    assert!(skipped > 0, "no assignment was left out");
}

/// Examples 1 and 2 and Pattern B, in a deployment that keeps no raw data,
/// under every cost model.
#[test]
fn without_raw_data_planner_layering_examples_select_the_exhaustive_plan() {
    let deployment = no_raw_data();
    let example1 = example1();
    let (example2, catalog) = example2();
    let pattern_b = pattern_b();
    for (label, workload, roots) in [
        (
            "example 1",
            &example1,
            lower_promql_roots(&example1).unwrap(),
        ),
        (
            "example 2",
            &example2,
            lower_sql_roots(&example2, &catalog).unwrap(),
        ),
        (
            "pattern B",
            &pattern_b,
            lower_promql_roots(&pattern_b).unwrap(),
        ),
    ] {
        let entries: Vec<_> = workload.query_workload.entries().collect();
        let data = workload.data_workload.clone().unwrap_or_default();
        for settings in cost_models(64) {
            let models = PlanningModels::builtin()
                .with_plan_selection(&settings)
                .with_deployment_capabilities(&deployment);
            assert_equivalent(
                &format!("{label} without raw data under {:?}", settings.cost_model),
                &roots,
                &entries,
                &data,
                models,
            );
        }
    }
}

/// A fine and a coarse sum and two quantiles grouped differently, as
/// ASAPQuery's random workloads ask them, in a deployment that keeps no
/// raw data: the cheapest plan shares one DDSketch between the quantiles
/// (summary-capability rule) and answers the coarse sum by rolling up the
/// fine one (rollup rule). Both searches select it, and it is cheaper than
/// every candidate that applies only one of the two rules.
#[test]
fn a_shared_summary_is_combined_with_a_rollup() {
    let entry = |query: &str, interval_ms: u32| RepeatingEntry {
        query: Query(query.into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(interval_ms)),
        requirements: QueryRequirements {
            accuracy: AccuracyTarget::max_error(0.05).failure_probability(0.3),
            max_latency_ms: None,
        },
        predictability: Predictability::Predictable { known_at: None },
        range_evaluation: None,
    };
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![
                entry(
                    "sum by (region, service, job) (sum_over_time(m[10m]))",
                    20_000,
                ),
                entry("sum by (region, service) (sum_over_time(m[10m]))", 20_000),
                entry(
                    "avg by (region, service, job) (quantile_over_time(0.99, m[100s]))",
                    1_000,
                ),
                entry("avg by (region) (quantile_over_time(0.95, m[100s]))", 1_000),
            ]),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 100_000.0,
            },
            sources: [(
                "m".to_string(),
                SourceFacts {
                    series_count: Some(10_000),
                    metric_type: Some(MetricType::Gauge),
                    values_non_negative: true,
                    sample_interval: Some(DurationMs(100)),
                    ..SourceFacts::default()
                },
            )]
            .into(),
        }),
    };
    let roots = lower_promql_roots(&workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let deployment = no_raw_data();
    let models = PlanningModels::builtin().with_deployment_capabilities(&deployment);
    let indexed = || roots.iter().cloned().enumerate().collect::<Vec<_>>();
    let exhaustive = run_stage1_to_stage3_with(
        indexed(),
        &entries,
        &data,
        models,
        Stage1ToStage3Search::exhaustive(),
    )
    .unwrap();
    assert_equivalent("shared summary and rollup", &roots, &entries, &data, models);

    let rules = |logical: &str| {
        let pass2 = exhaustive
            .stage1_logical_asap_optimization
            .iter()
            .find(|l| l.id == logical)
            .unwrap()
            .pass2
            .as_ref();
        // A build is shared by the summary-capability rule, or by the
        // identical-expression rule over scans widened to one label set.
        let builds_shared_by_identical_expression =
            |p: &asap_logical_asap_optimization::workload_candidates::Pass2Candidate| {
                p.sharing_candidates
                    .iter()
                    .filter(|c| {
                        matches!(
                            c.nodes[0].asap(),
                            Some(
                                asap_types::ir::ASAPOp::SummaryAgg { .. }
                                    | asap_types::ir::ASAPOp::PaneChain { .. }
                            )
                        )
                    })
                    .count()
            };
        (
            pass2.map_or(0, |p| {
                p.shared_builds.len() + builds_shared_by_identical_expression(p)
            }),
            pass2.map_or(0, |p| p.rollup_candidates.len()),
        )
    };
    let shared_builds = |logical: &str| {
        exhaustive
            .stage1_logical_asap_optimization
            .iter()
            .find(|l| l.id == logical)
            .and_then(|l| l.pass2.as_ref())
            .map_or(Vec::new(), |p| p.shared_builds.clone())
    };
    let selected = exhaustive.selected().unwrap();
    assert_eq!(
        rules(&selected.from_logical),
        (1, 1),
        "one shared DDSketch and one rollup"
    );
    let shared = shared_builds(&selected.from_logical);
    assert!(
        format!("{:?}", shared[0][0]).contains("DDSketch"),
        "the shared build is a DDSketch: {:?}",
        shared[0][0]
    );
    let selection = exhaustive.stage3_plan_selection.as_ref().unwrap();
    let cost = |id: &str| selection.costs[id].cost_per_second.unwrap();
    let cheapest_with_one_rule = exhaustive
        .stage2_physical_asap_optimization
        .iter()
        .filter(|p| selection.costs.contains_key(&p.id))
        .filter(|p| {
            let (shared, rollups) = rules(&p.from_logical);
            shared == 0 || rollups == 0
        })
        .map(|p| cost(&p.id))
        .fold(f64::INFINITY, f64::min);
    assert!(
        cost(&selected.id) < cheapest_with_one_rule,
        "{} is not cheaper than {cheapest_with_one_rule}",
        cost(&selected.id)
    );
}

/// `roots` planned with the exhaustive search and with the default search
/// under `models`: both select a plan of the same price (within Stage 3's
/// tie tolerance), or both find none. Rules that keep one of several plans
/// of equal price (interchangeable queries) may select another plan than
/// the exhaustive search's first. Returns the default search's run.
fn assert_same_cost(
    label: &str,
    roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Stage1ToStage3Run<usize> {
    let indexed = || roots.iter().cloned().enumerate().collect::<Vec<_>>();
    let exhaustive = run_stage1_to_stage3_with(
        indexed(),
        entries,
        data,
        models,
        Stage1ToStage3Search::exhaustive(),
    )
    .unwrap_or_else(|e| panic!("{label}: {e}"));
    let pruned = run_stage1_to_stage3(indexed(), entries, data, models)
        .unwrap_or_else(|e| panic!("{label}: {e}"));
    match (
        &exhaustive.stage3_plan_selection,
        &pruned.stage3_plan_selection,
    ) {
        (Ok(all), Ok(searched)) => {
            let price =
                |s: &asap_plan_selection::Selection| s.costs[&s.selected].cost_per_second.unwrap();
            let (all, searched) = (price(all), price(searched));
            assert!(
                (all - searched).abs() <= 1e-9 * all.abs(),
                "{label}: exhaustive {all}, pruned {searched}"
            );
        }
        (Err(SelectionError::NoValidCandidate(_)), Err(SelectionError::NoValidCandidate(_))) => {}
        (all, searched) => panic!("{label}: exhaustive {all:?}, pruned {searched:?}"),
    }
    pruned
}

/// A workload of repeating `queries` (each with its interval in ms) over
/// the gauge `m`, every query with accuracy target `accuracy`.
fn workload_of(queries: &[(&str, u32)], accuracy: AccuracyTarget) -> PlanningWorkload {
    let mut workload = random_workload(1);
    workload.query_workload.repeating_queries = Some(
        queries
            .iter()
            .map(|&(query, interval_ms)| RepeatingEntry {
                query: Query(query.into()),
                demand: RepeatedDemand::FixedInterval(RepetitionInterval(interval_ms)),
                requirements: QueryRequirements {
                    accuracy: accuracy.clone(),
                    max_latency_ms: None,
                },
                predictability: Predictability::Predictable { known_at: None },
                range_evaluation: None,
            })
            .collect(),
    );
    workload
}

/// Queries over one gauge that differ at most in a quantile rank, with
/// equal workload entries, are one class of interchangeable queries: the
/// default search gives them one shared choice and still selects a plan of
/// the exhaustive search's price, with raw data kept and without, beside a
/// query of another shape. A query of another interval at one member's
/// rank, or a ratio of two members' ranks, can share one member's exact
/// quantile and not the other's: then there is no class, and the price is
/// still the exhaustive search's.
#[test]
fn interchangeable_queries_select_the_exhaustive_price() {
    let q = |phi: &str| format!("quantile_over_time({phi}, m[5m])");
    let (q5, q9, q99) = (q("0.5"), q("0.9"), q("0.99"));
    let avg = |phi: &str| format!("avg(quantile_over_time({phi}, m[5m]))");
    let (avg5, avg9) = (avg("0.5"), avg("0.9"));
    let ratio = format!("{q9} / {q5}");
    let sum = "sum_over_time(m[5m])";
    // (label, queries with their intervals, the largest class's size)
    type Case<'a> = (&'a str, Vec<(&'a str, u32)>, usize);
    let cases: Vec<Case> = vec![
        (
            "three ranks",
            vec![(&q5, 60_000), (&q9, 60_000), (&q99, 60_000)],
            3,
        ),
        // The 0.9 duplicates can share their exact quantile, which the 0.5
        // query cannot join: the duplicates are a class of their own.
        (
            "two ranks and a duplicate",
            vec![(&q5, 60_000), (&q9, 60_000), (&q9, 60_000)],
            2,
        ),
        (
            "averages of two ranks and a duplicate",
            vec![(&avg9, 60_000), (&avg9, 60_000), (&avg5, 60_000)],
            2,
        ),
        // The 300 s query can share the 0.9 member's exact quantile, not the
        // 0.5 member's, so the two are no longer alike.
        (
            "two intervals",
            vec![(&q5, 60_000), (&q9, 60_000), (&q9, 300_000)],
            0,
        ),
        // So can the ratio, with each member's exact quantile.
        (
            "a ratio of ranks",
            vec![(&q5, 60_000), (&q9, 60_000), (&ratio, 60_000)],
            0,
        ),
        (
            "identical sums",
            vec![(sum, 60_000), (sum, 60_000), (&q5, 60_000)],
            2,
        ),
    ];
    let deployments = [
        asap_logical_asap_optimization::DeploymentCapabilities::default(),
        no_raw_data(),
    ];
    for (label, queries, class_size) in cases {
        for accuracy in [
            AccuracyTarget::max_error(0.05),
            AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Rank),
        ] {
            let workload = workload_of(&queries, accuracy);
            let roots = lower_promql_roots(&workload).unwrap();
            let entries: Vec<_> = workload.query_workload.entries().collect();
            let data = workload.data_workload.clone().unwrap();
            for deployment in &deployments {
                let models = PlanningModels::builtin().with_deployment_capabilities(deployment);
                let run = assert_same_cost(label, &roots, &entries, &data, models);
                if let Ok(selection) = &run.stage3_plan_selection {
                    let largest = selection
                        .search
                        .interchangeable_queries
                        .iter()
                        .map(Vec::len)
                        .max()
                        .unwrap_or(0);
                    assert_eq!(largest, class_size, "{label}: class size");
                }
            }
        }
    }
}

/// Two identical SQL queries and a third over another window, over one
/// table: the identical two are one class of interchangeable queries, and
/// the default search selects a plan of the exhaustive search's price.
#[test]
fn identical_sql_queries_are_one_class() {
    let (mut workload, catalog) = example2();
    let entries = workload.query_workload.repeating_queries.as_mut().unwrap();
    let first = entries[0].clone();
    let distinct = |minutes: u32| {
        RepeatingEntry {
        query: Query(format!(
            "SELECT COUNT(DISTINCT src_ip) FROM flows WHERE ts >= now() - INTERVAL '{minutes} minute'"
        )),
        ..first.clone()
    }
    };
    *entries = vec![distinct(1), distinct(5), distinct(1)];
    let roots = lower_sql_roots(&workload, &catalog).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let run = assert_same_cost(
        "identical SQL queries",
        &roots,
        &entries,
        &data,
        PlanningModels::builtin(),
    );
    let search = &run.stage3_plan_selection.as_ref().unwrap().search;
    assert_eq!(search.interchangeable_queries, vec![vec![0, 2]]);
}

/// Seeded random workloads over one gauge whose queries repeat each other
/// or differ in a quantile rank, at one of two intervals, with raw data
/// kept or not: the default search selects a plan of the exhaustive
/// search's price.
#[test]
fn random_workloads_with_interchangeable_queries_select_the_exhaustive_price() {
    let pool = [
        "quantile_over_time(0.5, m[5m])",
        "quantile_over_time(0.9, m[5m])",
        "quantile_over_time(0.99, m[5m])",
        "sum_over_time(m[5m])",
        "sum by (job) (sum_over_time(m[5m]))",
        "max_over_time(m[5m])",
    ];
    let mut searched = 0;
    for seed in 1..=30u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let count = 2 + rng.below(2);
        let queries: Vec<(&str, u32)> = (0..count)
            .map(|_| (*rng.pick(&pool), *rng.pick(&[60_000u32, 300_000])))
            .collect();
        let workload = workload_of(&queries, AccuracyTarget::max_error(0.05));
        let roots = lower_promql_roots(&workload).unwrap();
        let inventory =
            asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates(
                roots.iter().cloned().enumerate().collect(),
            )
            .unwrap();
        if inventory.combinations() > MAX_EXHAUSTIVE_COMBINATIONS {
            continue;
        }
        searched += 1;
        let entries: Vec<_> = workload.query_workload.entries().collect();
        let data = workload.data_workload.clone().unwrap();
        let deployment = match seed % 2 {
            0 => no_raw_data(),
            _ => asap_logical_asap_optimization::DeploymentCapabilities::default(),
        };
        let models = PlanningModels::builtin().with_deployment_capabilities(&deployment);
        assert_same_cost(&format!("seed {seed}"), &roots, &entries, &data, models);
    }
    assert!(
        searched >= 15,
        "only {searched} workloads were small enough"
    );
}

/// Queries over different windows, or with a latency bound (a check, not a
/// charge, so the class argument does not cover it), are in no class.
#[test]
fn queries_over_other_windows_or_with_a_latency_bound_are_in_no_class() {
    let mut workload = workload_of(
        &[
            ("quantile_over_time(0.5, m[5m])", 60_000),
            ("quantile_over_time(0.9, m[10m])", 60_000),
            ("quantile_over_time(0.99, m[5m])", 60_000),
        ],
        AccuracyTarget::max_error(0.05),
    );
    workload.query_workload.repeating_queries.as_mut().unwrap()[2]
        .requirements
        .max_latency_ms = Some(60_000.0);
    let roots = lower_promql_roots(&workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let run = assert_same_cost(
        "no class",
        &roots,
        &entries,
        &data,
        PlanningModels::builtin(),
    );
    let search = &run.stage3_plan_selection.as_ref().unwrap().search;
    assert!(search.interchangeable_queries.is_empty());
}

/// The memo-group search (not the default yet) on seeded random workloads
/// over one or two gauges, with raw data kept or not, under cost model 1:
/// it selects a plan of the exhaustive search's price, or both find none.
#[test]
fn memo_group_search_selects_the_exhaustive_price() {
    let mut searched = 0;
    let mut mismatches = Vec::new();
    for seed in 1..=60u64 {
        let gauges: &[&str] = match seed % 3 {
            0 => &["m", "n"],
            _ => &["m"],
        };
        let workload = random_workload_over(seed, gauges);
        let roots = lower_promql_roots(&workload).unwrap();
        let inventory =
            asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates(
                roots.iter().cloned().enumerate().collect(),
            )
            .unwrap();
        if inventory.combinations() > MAX_EXHAUSTIVE_COMBINATIONS {
            continue;
        }
        searched += 1;
        let entries: Vec<_> = workload.query_workload.entries().collect();
        let data = workload.data_workload.clone().unwrap();
        let deployment = match seed % 2 {
            0 => no_raw_data(),
            _ => asap_logical_asap_optimization::DeploymentCapabilities::default(),
        };
        let models = PlanningModels::builtin().with_deployment_capabilities(&deployment);
        let indexed = || roots.iter().cloned().enumerate().collect::<Vec<_>>();
        let exhaustive = run_stage1_to_stage3_with(
            indexed(),
            &entries,
            &data,
            models,
            Stage1ToStage3Search::exhaustive(),
        )
        .unwrap();
        let memo = run_stage1_to_stage3_with(
            indexed(),
            &entries,
            &data,
            models,
            Stage1ToStage3Search {
                memo_groups: true,
                ..Stage1ToStage3Search::default()
            },
        )
        .unwrap();
        let price = |run: &Stage1ToStage3Run<usize>| {
            run.stage3_plan_selection
                .as_ref()
                .ok()
                .map(|s| s.costs[&s.selected].cost_per_second.unwrap())
        };
        let (all, found) = (price(&exhaustive), price(&memo));
        let same = match (all, found) {
            (Some(a), Some(f)) => (a - f).abs() <= 1e-9 * a.abs(),
            (None, None) => true,
            _ => false,
        };
        if !same {
            mismatches.push(format!(
                "seed {seed}: exhaustive {all:?}, memo {found:?}: {:?}",
                workload
                    .query_workload
                    .repeating_queries
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|e| e.query.0.clone())
                    .collect::<Vec<_>>()
            ));
        }
    }
    assert!(
        searched >= 20,
        "only {searched} workloads were small enough"
    );
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}
