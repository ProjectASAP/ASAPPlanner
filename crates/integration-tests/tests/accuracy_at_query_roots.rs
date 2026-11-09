//! Constraint (K), each query's composed accuracy at its root, through
//! every planning stage on #1207's problem example (concepts doc,
//! section 1): A, the p95 of `latency` at rank error 0.01, and B, the
//! ratio of its p99 and p50 at relative error 0.03.

use asap_executor::physical_planner::promql_rows;
use asap_frontend_promql::lower_promql_query_workload;
use asap_planner::{run_planning, DeploymentCapabilities, PlanSearchSettings, PlanningModels};
use asap_types::ir::physical_export::PhysicalASAPDAG;
use asap_types::ir::properties::{AccuracyMetric, AccuracyScope, AccuracyTarget};
use asap_types::ir::schema::SketchParams;
use asap_types::ir::QueryRoot;
use asap_types::stages::ValueSign;
use asap_types::workload::{
    DataArrival, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload, RepeatedDemand, RepeatingEntry, RepetitionInterval,
    TimestampMs,
};

/// The plan of PromQL `queries` with their accuracy targets, each every
/// 10 s at phase 0, over `latency` (1,000 series sampled every second and
/// ingested, values declared non-negative), with no raw data kept: every
/// query reads summaries built at ingestion. `None` targets when
/// `ignore_accuracy`.
fn plan(queries: &[(&str, AccuracyTarget)], ignore_accuracy: bool) -> PhysicalASAPDAG {
    let mut data = DataWorkload {
        arrival: DataArrival::Ingesting {
            rows_per_second: 1_000.0,
            max_out_of_order_ms: None,
        },
        ..DataWorkload::sampled_every(["latency".to_string()], DurationMs(1_000))
            .with_series_count(1_000)
    };
    for facts in data.sources.values_mut() {
        facts
            .column_signs
            .insert("value".into(), ValueSign::NonNegative);
    }
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(
                queries
                    .iter()
                    .map(|(query, accuracy)| RepeatingEntry {
                        query: Query((*query).into()),
                        demand: RepeatedDemand::FixedIntervalAt {
                            interval: RepetitionInterval(10_000),
                            evaluation_phase: TimestampMs(0),
                        },
                        requirements: QueryRequirements {
                            accuracy: accuracy.clone(),
                            max_latency_ms: None,
                        },
                        predictability: Predictability::Unknown,
                        range_evaluation: None,
                    })
                    .collect(),
            ),
        },
        data_workload: Some(data.clone()),
    };
    let roots = lower_promql_query_workload(&workload)
        .unwrap()
        .into_iter()
        .map(|root| match root {
            QueryRoot::Operator(root) => {
                QueryRoot::Operator(promql_rows::with_series_identity(&root).unwrap())
            }
            scalar => scalar,
        })
        .collect();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let models = PlanningModels::builtin()
        .with_deployment_capabilities(&capabilities)
        .with_ignore_accuracy_guarantees(ignore_accuracy);
    let run = run_planning(
        roots,
        &entries,
        &data,
        models,
        PlanSearchSettings::default(),
    )
    .unwrap();
    run.selection.expect("a plan").0
}

/// #1207's problem example.
fn problem_example() -> [(&'static str, AccuracyTarget); 2] {
    [
        (
            "quantile_over_time(0.95, latency[1h])",
            AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Rank),
        ),
        (
            "quantile_over_time(0.99, latency[1h]) / quantile_over_time(0.5, latency[1h])",
            AccuracyTarget::max_error(0.03).accuracy_metric(AccuracyMetric::Relative),
        ),
    ]
}

/// The θ of the plan's sketch builds.
fn sketch_parameters(plan: &PhysicalASAPDAG) -> Vec<SketchParams> {
    plan.nodes
        .iter()
        .filter_map(|node| node.parameters.clone())
        .collect()
}

/// The `alpha` of the plan's DDSketch builds, ascending.
fn ddsketch_alphas(plan: &PhysicalASAPDAG) -> Vec<f64> {
    let mut alphas: Vec<f64> = sketch_parameters(plan)
        .iter()
        .filter_map(|p| match p {
            SketchParams::DDSketch { alpha } => Some(*alpha),
            _ => None,
        })
        .collect();
    alphas.sort_by(f64::total_cmp);
    alphas
}

/// A meets rank error 0.01 only with a KLL of k ≥ 269 (a DDSketch readout
/// bounds a relative error, not a rank); B reads DDSketch builds whose
/// readouts' composed bound $(\varepsilon_{p99} + \varepsilon_{p50}) / (1 -
/// 0.1)$ is within 0.03, which two alpha-0.02 readouts (each within 0.03
/// alone) are not.
#[test]
fn each_query_meets_its_target_at_its_root() {
    let plan = plan(&problem_example(), false);
    let parameters = sketch_parameters(&plan);
    assert!(
        parameters
            .iter()
            .any(|p| matches!(p, SketchParams::Kll { k } if *k >= 269)),
        "A reads a KLL meeting rank 0.01: {parameters:?}"
    );
    assert!(
        !parameters
            .iter()
            .any(|p| matches!(p, SketchParams::Kll { k } if *k < 269)),
        "{parameters:?}"
    );
    let alphas = ddsketch_alphas(&plan);
    assert!(!alphas.is_empty(), "B reads DDSketch: {parameters:?}");
    let (smallest, largest) = (alphas[0], alphas[alphas.len() - 1]);
    assert!(
        (smallest + largest) / (1.0 - 0.1) <= 0.03 * (1.0 + 1e-9),
        "B's composed bound: {alphas:?}"
    );
}

/// Ignoring accuracy, no query has a target: (K) has no rows and any
/// readout serves, so one DDSketch answers A's p95 too, whose rank target
/// it does not meet when accuracy is enforced.
#[test]
fn ignoring_accuracy_plans_without_k() {
    let reads_kll = |plan: &PhysicalASAPDAG| {
        sketch_parameters(plan)
            .iter()
            .any(|p| matches!(p, SketchParams::Kll { .. }))
    };
    assert!(reads_kll(&plan(&problem_example(), false)));
    let ignored = plan(&problem_example(), true);
    assert!(!reads_kll(&ignored), "{:?}", sketch_parameters(&ignored));
    assert!(!ddsketch_alphas(&ignored).is_empty());
}

/// A whole-result failure probability of 0.01 over 1,000 series is 1e-5
/// per value: a KLL read at that failure probability meets rank error
/// 0.01 only from k = 547 up (the concepts doc's KLL table).
#[test]
fn a_whole_result_failure_probability_is_divided_over_the_values() {
    let plan = plan(
        &[(
            "quantile_over_time(0.95, latency[1h])",
            AccuracyTarget::max_error(0.01)
                .accuracy_metric(AccuracyMetric::Rank)
                .failure_probability(0.01)
                .accuracy_scope(AccuracyScope::WholeResult),
        )],
        false,
    );
    let parameters = sketch_parameters(&plan);
    assert!(!parameters.is_empty());
    for parameters in parameters {
        assert!(
            matches!(parameters, SketchParams::Kll { k } if k >= 547),
            "{parameters:?}"
        );
    }
}
