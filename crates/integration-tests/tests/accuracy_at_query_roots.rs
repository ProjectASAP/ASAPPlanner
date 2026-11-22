//! Constraint (K), each query's composed accuracy at its root, through
//! every planning stage on a two-query example (`accuracy-models.md`,
//! "Example: allocation in a ratio"): A, the p95 of `latency` at rank
//! error 0.01, and B, the ratio of its p99 and p50 at relative error 0.03.

use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages_over};
use asap_planner::{DeploymentCapabilities, PlanningModels, PlanningRun};
use asap_types::deployment_capabilities::CandidateExclusion;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::plan::physical_asap_dag::PhysicalASAPDAG;
use asap_types::summary_type::SketchParams;
use asap_types::summary_type::SummaryFamily;
use asap_types::workload::PlanningWorkload;

/// The series of `latency` in the module's example.
const LATENCY_SERIES: usize = 1_000;

/// PromQL `queries`, each `(query, accuracy JSON)` every 10 s, over
/// `latency` (`series_count` series, one sample per second each,
/// ingested, values declared non-negative).
fn workload(queries: &[(&str, &str)], series_count: usize) -> PlanningWorkload {
    workload_with_sources(queries, series_count, "")
}

/// [`workload`] with `more_sources`, JSON members of `sources` after
/// `latency`'s (each starting with a comma).
fn workload_with_sources(
    queries: &[(&str, &str)],
    series_count: usize,
    more_sources: &str,
) -> PlanningWorkload {
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
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": {series_count}.0}}}},
              "sources": {{"latency": {{"series_count": {series_count}, "metric_type": "gauge", "sample_interval": 1000,
                "column_signs": {{"value": "non_negative"}}}}{more_sources}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap()
}

/// The plan Stage 3 selects for `queries` over `series_count` series of
/// `latency`, no raw data kept (every query reads summaries built at
/// ingestion), with accuracy enforced or ignored.
pub(crate) fn plan(
    queries: &[(&str, &str)],
    series_count: usize,
    ignore_accuracy: bool,
) -> PhysicalASAPDAG {
    selection(queries, series_count, ignore_accuracy).0
}

/// [`plan`], with the accepted accuracy Stage 3 reports per query.
fn selection(
    queries: &[(&str, &str)],
    series_count: usize,
    ignore_accuracy: bool,
) -> (PhysicalASAPDAG, Vec<String>) {
    let run = planning_run(workload(queries, series_count), ignore_accuracy);
    let (plan, selection) = run.selection.as_ref().expect("a plan is selected");
    let accepted = selection
        .accepted_accuracy_per_query
        .iter()
        .map(ToString::to_string)
        .collect();
    (plan.clone(), accepted)
}

/// Every planning stage over `workload`, no raw data kept, with accuracy
/// enforced or ignored.
fn planning_run(workload: PlanningWorkload, ignore_accuracy: bool) -> PlanningRun {
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        // Sketch-specific: compressed samples would answer exactly.
        excluded_candidates: vec![CandidateExclusion {
            summary_family: Some(SummaryFamily::CompressedSamples),
            ..Default::default()
        }],
        ..Default::default()
    };
    let models = PlanningModels::builtin()
        .with_deployment_capabilities(&capabilities)
        .with_ignore_accuracy_guarantees(ignore_accuracy);
    let roots = lower_promql_roots(&workload).unwrap();
    run_planning_stages_over(roots, &entries, &data, models)
        .unwrap()
        .run
}

/// The failure probability each readout the selected plan reads was
/// required to meet (`CandidateForMemoGroup::required_accuracy`), in node
/// order.
fn readout_failure_probabilities(run: &PlanningRun) -> Vec<f64> {
    let (plan, _) = run.selection.as_ref().expect("a plan is selected");
    let physical = run.physical_asap_candidates.as_ref().expect("Stage 2 ran");
    plan.nodes
        .iter()
        .filter(|node| matches!(node.payload, Operator::ASAP(ASAPOp::SummaryEstimate { .. })))
        .filter_map(|node| {
            let candidate = physical.physical_candidates[node.selected_physical_candidate?.0]
                .logical_candidate_id;
            let required = physical
                .logical_candidates
                .memo
                .candidate(candidate)
                .required_accuracy()?;
            Some(required.failure_probability)
        })
        .collect()
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
    let (plan, accepted) = selection(&PROBLEM_EXAMPLE, LATENCY_SERIES, false);
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
    assert_eq!(
        algorithms(&plan(&PROBLEM_EXAMPLE, LATENCY_SERIES, false)),
        2
    );
    let (ignored, accepted) = selection(&PROBLEM_EXAMPLE, LATENCY_SERIES, true);
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
        LATENCY_SERIES,
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
        LATENCY_SERIES,
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

/// The p90 of `latency{side="a"}` or of `latency{side="b"}`, over the
/// whole result at relative 0.05 with δ = 0.01. Stage 0 bounds the result
/// by the series both branches read, 2,000, and the readouts read 2,000
/// estimates, one per output value: every estimate read bounds it, so
/// Pass 2 gives each readout 0.01 / 2,000 = 5e-6 and (K) accepts them
/// (2,000 × 5e-6 = 0.01). Adding both sides' δ per output value against
/// 0.01 / 2,000 left this query no sketch plan.
#[test]
fn an_or_of_two_quantiles_reads_sketches() {
    let run = planning_run(
        workload(
            &[(
                r#"quantile_over_time(0.9, latency{side="a"}[5m]) or quantile_over_time(0.9, latency{side="b"}[5m])"#,
                r#"{"accuracy_metric": "relative", "max_error": 0.05, "failure_probability": 0.01}"#,
            )],
            LATENCY_SERIES,
        ),
        false,
    );
    let (_, selection) = run.selection.as_ref().expect("a plan is selected");
    assert_eq!(
        selection.accepted_accuracy_per_query[0].to_string(),
        "accepted relative (DDSketch)"
    );
    let levels = readout_failure_probabilities(&run);
    assert!(!levels.is_empty());
    for level in levels {
        assert!((level - 5e-6).abs() <= 1e-12, "{level}");
    }
}

/// `max_over_time(latency[5m]) * on() group_left
/// quantile_over_time(0.99, reference[5m])`, `reference` one series
/// matched onto each of `latency`'s 1,000 (read exactly), over the
/// whole result at relative 0.01 with δ = 0.01: every output value reads
/// the one estimate, so the readout keeps δ = 0.01 and (K) accepts it.
/// Splitting δ over the 1,000 output values left it no sketch plan.
#[test]
fn a_quantile_matched_onto_every_series_reads_a_sketch() {
    let run = planning_run(
        workload_with_sources(
            &[(
                "max_over_time(latency[5m]) * on() group_left quantile_over_time(0.99, reference[5m])",
                r#"{"accuracy_metric": "relative", "max_error": 0.02, "failure_probability": 0.01}"#,
            )],
            LATENCY_SERIES,
            r#", "reference": {"series_count": 1, "metric_type": "gauge", "sample_interval": 1000,
                "column_signs": {"value": "non_negative"}}"#,
        ),
        false,
    );
    let (_, selection) = run.selection.as_ref().expect("a plan is selected");
    assert_eq!(
        selection.accepted_accuracy_per_query[0].to_string(),
        "accepted relative (DDSketch)"
    );
    assert_eq!(readout_failure_probabilities(&run), [0.01]);
}

/// The p90 of `latency` per value at δ = 0.01 (its own level 0.01) and
/// `max` of it over the whole result at δ = 0.01 (its own level 1e-5):
/// `max` reads all 1,000 estimates, so the readout it reads must fail
/// with at most 1e-5, whatever level the per-value query was given.
#[test]
fn a_max_reads_the_level_its_own_split_gives() {
    let run = planning_run(
        workload(
            &[
                (
                    "quantile_over_time(0.9, latency[5m])",
                    r#"{"accuracy_metric": "relative", "max_error": 0.05, "failure_probability": 0.01, "accuracy_scope": "per_value"}"#,
                ),
                (
                    "max(quantile_over_time(0.9, latency[5m]))",
                    r#"{"accuracy_metric": "relative", "max_error": 0.05, "failure_probability": 0.01}"#,
                ),
            ],
            LATENCY_SERIES,
        ),
        false,
    );
    let levels = readout_failure_probabilities(&run);
    assert!(!levels.is_empty());
    assert!(
        levels.iter().all(|&level| level <= 1e-5 * (1.0 + 1e-9)),
        "{levels:?}"
    );
}
