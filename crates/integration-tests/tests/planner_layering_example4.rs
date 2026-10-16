//! Acceptance tests for #509 "Example 4: Materialization of window summaries
//! in physical planning".
//!
//! Spec: `docs/design_docs/proposals/planner-layering-example4-acceptance.md`.
//! Written by a test designer who does not implement the stages. Tests that
//! need an unimplemented feature are `#[ignore]`d, naming it and the PR that
//! delivers it. Materialization is read from the physical DAG (#667); window
//! summaries and retention are read through the pending adapters in
//! `planner_layering_common`, which the implementers fill in.
//!
//! The workloads are Example 3's, with Pattern A's recurrence and data
//! arrival varied as the doc does.

mod planner_layering_common;

use std::collections::{BTreeMap, BTreeSet};

use asap_aware_mapping::stage3_plan_selection::{
    ClusterLimits, ClusterObjective, CostModel, PlanSelectionSettings, ResourceUnitCost,
};
use asap_types::ir::flat::NodeId;
use asap_types::post_asap::SketchAlgorithm;
use asap_types::workload::{DataArrival, PlanningWorkload};
use planner_layering_common::*;

/// Pattern A as given: one ad hoc batch at T over mixed data.
fn once() -> PlanningWorkload {
    pattern_a(
        PatternARecurrence::OnceAdHoc,
        DataArrival::Mixed {
            rows_per_second: SHARED_ROWS_PER_SECOND,
        },
    )
}

fn monthly() -> PlanningWorkload {
    pattern_a(
        PatternARecurrence::MonthlyPredictable,
        DataArrival::Mixed {
            rows_per_second: SHARED_ROWS_PER_SECOND,
        },
    )
}

fn at_rest() -> PlanningWorkload {
    pattern_a(PatternARecurrence::OnceAdHoc, DataArrival::AtRest)
}

/// The window-summary build of `c` with form `want` (`Some(form)` matches
/// exactly; `None` matches any EH) read by `readers` queries.
fn window_build(c: &Logical, want: Option<WindowForm>, readers_count: usize) -> Option<NodeId> {
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
) -> BTreeMap<MaterializationOption, (String, NodeId)> {
    let logical = run
        .logical
        .iter()
        .find(|c| window_build(c, form, readers_count).is_some())
        .unwrap_or_else(|| {
            let form = form.map_or("EH".to_string(), |f| format!("{f:?}"));
            panic!("no logical candidate with a {form} KLL window summary")
        });
    let build = window_build(logical, form, readers_count).unwrap();
    // Physical ASAP-aware optimization renumbers nodes; find the build again
    // in each physical DAG.
    run.physical_of(logical)
        .map(|p| {
            let id = sketch_builds(p.dag())
                .into_iter()
                .find(|(id, a, _)| {
                    *a == SketchAlgorithm::Kll
                        && window_form(p.dag(), *id) == window_form(&logical.dag, build)
                })
                .map(|(id, ..)| id)
                .expect("the window summary survives physical ASAP-aware optimization");
            (materialization(p, id), (p.id.clone(), id))
        })
        .collect()
}

fn eh_options(run: &Run) -> BTreeMap<MaterializationOption, (String, NodeId)> {
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

use MaterializationOption::{MatAtIngestion, MatAtQuery, NotMaterialized};

// ── Constraints over every candidate ─────────────────────────────────────

/// Every node upstream of an ingestion-time node also runs at ingestion time, in every workload variant.
#[test]
fn stage2_physical_asap_optimization_ingestion_time_upstream_is_ingestion_time() {
    for workload in [once(), monthly(), at_rest(), pattern_b()] {
        assert_ingestion_upstream_is_ingestion(&run_promql(&workload));
    }
}

/// Data at rest has no ingestion, so no candidate runs anything at ingestion time.
#[test]
fn stage2_physical_asap_optimization_at_rest_runs_nothing_at_ingestion_time() {
    let run = run_promql(&at_rest());
    for p in &run.physical {
        for n in &p.dag().nodes {
            assert!(!runs_at_ingestion(p, n.id), "{}: {:?}", p.id, n.id);
        }
    }
}

/// A materialized output is kept for as long as its consumers read: it covers every reader's lookback and offset.
#[test]
fn stage2_physical_asap_optimization_materialized_output_covers_its_consumers() {
    // Each query's range window and offset, as its PromQL text states them.
    let windows: Vec<u64> = PATTERN_A.iter().map(|(_, window, _)| *window).collect();
    let offsets: Vec<u64> = PATTERN_A.iter().map(|(_, _, before_t)| *before_t).collect();
    for (workload, windows, offsets) in [
        (once(), windows.clone(), offsets.clone()),
        (monthly(), windows.clone(), offsets.clone()),
        (at_rest(), windows, offsets),
        (pattern_b(), vec![PATTERN_B_WINDOW_MS], vec![0]),
    ] {
        let run = run_promql(&workload);
        for p in &run.physical {
            for n in &p.dag().nodes {
                if materialization(p, n.id) == NotMaterialized {
                    continue;
                }
                let need = readers(p.dag(), &p.query_roots, n.id)
                    .into_iter()
                    .map(|q| windows[q] + offsets[q])
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
#[ignore = "needs Pass 2 window segments shared across queries (#628, #634); Stage 2 materialization (MatAtQuery within one batch) is in place"]
fn stage2_physical_asap_optimization_a_shared_eh_has_three_materialization_options() {
    let found: BTreeSet<_> = eh_options(&run_promql(&once())).into_keys().collect();
    assert_eq!(
        found,
        BTreeSet::from([MatAtIngestion, MatAtQuery, NotMaterialized])
    );
}

/// With data at rest, A2 is not generated; A1 and A3 remain.
#[test]
#[ignore = "needs Pass 2 window segments shared across queries (#628, #634); Stage 2 materialization (MatAtQuery within one batch) is in place"]
fn stage2_physical_asap_optimization_a_at_rest_drops_the_ingestion_time_option() {
    let found: BTreeSet<_> = eh_options(&run_promql(&at_rest())).into_keys().collect();
    assert_eq!(found, BTreeSet::from([MatAtQuery, NotMaterialized]));
}

/// A materialized shared EH is one build node read by all five queries, charged once.
#[test]
#[ignore = "needs Pass 2 window segments shared across queries (#628, #634); Stage 2 materialization (MatAtQuery within one batch) is in place"]
fn stage2_physical_asap_optimization_a_materialized_eh_is_built_once_for_all_consumers() {
    let run = run_promql(&once());
    for (m, (id, build)) in eh_options(&run) {
        if m == NotMaterialized {
            continue;
        }
        let p = run.physical(&id);
        let ehs = sketch_builds(p.dag())
            .into_iter()
            .filter(|(b, ..)| {
                matches!(
                    window_form(p.dag(), *b),
                    WindowForm::ExponentialHistogram { .. }
                )
            })
            .count();
        assert_eq!(ehs, 1, "{id}");
        assert_eq!(readers(p.dag(), &p.query_roots, build).len(), 5, "{id}");
        assert!(
            run.selection.costs[&id].per_node.contains_key(&build),
            "{id}"
        );
    }
}

/// A3 rebuilds the EH for each of the five queries (plan selection charges a
/// `NotMaterialized` node once per consuming query), so it costs more than A1,
/// which builds it once.
#[test]
#[ignore = "needs Pass 2 window segments shared across queries (#628, #634); Stage 2 materialization (MatAtQuery within one batch) is in place"]
fn stage3_plan_selection_a_rebuilding_per_query_costs_more_than_building_once() {
    let run = run_promql(&once());
    let options = eh_options(&run);
    let (a1, a3) = (&options[&MatAtQuery].0, &options[&NotMaterialized].0);
    assert!(
        cost(&run, a3) > cost(&run, a1),
        "A3 {} vs A1 {}",
        cost(&run, a3),
        cost(&run, a1)
    );
}

/// As given (run once, ad hoc), A1 is the cheapest of the three: A2 maintains years of history for one batch.
#[test]
#[ignore = "needs Pass 2 window segments shared across queries (#628, #634); Stage 2 materialization (MatAtQuery within one batch) is in place"]
fn stage3_plan_selection_a_once_adhoc_prefers_the_query_time_eh() {
    let run = run_promql(&once());
    let options = eh_options(&run);
    let a1 = cost(&run, &options[&MatAtQuery].0);
    for (m, (id, _)) in &options {
        assert!(a1 <= cost(&run, id), "A1 {a1} vs {m:?} {}", cost(&run, id));
    }
}

/// Repeated monthly and predictable, A2's maintenance is shared by many batches, so A2 gains on A1.
#[test]
#[ignore = "needs Pass 2 window segments shared across queries (#628, #634); Stage 2 materialization (MatAtQuery within one batch) is in place"]
fn stage3_plan_selection_a_monthly_amortizes_ingestion_time_maintenance() {
    let ratio = |workload: PlanningWorkload| {
        let run = run_promql(&workload);
        let options = eh_options(&run);
        cost(&run, &options[&MatAtIngestion].0) / cost(&run, &options[&MatAtQuery].0)
    };
    let (once, monthly) = (ratio(once()), ratio(monthly()));
    assert!(monthly < once, "A2/A1: monthly {monthly} vs once {once}");
}

// ── Pattern B ────────────────────────────────────────────────────────────

/// The 1-min tumbling KLL candidate yields B1 (ingestion time), B2 (not materialized) and B3 (query time, kept).
#[test]
#[ignore = "needs B3: panes built at query time and kept across evaluations (G9, #625)"]
fn stage2_physical_asap_optimization_b_tumbling_kll_has_three_materialization_options() {
    let found: BTreeSet<_> = options_of(&run_promql(&pattern_b()), tumbling(), 1)
        .into_keys()
        .collect();
    assert_eq!(
        found,
        BTreeSet::from([MatAtIngestion, MatAtQuery, NotMaterialized])
    );
}

/// B1 builds the tumbling KLLs at ingestion time and merges and estimates at query time.
#[test]
fn stage2_physical_asap_optimization_b_b1_builds_at_ingestion_and_merges_at_query_time() {
    let run = run_promql(&pattern_b());
    let (id, build) = options_of(&run, tumbling(), 1)[&MatAtIngestion].clone();
    let p = run.physical(&id);
    assert!(runs_at_ingestion(p, build));
    let estimates = estimates_of(p.dag(), build);
    assert!(!estimates.is_empty());
    for (estimate, _) in estimates {
        assert!(
            !runs_at_ingestion(p, estimate),
            "{id}: estimate at ingestion"
        );
        let merge = p
            .dag()
            .producers(estimate)
            .into_iter()
            .find(|&m| is_merge(p.dag().payload(m)));
        let merge = merge.unwrap_or_else(|| panic!("{id}: no merge before the estimate"));
        assert!(!runs_at_ingestion(p, merge), "{id}: merge at ingestion");
    }
}

/// Implementer test (until B3, G9): the tumbling KLL candidate yields B1
/// (maintained at ingestion time) and B2 (rebuilt at every evaluation);
/// both are priced, and B2's evaluation stays within the 200 ms bound.
#[test]
fn stage2_physical_asap_optimization_b_tumbling_kll_has_b1_and_b2() {
    let run = run_promql(&pattern_b());
    let options = options_of(&run, tumbling(), 1);
    assert_eq!(
        options.keys().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([MatAtIngestion, NotMaterialized])
    );
    for (id, _) in options.values() {
        assert!(run.cost(id).is_some(), "{id} is priced");
        let latency = run.selection.costs[id].usage.latency_per_query[0].total_ms();
        assert!(latency <= 200.0, "{id}: {latency} ms");
    }
}

/// B3 runs nothing at ingestion time; it keeps the tumbling KLLs it builds at query time.
#[test]
#[ignore = "needs B3: panes built at query time and kept across evaluations (G9, #625)"]
fn stage2_physical_asap_optimization_b_b3_keeps_query_time_windows() {
    let run = run_promql(&pattern_b());
    let (id, _) = options_of(&run, tumbling(), 1)[&MatAtQuery].clone();
    let p = run.physical(&id);
    assert!(
        p.dag().nodes.iter().all(|n| !runs_at_ingestion(p, n.id)),
        "{id}"
    );
}

/// The sliding-window KLL is kept from ingestion time or from query time; not materializing it is the no-window plan.
#[test]
#[ignore = "needs sliding windows (no PR yet)"]
fn stage2_physical_asap_optimization_b_sliding_kll_has_two_materialization_options() {
    let found: BTreeSet<_> = options_of(&run_promql(&pattern_b()), sliding(), 1)
        .into_keys()
        .collect();
    assert_eq!(found, BTreeSet::from([MatAtIngestion, MatAtQuery]));
}

/// B2 rebuilds all five tumbling KLLs at every evaluation, so it costs at least B1 and B3.
#[test]
#[ignore = "needs B3: panes built at query time and kept across evaluations (G9, #625)"]
fn stage3_plan_selection_b_rebuilding_every_window_costs_most() {
    let run = run_promql(&pattern_b());
    let options = options_of(&run, tumbling(), 1);
    let b2 = cost(&run, &options[&NotMaterialized].0);
    for m in [MatAtIngestion, MatAtQuery] {
        assert!(cost(&run, &options[&m].0) <= b2, "{m:?} vs B2 {b2}");
    }
}

/// Repeating over arriving data, the built-in models pick B1 among the tumbling options.
#[test]
#[ignore = "the built-in model contradicts it: B1 keeps 6 panes of 1M per-series KLLs (107 GB) for 13,326.5 cost/s, B2 rebuilds them for 2.183; needs B3 (G9) and the model-relative rewrite (Q49, G9)"]
fn stage3_plan_selection_b_prefers_ingestion_time_tumbling_windows() {
    let run = run_promql(&pattern_b());
    let options = options_of(&run, tumbling(), 1);
    let b1 = cost(&run, &options[&MatAtIngestion].0);
    for (m, (id, _)) in &options {
        assert!(b1 <= cost(&run, id), "B1 {b1} vs {m:?} {}", cost(&run, id));
    }
}

// ── Cost models (#683, #660) ─────────────────────────────────────────────

/// Pattern B under `cost_model`, with the default resource speed and horizon.
fn pattern_b_under(cost_model: CostModel) -> Run {
    run_promql_under(
        &pattern_b(),
        &PlanSelectionSettings {
            cost_model,
            ..PlanSelectionSettings::default()
        },
    )
}

/// The candidates the cost models choose between: the exact plan, the KLL
/// rebuilt over the whole window at query time, B2 (five 1-min panes rebuilt
/// at query time) and B1 (the panes maintained at ingestion time).
struct PatternBCandidates {
    exact: String,
    whole_window_kll: String,
    b2: String,
    b1: String,
}

fn pattern_b_candidates(run: &Run) -> PatternBCandidates {
    let panes = options_of(run, tumbling(), 1);
    PatternBCandidates {
        exact: run
            .physical
            .iter()
            .find(|p| sketch_builds(p.dag()).is_empty())
            .expect("an exact candidate")
            .id
            .clone(),
        whole_window_kll: options_of(run, Some(WindowForm::None), 1)[&NotMaterialized]
            .0
            .clone(),
        b2: panes[&NotMaterialized].0.clone(),
        b1: panes[&MatAtIngestion].0.clone(),
    }
}

/// A cluster with `memory_gb` GB of memory and 1 TB of disk.
fn cluster(memory_gb: u64, objective: ClusterObjective) -> CostModel {
    CostModel::ClusterCapacity(ClusterLimits {
        memory_bytes: memory_gb * 1_000_000_000,
        disk_bytes: 1_000_000_000_000,
        cpu_cores: None,
        objective,
    })
}

/// Each cost model chooses what its deployment pays for. Cost model 1
/// (usage over time) picks the KLL rebuilt every minute: it uses memory only
/// while it runs, and B1 pays for 107 GB of panes every second. Cost model 2
/// (provisioned capacity) picks the exact plan: every candidate needs one
/// core for its evaluation, and the exact plan's peak memory (192 MB of
/// per-series values) is the smallest, while the KLL's 1M states take
/// 17.8 GB at once. Cost model 3 (own cluster, lowest latency) picks B1 when
/// its 124 GB fit, since merging maintained panes takes 7 ms against 126 ms
/// for rebuilding; in 64 GB, B1 is invalid and the KLL is fastest.
#[test]
fn stage3_plan_selection_b_cost_models_choose_differently() {
    let usage_over_time =
        pattern_b_under(CostModel::UsageOverTime(ResourceUnitCost::illustrative()));
    let ids = pattern_b_candidates(&usage_over_time);
    assert_eq!(usage_over_time.selection.selected, ids.whole_window_kll);

    let provisioned = pattern_b_under(CostModel::ProvisionedCapacity(
        ResourceUnitCost::illustrative(),
    ));
    assert_eq!(provisioned.selection.selected, ids.exact);
    let peak = |run: &Run, id: &str| run.selection.costs[id].usage.peak_memory_bytes;
    assert!(peak(&provisioned, &ids.exact) < peak(&provisioned, &ids.whole_window_kll));

    let large = pattern_b_under(cluster(256, ClusterObjective::LowestLatency));
    assert_eq!(large.selection.selected, ids.b1);
    assert!(large
        .selection
        .costs
        .values()
        .all(|c| c.cost_per_second.is_none()));

    let small = pattern_b_under(cluster(64, ClusterObjective::LowestLatency));
    assert_eq!(small.selection.selected, ids.whole_window_kll);
    let invalid = small.invalid();
    let b1 = invalid[ids.b1.as_str()];
    assert!(b1.starts_with("peak memory"), "{b1}");
}

/// The Pareto front over peak memory, disk and latency keeps the exact
/// plan (least memory), the whole-window KLL and B1 (lowest latency); B2 is
/// dominated by the whole-window KLL, which needs less memory and is faster.
/// Least memory picks the exact plan.
#[test]
fn stage3_plan_selection_b_pareto_front_over_memory_disk_and_latency() {
    let run = pattern_b_under(cluster(256, ClusterObjective::ParetoFront));
    let ids = pattern_b_candidates(&run);
    let mut front = run.selection.front.clone();
    front.sort();
    let mut expected = vec![
        ids.exact.clone(),
        ids.whole_window_kll.clone(),
        ids.b1.clone(),
    ];
    expected.sort();
    assert_eq!(front, expected);
    assert_eq!(run.selection.selected, ids.b1);
    let b2 = run
        .selection
        .rejected
        .iter()
        .find(|r| r.id == ids.b2)
        .unwrap();
    assert!(
        b2.valid
            && b2
                .reason
                .starts_with(&format!("dominated by {}", ids.whole_window_kll)),
        "{}",
        b2.reason
    );

    let least_memory = pattern_b_under(cluster(256, ClusterObjective::LeastMemory));
    assert_eq!(least_memory.selection.selected, ids.exact);
}
