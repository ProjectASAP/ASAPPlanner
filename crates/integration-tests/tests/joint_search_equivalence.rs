//! End-to-end behavior of the joint candidate planner after solver cutover.
//!
//! Tiny tests compare bounded search with complete traversal of the *same*
//! generated catalog. This is not an independent solver oracle: exhaustive
//! assignment correctness is tested separately against the MILP model oracle.
//! Larger fixtures check validated behavior without asserting global optimality.

use crate::planner_layering_common;

use asap_accuracy::{AccuracyModel, BuiltinAccuracyModel};
use asap_cost::{ClusterLimits, ClusterObjective, CostInputs, CostModel, ResourceUnitCost};
use asap_devtools::planning_stages::{lower_promql_roots, lower_sql_roots};
use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_plan_selection::{
    run_stage1_to_stage3_with, PlanningModels, Stage1ToStage3Run, Stage1ToStage3Search,
};
use asap_types::ir::properties::{AccuracyGuarantee, AccuracyTarget};
use asap_types::ir::QueryRoot;
use asap_types::workload::{
    DataArrival, DataWorkload, DurationMs, MetricType, PlanningWorkload, Predictability, Query,
    QueryLanguage, QueryRequirements, QueryWorkload, QueryWorkloadEntry, RepeatedDemand,
    RepeatingEntry, RepetitionInterval, SourceFacts,
};

fn workload_of(queries: &[(&str, u32)], accuracy: AccuracyTarget) -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(
                queries
                    .iter()
                    .map(|&(query, interval)| RepeatingEntry {
                        query: Query(query.into()),
                        demand: RepeatedDemand::FixedInterval(RepetitionInterval(interval)),
                        requirements: QueryRequirements {
                            accuracy: accuracy.clone(),
                            max_latency_ms: None,
                        },
                        predictability: Predictability::Predictable { known_at: None },
                        range_evaluation: None,
                    })
                    .collect(),
            ),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 24.0,
            },
            sources: ["m", "n", "c_total"]
                .into_iter()
                .map(|metric| {
                    (
                        metric.into(),
                        SourceFacts {
                            series_count: Some(8),
                            rows_held: Some(80),
                            sample_interval: Some(DurationMs(1000)),
                            metric_type: Some(if metric == "c_total" {
                                MetricType::Counter
                            } else {
                                MetricType::Gauge
                            }),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
        }),
    }
}

fn run(
    roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    exhaustive: bool,
    budget_ms: u64,
) -> Stage1ToStage3Run<usize> {
    run_stage1_to_stage3_with(
        roots.iter().cloned().enumerate().collect(),
        entries,
        data,
        models,
        Stage1ToStage3Search {
            exhaustive,
            keep_every_candidate: exhaustive,
            milp_time_limit_ms: Some(budget_ms),
            ..Default::default()
        },
    )
    .unwrap()
}

fn assert_valid(run: &Stage1ToStage3Run<usize>, queries: usize) {
    let selection = run
        .stage3_plan_selection
        .as_ref()
        .unwrap_or_else(|error| panic!("joint planning failed: {error}"));
    let physical = run
        .selected()
        .expect("successful selection has a physical plan");
    physical.dag.validate().unwrap();
    assert_eq!(physical.roots.len(), queries);
    assert_eq!(selection.accepted[&selection.selected].len(), queries);
    let report = selection
        .search
        .candidate_connections
        .as_ref()
        .expect("joint planning reports its finite catalog");
    assert!(report.refinements <= report.max_refinements);
    assert_ne!(report.status, "invalid_cost_certificate", "{report:?}");
    for value in [
        report.model_construction_ms,
        report.solver_ms,
        report.validation_ms,
    ] {
        assert!(value.is_finite() && value >= 0.0);
    }
    if let Some(gap) = report.relative_gap {
        assert!(gap.is_finite() && gap >= 0.0);
    }
    if let (Some(bound), Some(cost)) = (
        report.lower_bound,
        selection.costs[&selection.selected].cost_per_second,
    ) {
        assert!(
            bound <= cost + 1e-8 * cost.abs().max(1e-12),
            "reported bound {bound} exceeds validated cost {cost}: {report:?}"
        );
    }
}

fn run_workload(
    workload: &PlanningWorkload,
    settings: &CostInputs,
    deployment: &DeploymentCapabilities,
) -> Stage1ToStage3Run<usize> {
    let roots = lower_promql_roots(workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.as_ref().unwrap();
    let run = run(
        &roots,
        &entries,
        data,
        PlanningModels::builtin()
            .with_plan_selection(settings)
            .with_deployment_capabilities(deployment),
        false,
        10_000,
    );
    assert_valid(&run, entries.len());
    run
}

fn settings_for_every_objective() -> Vec<CostInputs> {
    let cluster = |objective| {
        CostModel::ClusterCapacity(ClusterLimits {
            memory_bytes: 1_000_000_000,
            disk_bytes: 1_000_000_000,
            cpu_cores: Some(8),
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
    .map(|cost_model| CostInputs {
        cost_model,
        ..Default::default()
    })
    .collect()
}

fn objective(run: &Stage1ToStage3Run<usize>, model: &CostModel) -> Vec<Vec<f64>> {
    let selection = run.stage3_plan_selection.as_ref().unwrap();
    let ids = if matches!(
        model,
        CostModel::ClusterCapacity(ClusterLimits {
            objective: ClusterObjective::ParetoFront,
            ..
        })
    ) {
        selection.front.clone()
    } else {
        vec![selection.selected.clone()]
    };
    let mut values: Vec<_> = ids
        .iter()
        .map(|id| {
            let cost = &selection.costs[id];
            if let Some(price) = cost.cost_per_second {
                return vec![price];
            }
            let usage = &cost.usage;
            let latency = usage
                .latency_per_query
                .iter()
                .map(|query| query.total_ms())
                .fold(0.0_f64, f64::max);
            match model {
                CostModel::ClusterCapacity(ClusterLimits {
                    objective: ClusterObjective::LeastMemory,
                    ..
                }) => vec![
                    usage.peak_memory_bytes as f64,
                    usage.disk_bytes as f64,
                    latency,
                ],
                _ => vec![
                    latency,
                    usage.peak_memory_bytes as f64,
                    usage.disk_bytes as f64,
                ],
            }
        })
        .collect();
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values.dedup();
    values
}

// All scalar, lexicographic and Pareto policies compare against complete traversal
// of one tiny finite catalog, rather than aliases of retired search engines.
#[test]
fn bounded_search_matches_complete_tiny_catalog_under_every_objective() {
    let workload = workload_of(&[("m", 60_000), ("m", 60_000)], AccuracyTarget::exact());
    let roots = lower_promql_roots(&workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.as_ref().unwrap();
    for settings in settings_for_every_objective() {
        let models = PlanningModels::builtin().with_plan_selection(&settings);
        let bounded = run(&roots, &entries, data, models, false, 10_000);
        let complete = run(&roots, &entries, data, models, true, 10_000);
        assert_valid(&bounded, 2);
        assert_valid(&complete, 2);
        let report = complete
            .stage3_plan_selection
            .as_ref()
            .unwrap()
            .search
            .candidate_connections
            .as_ref()
            .unwrap();
        assert_eq!(
            report.evaluation_complete,
            Some(true),
            "complete traversal must actually finish"
        );
        let (bounded, complete) = (
            objective(&bounded, &settings.cost_model),
            objective(&complete, &settings.cost_model),
        );
        assert_eq!(bounded.len(), complete.len());
        for (left, right) in bounded.iter().zip(complete) {
            for (left, right) in left.iter().zip(right) {
                assert!((left - right).abs() <= 1e-8 * left.abs().max(right.abs()).max(1e-12));
            }
        }
    }
}

// Bounded runs can stop at different progressive pane-width domains. Compare
// validated output semantics here, not inventory sizes from different catalogs.
// Fixed-catalog terminal/producer scaling is tested in candidates::scopes.
#[test]
fn repeated_queries_preserve_all_outputs_and_exact_guarantees() {
    let settings = CostInputs::default();
    let deployment = DeploymentCapabilities::default();
    for count in [2, 8] {
        let workload = workload_of(
            &vec![("sum_over_time(m[1m])", 60_000); count],
            AccuracyTarget::exact(),
        );
        let run = run_workload(&workload, &settings, &deployment);
        let selection = run.stage3_plan_selection.as_ref().unwrap();
        let accepted = &selection.accepted[&selection.selected];
        assert_eq!(accepted.len(), count);
        for accuracy in accepted {
            assert!(!accuracy.accepted_regardless_of_guarantee);
            assert_eq!(accuracy.guarantee_check_rejection, None);
            assert_eq!(accuracy.accepted_metric, None, "every output remains exact");
        }
    }
}

// Different windows over the same source remain legal with retained raw data
// disabled; their selected ingestion state must cover both output contracts.
#[test]
fn different_windows_and_raw_retention_policies_produce_valid_plans() {
    let workload = workload_of(
        &[
            ("sum_over_time(m[1m])", 60_000),
            ("sum_over_time(m[5m])", 60_000),
        ],
        AccuracyTarget::exact(),
    );
    for raw_data_retained in [true, false] {
        run_workload(
            &workload,
            &CostInputs::default(),
            &DeploymentCapabilities {
                raw_data_retained,
                ..Default::default()
            },
        );
    }
}

// Factoring a fused TopK candidate must not absorb the separately requested sum.
#[test]
fn fused_top_k_preserves_its_independently_requested_inner_sum() {
    let workload = workload_of(
        &[
            ("topk(5, sum by (job) (sum_over_time(m[5m])))", 60_000),
            ("sum by (job) (sum_over_time(m[5m]))", 60_000),
        ],
        AccuracyTarget::max_error(0.01),
    );
    run_workload(
        &workload,
        &CostInputs::default(),
        &DeploymentCapabilities::default(),
    );
}

// Grouped rollups coexist with temporal quantiles without merging distinct outputs.
#[test]
fn sketch_rollup_coexists_with_other_quantile_outputs() {
    let workload = workload_of(
        &[
            ("quantile_over_time(0.5, m[1m])", 10_000),
            ("quantile by (host, job) (0.5, m)", 10_000),
            ("quantile by (job) (0.5, m)", 10_000),
        ],
        AccuracyTarget::max_error(0.01).failure_probability(0.001),
    );
    run_workload(
        &workload,
        &CostInputs::default(),
        &DeploymentCapabilities::default(),
    );
}

// Local sketch contracts do not replace composed output accuracy validation.
#[test]
fn composed_quantile_and_sum_outputs_are_validated_together() {
    let workload = workload_of(
        &[
            ("sum by (job) (sum_over_time(m[5m]))", 60_000),
            ("avg by (job) (quantile_over_time(0.99, m[5m]))", 60_000),
        ],
        AccuracyTarget::max_error(0.05),
    );
    run_workload(
        &workload,
        &CostInputs::default(),
        &DeploymentCapabilities::default(),
    );
}

// A custom accuracy policy still participates in final evaluation and prevents
// builtin-only permanent pruning assumptions from being applied to its outputs.
#[test]
fn custom_accuracy_policy_is_respected() {
    struct CustomAccuracy(std::cell::Cell<usize>);
    impl AccuracyModel for CustomAccuracy {
        fn satisfies(&self, guarantee: &AccuracyGuarantee, target: &AccuracyTarget) -> bool {
            self.0.set(self.0.get() + 1);
            BuiltinAccuracyModel.satisfies(guarantee, target)
        }
    }
    let workload = workload_of(&[("sum_over_time(m[1m])", 60_000)], AccuracyTarget::exact());
    let roots = lower_promql_roots(&workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let policy = CustomAccuracy(std::cell::Cell::new(0));
    let result = run(
        &roots,
        &entries,
        workload.data_workload.as_ref().unwrap(),
        PlanningModels::builtin().with_accuracy(&policy),
        false,
        10_000,
    );
    assert_valid(&result, 1);
    assert!(
        policy.0.get() > 0,
        "the supplied accuracy policy must evaluate outputs"
    );
}

// Frozen measured calibration applies consistently to bounded and full-catalog
// evaluation; a timeout-dependent subset must not silently refit the cost model.
#[test]
fn frozen_calibration_is_reported_and_preserved() {
    let workload = workload_of(&[("m", 60_000)], AccuracyTarget::exact());
    let settings = CostInputs {
        frozen_calibration_factor: Some(2.0),
        ..Default::default()
    };
    let run = run_workload(&workload, &settings, &DeploymentCapabilities::default());
    assert_eq!(
        run.stage3_plan_selection
            .as_ref()
            .unwrap()
            .search
            .candidate_connections
            .as_ref()
            .unwrap()
            .calibration_factor,
        2.0
    );
}

// Exhausting the entire budget before planning yields an explicit diagnostic,
// never an unvalidated plan advertised as a warm-start incumbent.
#[test]
fn zero_budget_returns_no_unvalidated_plan() {
    let workload = workload_of(&[("m", 60_000)], AccuracyTarget::exact());
    let roots = lower_promql_roots(&workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let run = run(
        &roots,
        &entries,
        workload.data_workload.as_ref().unwrap(),
        PlanningModels::builtin(),
        false,
        0,
    );
    assert!(run.selected().is_none());
    assert!(run
        .stage3_plan_selection
        .unwrap_err()
        .to_string()
        .contains("budget"));
}

// The SQL frontend keeps repeated and different-window COUNT DISTINCT outputs.
#[test]
fn repeated_sql_queries_preserve_separate_output_references() {
    let (mut workload, catalog) = planner_layering_common::example2();
    let entries = workload.query_workload.repeating_queries.as_mut().unwrap();
    let first = entries[0].clone();
    *entries = [1, 5, 1].into_iter().map(|minutes| RepeatingEntry {
        query: Query(format!("SELECT COUNT(DISTINCT src_ip) FROM flows WHERE ts >= now() - INTERVAL '{minutes} minute'")), ..first.clone()
    }).collect();
    let roots = lower_sql_roots(&workload, &catalog).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let run = run(
        &roots,
        &entries,
        workload.data_workload.as_ref().unwrap(),
        PlanningModels::builtin(),
        false,
        10_000,
    );
    assert_valid(&run, 3);
}

// Disjoint inputs remain independently optimizable under additive usage pricing;
// the emitted full plan is nevertheless validated as one complete workload.
#[test]
fn disjoint_sources_preserve_independent_components() {
    let workload = workload_of(
        &[
            ("sum_over_time(m[1m])", 60_000),
            ("sum_over_time(n[1m])", 60_000),
        ],
        AccuracyTarget::exact(),
    );
    let run = run_workload(
        &workload,
        &CostInputs::default(),
        &DeploymentCapabilities::default(),
    );
    assert_eq!(
        run.stage3_plan_selection
            .as_ref()
            .unwrap()
            .search
            .candidate_connections
            .as_ref()
            .unwrap()
            .independent_components,
        2
    );
}

// Request work on literal outputs still consumes CPU and latency budgets even
// when there is no operator node to which a fee could be attributed.
#[test]
fn rootless_request_overhead_respects_capacity_and_latency() {
    let mut workload = workload_of(&[("1", 1)], AccuracyTarget::exact());
    let roots = lower_promql_roots(&workload).unwrap();
    for capacity in [true, false] {
        let mut settings = CostInputs::default();
        settings.measured_operation_costs.ns_per_evaluation = 2_000_000.0;
        if capacity {
            settings.cost_model = CostModel::ClusterCapacity(ClusterLimits {
                memory_bytes: 1_000_000_000,
                disk_bytes: 1_000_000_000,
                cpu_cores: Some(1),
                objective: ClusterObjective::LowestLatency,
            });
        } else {
            workload.query_workload.repeating_queries.as_mut().unwrap()[0]
                .requirements
                .max_latency_ms = Some(1.0);
        }
        let entries: Vec<_> = workload.query_workload.entries().collect();
        let result = run(
            &roots,
            &entries,
            workload.data_workload.as_ref().unwrap(),
            PlanningModels::builtin().with_plan_selection(&settings),
            false,
            10_000,
        );
        assert!(result.stage3_plan_selection.is_err());
        assert!(result.selected().is_none());
    }
}

// Unsupported exact operators cannot become accepted merely because no summary
// candidate exists; failure must leave the exported physical plan absent.
#[test]
fn unsupported_exact_operation_never_returns_an_unvalidated_plan() {
    let workload = workload_of(
        &[("stddev_over_time(m[5m])", 60_000)],
        AccuracyTarget::exact(),
    );
    let roots = lower_promql_roots(&workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let result = run(
        &roots,
        &entries,
        workload.data_workload.as_ref().unwrap(),
        PlanningModels::builtin(),
        false,
        10_000,
    );
    assert!(result.stage3_plan_selection.is_err());
    assert!(result.selected().is_none());
}

// Deterministic small mixed workloads exercise exact temporal operators, counter
// semantics, different sources and windows without claiming timeout optimality.
#[test]
fn seeded_small_mixed_workloads_return_validated_plans() {
    let queries = [
        "sum_over_time(m[1m])",
        "sum_over_time(n[5m])",
        "max_over_time(m[1m])",
        "rate(c_total[5m])",
    ];
    for seed in [3u64, 17, 29] {
        let first = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let second = first.wrapping_mul(6364136223846793005).wrapping_add(1);
        let workload = workload_of(
            &[
                (queries[(first >> 32) as usize % queries.len()], 60_000),
                (queries[(second >> 32) as usize % queries.len()], 60_000),
            ],
            AccuracyTarget::exact(),
        );
        run_workload(
            &workload,
            &CostInputs::default(),
            &DeploymentCapabilities::default(),
        );
    }
}
