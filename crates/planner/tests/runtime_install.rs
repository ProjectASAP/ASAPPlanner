//! A deployment runtime installs and runs the Planner's plan with public
//! APIs only: `asap_planner` to plan, `asap_executor` to execute (issue
//! #749). The executor links here without its `test-support` feature, so
//! the compiler checks that no internal is used (CI runs
//! `cargo check -p asap-planner --tests --locked` on its own for that).

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::time_series;
use asap_executor::physical_planner::compile;
use asap_executor::runtime::{Limits, OutputStream, RunContext, Scope};
use asap_executor::sources::{
    DataStore, InMemoryDataStore, MaterializedViewRequest, RawDataRequest,
};
use asap_executor::values::{Batch, Value};
use asap_executor::Error as ExecutorError;
use asap_planner::{
    plan_installable, CostInputs, DeploymentCapabilities, FrontendInput, InstallablePlan,
    MaterializedView, OperatorKind, PhysicalASAPDAG, PlanningModels, PlanningWorkload,
    SelectedPlanReport, UserInput,
};
use asap_types::ir::operator::Source;

use futures::{executor::block_on, StreamExt};

use time_series::{series_labels, time_series_tables, RawDataSample};

/// Example 4b's evaluation time: the end of the 60-minute window.
const EVALUATION_MS: i64 = 3_600_000;

/// Repeating PromQL `queries`, each every 10 minutes at rank error 0.01 and
/// δ 0.01, over 1,000 ingesting `latency_ms` series sampled every second:
/// #509 Example 4b when the one query is its p99.
fn workload(queries: &[&str]) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|query| {
            format!(
                r#"{{"query": {query:?}, "demand": {{"fixed_interval_at": {{"interval": 600000, "evaluation_phase": 0}}}},
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

/// One sample of `metric` for series `instance`.
fn sample(metric: &str, instance: &str, timestamp_ms: i64, value: f64) -> RawDataSample {
    RawDataSample {
        labels: BTreeMap::from([
            ("__name__".to_string(), metric.to_string()),
            ("instance".to_string(), instance.to_string()),
        ]),
        timestamp_ms,
        value,
    }
}

thread_local! {
    /// How many `read_raw_data` calls the runs on this thread made.
    static RAW_DATA_READS: Cell<usize> = const { Cell::new(0) };
}

/// A store counting its `read_raw_data` calls in [`RAW_DATA_READS`].
struct CountingStore(InMemoryDataStore);

impl DataStore for CountingStore {
    fn read_raw_data(
        &self,
        source: &Source,
        request: &RawDataRequest,
        context: RunContext,
    ) -> Result<OutputStream<'_, Batch>, ExecutorError> {
        RAW_DATA_READS.with(|reads| reads.set(reads.get() + 1));
        self.0.read_raw_data(source, request, context)
    }

    fn read_materialized_view(
        &self,
        view: &MaterializedView,
        request: &MaterializedViewRequest,
        context: RunContext,
    ) -> Result<OutputStream<'_, Batch>, ExecutorError> {
        self.0.read_materialized_view(view, request, context)
    }
}

/// Compile `ids` of `dag` and run them in `scope` over the raw `samples`,
/// as a runtime does with the executor: each id's output batches.
fn run(
    dag: &PhysicalASAPDAG,
    samples: &[RawDataSample],
    ids: &[u64],
    scope: Scope,
) -> Vec<Vec<Batch>> {
    // The executor's plan for `ids`; every `Scan` it reaches is a raw data
    // slot.
    let compiled = compile(dag, &[], ids).expect("the executor compiles the plan");
    // The plan's raw samples at the `Scan`s by metric.
    let store = time_series_tables(dag, samples);
    let instance = compiled
        .instantiate(Arc::new(CountingStore(store)))
        .unwrap();
    block_on(async {
        // The run's scope (ingestion window or query time) and limits.
        let context = RunContext::new(scope, Limits::default()).unwrap();
        // One output stream per id.
        let outputs = instance.execute(compiled.roots(), context).unwrap();
        futures::future::join_all(outputs.into_iter().map(|mut output| async move {
            // Its batches, collected.
            let mut batches = Vec::new();
            while let Some(batch) = output.next().await {
                batches.push((*batch.unwrap()).clone());
            }
            batches
        }))
        .await
    })
}

/// One answer row: its series labels and its value.
type Answer = (BTreeMap<String, String>, f64);

/// Each answer row as its series labels and value, sorted by labels.
fn answers(batches: &[Batch]) -> Vec<Answer> {
    let mut answers: Vec<_> = batches
        .iter()
        .flat_map(|batch| {
            batch.to_rows().into_iter().map(|row| {
                let labels = series_labels(batch.schema(), &row);
                let value = row.iter().find_map(|v| match v {
                    Value::Float64(v) => Some(*v),
                    _ => None,
                });
                (labels, value.expect("a Float64 answer"))
            })
        })
        .collect();
    answers.sort_by(|a, b| a.0.cmp(&b.0));
    answers
}

/// The plan for `queries` on a deployment that keeps no raw samples, and
/// Stage 3's report of the selection it made (which the plan file does not
/// carry).
async fn planned_with_report(queries: &[&str]) -> (InstallablePlan, Option<SelectedPlanReport>) {
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
) -> (InstallablePlan, Option<SelectedPlanReport>) {
    // The deployment's Stage 3 prices, from its JSON file: cost model 1.
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

/// The node that answers each query of `plan`, in workload order.
fn query_roots(plan: &InstallablePlan) -> Vec<u64> {
    plan.dag.roots.iter().map(|&root| root as u64).collect()
}

/// The plan file carries Stage 3's predicted usage of the selected plan, so
/// a runtime reports it against what it measures without planning again:
/// for Example 4b's tumbling windows maintained at ingestion time, its nodes' prices
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
    let selection = selection.as_ref().expect("Stage 3's report");
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
        sample("latency_ms", "a", EVALUATION_MS - 900, 1.0),
        sample("latency_ms", "a", EVALUATION_MS - 100, 10.0),
        sample("latency_ms", "b", EVALUATION_MS - 200, 100.0),
    ];
    let query = Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
    };
    let answered = answers(&run(dag, &samples, &query_roots(&plan), query)[0]);
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
    let (plan, report) = plan_installable(UserInput::new(
        &p99_avg_workload(false),
        FrontendInput::Promql,
        PlanningModels::builtin()
            .with_deployment_capabilities(&capabilities)
            .with_cost_inputs(&settings)
            .with_ignore_accuracy_guarantees(true),
    ))
    .await
    .expect("a plan is selected");
    report.as_ref().expect("the plan's cost report");
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
