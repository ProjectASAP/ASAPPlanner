//! Constraint (K), each query's composed accuracy at its root, through
//! every planning stage on a two-query example (`accuracy-models.md`,
//! "Example: allocation in a ratio"): A, the p95 of `latency` at rank
//! error 0.01, and B, the ratio of its p99 and p50 at relative error 0.03.

use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages_over};
use asap_planner::{DeploymentCapabilities, PlanningModels};
use asap_types::plan::physical_asap_dag::PhysicalASAPDAG;
use asap_types::summary_type::SketchParams;
use asap_types::workload::PlanningWorkload;

/// PromQL `queries`, each `(query, accuracy JSON)` every 10 s, over
/// `latency` (1,000 series, one sample per second, ingested, values
/// declared non-negative).
fn workload(queries: &[(&str, &str)]) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|(query, accuracy)| {
            format!(
                r#"{{"query": {query:?}, "demand": {{"fixed_interval_at": {{"interval": 10000, "evaluation_phase": 0}}}},
                  "requirements": {{"accuracy": {accuracy}}}, "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 1000.0}}}},
              "sources": {{"latency": {{"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000,
                "column_signs": {{"value": "non_negative"}}}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap()
}

/// The plan Stage 3 selects for `queries`, no raw data kept (every query
/// reads summaries built at ingestion), with accuracy enforced or
/// ignored.
fn plan(queries: &[(&str, &str)], ignore_accuracy: bool) -> PhysicalASAPDAG {
    selection(queries, ignore_accuracy).0
}

/// [`plan`], with the accepted accuracy Stage 3 reports per query.
fn selection(queries: &[(&str, &str)], ignore_accuracy: bool) -> (PhysicalASAPDAG, Vec<String>) {
    let workload = workload(queries);
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..Default::default()
    };
    let models = PlanningModels::builtin()
        .with_deployment_capabilities(&capabilities)
        .with_ignore_accuracy_guarantees(ignore_accuracy);
    let roots = lower_promql_roots(&workload).unwrap();
    let run = run_planning_stages_over(roots, &entries, &data, models).unwrap();
    let (plan, selection) = run.run.selection.as_ref().expect("a plan is selected");
    let accepted = selection
        .accepted_accuracy_per_query
        .iter()
        .map(ToString::to_string)
        .collect();
    (plan.clone(), accepted)
}

/// Queries A and B of the module's example.
const PROBLEM_EXAMPLE: [(&str, &str); 2] = [
    (
        "quantile_over_time(0.95, latency[1h])",
        r#"{"accuracy_metric": "rank", "max_error": 0.01}"#,
    ),
    (
        "quantile_over_time(0.99, latency[1h]) / quantile_over_time(0.5, latency[1h])",
        r#"{"accuracy_metric": "relative", "max_error": 0.03}"#,
    ),
];

/// The θ of the plan's sketch state nodes.
fn sketch_parameters(plan: &PhysicalASAPDAG) -> Vec<SketchParams> {
    plan.nodes
        .iter()
        .filter_map(|node| node.parameters.clone())
        .collect()
}

/// A meets rank error 0.01 only with a KLL of k ≥ 269 (a DDSketch readout
/// bounds a relative error, not a rank); B reads DDSketch states whose
/// readouts' composed bound $(\alpha_{p99} + \alpha_{p50}) / (1 - 0.1)$ is
/// within 0.03, which two α = 0.02 readouts (each within 0.03 alone) are
/// not.
#[test]
fn each_query_meets_its_target_at_its_root() {
    let (plan, accepted) = selection(&PROBLEM_EXAMPLE, false);
    assert_eq!(
        accepted,
        ["accepted rank (Kll)", "accepted relative (DDSketch)"],
        "the metric of each query's readouts"
    );
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
    let mut alphas: Vec<f64> = parameters
        .iter()
        .filter_map(|p| match p {
            SketchParams::DDSketch { alpha } => Some(*alpha),
            _ => None,
        })
        .collect();
    alphas.sort_by(f64::total_cmp);
    let (Some(&smallest), Some(&largest)) = (alphas.first(), alphas.last()) else {
        panic!("B reads DDSketch: {parameters:?}");
    };
    assert!(
        (smallest + largest) / (1.0 - 0.1) <= 0.03 * (1.0 + 1e-9),
        "B's composed bound: {alphas:?}"
    );
}

/// Ignoring accuracy, no query has a target: (K) has no rows and any
/// readout serves, so one summary type answers A's p95 and B's p99 and p50
/// (which one is a matter of cost), where enforcing accuracy needs a KLL
/// for A's rank target and DDSketch for B's relative one. The report says
/// there is no target to accept (it said "accepted exact (…)").
#[test]
fn ignoring_accuracy_plans_without_k() {
    // The number of sketch algorithms the plan builds.
    let algorithms = |plan: &PhysicalASAPDAG| {
        sketch_parameters(plan)
            .iter()
            .map(std::mem::discriminant)
            .collect::<std::collections::HashSet<_>>()
            .len()
    };
    assert_eq!(algorithms(&plan(&PROBLEM_EXAMPLE, false)), 2);
    let (ignored, accepted) = selection(&PROBLEM_EXAMPLE, true);
    assert_eq!(algorithms(&ignored), 1, "{:?}", sketch_parameters(&ignored));
    assert!(
        accepted
            .iter()
            .all(|accepted| accepted.starts_with("no accuracy target (")),
        "{accepted:?}"
    );
}

/// A whole-result failure probability of 0.01 over 1,000 series is 1e-5
/// per value: a KLL read at that failure probability meets rank error
/// 0.01 only from k = 547 up (`accuracy-models.md`, the KLL row of the
/// built-in models: 2.296 / k^0.9723 scaled by the Gaussian quantile ratio).
#[test]
fn a_whole_result_failure_probability_is_divided_over_the_values() {
    let plan = plan(
        &[(
            "quantile_over_time(0.95, latency[1h])",
            r#"{"accuracy_metric": "rank", "max_error": 0.01, "failure_probability": 0.01}"#,
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

/// p90, p50 and p90 / p50 over `latency[15m]`, each at relative 0.05: the
/// two quantiles alone accept DDSketch readouts at α = 0.05, and sharing
/// those with the ratio is cheapest, but the ratio's composed bound would be
/// (0.05 + 0.05) / (1 − 0.1) = 0.111. (K) is what rules that out: the plan
/// reads readouts at α ≤ 0.02 for the ratio, (0.02 + 0.02) / 0.9 = 0.044.
#[test]
fn a_ratio_sharing_its_readouts_with_its_operands_still_meets_its_target() {
    let target = r#"{"accuracy_metric": "relative", "max_error": 0.05}"#;
    let plan = plan(
        &[
            ("quantile_over_time(0.9, latency[15m])", target),
            ("quantile_over_time(0.5, latency[15m])", target),
            (
                "quantile_over_time(0.9, latency[15m]) / quantile_over_time(0.5, latency[15m])",
                target,
            ),
        ],
        false,
    );
    let alphas: Vec<f64> = sketch_parameters(&plan)
        .iter()
        .filter_map(|p| match p {
            SketchParams::DDSketch { alpha } => Some(*alpha),
            _ => None,
        })
        .collect();
    assert!(
        alphas.iter().any(|&alpha| alpha <= 0.02),
        "the ratio reads readouts composing within 0.05: {alphas:?}"
    );
}
