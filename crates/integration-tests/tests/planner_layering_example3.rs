//! Rule-fixture and production acceptance for Example 3: window implementations.
//!
//! Numerical, accuracy, and operator assertions use explicitly supplied
//! configurations of the existing rules and physical evaluator. They do not
//! assume a bounded MILP run archives every supported workload plan. Separate
//! production checks exercise memo search and validated output selection.
//! Unsupported algorithms retain their existing ignored tests.

use crate::planner_layering_common;

use std::collections::BTreeSet;

use asap_types::ir::flat::NodeId;
use asap_types::ir::schema::{SketchAlgorithm, SketchParams, SketchStatistic};
use asap_types::workload::DataArrival;

use planner_layering_common::*;

// Fixed algorithm/window fixtures keep numerical evaluator checks independent
// of a time-bounded production search's archived incumbents.
fn run_a() -> Run {
    use asap_logical_asap_optimization::test_support::{
        compose_logical_candidate, generate_per_computation_candidates,
    };
    use asap_types::stages::HowToComputeAggregateMeasure;
    let workload = pattern_a(
        PatternARecurrence::OnceAdHoc,
        DataArrival::Mixed {
            rows_per_second: SHARED_ROWS_PER_SECOND,
        },
    );
    let roots = lower_promql(&workload);
    let local = generate_per_computation_candidates(
        &asap_logical_asap_optimization::generate_dag_of_logical_memo_groups(&roots),
    )
    .unwrap();
    let mut computations = Vec::new();
    for kll in [false, true] {
        let choices: Vec<_> = local
            .targets
            .iter()
            .map(|target| {
                if kll {
                    (0..target.choice_count())
                        .find(|&choice| {
                            matches!(
                                target.computation_for(choice),
                                Some((
                                    HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll),
                                    _
                                ))
                            )
                        })
                        .unwrap()
                } else {
                    0
                }
            })
            .collect();
        let whole = unshare(&compose_logical_candidate(&local, &choices).unwrap());
        computations.push(whole.clone());
        computations.push(share_identical(&whole));
    }
    run_explicit_candidates(
        &workload,
        roots,
        computations,
        asap_planner::PlanningModels::builtin(),
    )
}

fn run_b() -> Run {
    use asap_logical_asap_optimization::test_support::{
        compose_logical_candidate, generate_per_computation_candidates,
    };
    use asap_logical_asap_optimization::test_support::{
        pane_candidates, window_composition_pass2_candidates,
    };
    use asap_types::stages::HowToComputeAggregateMeasure;
    let workload = pattern_b();
    let roots = lower_promql(&workload);
    let local = generate_per_computation_candidates(
        &asap_logical_asap_optimization::generate_dag_of_logical_memo_groups(&roots),
    )
    .unwrap();
    let mut computations = vec![roots.clone()];
    for option in 0..local.targets[0].choice_count() {
        if !matches!(
            local.targets[0].computation_for(option),
            Some((HowToComputeAggregateMeasure::Summary(_), _))
        ) {
            continue;
        }
        let whole: Vec<_> = compose_logical_candidate(&local, &[option]).unwrap();
        let panes = pane_candidates(&whole, &[Some(PATTERN_B_INTERVAL_MS)]);
        let chain = window_composition_pass2_candidates(&whole, &panes)
            .unwrap()
            .remove(0)
            .roots
            .unwrap();
        computations.extend([whole, chain]);
    }
    run_explicit_candidates(
        &workload,
        roots,
        computations,
        asap_planner::PlanningModels::builtin(),
    )
}

#[test]
fn production_memo_covers_pattern_b_with_validated_accuracy() {
    let workload = pattern_b();
    let run = run_stages_under(&workload, lower_promql(&workload), &Default::default());
    assert_selects_cheapest_valid(&run);
    assert_eq!(run.selection.accepted[&run.selection.selected].len(), 1);
}

/// The per-computation options of `query` across all candidates.
fn options(run: &Run, query: usize) -> BTreeSet<String> {
    run.logical
        .iter()
        .map(|c| query_option(&c.dag, &c.query_roots, query))
        .collect()
}

fn op(name: &str) -> String {
    name.to_string()
}

/// The summary builds of `c`: (build, algorithm, window form, readers).
fn windowed_builds(c: &Logical) -> Vec<(NodeId, SketchAlgorithm, WindowForm, BTreeSet<usize>)> {
    sketch_builds(&c.dag)
        .into_iter()
        .map(|(id, algorithm, _)| {
            let form = window_form(&c.dag, id);
            (id, algorithm, form, readers(&c.dag, &c.query_roots, id))
        })
        .collect()
}

// ── Workloads ────────────────────────────────────────────────────────────

/// Pattern A is one ad hoc batch of five queries run once at T; Pattern B one repeating query.
#[test]
fn workload_encodes_example3() {
    let a = pattern_a(
        PatternARecurrence::OnceAdHoc,
        DataArrival::Mixed {
            rows_per_second: SHARED_ROWS_PER_SECOND,
        },
    );
    a.validate().expect("valid Pattern A");
    let queries: Vec<_> = a.query_workload.entries().map(|e| e.query.0).collect();
    assert_eq!(queries, PATTERN_A.map(|pattern| pattern.promql));
    let b = pattern_b();
    b.validate().expect("valid Pattern B");
    assert_eq!(evaluation_interval_ms(&b, 0), Some(PATTERN_B_INTERVAL_MS));
}

// ── Pattern A: query-language frontends ───────────────────────────────

/// Each Pattern A query lowers to scan → (time shift) → range → quantile.
#[test]
fn stage0_query_language_frontends_a_lowers_each_query_to_its_interval() {
    let roots = lower_promql(&pattern_a(
        PatternARecurrence::OnceAdHoc,
        DataArrival::Mixed {
            rows_per_second: SHARED_ROWS_PER_SECOND,
        },
    ));
    let ops: Vec<_> = roots.iter().map(frontend_operations).collect();
    let plain = vec![op("aggregate:quantile"), op("scan"), op("time_range")];
    let shifted = vec![
        op("aggregate:quantile"),
        op("scan"),
        op("time_range"),
        op("time_shift"),
    ];
    assert_eq!(
        ops,
        [
            plain.clone(),
            plain,
            shifted.clone(),
            shifted.clone(),
            shifted
        ]
    );
}

/// The frontends give one summary-free DAG whose five queries share no node.
#[test]
fn stage0_query_language_frontends_a_one_summary_free_dag_without_sharing() {
    let run = run_a();
    let stage0_query_language_frontends = &run.stage0_query_language_frontends;
    assert_eq!(stage0_query_language_frontends.query_roots.len(), 5);
    assert!(stage0_query_language_frontends
        .dag
        .nodes
        .iter()
        .all(|n| !is_summary(&n.operator)));
    assert!(cross_query_nodes(
        &stage0_query_language_frontends.dag,
        &stage0_query_language_frontends.query_roots
    )
    .is_empty());
}

// ── Pattern A: logical ASAP-aware optimization ───────────────────────────

/// Pass 1 offers each query an exact and a KLL candidate over its own interval.
#[test]
fn stage1_logical_asap_optimization_a_pass1_offers_exact_and_kll_per_query() {
    let run = run_a();
    for q in 0..5 {
        let found = options(&run, q);
        assert!(
            found.contains("exact") && found.contains("Kll"),
            "q{}: {found:?}",
            q + 1
        );
    }
}

/// The five independent KLLs: one per query, each read by its query only, all sized for ε = 0.005.
#[test]
fn stage1_logical_asap_optimization_a_keeps_five_independent_klls() {
    let run = run_a();
    let all_kll = run
        .logical
        .iter()
        .find(|c| {
            !c.shared_input() && (0..5).all(|q| query_option(&c.dag, &c.query_roots, q) == "Kll")
        })
        .expect("independent all-KLL candidate");
    let builds = sketch_builds(&all_kll.dag);
    assert_eq!(builds.len(), 5);
    let readers: BTreeSet<_> = builds
        .iter()
        .map(|(id, ..)| readers(&all_kll.dag, &all_kll.query_roots, *id))
        .collect();
    assert_eq!(readers, (0..5).map(|q| BTreeSet::from([q])).collect());
    let sizes: BTreeSet<_> = builds
        .iter()
        .map(|(_, _, params)| match params {
            SketchParams::Kll { k, .. } => *k,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(sizes.len(), 1, "one ε, one KLL size: {sizes:?}");
}

/// The identical-expression rule shares only raw input (scan, range, shift), never a quantile or summary, and keeps the unshared variant.
#[test]
fn stage1_logical_asap_optimization_a_identical_expression_rule_shares_only_raw_input() {
    let run = run_a();
    assert!(run.logical.iter().any(|c| c.shared_input()));
    assert!(run.logical.iter().any(|c| !c.shared_input()));
    for c in &run.logical {
        for id in cross_query_nodes(&c.dag, &c.query_roots) {
            let kind = relational(c.dag.payload(id));
            assert!(
                matches!(kind.as_deref(), Some("scan" | "time_range" | "time_shift")),
                "{}: shared {kind:?}",
                c.id
            );
        }
    }
}

/// One Exponential Histogram of KLLs over [T − 5y, T] serves all five queries, each through its own merge and p99 estimate.
#[test]
#[ignore = "needs Pass 2 window composition across queries: one window summary for all five (#628, #634)"]
fn stage1_logical_asap_optimization_a_window_composition_adds_one_eh_for_all_five() {
    let run = run_a();
    let found = run.logical.iter().find(|c| {
        windowed_builds(c).iter().any(|(_, a, form, readers)| {
            *a == SketchAlgorithm::Kll
                && matches!(form, WindowForm::ExponentialHistogram { horizon_ms } if *horizon_ms >= 5 * YEAR_MS)
                && readers.len() == 5
        })
    });
    let c = found.expect("a candidate with one EH of KLLs read by all five queries");
    let (eh, ..) = windowed_builds(c)
        .into_iter()
        .find(|(_, _, form, _)| matches!(form, WindowForm::ExponentialHistogram { .. }))
        .unwrap();
    let estimates = estimates_of(&c.dag, eh);
    assert_eq!(estimates.len(), 5, "one estimate per query");
    for (estimate, statistic) in estimates {
        assert_eq!(statistic, SketchStatistic::Quantile { q: 0.99 });
        let merged = c
            .dag
            .producers(estimate)
            .into_iter()
            .any(|p| is_merge(c.dag.payload(p)));
        assert!(
            merged,
            "{}: estimate {estimate:?} does not read a merge",
            c.id
        );
    }
}

/// The shared EH candidate is added next to the independent KLL candidates, not instead of them.
#[test]
#[ignore = "needs Pass 2 window composition across queries: one window summary for all five (#628, #634)"]
fn stage1_logical_asap_optimization_a_keeps_independent_and_shared_window_summaries() {
    let run = run_a();
    let shared = run.logical.iter().any(|c| {
        windowed_builds(c).iter().any(|(_, _, f, r)| {
            matches!(f, WindowForm::ExponentialHistogram { .. }) && r.len() == 5
        })
    });
    let independent = run.logical.iter().any(|c| {
        let builds = windowed_builds(c);
        builds.len() == 5
            && builds.iter().all(|(_, a, f, r)| {
                *a == SketchAlgorithm::Kll && *f == WindowForm::None && r.len() == 1
            })
    });
    assert!(shared && independent);
}

/// Pass 2 adds a candidate for each way of grouping two queries onto one shared window summary.
#[test]
#[ignore = "needs Pass 2 window composition across queries: partial groupings (#628, #634)"]
fn stage1_logical_asap_optimization_a_window_composition_groups_every_pair() {
    let run = run_a();
    let groups: BTreeSet<_> = run
        .logical
        .iter()
        .flat_map(windowed_builds)
        .filter(|(_, _, f, r)| *f != WindowForm::None && r.len() > 1)
        .map(|(.., r)| r)
        .collect();
    for a in 0..5 {
        for b in a + 1..5 {
            assert!(
                groups.contains(&BTreeSet::from([a, b])),
                "pair q{} q{}",
                a + 1,
                b + 1
            );
        }
    }
}

// ── Pattern A: plan selection ────────────────────────────────────────────

/// Plan selection selects one Pattern A candidate, explains the rest, and picks the cheapest valid one.
#[test]
fn stage3_plan_selection_a_selects_cheapest_valid() {
    let run = run_a();
    assert_selects_one_and_explains_the_rest(&run);
    assert_selects_cheapest_valid(&run);
}

/// Every node of a valid candidate is charged once per query that reads it: nothing is materialized, so a shared node is recomputed for each.
#[test]
fn stage3_plan_selection_a_charges_each_node_per_consuming_query() {
    assert_each_node_charged_per_consuming_query(&run_a());
}

/// Sharing the scan never costs more than scanning separately, for the same per-computation choices.
#[test]
#[ignore = "needs plan selection to charge a shared scan that is not materialized per query by the \
            window each query reads, not by the longest window it serves (#632)"]
fn stage3_plan_selection_a_shared_scan_is_not_costlier() {
    let run = run_a();
    let key = |c: &Logical| {
        (0..5)
            .map(|q| query_option(&c.dag, &c.query_roots, q))
            .collect::<Vec<_>>()
    };
    let cost = |c: &Logical| run.cost(&run.physical_of(c).next().unwrap().id);
    let mut compared = 0;
    for shared in run.logical.iter().filter(|c| c.shared_input()) {
        let separate = run
            .logical
            .iter()
            .find(|c| !c.shared_input() && key(c) == key(shared))
            .expect("independent counterpart");
        if let (Some(s), Some(i)) = (cost(shared), cost(separate)) {
            assert!(s <= i, "{} {s} vs {} {i}", shared.id, separate.id);
            compared += 1;
        }
    }
    assert!(compared > 0);
}

// ── Pattern B: query-language frontends and Pass 1 ────────────────────

/// Pattern B lowers to scan → range 5m → quantile, with no summary.
#[test]
fn stage0_query_language_frontends_b_lowers_to_one_range_quantile() {
    let run = run_b();
    let roots = lower_promql(&pattern_b());
    assert_eq!(
        frontend_operations(&roots[0]),
        [op("aggregate:quantile"), op("scan"), op("time_range")]
    );
    assert!(run
        .stage0_query_language_frontends
        .dag
        .nodes
        .iter()
        .all(|n| !is_summary(&n.operator)));
}

/// Pass 1 offers an exact and a KLL candidate for the 5-min window.
#[test]
fn stage1_logical_asap_optimization_b_pass1_offers_exact_and_kll() {
    let found = options(&run_b(), 0);
    assert!(
        found.contains("exact") && found.contains("Kll"),
        "{found:?}"
    );
}

// ── Pattern B: window composition ────────────────────────────────────────

/// Each Pass 1 option gets three window forms: none, a 5-min sliding window with a 1-min slide, and 1-min tumbling windows.
#[test]
#[ignore = "needs Pass 2 window composition: sliding windows (#522); tumbling windows pass in \
            stage1_logical_asap_optimization_b_window_composition_adds_tumbling_per_mergeable_option"]
fn stage1_logical_asap_optimization_b_window_composition_adds_sliding_and_tumbling_per_option() {
    let run = run_b();
    let summaries: BTreeSet<_> = options(&run, 0)
        .into_iter()
        .filter(|o| o != "exact")
        .collect();
    let found: BTreeSet<_> = run
        .logical
        .iter()
        .flat_map(windowed_builds)
        .map(|(_, a, f, _)| (format!("{a:?}"), f))
        .collect();
    let forms = [
        WindowForm::None,
        WindowForm::Sliding {
            length_ms: PATTERN_B_WINDOW_MS,
            slide_ms: PATTERN_B_INTERVAL_MS,
        },
        WindowForm::Tumbling {
            length_ms: PATTERN_B_INTERVAL_MS,
        },
    ];
    for option in &summaries {
        for form in forms {
            assert!(found.contains(&(option.clone(), form)), "{option} {form:?}");
        }
    }
}

/// Each summary option of Pass 1 (KLL and DDSketch, both mergeable) also
/// comes as 1-min tumbling panes, gcd(5 min, 1 min), merged five at a time
/// before its p99 estimate; the whole-window builds are kept and read no
/// merge. Exact has no summary to split: 2 options × 2 forms + exact = 5.
#[test]
fn stage1_logical_asap_optimization_b_window_composition_adds_tumbling_per_mergeable_option() {
    let run = run_b();
    assert_eq!(run.logical.len(), 5);
    let mut found = BTreeSet::new();
    for c in &run.logical {
        for (build, algorithm, form, _) in windowed_builds(c) {
            let merged = estimates_of(&c.dag, build).iter().all(|(e, _)| {
                c.dag
                    .producers(*e)
                    .into_iter()
                    .any(|p| is_merge(c.dag.payload(p)))
            });
            assert_eq!(merged, form != WindowForm::None, "{}: {form:?}", c.id);
            if let asap_types::ir::Operator::ASAP(asap_types::ir::ASAPOp::PaneChain {
                pane_count,
                ..
            }) = c.dag.payload(build)
            {
                assert_eq!(*pane_count, 5, "{}", c.id);
            }
            found.insert((format!("{algorithm:?}"), form));
        }
    }
    let tumbling = WindowForm::Tumbling {
        length_ms: PATTERN_B_INTERVAL_MS,
    };
    let expected: BTreeSet<_> = ["DDSketch", "Kll"]
        .into_iter()
        .flat_map(|a| [(a.to_string(), WindowForm::None), (a.to_string(), tumbling)])
        .collect();
    assert_eq!(found, expected);
}

/// Window parameters are legal: L divides W, s divides L and the evaluation interval, a tumbling length divides both.
#[test]
fn stage1_logical_asap_optimization_b_window_parameters_are_legal() {
    let tumbling = run_b()
        .logical
        .iter()
        .flat_map(windowed_builds)
        .filter(|(_, _, form, _)| matches!(form, WindowForm::Tumbling { .. }))
        .count();
    assert!(tumbling > 0, "not vacuous");
    let (w, every) = (PATTERN_B_WINDOW_MS, PATTERN_B_INTERVAL_MS);
    for c in &run_b().logical {
        for (_, _, form, _) in windowed_builds(c) {
            match form {
                WindowForm::Sliding {
                    length_ms,
                    slide_ms,
                } => {
                    assert_eq!(w % length_ms, 0, "{}: L | W", c.id);
                    assert_eq!(length_ms % slide_ms, 0, "{}: s | L", c.id);
                    assert_eq!(every % slide_ms, 0, "{}: s | interval", c.id);
                }
                WindowForm::Tumbling { length_ms } => {
                    assert_eq!(w % length_ms, 0, "{}: length | W", c.id);
                    assert_eq!(every % length_ms, 0, "{}: length | interval", c.id);
                }
                WindowForm::ExponentialHistogram { .. } => {
                    panic!("{}: Pattern B has no EH", c.id)
                }
                WindowForm::None => {}
            }
        }
    }
}

/// A sliding window with L = W reads one completed window (no merge); tumbling windows merge W / length of them.
#[test]
#[ignore = "needs Pass 2 window composition: sliding windows (#522); tumbling merges pass in \
            stage1_logical_asap_optimization_b_window_composition_adds_tumbling_per_mergeable_option"]
fn stage1_logical_asap_optimization_b_merge_only_where_the_window_form_needs_it() {
    let run = run_b();
    let mut seen = BTreeSet::new();
    for c in &run.logical {
        for (build, _, form, _) in windowed_builds(c) {
            let merged = estimates_of(&c.dag, build).iter().any(|(e, _)| {
                c.dag
                    .producers(*e)
                    .into_iter()
                    .any(|p| is_merge(c.dag.payload(p)))
            });
            assert_eq!(
                merged,
                needs_merge(form, PATTERN_B_WINDOW_MS),
                "{}: {form:?}",
                c.id
            );
            if form != WindowForm::None {
                seen.insert(merged);
            }
        }
    }
    assert_eq!(
        seen,
        BTreeSet::from([false, true]),
        "both a sliding and a tumbling candidate"
    );
}

/// A window form that merges is used only with a summary whose states merge (KLL, DDSketch).
#[test]
fn stage1_logical_asap_optimization_window_merges_use_mergeable_summaries() {
    for (run, window) in [(run_a(), 5 * YEAR_MS), (run_b(), PATTERN_B_WINDOW_MS)] {
        for c in &run.logical {
            for (_, algorithm, form, _) in windowed_builds(c) {
                if needs_merge(form, window) {
                    assert!(
                        matches!(algorithm, SketchAlgorithm::Kll | SketchAlgorithm::DDSketch),
                        "{}: {algorithm:?} {form:?}",
                        c.id
                    );
                }
            }
        }
    }
}

// ── Pattern B: plan selection ────────────────────────────────────────────

/// Plan selection selects one Pattern B candidate, explains the rest, and picks the cheapest valid one.
#[test]
fn stage3_plan_selection_b_selects_cheapest_valid() {
    let run = run_b();
    assert_selects_one_and_explains_the_rest(&run);
    assert_selects_cheapest_valid(&run);
    assert_each_node_charged_per_consuming_query(&run);
}

/// With nothing materialized, the 1-min KLL panes are all rebuilt at every
/// evaluation: they read the same 5 minutes of samples and update the same
/// states as one whole-window KLL, and merging five panes per series adds
/// 1M × 5 operations a minute. Each evaluation also holds its states while
/// it runs: the whole window's 1M KLLs, or the five panes' 5M smaller ones,
/// for the evaluation's latency. So whole-window KLL is selected at 2.363 per
/// second and the panes cost 3.611; exact costs 2.458. DDSketch, whole or in
/// panes, misses the rank target.
#[test]
fn stage3_plan_selection_b_panes_rebuilt_at_query_time_add_the_merge() {
    let run = run_b();
    let cost = |c: &Logical| run.cost(&run.physical_of(c).next().unwrap().id);
    let with = |algorithm: &str, panes: bool| {
        run.logical
            .iter()
            .find(|c| {
                query_option(&c.dag, &c.query_roots, 0) == algorithm
                    && windowed_builds(c)
                        .iter()
                        .any(|(_, _, f, _)| (*f != WindowForm::None) == panes)
            })
            .unwrap_or_else(|| panic!("{algorithm} panes={panes}"))
    };
    let whole = cost(with("Kll", false)).unwrap();
    let panes = cost(with("Kll", true)).unwrap();
    assert!((whole - 2.362_500_025_2).abs() < 1e-9, "{whole}");
    assert!((panes - 3.611_233_350_8).abs() < 1e-9, "{panes}");
    assert_eq!(
        run.selection.selected,
        run.physical_of(with("Kll", false)).next().unwrap().id
    );
    for panes in [false, true] {
        assert_eq!(cost(with("DDSketch", panes)), None, "panes={panes}");
    }
}
