//! A MetricsQL workload plans, installs and answers as its PromQL form
//! does: both languages read time series, so each query root carries every
//! series' full identity as a column before Stage 0, whichever of the two
//! frontends lowered it.
//!
//! The workload: 1,000 series per metric sampled every second, each query
//! every 10 s within relative error 0.01. Without the series identity a
//! MetricsQL `rate`, `increase` or `max_over_time` root has no valid plan
//! ("per-entity summary requires complete source identity") and `sum` over
//! an instant selector selects a plan whose output type differs from the
//! query's.

use std::collections::BTreeMap;
use std::sync::Arc;

use asap_executor::physical_planner::compile;
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::values::Batch;
use asap_planner::{
    plan_installable, FrontendInput, InstallablePlan, PlanningModels, PlanningWorkload, UserInput,
};
use futures::{executor::block_on, StreamExt};

use crate::time_series::{answers, time_series_tables, Answer, RawDataSample};

/// When every query is evaluated: the end of the first hour.
const EVALUATION_MS: i64 = 3_600_000;

/// Queries both languages write the same way, each over one of the two
/// metrics [`workload`] declares.
const QUERIES: [&str; 4] = [
    "sum by (job)(rate(requests_total[5m]))",
    "increase(requests_total[5m])",
    "max_over_time(latency[5m])",
    r#"sum(latency{job="j1"})"#,
];

/// `query` written in `language` ("promql" or "metricsql"), every 10 s
/// within relative error 0.01, over 1,000 `requests_total` counter series
/// and 1,000 `latency` gauge series, each sampled every second.
fn workload(language: &str, query: &str) -> PlanningWorkload {
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "{language}", "repeating_queries": [{{
              "query": {query:?},
              "demand": {{"fixed_interval_at": {{"interval": 10000, "evaluation_phase": 0}}}},
              "requirements": {{"accuracy": {{"max_error": 0.01}}}},
              "predictability": {{"predictable": {{"known_at": null}}}}}}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 2000.0}}}},
              "sources": {{
                "requests_total": {{"series_count": 1000, "metric_type": "counter",
                    "sample_interval": 1000}},
                "latency": {{"series_count": 1000, "metric_type": "gauge",
                    "sample_interval": 1000}}}}}}}}"#
    ))
    .unwrap()
}

/// The built-in plan of `query` written in MetricsQL, or the error the
/// Planner returned.
async fn plan_metricsql(query: &str) -> Result<InstallablePlan, String> {
    let workload = workload("metricsql", query);
    plan_installable(UserInput::new(
        &workload,
        FrontendInput::Metricsql,
        PlanningModels::builtin(),
    ))
    .await
    .map(|(plan, _report)| plan)
    .map_err(|e| e.to_string())
}

/// The built-in plan of `query` written in PromQL.
async fn plan_promql(query: &str) -> InstallablePlan {
    let workload = workload("promql", query);
    plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql,
        PlanningModels::builtin(),
    ))
    .await
    .unwrap_or_else(|e| panic!("{query} as PromQL: {e}"))
    .0
}

/// One sample of `metric` for series `(instance, job)`.
fn sample(metric: &str, instance: &str, job: &str, timestamp_ms: i64, value: f64) -> RawDataSample {
    RawDataSample {
        labels: BTreeMap::from([
            ("__name__".to_string(), metric.to_string()),
            ("instance".to_string(), instance.to_string()),
            ("job".to_string(), job.to_string()),
        ]),
        timestamp_ms,
        value,
    }
}

/// Four series of each metric (instances `a`..`d`, jobs `j1` and `j2`),
/// sampled every 15 s from 8 minutes before [`EVALUATION_MS`] through it:
/// `requests_total` grows by `index + 1` per sample, and `latency` cycles
/// through `index * 10 + step % 7`.
fn samples() -> Vec<RawDataSample> {
    let series = [("a", "j1"), ("b", "j1"), ("c", "j2"), ("d", "j2")];
    let mut samples = Vec::new();
    for (index, (instance, job)) in series.iter().enumerate() {
        for step in 0..=32i64 {
            let timestamp_ms = EVALUATION_MS - 480_000 + step * 15_000;
            let count = (index as f64 + 1.0) * step as f64;
            let latency = index as f64 * 10.0 + (step % 7) as f64;
            samples.push(sample("requests_total", instance, job, timestamp_ms, count));
            samples.push(sample("latency", instance, job, timestamp_ms, latency));
        }
    }
    samples
}

/// The answers of `plan`'s one query at [`EVALUATION_MS`] over `samples`,
/// sorted by labels: the executor compiles the plan and runs its root at
/// query time over the raw samples.
fn query_answers(plan: &InstallablePlan, samples: &[RawDataSample]) -> Vec<Answer> {
    let roots: Vec<u64> = plan.dag.roots.iter().map(|&root| root as u64).collect();
    let compiled = compile(&plan.dag, &[], &roots).expect("the executor compiles the plan");
    let instance = compiled
        .instantiate(Arc::new(time_series_tables(&plan.dag, samples)))
        .unwrap();
    let batches: Vec<Batch> = block_on(async {
        let scope = Scope::Query {
            evaluation_time_ms: EVALUATION_MS,
        };
        let context = RunContext::new(scope, Limits::default()).unwrap();
        let mut output = instance
            .execute(compiled.roots(), context)
            .unwrap()
            .remove(0);
        let mut batches = Vec::new();
        while let Some(batch) = output.next().await {
            batches.push((*batch.unwrap()).clone());
        }
        batches
    });
    answers(&batches)
}

/// Each query written in MetricsQL plans and installs: the executor
/// compiles its plan.
#[tokio::test]
async fn metricsql_queries_plan_and_install() {
    let mut failures = Vec::new();
    for query in QUERIES {
        let installed = plan_metricsql(query)
            .await
            .and_then(|plan| plan.check_compiles());
        if let Err(error) = installed {
            failures.push(format!("{query}: {error}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Each query's MetricsQL plan is its PromQL plan: the same selected DAG,
/// node for node.
#[tokio::test]
async fn metricsql_queries_select_the_promql_plan() {
    for query in QUERIES {
        let metricsql = plan_metricsql(query)
            .await
            .unwrap_or_else(|e| panic!("{query} as MetricsQL: {e}"));
        let promql = plan_promql(query).await;
        assert_eq!(metricsql.dag, promql.dag, "{query}");
    }
}

/// Each query's MetricsQL plan answers what its PromQL plan answers on the
/// same samples, and both match the answers worked out by hand from
/// [`samples`] at T. `increase` over 5 m sees 20 samples per series,
/// `(index + 1) * 19` apart over 285 s, extrapolated to the 300 s window:
/// `(index + 1) * 20`. `max_over_time` over 5 m sees steps 13..=32, whose
/// largest `step % 7` is 6. `sum(latency{job="j1"})` reads each `j1`
/// series' sample at T (step 32, `32 % 7` = 4): 4 + 14 = 18. The grouped
/// counter query answers no rows in either language today (issue #1338),
/// so it checks only that the two languages agree.
#[tokio::test]
async fn metricsql_queries_answer_as_promql_does() {
    let series = |instance: &str, job: &str| {
        BTreeMap::from([
            ("instance".to_string(), instance.to_string()),
            ("job".to_string(), job.to_string()),
        ])
    };
    let per_series = |values: [f64; 4]| -> Option<Vec<Answer>> {
        let names = [("a", "j1"), ("b", "j1"), ("c", "j2"), ("d", "j2")];
        Some(
            names
                .iter()
                .zip(values)
                .map(|(&(i, j), v)| (series(i, j), v))
                .collect(),
        )
    };
    // Each query's hand-worked answers; `None` where only the languages'
    // agreement is checked.
    let expected: [Option<Vec<Answer>>; 4] = [
        None,
        per_series([20.0, 40.0, 60.0, 80.0]),
        per_series([6.0, 16.0, 26.0, 36.0]),
        Some(vec![(BTreeMap::new(), 18.0)]),
    ];
    let samples = samples();
    for (query, expected) in QUERIES.into_iter().zip(expected) {
        let metricsql = plan_metricsql(query)
            .await
            .unwrap_or_else(|e| panic!("{query} as MetricsQL: {e}"));
        let metricsql_answers = query_answers(&metricsql, &samples);
        assert_eq!(
            metricsql_answers,
            query_answers(&plan_promql(query).await, &samples),
            "{query}"
        );
        let Some(expected) = expected else { continue };
        assert_eq!(
            metricsql_answers.len(),
            expected.len(),
            "{query}: {metricsql_answers:?}"
        );
        for (answer, expected) in metricsql_answers.iter().zip(&expected) {
            assert_eq!(answer.0, expected.0, "{query}: {metricsql_answers:?}");
            assert!(
                (answer.1 - expected.1).abs() <= 1e-9 * expected.1,
                "{query}: {metricsql_answers:?}"
            );
        }
    }
}
