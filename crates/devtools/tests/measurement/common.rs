//! Fixtures shared by the `asap_run` tests: workloads, hand-made samples
//! and planning through the facade.

use std::collections::BTreeMap;
use std::path::PathBuf;

use asap_accuracy::AccuracyEvidence;
use asap_cost::CostInputs;
use asap_devtools::measurement::samples::{Sample, METRIC_NAME_LABEL};
use asap_planner::{
    DeploymentCapabilities, InstallablePlan, PlanningModels, PlanningWorkload, SelectedPlanReport,
};
use serde_json::{json, Value};

/// An exact accuracy requirement.
pub fn exact() -> Value {
    json!({"max_error": 0.0, "failure_probability": 0.0})
}

/// Within `max_error` in `metric`, per value, failing with probability at
/// most `failure_probability`.
pub fn per_value(metric: &str, max_error: f64, failure_probability: f64) -> Value {
    json!({
        "accuracy_metric": metric,
        "max_error": max_error,
        "failure_probability": failure_probability,
        "accuracy_scope": "per_value",
    })
}

/// Within `max_error` in `metric` for the whole result.
pub fn whole_result(metric: &str, max_error: f64, failure_probability: f64) -> Value {
    json!({
        "accuracy_metric": metric,
        "max_error": max_error,
        "failure_probability": failure_probability,
        "accuracy_scope": "whole_result",
    })
}

/// The facts of a source of `series` series sampled every
/// `sample_interval_ms`.
pub fn facts(series: u64, sample_interval_ms: u64) -> Value {
    json!({"series_count": series, "sample_interval": sample_interval_ms})
}

/// Repeating PromQL `queries` (each with its accuracy), every
/// `every_ms`, over `sources` (metric name → facts). The data arrives at
/// the rate the facts declare (each source's `series_count` per
/// `sample_interval`), unknown when none declares one.
pub fn workload(
    queries: &[(&str, Value)],
    every_ms: u64,
    sources: &[(&str, Value)],
) -> PlanningWorkload {
    let rows_per_second: f64 = sources
        .iter()
        .filter_map(|(_, facts)| {
            Some(facts["series_count"].as_f64()? * 1_000.0 / facts["sample_interval"].as_f64()?)
        })
        .sum();
    let arrival = match rows_per_second > 0.0 {
        true => json!({"ingesting": {"rows_per_second": rows_per_second}}),
        false => json!("unknown"),
    };
    let value = json!({
        "query_workload": {"language": "promql", "query_batch": null, "repeating_queries":
            queries.iter().map(|(query, accuracy)| json!({
                "query": query,
                "demand": {"fixed_interval_at": {"interval": every_ms, "evaluation_phase": 0}},
                "requirements": {"accuracy": accuracy},
                "predictability": {"predictable": {"known_at": null}},
            })).collect::<Vec<_>>()},
        "data_workload": {
            "arrival": arrival,
            "sources": sources.iter().map(|(name, facts)| (name.to_string(), facts.clone()))
                .collect::<serde_json::Map<_, _>>(),
        },
    });
    serde_json::from_value(value).unwrap()
}

/// One sample of metric `metric` with `labels`.
pub fn sample(metric: &str, labels: &[(&str, &str)], ts_ms: i64, value: f64) -> Sample {
    let mut all: BTreeMap<String, String> = labels
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    all.insert(METRIC_NAME_LABEL.into(), metric.into());
    Sample {
        labels: all,
        ts_ms,
        value,
    }
}

/// The `job` label set of an answer.
pub fn job(name: &str) -> BTreeMap<String, String> {
    [("job".to_string(), name.to_string())].into()
}

/// Four 1 Hz series of `m` from t = 0 to `until_ms` inclusive, of constant
/// values 1 and 2 (`job="a"`, instances 1 and 2) and 3 and 4 (`job="b"`),
/// with `value_of(series, ts_ms)` overriding a value (series 0 to 3).
pub fn four_constant_series(
    until_ms: i64,
    value_of: impl Fn(usize, i64) -> Option<f64>,
) -> Vec<Sample> {
    let series = [
        ("a", "1", 1.0),
        ("a", "2", 2.0),
        ("b", "3", 3.0),
        ("b", "4", 4.0),
    ];
    let mut samples = Vec::new();
    for ts_ms in (0..=until_ms).step_by(1_000) {
        for (index, (job, instance, value)) in series.iter().enumerate() {
            samples.push(sample(
                "m",
                &[("job", job), ("instance", instance)],
                ts_ms,
                value_of(index, ts_ms).unwrap_or(*value),
            ));
        }
    }
    samples
}

/// The facts of [`four_constant_series`].
pub fn four_series_facts() -> Value {
    json!({
        "series_count": 4,
        "sample_interval": 1000,
        "distinct_values_per_label": {"job": 2, "instance": 4},
    })
}

/// Plan `workload` as a deployment runtime does.
pub fn plan(
    workload: &PlanningWorkload,
    cost_inputs: &CostInputs,
    evidence: &dyn AccuracyEvidence,
    capabilities: &DeploymentCapabilities,
) -> (InstallablePlan, SelectedPlanReport) {
    let models = PlanningModels::new(evidence)
        .with_cost_inputs(cost_inputs)
        .with_deployment_capabilities(capabilities);
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(asap_planner::plan_installable(
            asap_planner::UserInput::new(workload, asap_planner::FrontendInput::Promql, models),
        ))
        .unwrap()
}

/// Raw data not retained: every window is kept in ingestion-time state.
pub fn no_raw_data() -> DeploymentCapabilities {
    DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    }
}

/// A fresh empty directory under the system temp directory, removed when
/// dropped.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(name: &str) -> Self {
        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "asap_run_test_{name}_{}_{call}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Whether `a` and `b` agree to the exact-arm tolerance.
// provisional answer 21: exact plans agree within 1e-9 relative.
pub fn exact_match(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}
