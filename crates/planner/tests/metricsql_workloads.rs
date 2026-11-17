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

use asap_executor::runtime::Scope;
use asap_planner::{
    plan_installable, FrontendInput, InstallablePlan, PlanningModels, PlanningWorkload, UserInput,
};

use crate::time_series::{
    answers, assert_answers_near_relative, query_roots, run, sample, time_series_tables, Answer,
    RawDataSample,
};

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

/// Data arriving at 2,000 rows per second: the plans the Planner selects
/// for these queries in production.
const INGESTING: &str = r#"{"ingesting": {"rows_per_second": 2000.0}}"#;

/// Data at rest: what [`query_answers`] runs over, since it answers each
/// root at query time from the raw samples alone. With data arriving,
/// `increase` selects windows maintained at ingestion time, which this
/// harness never fills.
const AT_REST: &str = r#""at_rest""#;

/// `query` written in `language` ("promql" or "metricsql"), every 10 s
/// within relative error 0.01, over 1,000 `requests_total` counter series
/// and 1,000 `latency` gauge series, each sampled every second, with the
/// data `arrival` ([`INGESTING`] or [`AT_REST`]).
fn workload(language: &str, query: &str, arrival: &str) -> PlanningWorkload {
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "{language}", "repeating_queries": [{{
              "query": {query:?},
              "demand": {{"fixed_interval_at": {{"interval": 10000, "evaluation_phase": 0}}}},
              "requirements": {{"accuracy": {{"max_error": 0.01}}}},
              "predictability": {{"predictable": {{"known_at": null}}}}}}]}},
            "data_workload": {{"arrival": {arrival},
              "sources": {{
                "requests_total": {{"series_count": 1000, "metric_type": "counter",
                    "sample_interval": 1000}},
                "latency": {{"series_count": 1000, "metric_type": "gauge",
                    "sample_interval": 1000}}}}}}}}"#
    ))
    .unwrap()
}

/// The built-in plan of `query` written in MetricsQL, with the data
/// `arrival`, or the error the Planner returned.
async fn plan_metricsql(query: &str, arrival: &str) -> Result<InstallablePlan, String> {
    let workload = workload("metricsql", query, arrival);
    plan_installable(UserInput::new(
        &workload,
        FrontendInput::Metricsql,
        PlanningModels::builtin(),
    ))
    .await
    .map(|(plan, _report)| plan)
    .map_err(|e| e.to_string())
}

/// The built-in plan of `query` written in PromQL, with the data `arrival`.
async fn plan_promql(query: &str, arrival: &str) -> InstallablePlan {
    let workload = workload("promql", query, arrival);
    plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql,
        PlanningModels::builtin(),
    ))
    .await
    .unwrap_or_else(|e| panic!("{query} as PromQL: {e}"))
    .0
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
            let labels = [("instance", *instance), ("job", *job)];
            samples.push(sample("requests_total", &labels, timestamp_ms, count));
            samples.push(sample("latency", &labels, timestamp_ms, latency));
        }
    }
    samples
}

/// The answers of `plan`'s one query at [`EVALUATION_MS`] over `samples`,
/// sorted by labels: the executor compiles the plan and runs its root at
/// query time over the raw samples.
fn query_answers(plan: &InstallablePlan, samples: &[RawDataSample]) -> Vec<Answer> {
    let scope = Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
    };
    let store = time_series_tables(&plan.dag, samples);
    answers(&run(&plan.dag, store, &[], &query_roots(plan), scope)[0])
}

/// Each query written in MetricsQL plans and installs: the executor
/// compiles its plan.
#[tokio::test]
async fn metricsql_queries_plan_and_install() {
    let mut failures = Vec::new();
    for query in QUERIES {
        let installed = plan_metricsql(query, INGESTING)
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
        let metricsql = plan_metricsql(query, INGESTING)
            .await
            .unwrap_or_else(|e| panic!("{query} as MetricsQL: {e}"));
        let promql = plan_promql(query, INGESTING).await;
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
        let metricsql = plan_metricsql(query, AT_REST)
            .await
            .unwrap_or_else(|e| panic!("{query} as MetricsQL: {e}"));
        let metricsql_answers = query_answers(&metricsql, &samples);
        assert_eq!(
            metricsql_answers,
            query_answers(&plan_promql(query, AT_REST).await, &samples),
            "{query}"
        );
        let Some(expected) = expected else { continue };
        assert_answers_near_relative(&metricsql_answers, &expected, 1e-9);
    }
}

/// `avg by (job)(latency)` in MetricsQL averages each job's series at T, as
/// its PromQL form does: `j1` holds series `a` (latency 4 at step 32) and
/// `b` (14), so 9; `j2` holds `c` (24) and `d` (34), so 29. A plan whose
/// rows lack the series identity plans and runs without any error, yet
/// answers a different average, so this checks the values, not only that
/// the query plans.
#[tokio::test]
async fn metricsql_grouped_average_averages_every_series_of_each_group() {
    let query = "avg by (job)(latency)";
    let job = |name: &str| BTreeMap::from([("job".to_string(), name.to_string())]);
    let expected = vec![(job("j1"), 9.0), (job("j2"), 29.0)];
    let samples = samples();
    let metricsql = plan_metricsql(query, AT_REST)
        .await
        .unwrap_or_else(|e| panic!("{query} as MetricsQL: {e}"));
    let metricsql_answers = query_answers(&metricsql, &samples);
    assert_answers_near_relative(&metricsql_answers, &expected, 1e-9);
    assert_eq!(
        metricsql_answers,
        query_answers(&plan_promql(query, AT_REST).await, &samples)
    );
}

/// `query` in MetricsQL at T over [`samples`]: its answers, after checking
/// that they equal the PromQL form's answers and that both languages select
/// the same plan with data arriving.
async fn metricsql_answers_checked_against_promql(query: &str) -> Vec<Answer> {
    let metricsql = plan_metricsql(query, INGESTING)
        .await
        .unwrap_or_else(|e| panic!("{query} as MetricsQL: {e}"));
    assert_eq!(
        metricsql.dag,
        plan_promql(query, INGESTING).await.dag,
        "{query}"
    );
    let samples = samples();
    let metricsql = plan_metricsql(query, AT_REST)
        .await
        .unwrap_or_else(|e| panic!("{query} as MetricsQL: {e}"));
    let answers = query_answers(&metricsql, &samples);
    assert_eq!(
        answers,
        query_answers(&plan_promql(query, AT_REST).await, &samples),
        "{query}"
    );
    answers
}

/// A series' labels from `(name, value)` pairs.
fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// Binary operators with `on`/`ignoring`, `group_left`, the set operators
/// and `bool` answer what PromQL answers, and the values worked out by hand
/// from [`samples`] at T, where `latency` is 4, 14, 24 and 34 for series
/// `a`..`d` (jobs `j1`, `j1`, `j2`, `j2`): the per-job sums are 18 and 58,
/// the averages 9 and 29, and each counter's 5 m rate is `(index + 1) / 15`.
#[tokio::test]
async fn metricsql_vector_matching_answers_as_promql_does() {
    let a = labels(&[("instance", "a"), ("job", "j1")]);
    let b = labels(&[("instance", "b"), ("job", "j1")]);
    let c = labels(&[("instance", "c"), ("job", "j2")]);
    let d = labels(&[("instance", "d"), ("job", "j2")]);
    let j1 = labels(&[("job", "j1")]);
    let j2 = labels(&[("job", "j2")]);
    let named = |series: &BTreeMap<String, String>| {
        let mut series = series.clone();
        series.insert("__name__".to_string(), "latency".to_string());
        series
    };
    let cases: Vec<(&str, Vec<Answer>)> = vec![
        // Each series' share of its job's total.
        (
            "latency / on(job) group_left sum by (job)(latency)",
            vec![
                (a.clone(), 4.0 / 18.0),
                (b.clone(), 14.0 / 18.0),
                (c.clone(), 24.0 / 58.0),
                (d.clone(), 34.0 / 58.0),
            ],
        ),
        // Each series' distance from its job's average.
        (
            "latency - ignoring(instance) group_left avg by (job)(latency)",
            vec![
                (a.clone(), -5.0),
                (b.clone(), 5.0),
                (c.clone(), -5.0),
                (d.clone(), 5.0),
            ],
        ),
        // Per job, the total rate over the rate of instances a and c:
        // j1 (1 + 2) / 1, j2 (3 + 4) / 3.
        (
            r#"sum by (job)(rate(requests_total[5m])) / on(job) group_left sum by (job)(rate(requests_total{instance=~"a|c"}[5m]))"#,
            vec![(j1.clone(), 3.0), (j2.clone(), 7.0 / 3.0)],
        ),
        (
            r#"latency and on(job) latency{instance="a"}"#,
            vec![(named(&a), 4.0), (named(&b), 14.0)],
        ),
        (
            r#"latency unless on(job) latency{instance="a"}"#,
            vec![(named(&c), 24.0), (named(&d), 34.0)],
        ),
        // Plans before this change too (no written modifier): checks parity.
        (
            r#"latency{job="j1"} or latency{job="j2"}"#,
            vec![
                (named(&a), 4.0),
                (named(&b), 14.0),
                (named(&c), 24.0),
                (named(&d), 34.0),
            ],
        ),
        (
            "latency > bool 10",
            vec![
                (a.clone(), 0.0),
                (b.clone(), 1.0),
                (c.clone(), 1.0),
                (d.clone(), 1.0),
            ],
        ),
        // j1: 14 > 4 * 3; j2: 34 > 24 * 3 is false.
        (
            "max by (job)(latency) > bool on(job) min by (job)(latency) * 3",
            vec![(j1.clone(), 1.0), (j2.clone(), 0.0)],
        ),
        // `bool` alone between two vectors matches on every label.
        (
            "max by (job)(latency) > bool min by (job)(latency) * 3",
            vec![(j1.clone(), 1.0), (j2.clone(), 0.0)],
        ),
        // The right side is the many side and keeps its labels: each series
        // times its job's largest latency (14 for j1, 34 for j2).
        (
            "max by (job)(latency) * on(job) group_right latency",
            vec![
                (a.clone(), 56.0),
                (b.clone(), 196.0),
                (c.clone(), 816.0),
                (d.clone(), 1156.0),
            ],
        ),
        // `on()` matches on no label: every series against the one total, 76.
        (
            "latency / on() group_left sum(latency)",
            vec![
                (a.clone(), 4.0 / 76.0),
                (b.clone(), 14.0 / 76.0),
                (c.clone(), 24.0 / 76.0),
                (d.clone(), 34.0 / 76.0),
            ],
        ),
    ];
    for (query, expected) in cases {
        let answers = metricsql_answers_checked_against_promql(query).await;
        assert_answers_near_relative(&answers, &expected, 1e-9);
    }
}
