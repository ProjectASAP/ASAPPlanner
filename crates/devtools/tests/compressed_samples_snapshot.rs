//! D1 of `compressed-sample-windows.md`, section 7: a typical dashboard
//! under a memory limit selects compressed sample windows for both quantile
//! panels, and its selection model's size is the reviewed one.
//!
//! The snapshot `tests/compressed_samples_snapshot/typical_dashboard.txt`
//! holds only the size of the selection MILP (variables and constraints),
//! which follows from the candidates alone. The selected plan's text is not
//! kept: the solve stops at a 1 % gap, so a slower machine may stop at
//! another plan within it. Regenerate with
//! `UPDATE_SNAPSHOTS=1 cargo test -p asap-devtools --test devtools
//! compressed_samples_snapshot::`. Written before the implementation: it
//! names the planned interfaces and fails until they exist.

use std::path::PathBuf;

use asap_cost::{ClusterLimits, ClusterObjective, CostInputs, CostModel, LatencyCalibration};
use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages_over};
use asap_devtools::planning_stages_text::planning_stages_text;
use asap_planner::{DeploymentCapabilities, PlanningModels};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::summary_type::SketchStatistic;
use asap_types::workload::PlanningWorkload;

/// A typical dashboard: eleven panels over the gauge `data` and the
/// counter `data_total`, `(query, refresh interval in ms)`.
const TYPICAL_DASHBOARD: [(&str, u32); 11] = [
    ("sum by (job) (rate(data_total[5m]))", 10_000),
    ("sum by (region) (increase(data_total[1h]))", 30_000),
    ("avg by (job) (avg_over_time(data[5m]))", 10_000),
    ("max by (job) (max_over_time(data[15m]))", 15_000),
    ("topk(5, sum by (job) (rate(data_total[5m])))", 15_000),
    ("avg by (job) (quantile_over_time(0.99, data[15m]))", 20_000),
    (
        "avg by (region, service) (avg_over_time(data[30m]))",
        30_000,
    ),
    ("sum by (service) (rate(data_total[1h]))", 30_000),
    ("max by (region) (max_over_time(data[1h]))", 30_000),
    (
        "topk(10, sum by (service, job) (increase(data_total[15m])))",
        20_000,
    ),
    (
        "avg by (region) (quantile_over_time(0.5, data[1h]))",
        30_000,
    ),
];

/// The typical dashboard at 10,000 series per metric, one sample per
/// second, accuracy 0.05 at failure probability 0.05, with the bits per
/// sample calibration measures on the benchmark generator's data.
fn typical_dashboard() -> PlanningWorkload {
    let entries: Vec<String> = TYPICAL_DASHBOARD
        .iter()
        .map(|(query, interval_ms)| {
            format!(
                r#"{{"query": {query:?}, "demand": {{"fixed_interval_at": {{"interval": {interval_ms}, "evaluation_phase": 0}}}},
                  "requirements": {{"accuracy": {{"max_error": 0.05, "failure_probability": 0.05}}}},
                  "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    let mut workload: PlanningWorkload = serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 20000.0}}}},
              "sources": {{
                "data": {{"series_count": 10000, "metric_type": "gauge", "sample_interval": 1000,
                  "column_signs": {{"value": "non_negative"}}}},
                "data_total": {{"series_count": 10000, "metric_type": "counter", "sample_interval": 1000}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap();
    let sources = &mut workload.data_workload.as_mut().unwrap().sources;
    for (source, bits) in [("data", 58.2), ("data_total", 56.8)] {
        sources
            .get_mut(source)
            .unwrap()
            .column_compressed_bits_per_sample
            .insert("value".to_owned(), bits);
    }
    workload
}

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/compressed_samples_snapshot/typical_dashboard.txt")
}

/// D1. The typical dashboard under a 0.85 GB limit (cost model 3, lowest
/// mean latency) and no raw data: a plan is selected, both quantile panels
/// read through `DecodeSamplesInRange`, and the selection MILP's size
/// (variables and constraints) equals the reviewed snapshot. The size
/// follows from the candidates, not from where the solve stops, so it is
/// the same on any machine; the selected plan's text is not compared. It
/// fails on main (no compressed samples, so no plan fits), and afterwards
/// on any change to the candidates the selection is built over, which then
/// needs a reviewed snapshot update.
#[test]
fn compressed_samples_plan_snapshot() {
    let workload = typical_dashboard();
    let settings = CostInputs {
        cost_model: CostModel::ClusterCapacity(ClusterLimits {
            memory_bytes: 850_000_000,
            disk_bytes: 100_000_000_000,
            cpu_cores: None,
            objective: ClusterObjective::LowestMeanLatency,
        }),
        latency_calibration: LatencyCalibration::UncalibratedForTesting,
        ..Default::default()
    };
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..Default::default()
    };
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let roots = lower_promql_roots(&workload).unwrap();
    let models = PlanningModels::builtin()
        .with_cost_inputs(&settings)
        .with_deployment_capabilities(&capabilities);
    let stages = run_planning_stages_over(roots, &entries, &data, models).unwrap();
    let plan = match &stages.run.selection {
        Ok((plan, _)) => plan,
        Err(error) => panic!("no plan selected: {error}"),
    };

    // Both quantile panels answered from decoded samples: exactly, or from
    // a state built at query time over them.
    fn reaches_decode(plan: &PhysicalASAPDAG, id: PhysicalASAPNodeId) -> bool {
        let node = plan.nodes.iter().find(|node| node.id == id).unwrap();
        if matches!(
            node.payload,
            Operator::ASAP(ASAPOp::DecodeSamplesInRange { .. })
        ) {
            return true;
        }
        node.output_state.timing == ExecutionTiming::QueryTime
            && plan
                .edges
                .iter()
                .filter(|edge| edge.consumer == id)
                .any(|edge| reaches_decode(plan, edge.producer))
    }
    let quantiles_through_decode = plan
        .nodes
        .iter()
        .filter(|node| match &node.payload {
            Operator::NonASAP(NonASAPOp::Aggregate { measures, .. }) => measures
                .iter()
                .any(|measure| matches!(measure, AggIntent::Quantile { .. })),
            Operator::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
                matches!(query, SketchStatistic::Quantile { .. })
            }
            _ => false,
        })
        .filter(|node| reaches_decode(plan, node.id))
        .count();
    assert_eq!(quantiles_through_decode, 2, "both quantile panels");

    let text = planning_stages_text(&stages, None, None, false);
    // The selection model's size: its variables and constraints, not its
    // timings, objective or gap.
    let model_size = text
        .lines()
        .find_map(|line| line.strip_prefix("selection MILP: "))
        .expect("a selection MILP line")
        .split(", ")
        .filter(|part| part.ends_with(" variables") || part.ends_with(" constraints"))
        .collect::<Vec<_>>()
        .join(", ");
    let stage3 = format!("selection MILP: {model_size}\n");
    let path = snapshot_path();
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &stage3).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "no snapshot at {}: written and reviewed with the implementation \
             (UPDATE_SNAPSHOTS=1)",
            path.display()
        )
    });
    assert_eq!(stage3, expected);
}
