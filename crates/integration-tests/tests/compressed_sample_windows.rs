//! Compressed sample windows end to end (design doc
//! `compressed-sample-windows.md`, section 7, tests S1–S4): a
//! compressed-samples chain per metric, read back as raw rows by
//! `DecodeSamplesInRange`, lets Stage 3 answer quantiles exactly without
//! raw data and keep far less memory than per-query sketch chains.
//!
//! Written before the implementation: these tests name the planned
//! interfaces (`FieldDataType::CompressedSamples`,
//! `ASAPOp::DecodeSamplesInRange`, `SummaryFamily::CompressedSamples`,
//! `DeploymentCapabilities::unsealed_window_reads`,
//! `SourceFacts::column_compressed_bits_per_sample`) and fail until they
//! exist. S1 and S2's memory limits are cost model 3, which Stage 3 also
//! needs to select under.

use asap_cost::{
    plan_cost, ClusterLimits, ClusterObjective, CostInputs, CostModel, LatencyCalibration,
};
use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages_over};
use asap_planner::{
    CandidateExclusion, DeploymentCapabilities, PlanningModels, PlanningRun, SketchAlgorithm,
    SummaryFamily,
};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::summary_type::{ExactKind, SketchStatistic};
use asap_types::workload::PlanningWorkload;

// ---------------------------------------------------------------------------
// Workloads.

/// A typical dashboard: eleven panels over the gauge `data` and the
/// counter `data_total` (rates and increases over the counter, the rest
/// over the gauge), windows of 5 min to 1 h refreshed every 10–30 s,
/// `(query, refresh interval in ms)`.
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

/// The bits one sample takes compressed, as calibration measures them on
/// the benchmark generator's data (section 3.3): the i.i.d. Pareto gauge
/// `data` and the non-integer-scaled counter `data_total`.
const DATA_BITS_PER_SAMPLE: f64 = 58.2;
const DATA_TOTAL_BITS_PER_SAMPLE: f64 = 56.8;

/// Section 4: the compressed chains' kept bytes for the typical dashboard
/// at 10,000 series with the calibrated bits, 5 min windows (the
/// least-memory width): 0.297 GB for `data` + 0.290 GB for `data_total`.
const TYPICAL_DASHBOARD_COMPRESSED_BYTES: f64 = 587.5e6;

/// `queries` (each `(query, interval_ms, requirements JSON)`) over
/// `series` series per metric, one sample per second each, ingested.
fn workload(queries: &[(&str, u32, &str)], series: u64, sources: &str) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|(query, interval_ms, requirements)| {
            format!(
                r#"{{"query": {query:?}, "demand": {{"fixed_interval_at": {{"interval": {interval_ms}, "evaluation_phase": 0}}}},
                  "requirements": {{{requirements}}}, "predictability": {{"predictable": {{"known_at": null}}}}}}"#
            )
        })
        .collect();
    let metrics = sources.split(',').count() as u64;
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": {}.0}}}},
              "sources": {{{}}}}}}}"#,
        entries.join(","),
        series * metrics,
        sources
            .split(',')
            .map(|source| {
                let (name, kind) = source.split_once(':').unwrap();
                format!(
                    r#"{name:?}: {{"series_count": {series}, "metric_type": {kind:?}, "sample_interval": 1000,
                        "column_signs": {{"value": "non_negative"}}}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    ))
    .unwrap()
}

/// The typical dashboard at 10,000 series per metric, accuracy 0.05 at
/// failure probability 0.05 (the dashboards' 95% target), with the
/// calibrated bits per sample declared for both metrics.
fn typical_dashboard() -> PlanningWorkload {
    let requirements = r#""accuracy": {"max_error": 0.05, "failure_probability": 0.05}"#;
    let queries: Vec<(&str, u32, &str)> = TYPICAL_DASHBOARD
        .iter()
        .map(|&(query, interval)| (query, interval, requirements))
        .collect();
    let mut workload = workload(&queries, 10_000, "data:gauge,data_total:counter");
    declare_bits(&mut workload, "data", DATA_BITS_PER_SAMPLE);
    declare_bits(&mut workload, "data_total", DATA_TOTAL_BITS_PER_SAMPLE);
    workload
}

/// Declares `bits` per sample of `source`'s `value` column, as the
/// calibration step writes it.
fn declare_bits(workload: &mut PlanningWorkload, source: &str, bits: f64) {
    workload
        .data_workload
        .as_mut()
        .unwrap()
        .sources
        .get_mut(source)
        .unwrap()
        .column_compressed_bits_per_sample
        .insert("value".to_owned(), bits);
}

// ---------------------------------------------------------------------------
// Planning and reading the plan.

/// No raw data kept: the deployment of the memory-limit analysis.
fn no_raw_data() -> DeploymentCapabilities {
    DeploymentCapabilities {
        raw_data_retained: false,
        ..Default::default()
    }
}

/// Cost model 3 on a cluster of `memory_bytes`, choosing the plan with
/// the lowest mean latency among those that fit (the objective the
/// selection implements; uncalibrated, for testing).
fn cluster(memory_bytes: u64) -> CostInputs {
    CostInputs {
        cost_model: CostModel::ClusterCapacity(ClusterLimits {
            memory_bytes,
            disk_bytes: 100_000_000_000,
            cpu_cores: None,
            objective: ClusterObjective::LowestMeanLatency,
        }),
        latency_calibration: LatencyCalibration::UncalibratedForTesting,
        ..Default::default()
    }
}

/// Every planning stage over `workload` with `capabilities` and
/// `settings`.
fn planned(
    workload: &PlanningWorkload,
    capabilities: &DeploymentCapabilities,
    settings: &CostInputs,
) -> PlanningRun {
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let roots = lower_promql_roots(workload).unwrap();
    let models = PlanningModels::builtin()
        .with_cost_inputs(settings)
        .with_deployment_capabilities(capabilities);
    run_planning_stages_over(roots, &entries, &data, models)
        .unwrap()
        .run
}

/// The selected plan, or the selection's error as a panic message.
fn selected(run: &PlanningRun) -> &PhysicalASAPDAG {
    match &run.selection {
        Ok((plan, _)) => plan,
        Err(error) => panic!("no plan selected: {error}"),
    }
}

/// `plan_cost` of `plan` for `workload` under `settings`.
fn cost_of(
    plan: &PhysicalASAPDAG,
    workload: &PlanningWorkload,
    settings: &CostInputs,
) -> asap_cost::PlanCost {
    let entries: Vec<_> = workload.query_workload.entries().collect();
    plan_cost(
        plan,
        &entries,
        workload.data_workload.as_ref().unwrap(),
        settings,
    )
    .unwrap()
    .0
}

/// Whether `operator` builds a compressed-samples chain.
fn is_compressed_chain<C>(operator: &Operator<C>) -> bool {
    matches!(
        operator,
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            summary_type: FieldDataType::CompressedSamples,
            ..
        })
    )
}

/// The plan's `DecodeSamplesInRange` nodes, each with whether it reads
/// the unsealed window.
fn decodes(plan: &PhysicalASAPDAG) -> Vec<(PhysicalASAPNodeId, bool)> {
    plan.nodes
        .iter()
        .filter_map(|node| match &node.payload {
            Operator::ASAP(ASAPOp::DecodeSamplesInRange {
                reads_unsealed_window,
                ..
            }) => Some((node.id, *reads_unsealed_window)),
            _ => None,
        })
        .collect()
}

/// The quantile `Aggregate`s (`quantile_over_time` over raw rows) whose
/// input is a `DecodeSamplesInRange`: the quantiles answered exactly from
/// compressed samples.
fn quantiles_read_through_decode(plan: &PhysicalASAPDAG) -> usize {
    let decode_ids: Vec<PhysicalASAPNodeId> = decodes(plan).into_iter().map(|(id, _)| id).collect();
    plan.nodes
        .iter()
        .filter(|node| {
            matches!(
                &node.payload,
                Operator::NonASAP(NonASAPOp::Aggregate { measures, .. })
                    if measures.iter().any(|m| matches!(m, AggIntent::Quantile { .. }))
            )
        })
        .filter(|node| {
            plan.edges
                .iter()
                .any(|edge| edge.consumer == node.id && decode_ids.contains(&edge.producer))
        })
        .count()
}

/// The quantile answers (a `quantile_over_time` `Aggregate` or a
/// quantile `SummaryEstimate`) whose inputs reach a `DecodeSamplesInRange`
/// through query-time nodes only: the quantiles answered from compressed
/// samples, exactly or from a state built at query time over them.
fn quantiles_answered_from_decodes(plan: &PhysicalASAPDAG) -> usize {
    let node = |id: PhysicalASAPNodeId| plan.nodes.iter().find(|node| node.id == id).unwrap();
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
    plan.nodes
        .iter()
        .filter(|candidate| match &candidate.payload {
            Operator::NonASAP(NonASAPOp::Aggregate { measures, .. }) => measures
                .iter()
                .any(|measure| matches!(measure, AggIntent::Quantile { .. })),
            Operator::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
                matches!(query, SketchStatistic::Quantile { .. })
            }
            _ => false,
        })
        .filter(|candidate| reaches_decode(plan, node(candidate.id).id))
        .count()
}

/// The bytes a node's cost detail says it keeps (`keeps N bytes`), 0
/// when it keeps none.
fn kept_bytes(detail: &str) -> u64 {
    detail
        .split("keeps ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .map_or(0, |bytes| bytes.parse().unwrap())
}

// ---------------------------------------------------------------------------
// S1–S4.

/// S1. Under a 0.85 GB memory limit, the typical dashboard at 10,000
/// series has a plan (main: the least any plan needs is 3.72 GB, none
/// fits), and both quantiles are answered from `DecodeSamplesInRange`
/// (exactly, or from a state built at query time over the decoded rows);
/// the compressed chains keep within 5% of section 4's 0.59 GB. The limit
/// leaves room for the query-time memory Stage 3 charges each node over
/// the decoded rows (about 0.2 GB in all, the plan's other states
/// included). A wrong implementation
/// fails it by not offering the chain (no plan fits), by pricing it per
/// query instead of once per metric (twice or more the bytes), or by
/// mispricing its windows (outside 5%).
#[test]
fn memory_limit_selects_compressed_samples() {
    let workload = typical_dashboard();
    let settings = cluster(850_000_000);
    let run = planned(&workload, &no_raw_data(), &settings);
    let plan = selected(&run);
    assert_eq!(quantiles_answered_from_decodes(plan), 2, "both quantiles");
    let cost = cost_of(plan, &workload, &settings);
    let compressed: u64 = plan
        .nodes
        .iter()
        .filter(|node| is_compressed_chain(&node.payload))
        .map(|node| kept_bytes(&cost.per_node[&node.id].detail))
        .sum();
    let relative = (compressed as f64 - TYPICAL_DASHBOARD_COMPRESSED_BYTES).abs()
        / TYPICAL_DASHBOARD_COMPRESSED_BYTES;
    assert!(
        relative <= 0.05,
        "compressed chains keep {compressed} bytes, expected about {TYPICAL_DASHBOARD_COMPRESSED_BYTES}"
    );
    assert!(cost.usage.peak_memory_bytes <= 850_000_000);
}

/// The S2 queries: a 1 h quantile and a 30 min max over `x`, every 10 s.
fn oracle_workload() -> PlanningWorkload {
    let mut workload = workload(
        &[
            (
                "quantile_over_time(0.99, x[1h])",
                10_000,
                r#""accuracy": {"max_error": 0.05, "failure_probability": 0.05}"#,
            ),
            ("max_over_time(x[30m])", 10_000, ""),
        ],
        1_000,
        "x:gauge",
    );
    declare_bits(&mut workload, "x", DATA_BITS_PER_SAMPLE);
    workload
}

/// How the quantile is answered in one oracle assignment.
#[derive(Debug, Clone, Copy)]
enum QuantileBy {
    /// Exactly, from decoded compressed samples.
    Compressed,
    /// From a sketch chain of this algorithm built at ingestion.
    SketchChain(SketchAlgorithm),
}

/// How the max is answered in one oracle assignment.
#[derive(Debug, Clone, Copy)]
enum MaxBy {
    /// Exactly, from decoded compressed samples.
    Compressed,
    /// From an exact Max chain built at ingestion.
    MaxChain,
}

/// The exclusions that leave only `quantile` and `max`'s way of answering
/// (raw data is not retained, so no raw scan is left either).
fn assignment(quantile: QuantileBy, max: MaxBy) -> Vec<CandidateExclusion> {
    let family = |family: SummaryFamily| CandidateExclusion {
        summary_family: Some(family),
        ..Default::default()
    };
    let mut excluded = Vec::new();
    match quantile {
        QuantileBy::Compressed => {
            excluded.extend(
                SketchAlgorithm::ALL
                    .iter()
                    .map(|&algorithm| family(SummaryFamily::Sketch(algorithm))),
            );
        }
        QuantileBy::SketchChain(kept) => {
            // The exact quantile over decoded rows, every other sketch, and
            // the kept sketch built at query time.
            excluded.push(CandidateExclusion {
                aggregate_measure: Some("quantile".to_owned()),
                ..Default::default()
            });
            excluded.extend(
                SketchAlgorithm::ALL
                    .iter()
                    .filter(|&&algorithm| algorithm != kept)
                    .map(|&algorithm| family(SummaryFamily::Sketch(algorithm))),
            );
            excluded.push(CandidateExclusion {
                summary_family: Some(SummaryFamily::Sketch(kept)),
                timing: Some(ExecutionTiming::QueryTime),
                ..Default::default()
            });
        }
    }
    match max {
        MaxBy::Compressed => excluded.push(CandidateExclusion {
            summary_family: Some(SummaryFamily::Exact(ExactKind::Max)),
            timing: Some(ExecutionTiming::IngestionTime),
            ..Default::default()
        }),
        MaxBy::MaxChain => {
            excluded.push(CandidateExclusion {
                aggregate_measure: Some("max".to_owned()),
                ..Default::default()
            });
            excluded.push(CandidateExclusion {
                summary_family: Some(SummaryFamily::Exact(ExactKind::Max)),
                timing: Some(ExecutionTiming::QueryTime),
                ..Default::default()
            });
        }
    }
    excluded
}

/// What `settings` minimizes for the plan `run` selected: the cost per
/// second under cost model 1, the mean latency Stage 3 reports under cost
/// model 3's `lowest_mean_latency`; with whether the plan fits `settings`'
/// memory limit, and the selection's relative optimality gap (0 when
/// solved to optimality).
fn objective_and_fits(
    run: &PlanningRun,
    workload: &PlanningWorkload,
    settings: &CostInputs,
) -> (f64, bool, f64) {
    let (plan, selection) = run.selection.as_ref().expect("a selected plan");
    let cost = cost_of(plan, workload, settings);
    let gap = selection
        .optimality_gap
        .as_ref()
        .map_or(0.0, |gap| gap.relative_gap);
    match &settings.cost_model {
        CostModel::ClusterCapacity(limits) => (
            selection
                .selected_plan_mean_latency_ms
                .expect("lowest_mean_latency reports the mean latency"),
            cost.usage.peak_memory_bytes <= limits.memory_bytes,
            gap,
        ),
        _ => (cost.cost_per_second.expect("cost model 1"), true, gap),
    }
}

/// S2. For a 1 h quantile and a 30 min max over 1,000 series, under cost
/// model 1 and under cost model 3's lowest mean latency with two memory
/// limits (1 GB, which every assignment fits, and 1 MB, which none does),
/// the selected plan's objective is at most the best feasible one (within
/// the selection's reported optimality gap) over every assignment {compressed, DDSketch chain, KLL
/// chain} × {compressed, Max chain}, each assignment planned on its own
/// and priced by `plan_cost`; with no feasible assignment no plan is
/// selected. A wrong implementation fails it by mispricing the shared
/// compressed chain (charging it per reader makes the both-compressed
/// assignment look worse than it is) or by a selection that disagrees
/// with the prices.
#[test]
fn selection_matches_exhaustive_oracle() {
    let workload = oracle_workload();
    let quantile_ways = [
        QuantileBy::Compressed,
        QuantileBy::SketchChain(SketchAlgorithm::DDSketch),
        QuantileBy::SketchChain(SketchAlgorithm::Kll),
    ];
    let max_ways = [MaxBy::Compressed, MaxBy::MaxChain];
    for settings in [
        CostInputs::default(),
        cluster(1_000_000_000),
        cluster(1_000_000),
    ] {
        let mut best: Option<f64> = None;
        let mut planned_assignments = 0;
        for quantile in quantile_ways {
            for max in max_ways {
                let capabilities = DeploymentCapabilities {
                    excluded_candidates: assignment(quantile, max),
                    ..no_raw_data()
                };
                let run = planned(&workload, &capabilities, &settings);
                if run.selection.is_err() {
                    continue;
                }
                planned_assignments += 1;
                let (objective, fits, _) = objective_and_fits(&run, &workload, &settings);
                if fits {
                    best = Some(best.map_or(objective, |best: f64| best.min(objective)));
                }
            }
        }
        let run = planned(&workload, &no_raw_data(), &settings);
        match (best, &run.selection) {
            (Some(best), Ok(_)) => {
                let (objective, fits, gap) = objective_and_fits(&run, &workload, &settings);
                assert!(
                    fits,
                    "{:?}: the selected plan is over the limit",
                    settings.cost_model
                );
                // The full selection may stop within its reported gap.
                assert!(
                    objective <= best * (1.0 + gap) + 1e-9 * best.abs().max(1e-12),
                    "{:?}: selected {objective}, best assignment {best}",
                    settings.cost_model
                );
            }
            (None, Err(_)) => {}
            (best, selection) => panic!(
                "{:?}: best assignment {best:?}, selection {:?}",
                settings.cost_model,
                selection.as_ref().map(|_| "a plan")
            ),
        }
        if matches!(settings.cost_model, CostModel::UsageOverTime(_)) {
            // At least the compressed and one sketch assignment plan.
            assert!(
                planned_assignments >= 2,
                "{planned_assignments} assignments planned"
            );
        }
    }
}

/// S3. `quantile_over_time` with an exact target (`AccuracyTarget::exact`,
/// the default when no accuracy is given) and no raw data has a plan,
/// reading through `DecodeSamplesInRange` (main: none, the exact
/// quantile's raw-row input has no producer and no sketch is exact). A
/// wrong implementation fails it by not offering the decode as a producer
/// of the raw-range group, or by giving it a non-exact guarantee.
#[test]
fn exact_target_without_raw_data() {
    let workload = workload(
        &[("quantile_over_time(0.99, x[1h])", 10_000, "")],
        1_000,
        "x:gauge",
    );
    let run = planned(&workload, &no_raw_data(), &CostInputs::default());
    let plan = selected(&run);
    assert_eq!(quantiles_read_through_decode(plan), 1);
    assert!(plan
        .nodes
        .iter()
        .any(|node| is_compressed_chain(&node.payload)));
}

/// S4. The deployment vocabulary turns the state off: with
/// `{"summary_family": "compressed_samples"}` excluded no plan node builds
/// or decodes compressed samples (the plan is main's), and with
/// `unsealed_window_reads: false` every decode reads sealed windows only.
/// A wrong implementation fails it by matching the family on the wrong
/// operators (a decode left over the excluded chain) or by not expanding
/// the capability into its exclusion.
#[test]
fn excluding_the_family_restores_main() {
    let workload = typical_dashboard();
    let settings = CostInputs::default();
    let excluded = DeploymentCapabilities {
        excluded_candidates: vec![CandidateExclusion {
            summary_family: Some(SummaryFamily::CompressedSamples),
            ..Default::default()
        }],
        ..no_raw_data()
    };
    let run = planned(&workload, &excluded, &settings);
    let plan = selected(&run);
    assert!(decodes(plan).is_empty());
    assert!(!plan
        .nodes
        .iter()
        .any(|node| is_compressed_chain(&node.payload)));

    let sealed_only = DeploymentCapabilities {
        unsealed_window_reads: false,
        ..no_raw_data()
    };
    let run = planned(&workload, &sealed_only, &settings);
    let plan = selected(&run);
    for (node, reads_unsealed_window) in decodes(plan) {
        assert!(!reads_unsealed_window, "n{node} reads the unsealed window");
    }
}
