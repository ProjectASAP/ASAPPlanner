//! #1207 PR 8: constraint (K), each query's composed accuracy at its root,
//! end to end through the facade on #1207's problem example.

use asap_frontend_promql::promql_metric_names;
use asap_planner::{e2e_plan, FrontendInput, PlanOutput, PlanningModels, UserInput};
use asap_types::ir::properties::{AccuracyMetric, AccuracyScope, AccuracyTarget};
use asap_types::ir::schema::SketchParams;
use asap_types::workload::{
    DataArrival, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload, RepeatedDemand, RepeatingEntry, RepetitionInterval,
    TimestampMs,
};

/// PromQL queries with their accuracy targets, every 10 s at phase 0, over
/// stored samples of `latency` (1,000 series sampled every 15 s, values
/// declared non-negative).
fn workload(queries: &[(&str, AccuracyTarget)]) -> PlanningWorkload {
    let mut data = DataWorkload {
        arrival: DataArrival::AtRest,
        ..DataWorkload::sampled_every(
            queries
                .iter()
                .flat_map(|(query, _)| promql_metric_names(query).unwrap_or_default()),
            DurationMs(15_000),
        )
        .with_series_count(1_000)
    };
    for facts in data.sources.values_mut() {
        facts.values_non_negative = true;
    }
    PlanningWorkload {
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
        data_workload: Some(data),
    }
}

async fn plan(queries: &[(&str, AccuracyTarget)], ignore_accuracy_guarantees: bool) -> PlanOutput {
    let workload = workload(queries);
    let models =
        PlanningModels::builtin().with_ignore_accuracy_guarantees(ignore_accuracy_guarantees);
    e2e_plan(UserInput::new(&workload, FrontendInput::Promql, models))
        .await
        .expect("workload plans")
}

/// #1207's problem example: A, the p95 at rank error 0.01, and B, the
/// ratio of the p99 and the p50 at relative error 0.03.
fn problem_example() -> [(&'static str, AccuracyTarget); 2] {
    [
        (
            "quantile_over_time(0.95, latency[1m])",
            AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Rank),
        ),
        (
            "quantile_over_time(0.99, latency[1m]) / quantile_over_time(0.5, latency[1m])",
            AccuracyTarget::max_error(0.03).accuracy_metric(AccuracyMetric::Relative),
        ),
    ]
}

/// The θ of the selected plan's sketch builds.
fn sketch_parameters(output: &PlanOutput) -> Vec<SketchParams> {
    output
        .realized_plan
        .as_ref()
        .expect("a realized plan")
        .plan
        .dag
        .nodes
        .iter()
        .filter_map(|node| node.parameters.clone())
        .collect()
}

/// A meets rank error 0.01 with a KLL (a DDSketch readout's relative error
/// does not meet a rank target); B reads one DDSketch through both
/// operands, with `alpha` small enough that the ratio's composed bound
/// $(\alpha + \alpha) / (1 - 0.1)$ is within 0.03, which `alpha` 0.02 (each
/// operand alone within 0.03) is not.
#[tokio::test]
async fn each_query_meets_its_target_at_its_root() {
    let output = plan(&problem_example(), false).await;
    let selection = output.selection.as_ref().expect("a plan is selected");
    let accepted = &selection.accepted_accuracy_per_query;
    assert_eq!(accepted[0].accepted_metric, Some(AccuracyMetric::Rank));
    assert_eq!(accepted[0].sketches, ["Kll"]);
    assert_eq!(accepted[1].accepted_metric, Some(AccuracyMetric::Relative));
    assert_eq!(accepted[1].sketches, ["DDSketch"]);
    let parameters = sketch_parameters(&output);
    let alphas: Vec<f64> = parameters
        .iter()
        .filter_map(|p| match p {
            SketchParams::DDSketch { alpha } => Some(*alpha),
            _ => None,
        })
        .collect();
    assert_eq!(
        alphas.len(),
        1,
        "one DDSketch for both operands: {parameters:?}"
    );
    assert!((alphas[0] + alphas[0]) / (1.0 - 0.1) <= 0.03, "{alphas:?}");
}

/// Ignoring accuracy guarantees, (K) applies to no query: every output is
/// accepted regardless of its guarantee.
#[tokio::test]
async fn ignoring_accuracy_guarantees_drops_k() {
    let ignored = plan(&problem_example(), true).await;
    let selection = ignored.selection.expect("a plan");
    assert!(selection
        .accepted_accuracy_per_query
        .iter()
        .all(|accepted| accepted.accepted_regardless_of_guarantee));
}

/// The listed sizes fail with probability 0.01 per value, so a
/// whole-result target of 0.01 over 1,000 series allows 1e-5 per value:
/// no sketch meets it, and the plan reads the raw data exactly.
#[tokio::test]
async fn a_whole_result_failure_probability_is_divided_over_the_values() {
    let output = plan(
        &[(
            "quantile_over_time(0.95, latency[1m])",
            AccuracyTarget::max_error(0.01)
                .accuracy_metric(AccuracyMetric::Rank)
                .failure_probability(0.01)
                .accuracy_scope(AccuracyScope::WholeResult),
        )],
        false,
    )
    .await;
    let selection = output.selection.as_ref().expect("a plan is selected");
    assert_eq!(
        selection.accepted_accuracy_per_query[0].accepted_metric,
        None
    );
    assert!(sketch_parameters(&output).is_empty());
}
