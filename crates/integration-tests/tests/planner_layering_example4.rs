//! Acceptance tests for #509 "Example 4: Materialization of window summaries
//! in physical planning".
//!
//! Spec: `docs/design_docs/proposals/planner-layering-example4-acceptance.md`
//! (#590). Written by a test designer who does not implement the stages;
//! ported with Stage 2 materialization (#604). Tests that need an
//! unimplemented feature, or that the built-in model contradicts, are
//! `#[ignore]`d, naming it. Window summaries, materialization and retention
//! are read through the adapters in `planner_layering_common`.
//!
//! The workloads are Example 3's, with Pattern A's recurrence and data
//! arrival varied as the doc does.

mod planner_layering_common;

use asap_types::ir::{ASAPOp, Operator};
use std::collections::{BTreeMap, BTreeSet};

use asap_plan_selection::{DeploymentCapabilities, PlanningModels};
use asap_types::ir::physical_export::PhysicalASAPNodeId;
use asap_types::ir::schema::SketchAlgorithm;
use asap_types::workload::{
    DataArrival, DurationMs, PlanningWorkload, Query, Rate, RepeatedDemand, RepetitionInterval,
};
use planner_layering_common::*;

/// Pattern A as given: one ad hoc batch at T over mixed data.
fn once() -> PlanningWorkload {
    pattern_a(PatternARecurrence::OnceAdHoc, DataArrival::Mixed)
}

fn monthly() -> PlanningWorkload {
    pattern_a(PatternARecurrence::MonthlyPredictable, DataArrival::Mixed)
}

fn at_rest() -> PlanningWorkload {
    pattern_a(PatternARecurrence::OnceAdHoc, DataArrival::AtRest)
}

/// The window-summary build of `c` with form `want` (`Some(form)` matches
/// exactly; `None` matches any EH) read by `readers` queries.
fn window_build(
    c: &Logical,
    want: Option<WindowForm>,
    readers_count: usize,
) -> Option<PhysicalASAPNodeId> {
    sketch_builds(&c.dag)
        .into_iter()
        .find_map(|(id, algorithm, _)| {
            let form = window_form(&c.dag, id);
            let matches = match want {
                Some(want) => form == want,
                None => matches!(form, WindowForm::ExponentialHistogram { .. }),
            };
            (algorithm == SketchAlgorithm::Kll
                && matches
                && readers(&c.dag, &c.query_roots, id).len() == readers_count)
                .then_some(id)
        })
}

/// The physical candidates of the logical candidate with a KLL window
/// summary of `form` read by `readers_count` queries, keyed by the
/// materialization of that build. Several logical candidates may match (for
/// example with and without the shared input); the first is used.
fn options_of(
    run: &Run,
    form: Option<WindowForm>,
    readers_count: usize,
) -> BTreeMap<Materialization, (String, PhysicalASAPNodeId)> {
    let logical = run
        .logical
        .iter()
        .find(|c| window_build(c, form, readers_count).is_some())
        .unwrap_or_else(|| {
            let form = form.map_or("EH".to_string(), |f| format!("{f:?}"));
            panic!("no logical candidate with a {form} KLL window summary")
        });
    let build = window_build(logical, form, readers_count).unwrap();
    // Stage 2 keeps node ids from the logical export only where it does not
    // rewrite; find the build again in each physical DAG.
    run.physical_of(logical)
        .map(|p| {
            let id = sketch_builds(&p.dag)
                .into_iter()
                .find(|(id, a, _)| {
                    *a == SketchAlgorithm::Kll
                        && window_form(&p.dag, *id) == window_form(&logical.dag, build)
                })
                .map(|(id, ..)| id)
                .expect("the window summary survives Stage 2");
            (materialization(p, id), (p.id.clone(), id))
        })
        .collect()
}

fn eh_options(run: &Run) -> BTreeMap<Materialization, (String, PhysicalASAPNodeId)> {
    options_of(run, None, 5)
}

fn tumbling() -> Option<WindowForm> {
    Some(WindowForm::Tumbling {
        length_ms: PATTERN_B_INTERVAL_MS,
    })
}

fn cost(run: &Run, id: &str) -> f64 {
    run.cost(id).unwrap_or_else(|| panic!("{id} is not priced"))
}

use Materialization::{IngestionTime, NotMaterialized, QueryTimeKept};

// ── Constraints over every candidate ─────────────────────────────────────

/// Every node upstream of an ingestion-time node also runs at ingestion time, in every workload variant.
#[test]
fn stage2_ingestion_time_upstream_is_ingestion_time() {
    for workload in [once(), monthly(), at_rest(), pattern_b()] {
        assert_ingestion_upstream_is_ingestion(&run_promql(&workload));
    }
}

/// Data at rest has no ingestion, so no candidate runs anything at ingestion time.
#[test]
fn stage2_at_rest_runs_nothing_at_ingestion_time() {
    let run = run_promql(&at_rest());
    for p in &run.physical {
        for n in &p.dag.nodes {
            assert!(!runs_at_ingestion(p, n.id), "{}: {:?}", p.id, n.id);
        }
    }
}

/// A materialized output is kept for as long as its consumers read: it covers every reader's lookback and offset.
#[test]
fn stage2_materialized_output_covers_its_consumers() {
    let offsets: Vec<u64> = PATTERN_A.iter().map(|(_, _, before_t)| *before_t).collect();
    for (workload, offsets) in [
        (once(), offsets.clone()),
        (monthly(), offsets.clone()),
        (at_rest(), offsets),
        (pattern_b(), vec![0]),
    ] {
        let run = run_promql(&workload);
        for p in &run.physical {
            for n in &p.dag.nodes {
                if materialization(p, n.id) == NotMaterialized {
                    continue;
                }
                let need = readers(&p.dag, &p.query_roots, n.id)
                    .into_iter()
                    .map(|q| lookback_ms(&workload, q) + offsets[q])
                    .max()
                    .unwrap_or(0);
                let kept = retention_ms(p, n.id).unwrap_or_else(|| {
                    panic!("{}: {:?} is materialized without a retention", p.id, n.id)
                });
                assert!(kept >= need, "{}: {:?} kept {kept} < {need}", p.id, n.id);
            }
        }
    }
}

// ── Pattern A ────────────────────────────────────────────────────────────

/// The shared EH candidate yields A1 (query time, kept), A2 (ingestion time) and A3 (not materialized).
#[test]
#[ignore = "needs Pass 2 window composition (#580): Exponential Histogram; and A3, not materialized for several consumers (Q44)"]
fn stage2_a_shared_eh_has_three_materialization_options() {
    let found: BTreeSet<_> = eh_options(&run_promql(&once())).into_keys().collect();
    assert_eq!(
        found,
        BTreeSet::from([IngestionTime, QueryTimeKept, NotMaterialized])
    );
}

/// With data at rest, A2 is not generated; A1 and A3 remain.
#[test]
#[ignore = "needs Pass 2 window composition (#580): Exponential Histogram"]
fn stage2_a_at_rest_drops_the_ingestion_time_option() {
    let found: BTreeSet<_> = eh_options(&run_promql(&at_rest())).into_keys().collect();
    assert_eq!(found, BTreeSet::from([QueryTimeKept, NotMaterialized]));
}

/// A materialized shared EH is one build node read by all five queries, charged once.
#[test]
#[ignore = "needs Pass 2 window composition (#580): Exponential Histogram"]
fn stage2_a_materialized_eh_is_built_once_for_all_consumers() {
    let run = run_promql(&once());
    for (m, (id, build)) in eh_options(&run) {
        if m == NotMaterialized {
            continue;
        }
        let p = run.physical(&id);
        let ehs = sketch_builds(&p.dag)
            .into_iter()
            .filter(|(b, ..)| {
                matches!(
                    window_form(&p.dag, *b),
                    WindowForm::ExponentialHistogram { .. }
                )
            })
            .count();
        assert_eq!(ehs, 1, "{id}");
        assert_eq!(readers(&p.dag, &p.query_roots, build).len(), 5, "{id}");
        assert!(
            run.selection.costs[&id].per_node.contains_key(&build),
            "{id}"
        );
    }
}

/// A3 rebuilds the EH for each of the five queries, so it costs more than A1, which builds it once.
#[test]
#[ignore = "needs Pass 2 window composition (#580): Exponential Histogram; and A3 (Q44)"]
fn stage3_a_rebuilding_per_query_costs_more_than_building_once() {
    let run = run_promql(&once());
    let options = eh_options(&run);
    let (a1, a3) = (&options[&QueryTimeKept].0, &options[&NotMaterialized].0);
    assert!(
        cost(&run, a3) > cost(&run, a1),
        "A3 {} vs A1 {}",
        cost(&run, a3),
        cost(&run, a1)
    );
}

/// As given (run once, ad hoc), A1 is the cheapest of the three: A2 maintains years of history for one batch.
#[test]
#[ignore = "needs Pass 2 window composition (#580): Exponential Histogram"]
fn stage3_a_once_adhoc_prefers_the_query_time_eh() {
    let run = run_promql(&once());
    let options = eh_options(&run);
    let a1 = cost(&run, &options[&QueryTimeKept].0);
    for (m, (id, _)) in &options {
        assert!(a1 <= cost(&run, id), "A1 {a1} vs {m:?} {}", cost(&run, id));
    }
}

/// Repeated monthly and predictable, A2's maintenance is shared by many batches, so A2 gains on A1.
#[test]
#[ignore = "needs Pass 2 window composition (#580): Exponential Histogram, maintainable over a monthly window"]
fn stage3_a_monthly_amortizes_ingestion_time_maintenance() {
    let ratio = |workload: PlanningWorkload| {
        let run = run_promql(&workload);
        let options = eh_options(&run);
        cost(&run, &options[&IngestionTime].0) / cost(&run, &options[&QueryTimeKept].0)
    };
    let (once, monthly) = (ratio(once()), ratio(monthly()));
    assert!(monthly < once, "A2/A1: monthly {monthly} vs once {once}");
}

// ── Pattern B ────────────────────────────────────────────────────────────

/// The 1-min tumbling KLL candidate yields B1 (ingestion time), B2 (not materialized) and B3 (query time, kept).
#[test]
fn stage2_b_tumbling_kll_has_three_materialization_options() {
    let found: BTreeSet<_> = options_of(&run_promql(&pattern_b()), tumbling(), 1)
        .into_keys()
        .collect();
    assert_eq!(
        found,
        BTreeSet::from([IngestionTime, QueryTimeKept, NotMaterialized])
    );
}

/// B1 builds the tumbling KLLs at ingestion time and merges and estimates at query time.
#[test]
fn stage2_b_b1_builds_at_ingestion_and_merges_at_query_time() {
    let run = run_promql(&pattern_b());
    let (id, build) = options_of(&run, tumbling(), 1)[&IngestionTime].clone();
    let p = run.physical(&id);
    assert!(runs_at_ingestion(p, build));
    let estimates = estimates_of(&p.dag, build);
    assert!(!estimates.is_empty());
    for (estimate, _) in estimates {
        assert!(
            !runs_at_ingestion(p, estimate),
            "{id}: estimate at ingestion"
        );
        let merge = p.dag.producers(estimate).into_iter().find(|&m| {
            matches!(
                p.dag.payload(m),
                Operator::ASAP(ASAPOp::SummaryMerge { .. })
            )
        });
        let merge = merge.unwrap_or_else(|| panic!("{id}: no merge before the estimate"));
        assert!(!runs_at_ingestion(p, merge), "{id}: merge at ingestion");
    }
}

/// B3 runs nothing at ingestion time; it keeps the tumbling KLLs it builds at query time.
#[test]
fn stage2_b_b3_keeps_query_time_windows() {
    let run = run_promql(&pattern_b());
    let (id, _) = options_of(&run, tumbling(), 1)[&QueryTimeKept].clone();
    let p = run.physical(&id);
    assert!(
        p.dag.nodes.iter().all(|n| !runs_at_ingestion(p, n.id)),
        "{id}"
    );
    let kept: Vec<_> = p.dag.nodes.iter().filter(|n| n.kept).collect();
    assert_eq!(kept.len(), 5, "{id}: the five panes are kept");
    for n in kept {
        assert_eq!(
            window_form(&p.dag, n.id),
            WindowForm::Tumbling {
                length_ms: PATTERN_B_INTERVAL_MS
            }
        );
    }
}

/// The executor cannot keep query-time state (Q56), so Stage 3 rejects B3
/// on its capabilities, with the reason.
#[test]
fn stage3_b_executor_rejects_b3() {
    let run = run_promql(&pattern_b());
    let (id, _) = options_of(&run, tumbling(), 1)[&QueryTimeKept].clone();
    let reason = run.invalid()[id.as_str()];
    assert!(
        reason.contains("cannot keep query-time state across evaluations"),
        "{id}: {reason}"
    );
}

// Which of B1 and B2 is cheaper depends on the workload and the deployment,
// not on the materialization alone (Q49). B1 keeps one KLL per series per
// pane for as long as the window; B2 keeps nothing but reads the window's raw
// samples at every evaluation, and pays to keep them when the deployment does
// not. So B1 wins when its panes are smaller than the raw samples they cover
// and those samples would otherwise be kept for the plan.

/// Pattern B with `series` series sampled every `interval_ms`, asking for the
/// p99 over the last `window_min` minutes every `every_min` minutes.
fn pattern_b_variant(
    series: u64,
    interval_ms: u64,
    window_min: u64,
    every_min: u64,
) -> PlanningWorkload {
    let mut workload = pattern_b();
    let data = workload.data_workload.as_mut().expect("data workload");
    data.input_cardinality = declared(series);
    data.data_ingestion_interval = declared(DurationMs(interval_ms));
    data.ingestion_rate = declared(Rate(series as f64 * 1000.0 / interval_ms as f64));
    let entry = &mut workload
        .query_workload
        .repeating_queries
        .as_mut()
        .expect("repeating")[0];
    entry.query = Query(format!(
        "quantile_over_time(0.99, latency_ms[{window_min}m])"
    ));
    entry.demand =
        RepeatedDemand::FixedInterval(RepetitionInterval((every_min * MINUTE_MS) as u32));
    entry.time_selection.lookback = Some(DurationMs(window_min * MINUTE_MS));
    workload
}

/// Plans `workload` on the executor with raw data kept or not by the deployment.
fn run_with_raw_data(workload: &PlanningWorkload, raw_data_retained: bool) -> Run {
    let capabilities = DeploymentCapabilities {
        raw_data_retained,
        ..asap_executor::capabilities()
    };
    let models = PlanningModels::builtin().with_capabilities(&capabilities);
    run_stages_with(workload, lower_promql(workload), models)
}

/// The costs of B1 and B2 for the tumbling KLL of `every_min` minutes.
fn b1_b2(run: &Run, every_min: u64) -> (f64, f64) {
    let form = Some(WindowForm::Tumbling {
        length_ms: every_min * MINUTE_MS,
    });
    let options = options_of(run, form, 1);
    (
        cost(run, &options[&IngestionTime].0),
        cost(run, &options[&NotMaterialized].0),
    )
}

/// At Pattern B's 1M series, rebuilding the five panes at query time breaks
/// the 200 ms latency bound, so B2 is invalid and B1 is priced.
#[test]
fn stage3_b_rebuilding_a_million_series_breaks_the_latency_bound() {
    let run = run_promql(&pattern_b());
    let options = options_of(&run, tumbling(), 1);
    let b2 = &options[&NotMaterialized].0;
    let reason = run.invalid()[b2.as_str()];
    assert!(reason.contains("latency bound"), "{b2}: {reason}");
    assert!(run.cost(&options[&IngestionTime].0).is_some());
}

/// When the deployment keeps raw data, B2 pays only to read it, while B1
/// keeps its panes: B2 is cheaper whenever it meets the latency bound.
#[test]
fn stage3_b_kept_raw_data_makes_rebuilding_cheaper() {
    let run = run_with_raw_data(&pattern_b_variant(10_000, 15_000, 5, 1), true);
    let (b1, b2) = b1_b2(&run, 1);
    assert!(b2 < b1, "B2 {b2} vs B1 {b1}");
}

/// Sampled every 15 s, a 1-min pane covers 4 samples per series, fewer bytes
/// than its KLL: B2 is cheaper even when it pays to keep the raw data.
#[test]
fn stage3_b_panes_larger_than_their_raw_data_lose() {
    let run = run_with_raw_data(&pattern_b_variant(10_000, 15_000, 5, 1), false);
    let (b1, b2) = b1_b2(&run, 1);
    assert!(b2 < b1, "B2 {b2} vs B1 {b1}");
}

/// Sampled every second, a 10-min pane covers 600 samples per series, far
/// more bytes than its KLL. When the deployment does not keep raw data, B2
/// must keep the hour's samples, so B1 is cheaper, and the planner selects it.
#[test]
fn stage3_b_panes_smaller_than_their_raw_data_win() {
    let run = run_with_raw_data(&pattern_b_variant(1_000, 1_000, 60, 10), false);
    let (b1, b2) = b1_b2(&run, 10);
    assert!(b1 < b2, "B1 {b1} vs B2 {b2}");
    let options = options_of(
        &run,
        Some(WindowForm::Tumbling {
            length_ms: 10 * MINUTE_MS,
        }),
        1,
    );
    assert_eq!(run.selection.selected, options[&IngestionTime].0);
}

// B3 (query time, kept) builds only the newest pane at each evaluation, from
// one pane width of raw data, and keeps the N − 1 older panes. Per
// evaluation it does what B1 does at ingestion time, so the choice is about
// retention (Q59): B3 keeps N − 1 panes and one pane of raw data, B1 keeps
// N + 1 panes, B2 keeps all N panes' raw data. Sampled every 7.5 s, a 10-min
// pane covers 80 samples per series (1280 bytes), a little more than its KLL
// (1024 bytes): B3 keeps less than either.

/// Plans `workload` on the executor without raw data retention, but able to
/// keep query-time state across evaluations.
fn run_keeping_query_time_state(workload: &PlanningWorkload) -> Run {
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        query_time_retention: true,
        ..asap_executor::capabilities()
    };
    let models = PlanningModels::builtin().with_capabilities(&capabilities);
    run_stages_with(workload, lower_promql(workload), models)
}

/// On a deployment that can keep query-time state, B3 is priced, and where
/// a pane's raw data is a little larger than its KLL it keeps the least, so
/// the planner selects it.
#[test]
fn stage3_b_kept_panes_win_when_the_deployment_can_keep_them() {
    let run = run_keeping_query_time_state(&pattern_b_variant(1_000, 7_500, 60, 10));
    let options = options_of(
        &run,
        Some(WindowForm::Tumbling {
            length_ms: 10 * MINUTE_MS,
        }),
        1,
    );
    let b3 = cost(&run, &options[&QueryTimeKept].0);
    let (b1, b2) = b1_b2(&run, 10);
    assert!(b3 < b1 && b3 < b2, "B3 {b3} vs B1 {b1}, B2 {b2}");
    assert_eq!(run.selection.selected, options[&QueryTimeKept].0);
}
