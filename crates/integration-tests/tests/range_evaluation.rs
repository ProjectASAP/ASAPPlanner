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

/// Price explicitly named computations under the same models as production.
/// This fixture controls its pane width; a timed MILP archive is not a catalog
/// of every legal materialization or a stable source of P1/P2 identifiers.
fn run_explicit_for(workload: &PlanningWorkload, deployment: &DeploymentCapabilities) -> Run {
    use asap_logical_asap_optimization::pass1::per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates, ComputationCandidate,
    };
    use asap_logical_asap_optimization::pass2::asap_aware_cse::{
        pane_candidates, window_composition_pass2_candidates,
    };
    use asap_types::ir::schema::SketchAlgorithm;
    let roots = lower_promql(workload);
    let inventory =
        generate_per_computation_candidates(roots.iter().cloned().enumerate().collect()).unwrap();
    assert_eq!(inventory.targets.len(), 1);
    let compose = |choice| {
        compose_logical_candidate(&inventory, &[choice])
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect::<Vec<_>>()
    };
    let mut candidates = vec![compose(0)];
    if let Some(choice) = inventory.targets[0]
        .choices
        .candidates
        .iter()
        .position(|candidate| {
            matches!(
                candidate,
                ComputationCandidate::Summary(SketchAlgorithm::Kll)
            )
        })
    {
        candidates.push(compose(choice));
    }
    let build = candidates.last().unwrap();
    let grids = workload
        .query_workload
        .entries()
        .map(|entry| entry.evaluation_grid_ms())
        .collect::<Vec<_>>();
    let panes = pane_candidates(build, &grids);
    assert_eq!(panes.len(), 1, "one explicit pane computation");
    candidates.push(
        window_composition_pass2_candidates(build, &panes)
            .unwrap()
            .remove(0)
            .roots
            .unwrap(),
    );
    let settings = asap_plan_selection::PlanSelectionSettings::default();
    run_explicit_candidates(
        workload,
        roots,
        candidates,
        asap_plan_selection::PlanningModels::builtin()
            .with_plan_selection(&settings)
            .with_deployment_capabilities(deployment),
    )
}

fn run_explicit(workload: &PlanningWorkload) -> Run {
    run_explicit_for(workload, &DeploymentCapabilities::default())
}

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

/// `query` every `interval_ms` over 1M series of `m` sampled every 1.5 s,
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
                    sample_interval: Some(DurationMs(1_500)),
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
/// (2.3625 → 144.1125 cost/s), the panes rebuilt per step (3.6112 →
/// 220.2852) and the exact plan (2.4576 → 149.9136) cost 61 times as much,
/// the memory each step holds while it runs included. The kept panes (B3)
/// are built once per request (one new 1-min pane) but merged at every
/// step, and keep 64 older panes instead of 4 (1 h + 5 min of panes):
/// 436.59 → 6,987.05. B1 keeps 66 instead of 6: 654.54 → 7,202.31. The
/// whole-window KLL is still selected.
#[test]
fn stage3_plan_selection_prices_61_evaluations_per_dashboard_request() {
    let deployment = keeping_query_time_results();
    let none = run_explicit_for(&pattern_b_with(None), &deployment);
    let range = run_explicit_for(&pattern_b_with(Some(LAST_HOUR_BY_MINUTE)), &deployment);
    assert_eq!(LAST_HOUR_BY_MINUTE.evaluations_per_request(), 61);
    for (id, before, after) in [
        ("P1", 2.457_600_019_2, 149.913_601_171_2),
        ("P2", 2.362_500_025_2, 144.112_501_537_2),
        ("P3", 3.611_233_350_8, 220.285_234_398_8),
        ("P3-m1", 654.542_718_733_8, 7_202.305_718_761_8),
        ("P3-m2", 436.587_770_004_24, 6_987.053_970_131_44),
    ] {
        assert!(close(cost(&none, id), before), "{id}: {}", cost(&none, id));
        assert!(close(cost(&range, id), after), "{id}: {}", cost(&range, id));
    }
    // Every query-time computation that is not kept runs once per step.
    for id in ["P1", "P2", "P3"] {
        assert!(close(cost(&range, id), 61.0 * cost(&none, id)), "{id}");
    }
    // B3: 4 → 64 panes kept between requests; B1: 6 → 66 panes kept.
    let keeps = |run: &Run, id: &str, bytes: &str| {
        assert!(
            run.selection.costs[id]
                .per_node
                .values()
                .any(|n| n.detail.contains(&format!("; keeps {bytes} bytes"))),
            "{id} keeps {bytes} bytes"
        )
    };
    keeps(&none, "P3-m2", "3488000000");
    keeps(&range, "P3-m2", "55808000000");
    keeps(&none, "P3-m1", "5232000000");
    keeps(&range, "P3-m1", "57552000000");
    assert_eq!(range.selection.selected, "P2");
}

/// (b) For this explicit fixture, refreshing every minute at 30-second steps
/// supplies panes of
/// `gcd(5 min, 1 min, 30 s) = 30 s`, on the step grid, and Stage 2 keeps
/// them; without the range, the fixture uses 1 min panes. Both configurations
/// must divide the evaluation step; production also offers finer legal widths.
#[test]
fn explicit_pane_candidates_follow_the_evaluation_step_grid() {
    let step_30s = RangeEvaluation {
        step_ms: 30_000,
        span_ms: 3_600_000,
    };
    let range = run_explicit(&exact_panel("max_over_time(m[5m])", 60_000, Some(step_30s)));
    let widths: Vec<u64> = pane_chains(&range).into_iter().map(|(_, w, _)| w).collect();
    assert!(!widths.is_empty(), "panes are offered");
    assert!(widths.iter().all(|&w| w == 30_000), "{widths:?}");
    assert!(range.pruned.is_empty(), "{:?}", range.pruned);

    let none = run_explicit(&exact_panel("max_over_time(m[5m])", 60_000, None));
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
    let range = run_explicit_for(
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
    let none = run_explicit(&pattern_b_with(None));
    assert!(none.selection.pane_retention_ms.is_empty());
    let b1 = run_explicit_for(
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
/// costs (#509 Example 3 and 4: whole-window KLL 2.3625, panes 3.6112, B3
/// 436.59, B1 5.232 GB kept) are as before, and a range of one step per
/// request (`span_ms: 0`) prices exactly as no range.
#[test]
fn no_range_evaluation_leaves_every_price_unchanged() {
    let deployment = keeping_query_time_results();
    let none = run_explicit_for(&pattern_b_with(None), &deployment);
    assert!(close(cost(&none, "P2"), 2.362_500_025_2));
    assert!(close(cost(&none, "P3"), 3.611_233_350_8));
    assert!(close(cost(&none, "P3-m2"), 436.587_770_004_24));
    assert_eq!(
        none.selection.costs["P3-m1"].usage.average_memory_bytes,
        5_232_101_733.337_067
    );
    let one_step = run_explicit_for(
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
/// per minute, rebuilding the window (P1, 20.8281 cost/s) beats the kept
/// panes (P2-m2, 32.2630: 224 MB of 56-byte per-series maxima kept) and the
/// panes maintained at ingestion (P2-m1, 46.2340). As a dashboard over the
/// last hour at 1-minute steps, rebuilding costs 61 times as much
/// (1,270.5151), while the kept panes add only the per-step merges and 1 h
/// of panes (460.0410), and B1 472.2760: the kept panes are selected, and without
/// query-time retention, the ingestion-time panes.
#[test]
fn stage3_plan_selection_prefers_kept_panes_once_the_range_is_declared() {
    let query = "max_over_time(m[5m])";
    let deployment = keeping_query_time_results();
    let none = run_explicit_for(&exact_panel(query, 60_000, None), &deployment);
    let range = run_explicit_for(
        &exact_panel(query, 60_000, Some(LAST_HOUR_BY_MINUTE)),
        &deployment,
    );
    let kept = with_chain(
        &range,
        Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory),
    );
    let ingestion = with_chain(&range, Materialization::MatAtIngestion(Medium::Memory));
    for (run, id, expected) in [
        (&none, "P1", 20.828_117),
        (&none, kept.as_str(), 32.262_967),
        (&none, ingestion.as_str(), 46.234_035),
        (&range, "P1", 1_270.515_127),
        (&range, kept.as_str(), 460.040_967),
        (&range, ingestion.as_str(), 472.276_035),
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

    let without_retention = run_explicit(&exact_panel(query, 60_000, Some(LAST_HOUR_BY_MINUTE)));
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
    let plain = run_explicit_for(
        &exact_panel("max_over_time(m[60m])", 600_000, None),
        &no_raw_data,
    );
    let offset = run_explicit_for(
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
