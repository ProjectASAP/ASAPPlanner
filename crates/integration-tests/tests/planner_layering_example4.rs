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

use std::collections::{BTreeMap, BTreeSet};

use asap_types::ir::export::{LogicalASAPNodeId, LogicalASAPOperatorPayload};
use asap_types::ir::schema::SketchAlgorithm;
use asap_types::workload::{DataArrival, PlanningWorkload};
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
) -> Option<LogicalASAPNodeId> {
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
) -> BTreeMap<Materialization, (String, LogicalASAPNodeId)> {
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

fn eh_options(run: &Run) -> BTreeMap<Materialization, (String, LogicalASAPNodeId)> {
    options_of(run, None, 5)
}

fn tumbling() -> Option<WindowForm> {
    Some(WindowForm::Tumbling {
        length_ms: PATTERN_B_INTERVAL_MS,
    })
}

fn sliding() -> Option<WindowForm> {
    Some(WindowForm::Sliding {
        length_ms: PATTERN_B_WINDOW_MS,
        slide_ms: PATTERN_B_INTERVAL_MS,
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
#[ignore = "needs Stage 2 query time, kept (B3); B1 and B2 alone pass in stage2_b_tumbling_kll_has_b1_and_b2"]
fn stage2_b_tumbling_kll_has_three_materialization_options() {
    let found: BTreeSet<_> = options_of(&run_promql(&pattern_b()), tumbling(), 1)
        .into_keys()
        .collect();
    assert_eq!(
        found,
        BTreeSet::from([IngestionTime, QueryTimeKept, NotMaterialized])
    );
}

/// B1 (ingestion time) and B2 (not materialized) are generated for the
/// tumbling KLL; B3 is not yet (added by the implementer, not part of the
/// spec).
#[test]
fn stage2_b_tumbling_kll_has_b1_and_b2() {
    let found: BTreeSet<_> = options_of(&run_promql(&pattern_b()), tumbling(), 1)
        .into_keys()
        .collect();
    assert_eq!(found, BTreeSet::from([IngestionTime, NotMaterialized]));
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
        let merge = p
            .dag
            .producers(estimate)
            .into_iter()
            .find(|&m| matches!(p.dag.payload(m), LogicalASAPOperatorPayload::SummaryMerge));
        let merge = merge.unwrap_or_else(|| panic!("{id}: no merge before the estimate"));
        assert!(!runs_at_ingestion(p, merge), "{id}: merge at ingestion");
    }
}

/// B3 runs nothing at ingestion time; it keeps the tumbling KLLs it builds at query time.
#[test]
#[ignore = "needs Stage 2 query time, kept (B3)"]
fn stage2_b_b3_keeps_query_time_windows() {
    let run = run_promql(&pattern_b());
    let (id, _) = options_of(&run, tumbling(), 1)[&QueryTimeKept].clone();
    let p = run.physical(&id);
    assert!(
        p.dag.nodes.iter().all(|n| !runs_at_ingestion(p, n.id)),
        "{id}"
    );
}

/// The sliding-window KLL is kept from ingestion time or from query time; not materializing it is the no-window plan.
#[test]
#[ignore = "needs Pass 2 window composition (#580): sliding windows"]
fn stage2_b_sliding_kll_has_two_materialization_options() {
    let found: BTreeSet<_> = options_of(&run_promql(&pattern_b()), sliding(), 1)
        .into_keys()
        .collect();
    assert_eq!(found, BTreeSet::from([IngestionTime, QueryTimeKept]));
}

/// B2 rebuilds all five tumbling KLLs at every evaluation, so it costs at least B1 and B3.
#[test]
#[ignore = "built-in model (#604): B2 rebuilds the panes in 310 ms, over the 200 ms latency bound, so it is not priced; and B1 retains 6 panes of 1M per-series KLLs (6.1 GB, 768 cost/s) against B2's 5.17 cost/s of rebuilds"]
fn stage3_b_rebuilding_every_window_costs_most() {
    let run = run_promql(&pattern_b());
    let options = options_of(&run, tumbling(), 1);
    let b2 = cost(&run, &options[&NotMaterialized].0);
    for m in [IngestionTime, QueryTimeKept] {
        assert!(cost(&run, &options[&m].0) <= b2, "{m:?} vs B2 {b2}");
    }
}

/// Repeating over arriving data, the built-in models pick B1 among the tumbling options.
#[test]
#[ignore = "built-in model (#604): B2 is over the 200 ms latency bound, so it is not priced; and B1 retains 6 panes of 1M per-series KLLs (6.1 GB, 768 cost/s) against B2's 5.17 cost/s"]
fn stage3_b_prefers_ingestion_time_tumbling_windows() {
    let run = run_promql(&pattern_b());
    let options = options_of(&run, tumbling(), 1);
    let b1 = cost(&run, &options[&IngestionTime].0);
    for (m, (id, _)) in &options {
        assert!(b1 <= cost(&run, id), "B1 {b1} vs {m:?} {}", cost(&run, id));
    }
}
