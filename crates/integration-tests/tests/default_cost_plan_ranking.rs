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

use std::collections::BTreeSet;

use asap_cost::{CostInputs, CostModel, ResourceUnitCost};
use asap_devtools::planning_stages::{
    lower_promql_roots, run_planning_stages_over, PlanningStagesRun,
};
use asap_planner::{DeploymentCapabilities, PlanningModels};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator};
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

/// The exclusions of a deployment that runs neither wider tumbling-window
/// levels nor covers over them: both operators, as a runtime lacking them
/// declares.
fn without_wider_levels() -> Vec<asap_planner::CandidateExclusion> {
    serde_json::from_str(r#"[{"operator": "SummaryMergeWindows"}, {"operator": "TimeRangeCover"}]"#)
        .unwrap()
}

/// The planning stages' run over `workload` under the default cost inputs,
/// with raw data kept or not.
fn planning_run(workload: &PlanningWorkload, raw_data_retained: bool) -> PlanningStagesRun {
    let settings = cpu_ranked_cost_inputs();
    // The stored plans were measured with one tumbling-window chain per
    // range, before wider levels and covers: a deployment without both
    // operators plans those.
    let capabilities = DeploymentCapabilities {
        raw_data_retained,
        excluded_candidates: without_wider_levels(),
        ..Default::default()
    };
    let models = PlanningModels::builtin()
        .with_cost_inputs(&settings)
        .with_deployment_capabilities(&capabilities);
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let roots = lower_promql_roots(workload).unwrap();
    run_planning_stages_over(roots, &entries, &data, models).unwrap()
}

/// The plan Stage 3 selects for `workload` under the default cost inputs,
/// with raw data kept or not, and its predicted CPU core-seconds per second.
fn selected(workload: &PlanningWorkload, raw_data_retained: bool) -> (PhysicalASAPDAG, f64) {
    let run = planning_run(workload, raw_data_retained);
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
    let (cost, _) = asap_cost::plan_cost(plan, &entries, &data, &cpu_ranked_cost_inputs()).unwrap();
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

/// 10 Hz, every 10 s.
const TEN_HZ_EVERY_10_S: Setting = Setting {
    sample_interval_ms: 100,
    evaluation_interval_ms: 10_000,
};
/// 1 Hz, every 60 s.
const ONE_HZ_EVERY_60_S: Setting = Setting {
    sample_interval_ms: 1_000,
    evaluation_interval_ms: 60_000,
};

/// 10 Hz, every 300 s.
const TEN_HZ_EVERY_300_S: Setting = Setting {
    sample_interval_ms: 100,
    evaluation_interval_ms: 300_000,
};

/// `quantile_over_time(0.99, latency[1h])`, 1% relative, at 10 Hz, raw data
/// kept. Measured cost per second (cost model 1's default prices on the
/// measured CPU and memory), exact plan against stored DDSketch windows:
/// 417.8 against 287.0 every 10 s, 71.3 against 44.0 every 60 s, 13.9
/// against 11.0 every 300 s; the stored plan answers 4–80x faster. Each
/// stored state holds 4.3–5.4 KB (`DdSketch::memory_bytes`); charged the
/// 18.8 KB bound for values over nine decades, the stored plan was
/// predicted 2x more expensive than exact and never selected.
///
/// The same with the realistic mode's declared ranges
/// (`column_magnitudes`: latency [0.12, 1.83e6], counter [1, 3.27e8]).
/// The ranking rests on the wide-spread margin (σ = 2.5) staying below the
/// point where it flips (σ = 3, `wide_spread_values_magnitude_ratio`).
#[test]
fn quantile_at_10_hz_with_raw_data_keeps_stored_ddsketch_windows() {
    for (gauge, counter) in [BENCHMARK_RANGES[0], BENCHMARK_RANGES[3]] {
        for setting in [TEN_HZ_EVERY_10_S, TEN_HZ_EVERY_60_S, TEN_HZ_EVERY_300_S] {
            let workload = declaring_ranges(workload(QUANTILE, setting), gauge, counter);
            let (plan, _) = selected(&workload, true);
            assert_eq!(
                plan_kind(&plan),
                PlanKind::StoredStates,
                "{setting:?}, declared {gauge:?} / {counter:?}"
            );
            assert!(has_sketch(&plan), "{setting:?}, declared {gauge:?}");
        }
    }
}

/// With the default (Pareto) and Zipf modes' declared ranges, ten and nine
/// decades across every series' scale, a stored DDSketch state is charged
/// at least the spread of its rows log-uniform over that range (the
/// declared range's floor, so values that fill it are not under-charged).
/// Priced at the view layout a stored window takes (narrow counts, sparse
/// when few buckets are occupied), the stored plan stays below the exact
/// one at every interval even at that floor; at 8 bytes per bucket of an
/// owned sketch it was priced above it at 10 s and 300 s.
/// Per-series values of these modes may span much less than the column
/// does; which plan is cheaper for them is for a measured table's bytes
/// per state (fitted from the data plane's stored states) to settle. These
/// pin the selection the floor gives until calibration.
#[test]
fn quantile_at_10_hz_under_column_wide_declared_ranges_follows_the_floor() {
    use PlanKind::StoredStates;
    let expected = [
        (BENCHMARK_RANGES[1], [StoredStates; 3]),
        (BENCHMARK_RANGES[2], [StoredStates; 3]),
    ];
    for ((gauge, counter), kinds) in expected {
        let settings = [TEN_HZ_EVERY_10_S, TEN_HZ_EVERY_60_S, TEN_HZ_EVERY_300_S];
        for (setting, kind) in settings.into_iter().zip(kinds) {
            let workload = declaring_ranges(workload(QUANTILE, setting), gauge, counter);
            let (plan, _) = selected(&workload, true);
            assert_eq!(
                plan_kind(&plan),
                kind,
                "{setting:?}, declared {gauge:?} / {counter:?}"
            );
        }
    }
}

/// A declared `column_magnitudes` entry: the (min, max) of the values'
/// magnitudes.
type MagnitudeRange = (f64, f64);

/// The `column_magnitudes` (min, max of the values' magnitudes) the
/// benchmark's value modes declare for its gauge and its counter: none; the
/// default (Pareto gauge) mode; Zipf integers; the realistic mode.
const BENCHMARK_RANGES: [(Option<MagnitudeRange>, Option<MagnitudeRange>); 4] = [
    (None, None),
    (Some((0.0603, 1.51e9)), Some((0.0603, 1.64e8))),
    (Some((1.0, 1.51e9)), Some((1.0, 5.6e9))),
    (Some((0.12, 1.83e6)), Some((1.0, 3.27e8))),
];

/// `workload` with `latency`'s `value` declared within `gauge` and
/// `requests_total`'s within `counter` (min, max of their magnitudes);
/// `None` declares nothing.
fn declaring_ranges(
    workload: PlanningWorkload,
    gauge: Option<(f64, f64)>,
    counter: Option<(f64, f64)>,
) -> PlanningWorkload {
    let mut workload = serde_json::to_value(workload).unwrap();
    let sources = &mut workload["data_workload"]["sources"];
    for (source, range) in [("latency", gauge), ("requests_total", counter)] {
        if let Some((min_abs, max_abs)) = range {
            sources[source]["column_magnitudes"] =
                serde_json::json!({"value": {"min_abs": min_abs, "max_abs": max_abs}});
        }
    }
    serde_json::from_value(workload).unwrap()
}

/// `plan` with every KLL it keeps or builds at `k`.
fn with_kll_k(plan: &PhysicalASAPDAG, k: u32) -> PhysicalASAPDAG {
    use asap_types::summary_type::SketchParams;
    let mut plan = plan.clone();
    for node in &mut plan.nodes {
        if let Some(SketchParams::Kll { k: old }) = &mut node.parameters {
            *old = k;
        }
    }
    plan
}

/// `quantile_over_time(0.99, latency[1h])`, 1% rank, at 1 Hz every 10 s and
/// every 60 s, raw data not kept. Stage 1 offers stored KLL windows at
/// several `k` (269, 547, 1,404 and 2,863: one per split of the failure
/// probability over the windows' merge). Merging 360 windows of 10 rows per
/// series measures 4.8 ms per series at k = 1,404 and 1.7 ms at k = 547
/// (the merged sketch grows towards its `k`). Measured cost per second,
/// k = 1,404 against k = 547: 589 against 240 every 10 s, 31.4 against 19.4
/// every 60 s. Charged a flat merge per state, every `k` cost the same to
/// the last digit and the MILP picked any. The plan now keeps the smallest
/// `k` offered, k = 1,404 is charged over 1.25x more than k = 547 (the
/// measured pair), and the next larger `k` than the smallest is charged
/// more (not measured).
#[test]
fn quantile_rank_without_raw_data_charges_the_larger_kll_more() {
    use asap_types::summary_type::SketchParams;
    for setting in [ONE_HZ_EVERY_10_S, ONE_HZ_EVERY_60_S] {
        let w = workload(QUANTILE_RANK, setting);
        let run = planning_run(&w, false);
        // The `k`s of the stored KLL windows (tumbling-window chains)
        // Stage 1 offers, smallest first.
        let offered_kll_ks: Vec<u32> = run
            .run
            .logical_asap_candidates
            .as_ref()
            .expect("Stage 1 ran")
            .memo
            .groups()
            .iter()
            .flat_map(|group| &group.candidates)
            .filter(|candidate| {
                matches!(
                    candidate.expression.operator,
                    Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
                )
            })
            .filter_map(
                |candidate| match candidate.logical_properties.parameters() {
                    Some(SketchParams::Kll { k }) => Some(*k),
                    _ => None,
                },
            )
            .collect::<BTreeSet<u32>>()
            .into_iter()
            .collect();
        let (plan, _) = run.run.selection.as_ref().expect("a plan is selected");
        assert_eq!(plan_kind(plan), PlanKind::StoredStates, "{setting:?}");
        let [smallest, next_larger, ..] = offered_kll_ks[..] else {
            panic!("{setting:?}: Stage 1 offers stored KLL windows at k {offered_kll_ks:?}");
        };
        let selected_kll_ks: Vec<u32> = plan
            .nodes
            .iter()
            .filter_map(|node| match node.parameters {
                Some(SketchParams::Kll { k }) => Some(k),
                _ => None,
            })
            .collect();
        assert!(
            !selected_kll_ks.is_empty() && selected_kll_ks.iter().all(|&k| k == smallest),
            "{setting:?}: selected KLL k {selected_kll_ks:?}, offered {offered_kll_ks:?}"
        );
        let entries: Vec<_> = w.query_workload.entries().collect();
        let data = w.data_workload.clone().unwrap();
        let cost = |k| {
            let (cost, _) = asap_cost::plan_cost(
                &with_kll_k(plan, k),
                &entries,
                &data,
                &cpu_ranked_cost_inputs(),
            )
            .unwrap();
            cost.cost_per_second.unwrap()
        };
        assert!(
            cost(1_404) > 1.25 * cost(547),
            "{setting:?}: k = 1,404 predicted {}/s, k = 547 {}/s; measured 1.6-2.5x",
            cost(1_404),
            cost(547)
        );
        let (large, small) = (cost(next_larger), cost(smallest));
        assert!(
            large > small,
            "{setting:?}: k = {next_larger} predicted {large}/s, k = {smallest} {small}/s"
        );
    }
}

/// `sum by (job) (sum_over_time(latency[5m]))` at 1 Hz, raw data kept.
/// Measured cost per second, the query-time Sum accumulator grouped by
/// `job` against the selected plan: 1.44 against 1.73 (stored 10 s Sum
/// windows) every 10 s, 0.047 against 0.094 (the exact per-series
/// aggregate) every 300 s. The stored plan
/// pays a finalize of 2.5 µs per series (charged 40 ns), and the exact plan
/// carries each row's `job` label through its per-series aggregate (37 ns
/// per row, uncharged).
///
/// Every 10 s, the stored 10 s Sum windows are selected since exact state
/// is priced at its sealed 24-byte record (it was 56 bytes, a `ScalarState`
/// with vector slack, when this was measured): to be re-measured with
/// sealed records before this setting asserts a plan again. Every 300 s the
/// query-time accumulator still wins.
#[test]
fn sum_over_time_5m_at_1_hz_with_raw_data_builds_the_sum_at_query_time() {
    let query = relative("sum by (job) (sum_over_time(latency[5m]))");
    let (plan, _) = selected(&workload(query, ONE_HZ_EVERY_300_S), true);
    assert_eq!(plan_kind(&plan), PlanKind::QueryTimeExactAccumulator);
    let (plan, _) = selected(&workload(query, ONE_HZ_EVERY_10_S), true);
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
/// plan is the query-time counter accumulator over raw data, which the
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
/// for a rate the query-time counter accumulator over raw data the
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
        &cpu_ranked_cost_inputs(),
    )
    .unwrap();
    // The newest window's refresh, its compaction included.
    let compaction_ms = cost.usage.latency_per_query[0].precompute_execution_ms;
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
            asap_cost::plan_cost(&plan, &entries, data, &cpu_ranked_cost_inputs()).unwrap();
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
            cost.usage.latency_per_query[0].precompute_execution_ms, 0.0,
            "{kept} kept at ingestion time"
        );
    }
}

/// `max_over_time(latency[5m])` at 1 Hz, raw data kept, every 10 s and
/// every 60 s. Measured cost per second, stored Max windows against the
/// exact plan: 1.44 against 1.62–1.66 every 10 s, 0.240 against about
/// 0.27 every 60 s (regret 1.13–1.24 when exact was selected). The stored
/// plan was over-charged 29%: a Max state's merge measures 125 ns, not a
/// Sum's 230, and a chain emits an exact state in 1.3–2.1 µs, not 2.5.
#[test]
fn max_over_time_5m_at_1_hz_with_raw_data_keeps_stored_max_windows() {
    let query = relative("max_over_time(latency[5m])");
    for setting in [ONE_HZ_EVERY_10_S, ONE_HZ_EVERY_60_S] {
        let (plan, _) = selected(&workload(query, setting), true);
        assert_eq!(plan_kind(&plan), PlanKind::StoredStates, "{setting:?}");
    }
}

/// `sum by (job) (rate(requests_total[5m]))` and the same `increase` at
/// 10 Hz every 300 s, raw data kept. Measured cost per second (cost model
/// 1's default prices on the measured usage), the Rate or Increase built at
/// query time from raw rows against stored 300 s windows: 0.86 against
/// 0.95 for both. A grouping's cost per row depends on the sample rate:
/// the executor visits each group's state once per 10 s raw batch, which
/// holds 100 rows of a series at 10 Hz and 10 at 1 Hz, and a chain also
/// splits each group's rows of a batch by window. Charged one per-row
/// constant fitted across both rates, the query-time build was
/// over-charged at 10 Hz (69 ns per row measured, 82 charged) while the
/// stored chain was charged about right, so the stored plan was predicted
/// cheaper (regret 1.08–1.10 in every run).
#[test]
fn sum_rate_and_increase_over_5m_at_10_hz_every_300_s_with_raw_data_build_at_query_time() {
    for query in [
        relative("sum by (job) (rate(requests_total[5m]))"),
        relative("sum by (job) (increase(requests_total[5m]))"),
    ] {
        let (plan, _) = selected(&workload(query, TEN_HZ_EVERY_300_S), true);
        assert_eq!(
            plan_kind(&plan),
            PlanKind::QueryTimeExactAccumulator,
            "{query:?}"
        );
    }
}

/// Two identical `sum_over_time(m[5m])` every 60 s over 1,000 series of a
/// gauge at 1 Hz, raw data kept, no accuracy target. Measured cost per
/// evaluation (cost model 1's default prices on the measured usage), the
/// exact scan against stored 10 s Sum windows: 0.323 against 0.261. The
/// exact plan's per-series hash aggregate measures 46 ns per row where it
/// was charged 35: at 1 Hz a 10 s raw batch holds 10 rows of a series, and
/// the executor visits each series' group once per batch. Charged per row
/// alone, the two plans were predicted within 0.4% (0.2527 against 0.2538)
/// and the exact scan was selected.
#[test]
fn identical_sum_over_time_pair_at_1_hz_keeps_stored_sum_windows() {
    let entry = r#"{"query": "sum_over_time(m[5m])", "demand": {"fixed_interval_at": {"interval": 60000, "evaluation_phase": 0}},
        "requirements": {"max_latency_ms": null}, "predictability": {"predictable": {"known_at": null}}}"#;
    let workload: PlanningWorkload = serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{entry}, {entry}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 2000.0}}}},
              "sources": {{
                "m": {{"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000}},
                "c": {{"series_count": 1000, "metric_type": "counter", "sample_interval": 1000}}}}}}}}"#
    ))
    .unwrap();
    let (plan, _) = selected(&workload, true);
    assert_eq!(plan_kind(&plan), PlanKind::StoredStates);
}

/// The default cost inputs with disk at no cost: the module's measurements
/// are CPU only.
fn cpu_ranked_cost_inputs() -> CostInputs {
    CostInputs {
        cost_model: CostModel::UsageOverTime(ResourceUnitCost {
            cost_per_disk_byte_second: 0.0,
            ..ResourceUnitCost::illustrative()
        }),
        ..CostInputs::default()
    }
}
