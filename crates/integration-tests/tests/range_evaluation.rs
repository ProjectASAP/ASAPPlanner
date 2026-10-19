//! #757: a query answered as a range request (`RangeEvaluation`), e.g. a
//! dashboard panel that evaluates the last hour at 1-minute steps on every
//! refresh. Stage 1 and Stage 2 keep pane widths on the step grid, and
//! Stage 3 prices `span_ms / step_ms + 1` evaluations per request, credits
//! the panes kept across steps and requests, and retains kept panes for
//! the span plus the window.

mod planner_layering_common;

use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_plan_selection::RejectionCause;
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::ir::ASAPOp;
use asap_types::physical::{Materialization, Medium};
use asap_types::workload::{
    DataArrival, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload, RangeEvaluation, RepeatedDemand, RepeatingEntry,
    RepetitionInterval, SourceFacts,
};
use planner_layering_common::*;

/// The dashboard's range: the last hour at 1-minute steps, 61 evaluations
/// per request.
const LAST_HOUR_BY_MINUTE: RangeEvaluation = RangeEvaluation {
    step_ms: 60_000,
    span_ms: 3_600_000,
};

/// Pattern B (#509 Example 3: `quantile_over_time(0.99, latency_ms[5m])`
/// every minute over 1M series) with each request answered as `range`.
fn pattern_b_with(range: Option<RangeEvaluation>) -> PlanningWorkload {
    let mut workload = pattern_b();
    for entry in workload
        .query_workload
        .repeating_queries
        .iter_mut()
        .flatten()
    {
        entry.range_evaluation = range;
    }
    workload
}

/// A deployment that keeps query-time results across evaluations (B3).
fn keeping_query_time_results() -> DeploymentCapabilities {
    DeploymentCapabilities {
        query_time_results_retained: true,
        ..DeploymentCapabilities::default()
    }
}

/// `query` every `interval_ms` over 1M series of `m` sampled every 15 s,
/// arriving continuously, exact, answered as `range`.
fn exact_panel(query: &str, interval_ms: u32, range: Option<RangeEvaluation>) -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![RepeatingEntry {
                query: Query(query.into()),
                demand: RepeatedDemand::FixedInterval(RepetitionInterval(interval_ms)),
                requirements: QueryRequirements::default(),
                predictability: Predictability::Predictable { known_at: None },
                range_evaluation: range,
            }]),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: SHARED_ROWS_PER_SECOND,
            },
            sources: [(
                "m".to_string(),
                SourceFacts {
                    series_count: Some(1_000_000),
                    sample_interval: Some(DurationMs(15_000)),
                    ..SourceFacts::default()
                },
            )]
            .into(),
        }),
    }
}

fn cost(run: &Run, id: &str) -> f64 {
    run.cost(id).unwrap_or_else(|| panic!("{id} is not priced"))
}

fn close(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() <= 1e-6 * expected.abs().max(1.0)
}

/// Each physical candidate's pane chains: (pane width, materialization).
fn pane_chains(run: &Run) -> Vec<(String, u64, Materialization)> {
    run.physical
        .iter()
        .flat_map(|p| {
            p.dag().nodes.iter().filter_map(|n| match &n.payload {
                PhysicalASAPOperatorPayload::ASAP(ASAPOp::PaneChain { pane_width_ms, .. }) => {
                    Some((p.id.clone(), *pane_width_ms, n.materialization))
                }
                _ => None,
            })
        })
        .collect()
}

/// The candidate whose single pane chain is materialized as `wanted`.
fn with_chain(run: &Run, wanted: Materialization) -> String {
    pane_chains(run)
        .into_iter()
        .find(|(_, _, m)| *m == wanted)
        .unwrap_or_else(|| panic!("no candidate with a {wanted:?} chain"))
        .0
}

/// (a) The issue's acceptance example: Pattern B as a dashboard over the
/// last hour at 1-minute steps prices 61 evaluations per request. Every
/// query-time computation runs at every step, so the whole-window KLL
/// (2.1 → 128.1 cost/s), the panes rebuilt per step (2.18333 → 133.18333)
/// and the exact plan (2.4 → 146.4) cost 61 times as much. The kept panes
/// (B3) are built once per request (one new 1-min pane) but merged at every
/// step, and keep 64 older panes instead of 4 (1 h + 5 min of panes): 8,884.53
/// → 142,151.53. B1 keeps 66 instead of 6: 13,326.53 → 146,593.53. The
/// whole-window KLL is still selected.
#[test]
fn stage3_plan_selection_prices_61_evaluations_per_dashboard_request() {
    let deployment = keeping_query_time_results();
    let none = run_promql_for(&pattern_b_with(None), &deployment);
    let range = run_promql_for(&pattern_b_with(Some(LAST_HOUR_BY_MINUTE)), &deployment);
    assert_eq!(LAST_HOUR_BY_MINUTE.evaluations_per_request(), 61);
    for (id, before, after) in [
        ("P1", 2.4, 146.4),
        ("P2", 2.1, 128.1),
        ("P3", 2.183_333_333_333_333, 133.183_333_333_333_37),
        ("P3-m1", 13_326.530_002_066_667, 146_593.530_002_066_7),
        ("P3-m2", 8_884.53, 142_151.53),
    ] {
        assert!(close(cost(&none, id), before), "{id}: {}", cost(&none, id));
        assert!(close(cost(&range, id), after), "{id}: {}", cost(&range, id));
    }
    // Every query-time computation that is not kept runs once per step.
    for id in ["P1", "P2", "P3"] {
        assert!(close(cost(&range, id), 61.0 * cost(&none, id)), "{id}");
    }
    let memory = |run: &Run, id: &str| run.selection.costs[id].usage.average_memory_bytes;
    // B3: 4 → 64 panes kept between requests; B1: 6 → 66 panes kept.
    assert_eq!(memory(&none, "P3-m2"), 71_072_000_000.0);
    assert_eq!(memory(&range, "P3-m2"), 71_072_000_000.0 / 4.0 * 64.0);
    assert_eq!(memory(&none, "P3-m1"), 106_608_000_000.0);
    assert_eq!(memory(&range, "P3-m1"), 106_608_000_000.0 / 6.0 * 66.0);
    assert_eq!(range.selection.selected, "P2");
}

/// (b) Refreshing every minute at 30-second steps, Stage 1 offers panes of
/// `gcd(5 min, 1 min, 30 s) = 30 s`, on the step grid, and Stage 2 keeps
/// them; without the range, the panes are 1 min wide. Stage 2 never offers
/// a chain whose width does not divide the step.
#[test]
fn stage2_physical_asap_optimization_offers_only_step_aligned_pane_widths() {
    let step_30s = RangeEvaluation {
        step_ms: 30_000,
        span_ms: 3_600_000,
    };
    let range = run_promql(&exact_panel("max_over_time(m[5m])", 60_000, Some(step_30s)));
    let widths: Vec<u64> = pane_chains(&range).into_iter().map(|(_, w, _)| w).collect();
    assert!(!widths.is_empty(), "panes are offered");
    assert!(widths.iter().all(|&w| w == 30_000), "{widths:?}");
    assert!(range.pruned.is_empty(), "{:?}", range.pruned);

    let none = run_promql(&exact_panel("max_over_time(m[5m])", 60_000, None));
    let widths: Vec<u64> = pane_chains(&none).into_iter().map(|(_, w, _)| w).collect();
    assert!(
        !widths.is_empty() && widths.iter().all(|&w| w == 60_000),
        "{widths:?}"
    );
}

/// (c) A plan that keeps panes reports how long the runtime must keep
/// them: the window plus the range-evaluation span, 5 min + 1 h =
/// 3,900,000 ms, so the next refresh's earliest step finds its panes.
/// Without the range it is the window, 300,000 ms.
#[test]
fn stage3_plan_selection_reports_the_retention_of_kept_panes() {
    let deployment = keeping_query_time_results();
    let query = "max_over_time(m[5m])";
    let range = run_promql_for(
        &exact_panel(query, 60_000, Some(LAST_HOUR_BY_MINUTE)),
        &deployment,
    );
    let kept = with_chain(
        &range,
        Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory),
    );
    assert_eq!(range.selection.selected, kept);
    let retention: Vec<u64> = range
        .selection
        .pane_retention_ms
        .values()
        .copied()
        .collect();
    assert_eq!(retention, [3_900_000]);

    // Pattern B without a range selects the whole-window KLL, which keeps
    // nothing; B1 reports its window when it is the plan (no raw data).
    let none = run_promql(&pattern_b_with(None));
    assert!(none.selection.pane_retention_ms.is_empty());
    let b1 = run_promql_for(
        &pattern_b_with(None),
        &DeploymentCapabilities {
            raw_data_retained: false,
            ..DeploymentCapabilities::default()
        },
    );
    let retention: Vec<u64> = b1.selection.pane_retention_ms.values().copied().collect();
    assert_eq!(retention, [PATTERN_B_WINDOW_MS]);
}

/// (d) Without a range evaluation nothing changes: Pattern B's pinned
/// costs (#509 Example 3 and 4: whole-window KLL 2.1, panes 2.18333, B3
/// 8,884.53, B1 106.6 GB kept) are as before, and a range of one step per
/// request (`span_ms: 0`) prices exactly as no range.
#[test]
fn no_range_evaluation_leaves_every_price_unchanged() {
    let deployment = keeping_query_time_results();
    let none = run_promql_for(&pattern_b_with(None), &deployment);
    assert!(close(cost(&none, "P2"), 2.1));
    assert!(close(cost(&none, "P3"), 2.1 + 5e6 * 1e-6 / 60.0));
    assert!(close(cost(&none, "P3-m2"), 8_884.53));
    assert_eq!(
        none.selection.costs["P3-m1"].usage.average_memory_bytes,
        106_608_000_000.0
    );
    let one_step = run_promql_for(
        &pattern_b_with(Some(RangeEvaluation {
            step_ms: 60_000,
            span_ms: 0,
        })),
        &deployment,
    );
    assert_eq!(one_step.selection.costs, none.selection.costs);
    assert_eq!(one_step.selection.selected, none.selection.selected);
}

/// The example where declaring the range makes Stage 3 prefer sharing:
/// `max_over_time(m[5m])` every minute over 1M series. As one evaluation
/// per minute, rebuilding the window (P1, 2.0833 cost/s) beats the kept
/// panes (P2-m2, 4.5133: 32 MB of per-series maxima kept) and the panes
/// maintained at ingestion (P2-m1, 6.5133). As a dashboard over the last
/// hour at 1-minute steps, rebuilding costs 61 times as much (127.0833),
/// while the kept panes add only the per-step merges and 1 h of panes
/// (70.5133), and B1 72.5133: the kept panes are selected, and without
/// query-time retention, the ingestion-time panes.
#[test]
fn stage3_plan_selection_prefers_kept_panes_once_the_range_is_declared() {
    let query = "max_over_time(m[5m])";
    let deployment = keeping_query_time_results();
    let none = run_promql_for(&exact_panel(query, 60_000, None), &deployment);
    let range = run_promql_for(
        &exact_panel(query, 60_000, Some(LAST_HOUR_BY_MINUTE)),
        &deployment,
    );
    let kept = with_chain(
        &range,
        Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory),
    );
    let ingestion = with_chain(&range, Materialization::MatAtIngestion(Medium::Memory));
    for (run, id, expected) in [
        (&none, "P1", 2.083_333),
        (&none, kept.as_str(), 4.513_333),
        (&none, ingestion.as_str(), 6.513_333),
        (&range, "P1", 127.083_333),
        (&range, kept.as_str(), 70.513_333),
        (&range, ingestion.as_str(), 72.513_333),
    ] {
        assert!(
            (cost(run, id) - expected).abs() < 1e-5,
            "{id}: {}",
            cost(run, id)
        );
    }
    assert_eq!(none.selection.selected, "P1");
    assert_eq!(range.selection.selected, kept);
    let p1 = range
        .selection
        .rejected
        .iter()
        .find(|r| r.id == "P1")
        .unwrap();
    assert_eq!(p1.cause, RejectionCause::Costlier);

    let without_retention = run_promql(&exact_panel(query, 60_000, Some(LAST_HOUR_BY_MINUTE)));
    assert_eq!(without_retention.selection.selected, ingestion);
}

/// (e) A chain over an offset input reaches back its offset further: with
/// no raw data kept, `max_over_time(m[60m] offset 10m)` every 10 minutes
/// keeps six 10-minute panes ending 10 minutes before each evaluation, so
/// its panes must be kept for 10 min + 60 min = 4,200,000 ms, and the kept
/// memory Stage 3 prices covers that seventh pane; without the offset the
/// retention is the window, 3,600,000 ms.
#[test]
fn an_offset_chain_is_retained_and_priced_for_its_offset() {
    let no_raw_data = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let plain = run_promql_for(
        &exact_panel("max_over_time(m[60m])", 600_000, None),
        &no_raw_data,
    );
    let offset = run_promql_for(
        &exact_panel("max_over_time(m[60m] offset 10m)", 600_000, None),
        &no_raw_data,
    );
    let retention =
        |run: &Run| -> Vec<u64> { run.selection.pane_retention_ms.values().copied().collect() };
    assert_eq!(retention(&plain), [3_600_000]);
    assert_eq!(retention(&offset), [4_200_000]);
    let memory = |run: &Run| {
        run.selection.costs[&run.selection.selected]
            .usage
            .average_memory_bytes
    };
    assert!(
        memory(&offset) > memory(&plain),
        "{} vs {}",
        memory(&offset),
        memory(&plain)
    );
}
