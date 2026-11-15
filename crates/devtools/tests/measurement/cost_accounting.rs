//! §2.3 and §2.8: the planner prices from the measured table, state
//! bytes, and a shared node charged once.

use asap_accuracy::NoMeasuredGuarantees;
use asap_cost::{CostInputs, MeasuredOperationCosts, OperatorCost, OperatorKind};
use asap_devtools::measurement::calibration_tables::{calibrate, CalibrateRequest};
use asap_devtools::measurement::cost_calibration::CalibrationFamilies;
use asap_devtools::measurement::execution::{execute, DataInput, ExecuteRequest};
use asap_executor::factory::create_planner_accumulator;
use asap_planner::{DeploymentCapabilities, MaterializedViewRefresh, PlanningWorkload};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::summary_type::{
    ExactKind, ExactParams, SketchAlgorithm, SketchParams, SummaryInputExpr, SummaryUpdate,
};

use super::common::{
    exact, facts, four_constant_series, four_series_facts, no_raw_data, per_value, plan, workload,
};

/// DDSketch builds (`SummaryAgg` and the tumbling-window chain) at
/// `ns_per_unit` per row, both phases.
fn ddsketch_build_table(ns_per_unit: f64) -> MeasuredOperationCosts {
    let entry = |operator| OperatorCost {
        operator,
        phase: None,
        sketch: Some(SketchParams::DDSketch { alpha: 0.01 }),
        exact: None,
        measure: None,
        ns_per_unit,
        ns_per_invocation: 0.0,
        memory_bytes_per_state: None,
    };
    MeasuredOperationCosts {
        operators: vec![
            entry(OperatorKind::SummaryAgg),
            entry(OperatorKind::SummaryTumblingWindowChain),
        ],
        ..MeasuredOperationCosts::default()
    }
}

/// `quantile_over_time(0.99, latency[1h])` every 10 s over 1,000 series
/// sampled each second, within 1% per value at δ = 0.01.
fn quantile_workload() -> PlanningWorkload {
    workload(
        &[(
            "quantile_over_time(0.99, latency[1h])",
            per_value("relative", 0.01, 0.01),
        )],
        10_000,
        &[("latency", facts(1_000, 1_000))],
    )
}

/// The same plan priced with table A (DDSketch builds at 1 ns per row) and
/// with A's builds at 10 ns: each build node's CPU rises by its rows × 9 ns.
/// A query-time build reads 1,000 series × 3,600 samples per evaluation,
/// 0.1 evaluations per second: Δ = 3.6e6 · 0.1 · 9 ns = 3.24e-3 core-s/s.
/// An ingestion-time build reads the 1,000 arriving rows per second:
/// Δ = 1,000 · 9 ns = 9e-6 core-s/s.
#[test]
fn planner_prices_from_table() {
    let workload = quantile_workload();
    let table_a = CostInputs {
        measured_operation_costs: ddsketch_build_table(1.0),
        ..CostInputs::default()
    };
    let table_a10 = CostInputs {
        measured_operation_costs: ddsketch_build_table(10.0),
        ..CostInputs::default()
    };
    let (installable, _) = plan(
        &workload,
        &table_a,
        &NoMeasuredGuarantees,
        &DeploymentCapabilities::default(),
    );
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let (cost_a, _) = asap_cost::plan_cost(&installable.dag, &entries, &data, &table_a).unwrap();
    let (cost_a10, _) =
        asap_cost::plan_cost(&installable.dag, &entries, &data, &table_a10).unwrap();
    let builds: Vec<_> = installable
        .dag
        .nodes
        .iter()
        .filter(|node| matches!(node.parameters, Some(SketchParams::DDSketch { .. })))
        .filter(|node| {
            matches!(
                node.payload,
                Operator::ASAP(
                    ASAPOp::SummaryAgg { .. } | ASAPOp::SummaryTumblingWindowChain { .. }
                )
            )
        })
        .collect();
    assert!(
        !builds.is_empty(),
        "the plan builds no DDSketch: {:?}",
        installable.dag
    );
    for node in builds {
        let expected = match node.output_state.timing {
            ExecutionTiming::QueryTime => 3.6e6 * 0.1 * 9e-9,
            ExecutionTiming::IngestionTime => 1_000.0 * 9e-9,
        };
        let delta = cost_a10.per_node[&node.id].cpu_core_seconds_per_second
            - cost_a.per_node[&node.id].cpu_core_seconds_per_second;
        assert!(
            (delta - expected).abs() <= 1e-6 * expected,
            "node {}: {delta} vs {expected}",
            node.id
        );
    }
}

/// After a small calibration, every node of every small showcase plan is
/// priced from the table: no operator is left to the formulas.
#[test]
fn calibrated_table_covers_showcase_plans() {
    let calibration = calibrate(&CalibrateRequest {
        families: CalibrationFamilies::All,
        small: true,
        seed: 7,
    })
    .unwrap();
    let inputs = CostInputs {
        measured_operation_costs: calibration.costs,
        ..CostInputs::default()
    };
    for (name, workload, settings) in asap_devtools::measurement::benchmark::showcase_suite(true) {
        for setting in settings {
            let caps = DeploymentCapabilities {
                raw_data_retained: setting.raw_data_retained,
                ..DeploymentCapabilities::default()
            };
            let (_, report) = plan(&workload, &inputs, &calibration.accuracy, &caps);
            for node in &report.predicted_usage.per_node {
                assert_eq!(
                    node.cost_source,
                    asap_cost::NodeCostSource::Measured,
                    "{name} {setting:?}: {:?} at {:?}",
                    node.operator,
                    node.timing
                );
            }
        }
    }
}

/// `sum by (job) (sum_over_time(m[1h]))` every 10 s without raw data,
/// with a measured merge of 1 ns per state and 10 µs per invocation: a
/// 10 s chain merges 360 windows per evaluation, a 1 s chain 3,600, so the
/// planner keeps the 10 s chain.
///
/// The plan's version of this test compares invocation costs at equal
/// units; the cost model charges an ingestion-time `ns_per_invocation`
/// per second of arriving rows, whatever the chain width, so here only
/// the merged states differ.
// provisional answer 10
#[test]
fn invocation_cost_ranks_chain_widths() {
    let merge = |phase| OperatorCost {
        operator: OperatorKind::SummaryMerge,
        phase: Some(phase),
        sketch: None,
        exact: Some(ExactKind::Sum),
        measure: None,
        ns_per_unit: 1.0,
        ns_per_invocation: 10_000.0,
        memory_bytes_per_state: None,
    };
    let inputs = CostInputs {
        measured_operation_costs: MeasuredOperationCosts {
            operators: vec![
                merge(ExecutionTiming::QueryTime),
                merge(ExecutionTiming::IngestionTime),
            ],
            ..MeasuredOperationCosts::default()
        },
        ..CostInputs::default()
    };
    let workload = workload(
        &[("sum by (job) (sum_over_time(m[1h]))", exact())],
        10_000,
        &[("m", four_series_facts())],
    );
    let (installable, _) = plan(&workload, &inputs, &NoMeasuredGuarantees, &no_raw_data());
    let widths: Vec<u64> = installable
        .dag
        .nodes
        .iter()
        .filter_map(|node| match &node.payload {
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => Some(*tumbling_window_width_ms),
            _ => None,
        })
        .collect();
    assert_eq!(widths, vec![10_000]);
}

/// The bytes the executor charges one state of `family` with `parameters`
/// after one update: of the sample value, or (`keyed`) of one item.
fn kernel_state_bytes(
    family: FieldDataType,
    parameters: Option<&SketchParams>,
    keyed: bool,
) -> u64 {
    let value = SummaryUpdate::column(ColumnRef::SampleValue);
    let input = match keyed {
        false => value,
        true => SummaryUpdate {
            item: Some(SummaryInputExpr::Column(ColumnRef::Named("item".into()))),
            ..value
        },
    };
    let mut updater = create_planner_accumulator(&family, parameters, &input).unwrap();
    match keyed {
        false => updater.update_single(1.0, 0).unwrap(),
        true => updater
            .update_keyed(&["checkout".to_string()], 1.0, 0)
            .unwrap(),
    }
    updater.into_accumulator().approx_memory_bytes() as u64
}

/// `sum by (job) (sum_over_time(m[1m]))` as a 10 s chain holds 6 windows ×
/// 2 groups of one exact-sum state each: 12 · B_sum bytes, B_sum the
/// kernel's own charge for one state. The measured view bytes and the
/// planner's average memory are within 1.5× of it.
#[test]
fn memory_per_state_hand() {
    let state_bytes = kernel_state_bytes(
        FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
        None,
        false,
    );
    let expected = 12.0 * state_bytes as f64;
    let workload = workload(
        &[("sum by (job) (sum_over_time(m[1m]))", exact())],
        10_000,
        &[("m", four_series_facts())],
    );
    let mut request = ExecuteRequest::new(
        &workload,
        DataInput::Samples(four_constant_series(700_000, |_, _| None)),
        vec![650_000, 700_000],
    );
    request.deployment_capabilities = no_raw_data();
    let report = execute(&request).unwrap();
    let view = report
        .views
        .iter()
        .find(|view| view.refresh == MaterializedViewRefresh::AtIngestion)
        .expect("an ingestion-time view");
    let measured = *view.bytes_after_evaluation.last().unwrap() as f64;
    assert!(
        measured <= 1.5 * expected && expected <= 1.5 * measured,
        "{measured} vs {expected}"
    );
    let predicted = report.predicted.predicted_usage.average_memory_bytes;
    assert!(
        predicted <= 1.5 * expected && expected <= 1.5 * predicted,
        "{predicted} vs {expected}"
    );
}

/// Calibration writes, for a fixed-size state, the kernel's own charge:
/// Count-Min 272 × 5 holds what `create_planner_accumulator` charges
/// for it, at least d · w · 8 bytes of counters.
// provisional answer 9
#[test]
fn fixed_size_state_bytes() {
    let cms = SketchParams::Cms {
        width: 272,
        depth: 5,
    };
    let charged = kernel_state_bytes(
        FieldDataType::Sketch(SketchAlgorithm::Cms),
        Some(&cms),
        true,
    );
    assert!(charged >= 272 * 5 * 8, "{charged}");
    let calibration = calibrate(&CalibrateRequest {
        families: CalibrationFamilies::Only(vec!["Cms".into()]),
        small: true,
        seed: 7,
    })
    .unwrap();
    let entry = calibration
        .costs
        .operators
        .iter()
        .find(|entry| {
            entry.operator == OperatorKind::SummaryAgg && entry.sketch.as_ref() == Some(&cms)
        })
        .expect("an entry for Count-Min 272 x 5");
    assert_eq!(entry.memory_bytes_per_state, Some(charged as f64));
}

/// Two queries over one 10 s chain without raw data,
/// `sum by (job) (sum_over_time(m[5m]))` and the same over 10m: the joint
/// plan builds the chain once, and charges it once (one `per_node` entry,
/// read by both queries).
#[test]
fn shared_node_priced_once() {
    let workload = workload(
        &[
            ("sum by (job) (sum_over_time(m[5m]))", exact()),
            ("sum by (job) (sum_over_time(m[10m]))", exact()),
        ],
        10_000,
        &[("m", four_series_facts())],
    );
    let (installable, report) = plan(
        &workload,
        &CostInputs::default(),
        &NoMeasuredGuarantees,
        &no_raw_data(),
    );
    let chains: Vec<_> = installable
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
    assert_eq!(chains.len(), 1, "{:?}", installable.dag);
    let chain = chains[0].id;
    let priced = report
        .predicted_usage
        .per_node
        .iter()
        .filter(|node| node.node_id == chain)
        .count();
    assert_eq!(priced, 1);
    // Both roots read the chain.
    for &root in &installable.dag.roots {
        assert!(
            reaches(&installable.dag, root, chain),
            "root {root} does not read the chain"
        );
    }
}

/// Whether `from` reaches `to` through its inputs.
fn reaches(
    dag: &asap_planner::PhysicalASAPDAG,
    from: asap_planner::PhysicalASAPNodeId,
    to: asap_planner::PhysicalASAPNodeId,
) -> bool {
    let mut stack = vec![from];
    let mut seen = std::collections::BTreeSet::new();
    while let Some(node) = stack.pop() {
        if node == to {
            return true;
        }
        if seen.insert(node) {
            stack.extend(
                dag.edges
                    .iter()
                    .filter(|edge| edge.consumer == node)
                    .map(|edge| edge.producer),
            );
        }
    }
    false
}

/// The joint plan's measured ingestion CPU is about one chain's, not two
/// (≤ 1.2 × the single-query plan's).
#[test]
#[ignore = "timing: run on a dedicated host"]
fn shared_node_measured_once() {
    use asap_devtools::measurement::benchmark::{bench, BenchArm, BenchRequest, DataSetting};
    use asap_devtools::measurement::calibration_tables::PlanningTables;
    let tables = PlanningTables {
        cost_inputs: CostInputs::default(),
        accuracy: Default::default(),
        warnings: Vec::new(),
    };
    let setting = DataSetting {
        sample_interval_ms: 1_000,
        evaluation_interval_ms: 10_000,
        raw_data_retained: false,
    };
    let ingestion_cpu = |workload: &PlanningWorkload| {
        let report = bench(&BenchRequest {
            workload,
            arms: vec![BenchArm::NoRaw],
            settings: vec![setting.clone()],
            series_count: 100,
            duration_ms: 600_000,
            repetitions: 5,
            tables: tables.clone(),
            seed: 1,
            cost_gate: false,
            output_root: None,
        })
        .unwrap();
        report.arms[0]
            .measured
            .as_ref()
            .unwrap()
            .ingestion_cpu_seconds_per_second
    };
    let single = workload(
        &[("sum by (job) (sum_over_time(m[5m]))", exact())],
        10_000,
        &[("m", facts(100, 1_000))],
    );
    let joint = workload(
        &[
            ("sum by (job) (sum_over_time(m[5m]))", exact()),
            ("sum by (job) (sum_over_time(m[10m]))", exact()),
        ],
        10_000,
        &[("m", facts(100, 1_000))],
    );
    let (single, joint) = (ingestion_cpu(&single), ingestion_cpu(&joint));
    assert!(joint <= 1.2 * single, "joint {joint} vs single {single}");
}
