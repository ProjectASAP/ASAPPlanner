//! A deployment runtime installs and runs the Planner's plan with public
//! APIs only: `asap_planner` to plan, `asap_executor` to execute (issue
//! #749). The executor links here without its `test-support` feature, so
//! the compiler checks that no internal is used (CI runs
//! `cargo check -p asap-planner --tests --locked` on its own for that).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::time_series;
use asap_executor::runtime::Scope;
use asap_executor::values::Batch;
use asap_planner::{
    plan_installable, CostInputs, DeploymentCapabilities, FrontendInput, InstallablePlan,
    OperatorKind, PlanningModels, PlanningWorkload, SelectedPlanReport, UserInput,
};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};

use time_series::{
    answers, assert_answers_near, labels, query_roots, run, sample, time_series_tables, Answer,
    RawDataSample,
};

/// Example 4b's evaluation time: the end of the 60-minute window.
const EVALUATION_MS: i64 = 3_600_000;

/// Repeating PromQL `queries`, each every 10 minutes at rank error 0.01 and
/// δ 0.01, over 1,000 ingesting `latency_ms` series sampled every second:
/// #509 Example 4b when the one query is its p99.
fn workload(queries: &[&str]) -> PlanningWorkload {
    let every_10_minutes: Vec<(&str, u64)> =
        queries.iter().map(|&query| (query, 600_000)).collect();
    workload_every(&every_10_minutes)
}

/// [`workload`] with each query repeating every `interval_ms` of its own, at
/// phase 0.
fn workload_every(queries: &[(&str, u64)]) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|(query, interval_ms)| {
            format!(
                r#"{{"query": {query:?}, "demand": {{"fixed_interval_at": {{"interval": {interval_ms}, "evaluation_phase": 0}}}},
                  "requirements": {{"accuracy": {{"accuracy_metric": "rank", "max_error": 0.01,
                      "failure_probability": 0.01, "accuracy_scope": "whole_result"}},
                      "max_latency_ms": 200.0}},
                  "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 1000.0}}}},
              "sources": {{"latency_ms": {{"series_count": 1000, "sample_interval": 1000}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap()
}

/// The plan for `queries` on a deployment that keeps no raw samples, and
/// Stage 3's report of the selection it made (which the plan file does not
/// carry).
async fn planned_with_report(queries: &[&str]) -> (InstallablePlan, SelectedPlanReport) {
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    plan_with_report_for(workload(queries), &capabilities).await
}

/// The one Planner call for `workload` on a deployment with `capabilities`:
/// the selected plan, ready to install, and Stage 3's report.
async fn plan_with_report_for(
    workload: PlanningWorkload,
    capabilities: &DeploymentCapabilities,
) -> (InstallablePlan, SelectedPlanReport) {
    // The deployment's unit costs, from its JSON file: cost model 1.
    let settings: CostInputs = serde_json::from_str(
        r#"{"cost_model": {"usage_over_time": {
              "cost_per_cpu_op": 1e-6, "cost_per_scanned_byte": 1e-7,
              "cost_per_memory_byte_second": 1.25e-7,
              "cost_per_provisioned_memory_byte_second": 1.25e-7,
              "cost_per_provisioned_cpu_core_second": 1000.0,
              "cost_per_disk_byte_second": 0.0}}}"#,
    )
    .unwrap();
    // The models Stages 1-3 read: built-in accuracy, the deployment inputs.
    let models = PlanningModels::builtin()
        .with_deployment_capabilities(capabilities)
        .with_cost_inputs(&settings);
    // The one Planner call: the selected plan, ready to install.
    plan_installable(UserInput::new(&workload, FrontendInput::Promql, models))
        .await
        .unwrap()
}

/// The plan file carries Stage 3's predicted usage of the selected plan, so
/// a runtime reports it against what it measures without planning again:
/// for Example 4b's tumbling windows maintained at ingestion time, its nodes' costs
/// add up to Stage 3's selection cost and their CPU to the plan's, with
/// one latency per query, each node naming the installed nodes that run it.
#[tokio::test]
async fn the_plan_carries_stage3s_predicted_usage() {
    let (plan, selection) =
        planned_with_report(&["quantile_over_time(0.99, latency_ms[60m])"]).await;
    let views = plan.materialized_views().unwrap();
    assert!(!views.is_empty(), "a tumbling-window-only plan");
    assert!(
        plan.raw_data_retention().unwrap().is_empty(),
        "a tumbling-window-only plan"
    );
    let usage = &selection.predicted_usage;
    let node_costs: f64 = usage
        .per_node
        .iter()
        .map(|node| node.cost_per_second.unwrap())
        .sum();
    let selection_cost = selection.cost_per_second.unwrap();
    assert!(
        (node_costs - selection_cost).abs() <= 1e-9 * selection_cost.abs(),
        "{node_costs} vs {selection_cost}"
    );
    let node_cpu: f64 = usage
        .per_node
        .iter()
        .map(|node| node.cpu_core_seconds_per_second)
        .sum();
    assert_eq!(node_cpu, usage.cpu_core_seconds_per_second);
    assert!(usage.cpu_core_seconds_per_second > 0.0);
    assert!(usage.per_node.iter().any(|node| node.operator
        == OperatorKind::SummaryTumblingWindowChain
        && node.timing == asap_planner::ExecutionTiming::IngestionTime));
    assert_eq!(usage.per_query.len(), 1);
    assert!(usage.peak_memory_bytes > 0);
    // Each prediction names its node of the installed DAG: the chain is
    // one materialized view, and the query's root has its own.
    let installed: BTreeSet<_> = plan.dag.nodes.iter().map(|node| node.id).collect();
    assert!(usage
        .per_node
        .iter()
        .all(|node| installed.contains(&node.node_id)));
    let chain = usage
        .per_node
        .iter()
        .find(|node| node.operator == OperatorKind::SummaryTumblingWindowChain)
        .unwrap();
    assert!(views.iter().any(|view| view.node_id == chain.node_id));
    let root = plan.dag.roots[0];
    assert!(usage.per_node.iter().any(|node| node.node_id == root));
}

/// Example 4b's p99 every 10 minutes and its p50 every 30 minutes: two
/// evaluation contexts. The chain of windows maintained at ingestion time
/// runs once, whoever reads it, and retains the hour both read back
/// ($h_v$, constraint (R)); each context reads it with query-time nodes of
/// its own.
#[tokio::test]
async fn one_chain_serves_two_evaluation_contexts() {
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let (plan, _) = plan_with_report_for(
        workload_every(&[
            ("quantile_over_time(0.99, latency_ms[60m])", 600_000),
            ("quantile_over_time(0.5, latency_ms[60m])", 1_800_000),
        ]),
        &capabilities,
    )
    .await;
    let chains: Vec<_> = plan
        .dag
        .nodes
        .iter()
        .filter(|node| {
            matches!(
                node.payload,
                Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
            )
        })
        .collect();
    assert_eq!(chains.len(), 1, "{:#?}", plan.dag.nodes);
    assert_eq!(chains[0].retention_ms, Some(3_600_000));
    let ranges_over_the_chain = plan
        .dag
        .nodes
        .iter()
        .filter(|node| {
            matches!(
                node.payload,
                Operator::NonASAP(NonASAPOp::TimeRange { child, .. }) if child == chains[0].id
            )
        })
        .count();
    assert_eq!(ranges_over_the_chain, 2);
    assert_ne!(plan.dag.roots[0], plan.dag.roots[1]);
}

/// `sum(latency_ms)` reads an instant selector: per series, only the latest
/// sample in its 1 s lookback counts. Series `a` has samples at T-900 ms (1)
/// and T-100 ms (10), series `b` at T-200 ms (100): the sum is 10 + 100.
#[tokio::test]
async fn an_instant_selector_reads_the_latest_sample_per_series() {
    let workload = workload(&["sum(latency_ms)"]);
    let plan = plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql,
        PlanningModels::builtin(),
    ))
    .await
    .unwrap()
    .0;
    let dag = &plan.dag;
    let [retention] = plan
        .raw_data_retention()
        .unwrap()
        .try_into()
        .unwrap_or_else(|r: Vec<_>| {
            panic!("{r:?}");
        });
    assert_eq!(retention.lookback_ms(), Some(1_000));
    let samples = [
        sample("latency_ms", &[("instance", "a")], EVALUATION_MS - 900, 1.0),
        sample(
            "latency_ms",
            &[("instance", "a")],
            EVALUATION_MS - 100,
            10.0,
        ),
        sample(
            "latency_ms",
            &[("instance", "b")],
            EVALUATION_MS - 200,
            100.0,
        ),
    ];
    let query = Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
    };
    let store = time_series_tables(dag, &samples);
    let answered = answers(&run(dag, store, &[], &query_roots(&plan), query)[0]);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].1, 110.0);
}

// ── A ratio of two relative-error quantiles (issue #852) ────────────────────

// ── Materialized views refreshed from materialized views (issue #781) ──────────────────

/// The MVP benchmark's p99 query: the per-series p99 of a gauge over 100 s,
/// averaged per (region, service, job), every second, within 0.05 at
/// failure probability 0.3, over 1,000 `data` series sampled every 100 ms,
/// with `data` declared non-negative or not.
fn p99_avg_workload(values_non_negative: bool) -> PlanningWorkload {
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{{
              "query": "avg by (region, service, job) (quantile_over_time(0.99, data[100s]))",
              "demand": {{"fixed_interval_at": {{"interval": 1000, "evaluation_phase": 0}}}},
              "requirements": {{"accuracy": {{"max_error": 0.05, "failure_probability": 0.3,
                  "accuracy_scope": "whole_result"}}}},
              "predictability": {{"predictable": {{"known_at": null}}}}}}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 10000.0}}}},
              "sources": {{"data": {{"series_count": 1000, "metric_type": "gauge",
                  "sample_interval": 100, "column_signs": {{"value": "{sign}"}}}}}}}}}}"#,
        sign = if values_non_negative {
            "non_negative"
        } else {
            "unknown"
        }
    ))
    .unwrap()
}

/// The MVP benchmark's p99 query over an undeclared gauge, which has no
/// valid plan with the built-in model, gets one with
/// `ignore_accuracy_guarantees`.
#[tokio::test]
async fn ignoring_accuracy_guarantees_plans_the_undeclared_p99_query() {
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let settings = CostInputs::default();
    let (plan, _report) = plan_installable(UserInput::new(
        &p99_avg_workload(false),
        FrontendInput::Promql,
        PlanningModels::builtin()
            .with_deployment_capabilities(&capabilities)
            .with_cost_inputs(&settings)
            .with_ignore_accuracy_guarantees(true),
    ))
    .await
    .expect("a plan is selected");
    assert!(
        plan.raw_data_retention().unwrap().is_empty(),
        "no raw samples read"
    );
}

/// The ASAPQuery workloads' counter top-k query `topk(10, sum by (job)
/// (<f>(data_total[5m])))` for `f` `increase` or `rate`, every minute
/// within 0.05 at failure probability 0.3, over 24 `data_total` counter
/// series sampled every 10 s.
fn counter_topk_workload(function: &str) -> PlanningWorkload {
    let query = format!("topk(10, sum by (job) ({function}(data_total[5m])))");
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{{
              "query": {query:?},
              "demand": {{"fixed_interval_at": {{"interval": 60000, "evaluation_phase": 0}}}},
              "requirements": {{"accuracy": {{"max_error": 0.05, "failure_probability": 0.3}}}},
              "predictability": {{"predictable": {{"known_at": null}}}}}}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 100000.0}}}},
              "sources": {{"data_total": {{"series_count": 10000, "metric_type": "counter",
                  "sample_interval": 100, "distinct_values_per_label": {{"job": 12}}}}}}}}}}"#
    ))
    .unwrap()
}

/// `InstallablePlan::check_compiles` reports what the Planner's executor
/// refuses: the counter top-k plan with its answer column's type changed
/// from Float64 to Utf8 does not compile, with the executor's reason.
#[tokio::test]
async fn check_compiles_reports_the_executors_error() {
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let (mut plan, _) = plan_installable(UserInput::new(
        &counter_topk_workload("rate"),
        FrontendInput::Promql,
        PlanningModels::builtin().with_deployment_capabilities(&capabilities),
    ))
    .await
    .unwrap();
    plan.check_compiles().unwrap();
    let root = plan.dag.roots[0];
    let node = plan
        .dag
        .nodes
        .iter_mut()
        .find(|node| node.id == root)
        .unwrap();
    let answer = node
        .output_schema
        .fields
        .iter_mut()
        .find(|field| field.name == "topk_10")
        .unwrap();
    answer.dtype = asap_executor::planner::ir::schema::FieldDataType::Plain(
        asap_executor::planner::ir::schema::DataType::Utf8,
    );
    let error = plan.check_compiles().unwrap_err();
    assert!(
        error.starts_with("the executor cannot compile the plan's query run: "),
        "{error}"
    );
}

// ── Classic-histogram `histogram_quantile` (issue #890) ─────────────────────

// ── PromQL absent() (issue #895) ─────────────────────────────────────────────

// ── Tumbling-window chains built at query time ───────────────────────────────

/// Repeating PromQL `queries`, each `(query, requirements)` every minute at
/// phase 0, over 1,000 ingesting gauge series `m` sampled every second;
/// `requirements` is the JSON inside the entry's `requirements` object
/// (empty for none).
fn every_minute_workload(queries: &[(&str, &str)]) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|(query, requirements)| {
            format!(
                r#"{{"query": {query:?}, "demand": {{"fixed_interval_at": {{"interval": 60000, "evaluation_phase": 0}}}},
                  "requirements": {{{requirements}}}, "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 1000.0}}}},
              "sources": {{"m": {{"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap()
}

/// The built-in plan for `queries` ([`every_minute_workload`]), on a
/// deployment that keeps raw data.
async fn plan_every_minute(queries: &[(&str, &str)]) -> InstallablePlan {
    let workload = every_minute_workload(queries);
    let planned = plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql,
        PlanningModels::builtin(),
    ))
    .await;
    assert!(planned.is_ok(), "{:?}", planned.err());
    planned.unwrap().0
}

/// Samples of `m` for four series (instances `a`..`d`, jobs `x` and `y`)
/// every 15 s from 8 minutes before [`EVALUATION_MS`] through it, so some
/// lie exactly on the minute boundaries that tumbling windows end at.
fn m_samples() -> Vec<RawDataSample> {
    let series = [("a", "x"), ("b", "x"), ("c", "y"), ("d", "y")];
    let mut samples = Vec::new();
    for (index, (instance, job)) in series.iter().enumerate() {
        for step in 0..=32i64 {
            let timestamp_ms = EVALUATION_MS - 480_000 + step * 15_000;
            let value = (index as f64 + 1.0) * 10.0 + step as f64;
            samples.push(sample(
                "m",
                &[("instance", instance), ("job", job)],
                timestamp_ms,
                value,
            ));
        }
    }
    samples
}

/// Per `labels(sample)`, the sum of `value(sample)` over the samples in
/// `(T - end_offset_ms - width_ms, T - end_offset_ms]` at `T` =
/// [`EVALUATION_MS`]: the exact answer of a range function over that
/// window.
fn exact_over_window(
    samples: &[RawDataSample],
    end_offset_ms: i64,
    width_ms: i64,
    labels: impl Fn(&RawDataSample) -> BTreeMap<String, String>,
    value: impl Fn(&RawDataSample) -> f64,
) -> Vec<Answer> {
    let end = EVALUATION_MS - end_offset_ms;
    let mut sums = BTreeMap::<BTreeMap<String, String>, f64>::new();
    for sample in samples {
        if end - width_ms < sample.timestamp_ms && sample.timestamp_ms <= end {
            *sums.entry(labels(sample)).or_default() += value(sample);
        }
    }
    sums.into_iter().collect()
}

/// A sample's labels without its metric name, as range functions output.
fn labels_without_name(sample: &RawDataSample) -> BTreeMap<String, String> {
    let mut labels = sample.labels.clone();
    labels.remove("__name__");
    labels
}

/// Each query root's answers of `plan` at [`EVALUATION_MS`] over `samples`,
/// every node run in the query run (no materialized view bound): a
/// tumbling-window chain is built there from the raw samples.
fn query_answers(plan: &InstallablePlan, samples: &[RawDataSample]) -> Vec<Vec<Answer>> {
    let query = Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
    };
    run(
        &plan.dag,
        time_series_tables(&plan.dag, samples),
        &[],
        &query_roots(plan),
        query,
    )
    .iter()
    .map(|batches| answers(batches))
    .collect()
}

/// `sum_over_time(m[5m] offset 1m)` and `sum_over_time(m[5m])`, every
/// minute, may share one tumbling-window chain of `m` built at query time:
/// the plan installs, a query-time `Scan` read through such a chain keeps
/// the 6 minutes its readers read back (5 m + 1 m offset, not one
/// window), and both queries answer the exact sums of their windows.
#[tokio::test]
async fn shifted_and_unshifted_sums_over_a_shared_query_time_chain_install_and_answer_exactly() {
    let plan = plan_every_minute(&[
        ("sum_over_time(m[5m] offset 1m)", ""),
        ("sum_over_time(m[5m])", ""),
    ])
    .await;
    let query_time_chain = plan.dag.nodes.iter().any(|node| {
        matches!(
            node.payload,
            asap_types::ir::Operator::ASAP(
                asap_types::ir::ASAPOp::SummaryTumblingWindowChain { .. }
            )
        ) && node.output_state.timing == asap_planner::ExecutionTiming::QueryTime
    });
    if query_time_chain {
        for node in &plan.dag.nodes {
            if matches!(
                node.payload,
                asap_types::ir::Operator::NonASAP(asap_types::ir::NonASAPOp::Scan { .. })
            ) && node.output_state.timing == asap_planner::ExecutionTiming::QueryTime
            {
                assert!(
                    node.retention_ms.is_some_and(|kept_ms| kept_ms >= 360_000),
                    "scan {} keeps {:?}",
                    node.id,
                    node.retention_ms
                );
            }
        }
    }
    let samples = m_samples();
    let answered = query_answers(&plan, &samples);
    let value = |sample: &RawDataSample| sample.value;
    assert_answers_near(
        &answered[0],
        &exact_over_window(&samples, 60_000, 300_000, labels_without_name, value),
        1e-9,
    );
    assert_answers_near(
        &answered[1],
        &exact_over_window(&samples, 0, 300_000, labels_without_name, value),
        1e-9,
    );
}

/// `sum by (job) (count_over_time(m[1m]))` and `count_over_time(m[1m])`,
/// every minute within absolute error 0.05, may share one tumbling-window
/// chain of `m` built at query time: the plan installs and both queries
/// answer their counts within 0.05.
#[tokio::test]
async fn grouped_and_per_series_counts_over_a_shared_query_time_chain_install_and_answer() {
    let accuracy = r#""accuracy": {"max_error": 0.05, "accuracy_metric": "absolute"}"#;
    let plan = plan_every_minute(&[
        ("sum by (job) (count_over_time(m[1m]))", accuracy),
        ("count_over_time(m[1m])", accuracy),
    ])
    .await;
    let samples = m_samples();
    let answered = query_answers(&plan, &samples);
    let job = |sample: &RawDataSample| {
        BTreeMap::from([("job".to_string(), sample.labels["job"].clone())])
    };
    let one = |_: &RawDataSample| 1.0;
    assert_answers_near(
        &answered[0],
        &exact_over_window(&samples, 0, 60_000, job, one),
        0.05,
    );
    assert_answers_near(
        &answered[1],
        &exact_over_window(&samples, 0, 60_000, labels_without_name, one),
        0.05,
    );
}

// ── Counter rate and increase, per series and per group ─────────────────────

/// `queries`, each every `interval_ms` within relative error 0.01, over
/// 1,000 `requests_total` counter series sampled every second, planned
/// together with the built-in models on a deployment that keeps raw data
/// or not.
async fn plan_counter_queries(
    queries: &[&str],
    interval_ms: u64,
    raw_data_retained: bool,
) -> InstallablePlan {
    let entries: Vec<String> = queries
        .iter()
        .map(|query| {
            format!(
                r#"{{"query": {query:?},
                  "demand": {{"fixed_interval_at": {{"interval": {interval_ms}, "evaluation_phase": 0}}}},
                  "requirements": {{"accuracy": {{"max_error": 0.01}}}},
                  "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    let workload: PlanningWorkload = serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 1000.0}}}},
              "sources": {{"requests_total": {{"series_count": 1000,
                  "metric_type": "counter", "sample_interval": 1000}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap();
    let capabilities = DeploymentCapabilities {
        raw_data_retained,
        ..DeploymentCapabilities::default()
    };
    let models = PlanningModels::builtin().with_deployment_capabilities(&capabilities);
    plan_installable(UserInput::new(&workload, FrontendInput::Promql, models))
        .await
        .unwrap_or_else(|e| panic!("{queries:?} (raw data retained: {raw_data_retained}): {e}"))
        .0
}

/// Four `requests_total` series (instances `a`..`d`, jobs `j1` and `j2`),
/// sampled every 15 s from 8 minutes before [`EVALUATION_MS`] through it:
/// series `i` (0-based) counts `(i + 1) * step` at step 0..=32.
fn counter_samples() -> Vec<RawDataSample> {
    let series = [("a", "j1"), ("b", "j1"), ("c", "j2"), ("d", "j2")];
    let mut samples = Vec::new();
    for (index, (instance, job)) in series.iter().enumerate() {
        for step in 0..=32i64 {
            samples.push(sample(
                "requests_total",
                &[("instance", instance), ("job", job)],
                EVALUATION_MS - 480_000 + step * 15_000,
                (index as f64 + 1.0) * step as f64,
            ));
        }
    }
    samples
}

/// The counter queries over [`counter_samples`] and their answers at T,
/// worked out by hand. Over `[5m]` each series has the 20 samples of steps
/// 13..=32 (T - 285 s .. T), `(i + 1) * 19` apart; the 15 s gap to the
/// window's start is within 1.1 sample intervals, so Prometheus
/// extrapolates the increase to the whole 300 s: `(i + 1) * 20`, and the
/// rate is that over 300 s. Job `j1` is series 0 and 1, `j2` series 2 and
/// 3: increases 20 + 40 = 60 and 60 + 80 = 140, rates 0.2 and 0.4667.
fn counter_queries() -> Vec<(&'static str, Vec<Answer>)> {
    let series = |instance: &str, job: &str| labels(&[("instance", instance), ("job", job)]);
    let job = |job: &str| labels(&[("job", job)]);
    let per_series = |scale: f64| {
        [("a", "j1"), ("b", "j1"), ("c", "j2"), ("d", "j2")]
            .iter()
            .enumerate()
            .map(|(i, (instance, j))| (series(instance, j), (i as f64 + 1.0) * 20.0 * scale))
            .collect()
    };
    let rate = 1.0 / 300.0;
    vec![
        ("increase(requests_total[5m])", per_series(1.0)),
        ("rate(requests_total[5m])", per_series(rate)),
        (
            "sum by (job)(increase(requests_total[5m]))",
            vec![(job("j1"), 60.0), (job("j2"), 140.0)],
        ),
        (
            "sum by (job)(rate(requests_total[5m]))",
            vec![(job("j1"), 60.0 * rate), (job("j2"), 140.0 * rate)],
        ),
        (
            "sum(increase(requests_total[5m]))",
            vec![(BTreeMap::new(), 200.0)],
        ),
        (
            "sum(rate(requests_total[5m]))",
            vec![(BTreeMap::new(), 200.0 * rate)],
        ),
    ]
}

/// Each query root's answers of `plan` at [`EVALUATION_MS`] as a runtime
/// that installed it answers: each materialized view (a tumbling-window
/// chain) is refreshed from `samples` in one ingestion run per tumbling
/// window its retention keeps, ending at T, and stored; the query run at
/// T reads those views (and the raw samples the plan reads at query time).
fn installed_answers(plan: &InstallablePlan, samples: &[RawDataSample]) -> Vec<Vec<Answer>> {
    let dag = &plan.dag;
    // The raw samples, then each view's stored windows.
    let mut store = time_series_tables(dag, samples);
    let views = plan.materialized_views().unwrap();
    for view in &views {
        let node = dag.nodes.iter().find(|n| n.id == view.node_id).unwrap();
        let Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            tumbling_window_width_ms,
            ..
        }) = node.payload
        else {
            panic!("only tumbling-window chains are refreshed here: {node:?}");
        };
        let width_ms = tumbling_window_width_ms as i64;
        let windows = (view.retention_ms() as i64 / width_ms).max(1);
        // The windows oldest first, each built in its own ingestion run.
        for k in (0..windows).rev() {
            let end = EVALUATION_MS - k * width_ms;
            let ingestion = Scope::Ingestion {
                window_start_ms: end - width_ms,
                window_end_ms: end,
            };
            let batches = run(
                dag,
                time_series_tables(dag, samples),
                &[],
                &[view.node_id as u64],
                ingestion,
            )
            .remove(0);
            let rows = batches.iter().flat_map(|batch| batch.to_rows()).collect();
            let schema = Arc::new(node.output_schema.clone());
            let window = Batch::try_new(schema, rows).unwrap();
            store
                .append_materialized_view(view, (end - width_ms, end), vec![window])
                .unwrap();
        }
    }
    let query = Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
    };
    run(dag, store, &views, &query_roots(plan), query)
        .iter()
        .map(|batches| answers(batches))
        .collect()
}

/// Each counter query of [`counter_queries`], planned on a deployment that
/// keeps raw data and on one that does not, answered by `answer` over
/// [`counter_samples`]: the queries whose answers differ from the
/// hand-worked ones (relative error 1e-9).
async fn counter_query_mismatches(
    answer: fn(&InstallablePlan, &[RawDataSample]) -> Vec<Vec<Answer>>,
) -> Vec<String> {
    let samples = counter_samples();
    let mut mismatches = Vec::new();
    for raw_data_retained in [true, false] {
        for (query, expected) in counter_queries() {
            let plan = plan_counter_queries(&[query], 10_000, raw_data_retained).await;
            let answered = answer(&plan, &samples).remove(0);
            let matches = answered.len() == expected.len()
                && answered.iter().zip(&expected).all(|(answer, expected)| {
                    answer.0 == expected.0 && (answer.1 - expected.1).abs() <= 1e-9 * expected.1
                });
            if !matches {
                mismatches.push(format!(
                    "{query} (raw data retained: {raw_data_retained}): {answered:?}, \
                     expected {expected:?}"
                ));
            }
        }
    }
    mismatches
}

/// Counter `rate` and `increase`, per series, per job and in total, answer
/// the hand-worked values as installed: each tumbling-window chain built
/// window by window in ingestion runs and read from its materialized view
/// at query time.
#[tokio::test]
async fn counter_rates_and_increases_answer_from_views_built_at_ingestion_time() {
    let mismatches = counter_query_mismatches(installed_answers).await;
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

/// Counter `rate` and `increase`, per series, per job and in total, answer
/// the hand-worked values when the whole plan runs in the query run from
/// the raw samples, no materialized view bound: a tumbling-window chain
/// placed at ingestion time is then built in the query run, every window
/// its readers read, not only the newest.
#[tokio::test]
async fn counter_rates_and_increases_answer_at_query_time_from_raw_samples() {
    let mismatches = counter_query_mismatches(query_answers).await;
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

/// `rate(requests_total[5m] offset 1m)` and `rate(requests_total[5m])`,
/// each every minute, share a tumbling-window chain of 1-minute windows
/// that the plan builds at query time. Installed, the query run builds it
/// from the raw samples, each update at its sample's own time: over
/// [`counter_samples`] (four samples per window) both queries answer the
/// hand-worked rates `(i + 1) * 20 / 300` per series. `(T - 6m, T - 1m]`
/// holds steps 9..=28, 20 samples as `(T - 5m, T]` does.
#[tokio::test]
async fn counter_rates_over_a_shared_query_time_chain_answer_as_installed() {
    let plan = plan_counter_queries(
        &[
            "rate(requests_total[5m] offset 1m)",
            "rate(requests_total[5m])",
        ],
        60_000,
        true,
    )
    .await;
    assert!(
        plan.dag.nodes.iter().any(|node| matches!(
            node.payload,
            asap_types::ir::Operator::ASAP(
                asap_types::ir::ASAPOp::SummaryTumblingWindowChain { .. }
            )
        ) && node.output_state.timing
            == asap_types::ir::properties::ExecutionTiming::QueryTime),
        "the plan builds a chain at query time: {:#?}",
        plan.dag.nodes
    );
    let rates: Vec<f64> = (1..=4).map(|i| i as f64 * 20.0 / 300.0).collect();
    for answered in installed_answers(&plan, &counter_samples()) {
        let values: Vec<f64> = answered.iter().map(|(_, value)| *value).collect();
        assert_eq!(values.len(), rates.len(), "{answered:?}");
        for (value, rate) in values.iter().zip(&rates) {
            assert!((value - rate).abs() <= 1e-9 * rate, "{answered:?}");
        }
    }
}

// ── Global aggregates over an aggregate's output ─────────────────────────────

/// A PromQL aggregate with no grouping keys over another aggregate's output
/// is still a PromQL aggregate: over an empty vector it answers no sample,
/// as Prometheus does, not the one row (0) a SQL `SELECT count(*)` has over
/// no rows. Every value of `m` lies above 0 and below 1,000 ([`m_samples`]),
/// so `count(sum by (job) (m >= 1000))` counts an empty vector while
/// `count(sum by (job) (m >= 0))` counts the 2 jobs, and
/// `count(count by (x) (m))` counts one group, as no series has `x`. With
/// no sample at all every query is empty. Each query is planned on its
/// own, exactly.
#[tokio::test]
async fn a_global_promql_count_over_an_empty_aggregate_answers_no_sample() {
    // The query's answer values over `samples` (an answer row has no labels).
    let values = |plan: &InstallablePlan, samples: &[RawDataSample]| -> Vec<f64> {
        let query_time = Scope::Query {
            evaluation_time_ms: EVALUATION_MS,
        };
        run(
            &plan.dag,
            time_series_tables(&plan.dag, samples),
            &[],
            &query_roots(plan),
            query_time,
        )
        .concat()
        .iter()
        .flat_map(Batch::to_rows)
        .map(|row| match row.as_slice() {
            [Value::Int64(count)] => *count as f64,
            [Value::Float64(value)] => *value,
            other => panic!("an answer row {other:?}"),
        })
        .collect()
    };
    for (query, with_samples) in [
        ("count(sum by (job) (m >= 1000))", vec![]),
        ("count(count by (x) (m))", vec![1.0]),
        ("count(sum by (job) (m >= 0))", vec![2.0]),
    ] {
        let plan = plan_every_minute(&[(query, "")]).await;
        assert_eq!(
            values(&plan, &[]),
            Vec::<f64>::new(),
            "{query} with no sample"
        );
        assert_eq!(values(&plan, &m_samples()), with_samples, "{query}");
    }
}
