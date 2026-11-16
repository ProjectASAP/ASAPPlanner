//! Stage 3 under the default `CostInputs` (no measured table) ranks the
//! exact plan against the alternatives as the executor measures them.
//!
//! The expectations are the executor's measured total CPU (query plus
//! ingestion) per second of wall time, for each plan kind of each workload
//! below: measured on one core of an idle server, in a release build,
//! serially, with the executor at commit 59365ce80 over the synthetic data the
//! workloads declare (`latency` a non-negative gauge, `requests_total` a
//! counter, 10 jobs), every accuracy target 1% relative at δ = 0.01.
//!
//! Example: `quantile_over_time(0.99, latency[1h])` over 1,000 series
//! sampled at 1 Hz, every 10 s, raw data kept. The exact plan (scan, keep
//! every value per series, sort) measures 39.0 CPU-ms per second; a
//! DDSketch built from the raw rows at query time measures 77.8, twice as
//! much, because inserting a row into a DDSketch costs more than keeping it
//! and sorting it. The planner must not predict the sketch cheaper.

use asap_cost::CostInputs;
use asap_devtools::planning_stages::{lower_promql_roots, run_planning_stages_over};
use asap_planner::{DeploymentCapabilities, PlanningModels};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::ASAPOp;
use asap_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPOperatorPayload};
use asap_types::workload::PlanningWorkload;

/// Data rate and evaluation interval of one measured setting, over 1,000
/// series of each source.
#[derive(Debug, Clone, Copy)]
struct Setting {
    /// Milliseconds between two samples of one series.
    sample_interval_ms: u32,
    /// Milliseconds between two evaluations of the query.
    evaluation_interval_ms: u32,
}

/// 1 Hz, every 10 s.
const ONE_HZ_EVERY_10_S: Setting = Setting {
    sample_interval_ms: 1_000,
    evaluation_interval_ms: 10_000,
};
/// 10 Hz, every 60 s.
const TEN_HZ_EVERY_60_S: Setting = Setting {
    sample_interval_ms: 100,
    evaluation_interval_ms: 60_000,
};
/// 1 Hz, every 300 s.
const ONE_HZ_EVERY_300_S: Setting = Setting {
    sample_interval_ms: 1_000,
    evaluation_interval_ms: 300_000,
};

/// A measured PromQL query and the metric of its 1% accuracy target.
#[derive(Debug, Clone, Copy)]
struct Query {
    promql: &'static str,
    /// `relative` or `rank`.
    accuracy_metric: &'static str,
}

/// `query` every `setting.evaluation_interval_ms` with its 1% target at
/// δ = 0.01, over 1,000 series each of `latency` and `requests_total`.
fn workload(query: Query, setting: Setting) -> PlanningWorkload {
    let Setting {
        sample_interval_ms,
        evaluation_interval_ms,
    } = setting;
    let Query {
        promql,
        accuracy_metric,
    } = query;
    let requirements = format!(
        r#""accuracy": {{"max_error": 0.01, "failure_probability": 0.01, "accuracy_scope": "per_value", "accuracy_metric": "{accuracy_metric}"}}"#
    );
    let rows_per_second = 2.0 * 1_000.0 * 1_000.0 / f64::from(sample_interval_ms);
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [
              {{"query": {promql:?}, "demand": {{"fixed_interval_at": {{"interval": {evaluation_interval_ms}, "evaluation_phase": 0}}}},
                "requirements": {{{requirements}}}, "predictability": {{"predictable": {{"known_at": null}}}}}}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": {rows_per_second}}}}},
              "sources": {{
                "latency": {{"series_count": 1000, "metric_type": "gauge", "column_signs": {{"value": "non_negative"}}, "sample_interval": {sample_interval_ms}}},
                "requests_total": {{"series_count": 1000, "metric_type": "counter", "sample_interval": {sample_interval_ms}}}}}}}}}"#
    ))
    .unwrap()
}

/// The plan Stage 3 selects for `workload` under the default cost inputs,
/// with raw data kept or not, and its predicted CPU core-seconds per second.
fn selected(workload: &PlanningWorkload, raw_data_retained: bool) -> (PhysicalASAPDAG, f64) {
    let settings = CostInputs::default();
    let capabilities = DeploymentCapabilities {
        raw_data_retained,
        ..Default::default()
    };
    let models = PlanningModels::builtin()
        .with_cost_inputs(&settings)
        .with_deployment_capabilities(&capabilities);
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let roots = lower_promql_roots(workload).unwrap();
    let run = run_planning_stages_over(roots, &entries, &data, models).unwrap();
    let (plan, _) = run.run.selection.as_ref().expect("a plan is selected");
    let cpu = predicted_cpu(plan, workload);
    (plan.clone(), cpu)
}

/// The exact plan the executor measured as the baseline: the query-time
/// lowering of `query` (scan raw data, compute exactly), predicted CPU
/// core-seconds per second.
fn exact_query_time_cpu(query: Query, setting: Setting) -> f64 {
    let w = workload(query, setting);
    let roots = lower_promql_roots(&w).unwrap();
    let (plan, _) =
        asap_types::test_support::forwarders::physical_asap_dag_at_query_time_with_node_ids(&roots)
            .unwrap();
    predicted_cpu(&plan, &w)
}

/// `plan`'s predicted CPU core-seconds per second under the default cost
/// inputs: query and ingestion time.
fn predicted_cpu(plan: &PhysicalASAPDAG, workload: &PlanningWorkload) -> f64 {
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let (cost, _) = asap_cost::plan_cost(plan, &entries, &data, &CostInputs::default()).unwrap();
    cost.per_node
        .values()
        .map(|node| node.cpu_core_seconds_per_second)
        .sum()
}

/// What a plan computes its answer from, as the measurements name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanKind {
    /// Raw data scanned at query time and computed exactly (no summary).
    ExactScan,
    /// An exact accumulator built from raw data at query time.
    QueryTimeExactAccumulator,
    /// A sketch built from raw data at query time.
    QueryTimeSketch,
    /// States maintained at ingestion time in tumbling windows and merged
    /// at query time.
    StoredStates,
}

fn plan_kind(plan: &PhysicalASAPDAG) -> PlanKind {
    let mut kind = PlanKind::ExactScan;
    for node in &plan.nodes {
        match (&node.payload, node.output_state.timing) {
            (
                PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }),
                ExecutionTiming::IngestionTime,
            ) => return PlanKind::StoredStates,
            (
                PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg { summary_type, .. }),
                ExecutionTiming::QueryTime,
            ) if kind != PlanKind::QueryTimeSketch => {
                kind = match summary_type {
                    FieldDataType::Sketch(..) => PlanKind::QueryTimeSketch,
                    _ => PlanKind::QueryTimeExactAccumulator,
                }
            }
            _ => {}
        }
    }
    kind
}

/// Whether `plan` keeps or builds a sketch anywhere.
fn has_sketch(plan: &PhysicalASAPDAG) -> bool {
    plan.nodes.iter().any(|node| {
        matches!(
            &node.payload,
            PhysicalASAPOperatorPayload::ASAP(
                ASAPOp::SummaryAgg {
                    summary_type: FieldDataType::Sketch(..),
                    ..
                } | ASAPOp::SummaryTumblingWindowChain {
                    summary_type: FieldDataType::Sketch(..),
                    ..
                }
            )
        )
    })
}

const fn relative(promql: &'static str) -> Query {
    Query {
        promql,
        accuracy_metric: "relative",
    }
}

const QUANTILE: Query = relative("quantile_over_time(0.99, latency[1h])");
/// The same quantile with a 1% rank target: a KLL instead of a DDSketch.
const QUANTILE_RANK: Query = Query {
    promql: "quantile_over_time(0.99, latency[1h])",
    accuracy_metric: "rank",
};
const MAX: Query = relative("max_over_time(latency[1h])");
const SUM_OVER_TIME: Query = relative("sum by (job) (sum_over_time(latency[1h]))");
const COUNT_OVER_TIME: Query = relative("sum by (job) (count_over_time(latency[1h]))");
const SUM_RATE: Query = relative("sum by (job) (rate(requests_total[1h]))");

/// With raw data kept at 1 Hz every 10 s, a DDSketch built at query time
/// measures 77.8 CPU-ms/s against the exact plan's 39.0, and stored DDSketch
/// windows 69.7: the exact plan is selected.
#[test]
fn quantile_at_1_hz_every_10_s_with_raw_data_selects_the_exact_plan() {
    let (plan, _) = selected(&workload(QUANTILE, ONE_HZ_EVERY_10_S), true);
    assert!(
        !has_sketch(&plan),
        "selected a sketch plan ({:?}), measured 2x slower than exact",
        plan_kind(&plan)
    );
}

/// With raw data kept at 1 Hz every 10 s, a Max accumulator built at query
/// time measures 19.1 CPU-ms/s, slower than the exact plan's 14.6; stored
/// Max windows measure 5.6. Either of the two measured no slower than
/// exact may be selected, not the query-time build.
#[test]
fn max_at_1_hz_every_10_s_with_raw_data_does_not_build_at_query_time() {
    let (plan, _) = selected(&workload(MAX, ONE_HZ_EVERY_10_S), true);
    let kind = plan_kind(&plan);
    assert!(
        matches!(kind, PlanKind::ExactScan | PlanKind::StoredStates),
        "selected {kind:?}, measured slower than exact"
    );
}

/// With no raw data at 10 Hz every 60 s, stored DDSketch windows measure
/// 6.1 CPU-ms/s against the exact plan's 76.9 (with raw data): the
/// quantile still keeps a stored sketch.
#[test]
fn quantile_at_10_hz_every_60_s_without_raw_data_keeps_a_stored_sketch() {
    let (plan, _) = selected(&workload(QUANTILE, TEN_HZ_EVERY_60_S), false);
    assert_eq!(plan_kind(&plan), PlanKind::StoredStates);
    assert!(has_sketch(&plan));
}

/// Stored exact accumulators measure 3.3–4.0x cheaper than exact at 1 Hz
/// every 10 s and 37–41x at 10 Hz every 60 s, for `sum(rate)` and
/// `sum_over_time`: both keep them, with raw data kept.
#[test]
fn sum_rate_and_sum_over_time_keep_stored_exact_accumulators() {
    for setting in [ONE_HZ_EVERY_10_S, TEN_HZ_EVERY_60_S] {
        for query in [SUM_RATE, SUM_OVER_TIME] {
            let (plan, _) = selected(&workload(query, setting), true);
            assert_eq!(
                plan_kind(&plan),
                PlanKind::StoredStates,
                "{query:?} at {setting:?}"
            );
            assert!(!has_sketch(&plan), "{query:?} at {setting:?}");
        }
    }
}

/// `sum by (job) (rate(requests_total[1h]))` at 1 Hz every 300 s, raw data
/// kept: the query-time Rate over raw data measures 1.12 CPU-ms/s, stored
/// 300 s Rate windows 0.137, 8x less work and 45x lower latency. The
/// stored windows are selected.
#[test]
fn sum_rate_at_1_hz_every_300_s_keeps_stored_rate_states() {
    let (plan, _) = selected(&workload(SUM_RATE, ONE_HZ_EVERY_300_S), true);
    assert_eq!(plan_kind(&plan), PlanKind::StoredStates);
}

/// A plan measured against the exact plan: the stored-state plan (Stage 3's
/// selection without raw data), or a plan built at query time from raw data
/// that Stage 3 selected before the measured unit costs, kept as a fixture
/// (`default_cost_plan_ranking_fixtures/<name>.json`).
#[derive(Debug, Clone, Copy)]
enum Alternative {
    Stored,
    QueryTimeFixture(&'static str),
}

/// Measured total CPU, in CPU-ms per second of wall time, of the exact
/// plan and of an alternative, for each query and setting. Rate's exact
/// plan is the query-time Rate accumulator over raw data, which the
/// measurement used because the executor cannot run rate's query-time
/// lowering.
const MEASURED_CPU_MS_PER_S: [(Query, Setting, Alternative, f64, f64); 15] = [
    (
        QUANTILE,
        ONE_HZ_EVERY_10_S,
        Alternative::Stored,
        39.047,
        69.718,
    ),
    (
        QUANTILE,
        TEN_HZ_EVERY_60_S,
        Alternative::Stored,
        76.912,
        6.077,
    ),
    (MAX, ONE_HZ_EVERY_10_S, Alternative::Stored, 14.557, 5.572),
    (MAX, TEN_HZ_EVERY_60_S, Alternative::Stored, 17.763, 0.620),
    (
        SUM_OVER_TIME,
        ONE_HZ_EVERY_10_S,
        Alternative::Stored,
        30.101,
        8.758,
    ),
    (
        SUM_OVER_TIME,
        TEN_HZ_EVERY_60_S,
        Alternative::Stored,
        39.236,
        1.055,
    ),
    (
        COUNT_OVER_TIME,
        ONE_HZ_EVERY_10_S,
        Alternative::Stored,
        28.740,
        8.746,
    ),
    (
        COUNT_OVER_TIME,
        TEN_HZ_EVERY_60_S,
        Alternative::Stored,
        37.644,
        1.028,
    ),
    (
        SUM_RATE,
        ONE_HZ_EVERY_10_S,
        Alternative::Stored,
        35.126,
        13.079,
    ),
    (
        SUM_RATE,
        TEN_HZ_EVERY_60_S,
        Alternative::Stored,
        48.367,
        1.227,
    ),
    (
        SUM_RATE,
        ONE_HZ_EVERY_300_S,
        Alternative::Stored,
        1.118,
        0.137,
    ),
    (
        QUANTILE,
        ONE_HZ_EVERY_10_S,
        Alternative::QueryTimeFixture("quantile_1_hz_every_10_s_query_time_ddsketch"),
        39.047,
        77.786,
    ),
    (
        QUANTILE,
        TEN_HZ_EVERY_60_S,
        Alternative::QueryTimeFixture("quantile_10_hz_every_60_s_query_time_ddsketch"),
        76.912,
        140.022,
    ),
    (
        QUANTILE_RANK,
        ONE_HZ_EVERY_10_S,
        Alternative::QueryTimeFixture("quantile_rank_1_hz_every_10_s_query_time_kll"),
        39.118,
        43.483,
    ),
    (
        MAX,
        ONE_HZ_EVERY_10_S,
        Alternative::QueryTimeFixture("max_1_hz_every_10_s_query_time_max"),
        14.557,
        19.130,
    ),
];

/// The plan kept as fixture `name` (`default_cost_plan_ranking_fixtures/<name>.json`).
fn fixture_plan(name: &str) -> PhysicalASAPDAG {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/default_cost_plan_ranking_fixtures")
        .join(format!("{name}.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The exact plan's predicted CPU for `query`: its query-time lowering, or
/// for a rate the query-time Rate accumulator over raw data the
/// measurement ran (fixture `sum_rate_query_time_rate`, Stage 3's exact
/// plan over data at rest before the measured unit costs).
fn exact_cpu(query: Query, setting: Setting) -> f64 {
    if !query.promql.contains("rate(") {
        return exact_query_time_cpu(query, setting);
    }
    let plan = fixture_plan("sum_rate_query_time_rate");
    assert_eq!(plan_kind(&plan), PlanKind::QueryTimeExactAccumulator);
    predicted_cpu(&plan, &workload(query, setting))
}

/// The alternative's predicted CPU for `query` at `setting`.
fn alternative_cpu(query: Query, setting: Setting, alternative: Alternative) -> f64 {
    let w = workload(query, setting);
    match alternative {
        Alternative::Stored => {
            let (plan, cpu) = selected(&w, false);
            assert_eq!(plan_kind(&plan), PlanKind::StoredStates, "{query:?}");
            cpu
        }
        Alternative::QueryTimeFixture(name) => {
            let plan = fixture_plan(name);
            assert!(
                matches!(
                    plan_kind(&plan),
                    PlanKind::QueryTimeSketch | PlanKind::QueryTimeExactAccumulator
                ),
                "{name}"
            );
            predicted_cpu(&plan, &w)
        }
    }
}

/// For every measured query, setting and alternative, the predicted CPU
/// orders the exact plan and the alternative as the measurement does.
#[test]
fn predicted_cpu_orders_exact_and_alternative_plans_as_measured() {
    let mut wrong = Vec::new();
    for (query, setting, alternative, measured_exact, measured_alternative) in MEASURED_CPU_MS_PER_S
    {
        let exact = exact_cpu(query, setting);
        let predicted = alternative_cpu(query, setting, alternative);
        if (exact > predicted) != (measured_exact > measured_alternative) {
            wrong.push(format!(
                "{query:?} at {setting:?}, {alternative:?}: predicted exact {:.3} vs {:.3} \
                 CPU-ms/s, measured {measured_exact} vs {measured_alternative}",
                exact * 1e3,
                predicted * 1e3
            ));
        }
    }
    assert!(wrong.is_empty(), "misranked:\n{}", wrong.join("\n"));
}

/// A query reading stored windows waits for the newest window's
/// compaction: each ingest worker beyond the first builds its own partial
/// copy of that window, merged at query time, one merge per state per extra
/// worker. Example: the stored DDSketch windows of the quantile over
/// 100,000 series sampled every 10 ms (10 M rows per second, at 210 ns per
/// row more than one core of ingestion work) merge at least one extra copy
/// of each of the window's 100,000 states, and a DDSketch merge measures
/// 1.8–2.5 µs: at least 180 ms. One operation (1 ns) per merge would
/// predict 0.2 ms.
#[test]
fn compaction_latency_charges_each_merge_its_family_s_merge_time() {
    let (plan, _) = selected(&workload(QUANTILE, TEN_HZ_EVERY_60_S), false);
    assert_eq!(plan_kind(&plan), PlanKind::StoredStates);
    assert!(has_sketch(&plan));
    let mut heavy = serde_json::to_value(workload(QUANTILE, TEN_HZ_EVERY_60_S)).unwrap();
    let data = &mut heavy["data_workload"];
    data["arrival"]["ingesting"]["rows_per_second"] = serde_json::json!(10_000_000.0);
    data["sources"]["latency"]["series_count"] = serde_json::json!(100_000);
    data["sources"]["latency"]["sample_interval"] = serde_json::json!(10);
    let heavy: PlanningWorkload = serde_json::from_value(heavy).unwrap();
    let entries: Vec<_> = heavy.query_workload.entries().collect();
    let (cost, _) = asap_cost::plan_cost(
        &plan,
        &entries,
        heavy.data_workload.as_ref().unwrap(),
        &CostInputs::default(),
    )
    .unwrap();
    let compaction_ms = cost.usage.latency_per_query[0].compaction_ms;
    assert!(
        compaction_ms >= 100_000.0 * 1.8e-3,
        "predicted compaction {compaction_ms} ms, measured merges take at least 180 ms"
    );
}

/// Partial copies of an ingestion-time node that holds no states (raw rows
/// a time range keeps, or the values of an exact `Aggregate`) are
/// concatenated, not merged: the executor merges ingest shards only of
/// summary state columns. So reading one adds no compaction latency.
/// Example: `max_over_time(latency[5m])` over 100,000 series sampled every
/// 1 ms (100 M rows per second, more than one core of ingestion work) with
/// its 5m time range, and then also its per-series max, kept at ingestion
/// time: an exact accumulator's merge (230 ns) per row of the 5m window
/// read would predict seconds of compaction for no merge work.
#[test]
fn compaction_latency_is_zero_for_ingestion_time_nodes_holding_no_states() {
    use asap_types::ir::properties::ExecutionDataState;
    use asap_types::ir::NonASAPOp;
    use asap_types::stages::{Lifetime, StorageMedium};
    let query = Query {
        promql: "max_over_time(latency[5m])",
        accuracy_metric: "relative",
    };
    let mut heavy = serde_json::to_value(workload(query, TEN_HZ_EVERY_60_S)).unwrap();
    let data = &mut heavy["data_workload"];
    data["arrival"]["ingesting"]["rows_per_second"] = serde_json::json!(100_000_000.0);
    data["sources"]["latency"]["series_count"] = serde_json::json!(100_000);
    data["sources"]["latency"]["sample_interval"] = serde_json::json!(1);
    let heavy: PlanningWorkload = serde_json::from_value(heavy).unwrap();
    let roots = lower_promql_roots(&heavy).unwrap();
    let (exact, _) =
        asap_types::test_support::forwarders::physical_asap_dag_at_query_time_with_node_ids(&roots)
            .unwrap();
    let entries: Vec<_> = heavy.query_workload.entries().collect();
    let data = heavy.data_workload.as_ref().unwrap();
    // Scan, then the 5m `TimeRange`, then the per-series max `Aggregate`.
    let is_time_range = |payload: &PhysicalASAPOperatorPayload| {
        matches!(
            payload,
            PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::TimeRange { .. })
        )
    };
    let is_aggregate = |payload: &PhysicalASAPOperatorPayload| {
        matches!(
            payload,
            PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Aggregate { .. })
        )
    };
    let kept_at_ingestion = |up_to_aggregate: bool| {
        let mut plan = exact.clone();
        let ingested: Vec<_> = plan
            .nodes
            .iter()
            .filter(|node| !is_aggregate(&node.payload) || up_to_aggregate)
            .map(|node| node.id)
            .collect();
        for node in &mut plan.nodes {
            if !ingested.contains(&node.id) {
                continue;
            }
            node.output_state = ExecutionDataState::INGESTION_ROWS;
            if is_time_range(&node.payload) || is_aggregate(&node.payload) {
                node.lifetime = Lifetime::AcrossEvaluations(StorageMedium::Memory);
                node.retention_ms = Some(300_000);
            }
        }
        for edge in &mut plan.edges {
            if ingested.contains(&edge.producer) {
                *edge.data_state_mut() = ExecutionDataState::INGESTION_ROWS;
            }
        }
        plan
    };
    for (kept, plan) in [
        ("time range", kept_at_ingestion(false)),
        ("time range and max", kept_at_ingestion(true)),
    ] {
        let (cost, _) =
            asap_cost::plan_cost(&plan, &entries, data, &CostInputs::default()).unwrap();
        let ingest_ms_per_second: f64 = cost
            .per_node
            .values()
            .filter(|node| node.timing == ExecutionTiming::IngestionTime)
            .map(|node| node.cpu_core_seconds_per_second * 1_000.0)
            .sum();
        assert!(
            ingest_ms_per_second > 1_000.0,
            "{kept}: ingestion takes {ingest_ms_per_second} ms per second, one core"
        );
        assert_eq!(
            cost.usage.latency_per_query[0].compaction_ms, 0.0,
            "{kept} kept at ingestion time"
        );
    }
}
