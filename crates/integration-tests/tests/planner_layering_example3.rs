//! Acceptance tests for #509 "Example 3: Aggregation over windows — the
//! window-composition rule in Pass 2".
//!
//! Spec: `docs/design_docs/proposals/planner-layering-example3-acceptance.md`.
//! Written by a test designer who does not implement the stages (#590);
//! ported with the window-composition rule's tumbling windows (#580), which
//! added the tumbling-only tests and the Pattern B runtime check. Tests that
//! need an unimplemented feature are `#[ignore]`d, naming it. Window
//! summaries are read through `planner_layering_common::window_form`.

mod physical_common;
mod planner_layering_common;

use std::collections::BTreeSet;

use asap_types::ir::export::{LogicalASAPNodeId, LogicalASAPOperatorPayload};
use asap_types::ir::schema::{SketchAlgorithm, SketchParams, SketchStatistic};
use asap_types::workload::DataArrival;
use planner_layering_common::*;

fn run_a() -> Run {
    run_promql(&pattern_a(
        PatternARecurrence::OnceAdHoc,
        DataArrival::Mixed,
    ))
}

fn run_b() -> Run {
    run_promql(&pattern_b())
}

/// The local options of `query` across all candidates.
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
fn windowed_builds(
    c: &Logical,
) -> Vec<(
    LogicalASAPNodeId,
    SketchAlgorithm,
    WindowForm,
    BTreeSet<usize>,
)> {
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
    let a = pattern_a(PatternARecurrence::OnceAdHoc, DataArrival::Mixed);
    a.validate().expect("valid Pattern A");
    let queries: Vec<_> = a.query_workload.entries().map(|e| e.query.0).collect();
    assert_eq!(queries, PATTERN_A.map(|(q, ..)| q));
    let b = pattern_b();
    b.validate().expect("valid Pattern B");
    assert_eq!(evaluation_interval_ms(&b, 0), Some(PATTERN_B_INTERVAL_MS));
    assert_eq!(lookback_ms(&b, 0), PATTERN_B_WINDOW_MS);
}

// ── Pattern A: Stage 0 ───────────────────────────────────────────────────

/// Each Pattern A query lowers to scan → (time shift) → range → quantile.
#[test]
fn stage0_a_lowers_each_query_to_its_interval() {
    let roots = lower_promql(&pattern_a(
        PatternARecurrence::OnceAdHoc,
        DataArrival::Mixed,
    ));
    let ops: Vec<_> = roots.iter().map(stage0_operations).collect();
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

/// Stage 0 is one summary-free DAG whose five queries share no node.
#[test]
fn stage0_a_one_summary_free_dag_without_sharing() {
    let run = run_a();
    let s0 = &run.stage0;
    s0.dag.validate().expect("valid DAG");
    assert_eq!(s0.query_roots.len(), 5);
    assert!(s0.dag.nodes.iter().all(|n| !is_summary(&n.payload)));
    assert!(cross_query_nodes(&s0.dag, &s0.query_roots).is_empty());
}

// ── Pattern A: Stage 1 ───────────────────────────────────────────────────

/// Pass 1 offers each query an exact and a KLL candidate over its own interval.
#[test]
fn stage1_a_pass1_offers_exact_and_kll_per_query() {
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
fn stage1_a_keeps_five_independent_klls() {
    let run = run_a();
    let all_kll = run
        .logical
        .iter()
        .find(|c| {
            !c.shared_input && (0..5).all(|q| query_option(&c.dag, &c.query_roots, q) == "Kll")
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
            SketchParams::Kll { k } => *k,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(sizes.len(), 1, "one ε, one KLL size: {sizes:?}");
}

/// The identical-expression rule shares only raw input (scan, range, shift), never a quantile or summary, and keeps the unshared variant.
#[test]
fn stage1_a_identical_expression_rule_shares_only_raw_input() {
    let run = run_a();
    assert!(run.logical.iter().any(|c| c.shared_input));
    assert!(run.logical.iter().any(|c| !c.shared_input));
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
#[ignore = "needs Pass 2 window composition (#580): Exponential Histogram"]
fn stage1_a_window_composition_adds_one_eh_for_all_five() {
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
            .any(|p| matches!(c.dag.payload(p), LogicalASAPOperatorPayload::SummaryMerge));
        assert!(
            merged,
            "{}: estimate {estimate:?} does not read a merge",
            c.id
        );
    }
}

/// The shared EH candidate is added next to the independent KLL candidates, not instead of them.
#[test]
#[ignore = "needs Pass 2 window composition (#580): Exponential Histogram"]
fn stage1_a_keeps_independent_and_shared_window_summaries() {
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
#[ignore = "needs Pass 2 window composition (#580): partial groupings"]
fn stage1_a_window_composition_groups_every_pair() {
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

// ── Pattern A: Stage 3 ───────────────────────────────────────────────────

/// Stage 3 selects one Pattern A candidate, explains the rest, and picks the cheapest valid one.
#[test]
fn stage3_a_selects_cheapest_valid() {
    let run = run_a();
    assert_selects_one_and_explains_the_rest(&run);
    assert_selects_cheapest_valid(&run);
}

/// Every node is charged once, so a node read by several queries is costed once.
#[test]
fn stage3_a_charges_each_node_once() {
    assert_each_node_charged_once(&run_a());
}

/// Sharing the scan never costs more than scanning separately, for the same local choices.
#[test]
fn stage3_a_shared_scan_is_not_costlier() {
    let run = run_a();
    let key = |c: &Logical| {
        (0..5)
            .map(|q| query_option(&c.dag, &c.query_roots, q))
            .collect::<Vec<_>>()
    };
    let cost = |c: &Logical| run.cost(&run.physical_of(c).next().unwrap().id);
    let mut compared = 0;
    for shared in run.logical.iter().filter(|c| c.shared_input) {
        let separate = run
            .logical
            .iter()
            .find(|c| !c.shared_input && key(c) == key(shared))
            .expect("independent counterpart");
        if let (Some(s), Some(i)) = (cost(shared), cost(separate)) {
            assert!(s <= i, "{} {s} vs {} {i}", shared.id, separate.id);
            compared += 1;
        }
    }
    assert!(compared > 0);
}

// ── Pattern B: Stage 0 and Pass 1 ────────────────────────────────────────

/// Pattern B lowers to scan → range 5m → quantile, with no summary.
#[test]
fn stage0_b_lowers_to_one_range_quantile() {
    let run = run_b();
    let roots = lower_promql(&pattern_b());
    assert_eq!(
        stage0_operations(&roots[0]),
        [op("aggregate:quantile"), op("scan"), op("time_range")]
    );
    assert!(run.stage0.dag.nodes.iter().all(|n| !is_summary(&n.payload)));
}

/// Pass 1 offers an exact and a KLL candidate for the 5-min window.
#[test]
fn stage1_b_pass1_offers_exact_and_kll() {
    let found = options(&run_b(), 0);
    assert!(
        found.contains("exact") && found.contains("Kll"),
        "{found:?}"
    );
}

// ── Pattern B: window composition ────────────────────────────────────────

/// Each Pass 1 option gets three window forms: none, a 5-min sliding window with a 1-min slide, and 1-min tumbling windows.
#[test]
#[ignore = "needs Pass 2 window composition (#580): sliding windows; the tumbling part passes in stage1_b_window_composition_adds_tumbling_per_option"]
fn stage1_b_window_composition_adds_sliding_and_tumbling_per_option() {
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

/// Window parameters are legal: L divides W, s divides L and the evaluation interval, a tumbling length divides both.
#[test]
fn stage1_b_window_parameters_are_legal() {
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
#[ignore = "needs Pass 2 window composition (#580): sliding windows (the form without a merge); tumbling merges pass in stage1_b_tumbling_merges_five_panes_before_the_estimate"]
fn stage1_b_merge_only_where_the_window_form_needs_it() {
    let run = run_b();
    let mut seen = BTreeSet::new();
    for c in &run.logical {
        for (build, _, form, _) in windowed_builds(c) {
            let merged = estimates_of(&c.dag, build).iter().any(|(e, _)| {
                c.dag
                    .producers(*e)
                    .into_iter()
                    .any(|p| matches!(c.dag.payload(p), LogicalASAPOperatorPayload::SummaryMerge))
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
fn stage1_window_merges_use_mergeable_summaries() {
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

/// Each summary option (KLL, DDSketch) comes with no window form and in 1-min tumbling panes; the exact option builds no summary.
#[test]
fn stage1_b_window_composition_adds_tumbling_per_option() {
    let run = run_b();
    let found: BTreeSet<_> = run
        .logical
        .iter()
        .flat_map(windowed_builds)
        .map(|(_, a, f, _)| (a, f))
        .collect();
    let tumbling = WindowForm::Tumbling {
        length_ms: PATTERN_B_INTERVAL_MS,
    };
    assert_eq!(
        found,
        BTreeSet::from([
            (SketchAlgorithm::Kll, WindowForm::None),
            (SketchAlgorithm::Kll, tumbling),
            (SketchAlgorithm::DDSketch, WindowForm::None),
            (SketchAlgorithm::DDSketch, tumbling),
        ])
    );
    assert_eq!(run.logical.len(), 5, "exact, 2 summaries x 2 window forms");
}

/// A tumbling candidate merges five 1-min panes, newest first, whose coverages tile the 5-min window relative to the evaluation, and estimates p99 from the merge.
#[test]
fn stage1_b_tumbling_merges_five_panes_before_the_estimate() {
    use asap_types::ir::properties::summary_coverage::CoverageTime;
    let run = run_b();
    let mut tumbling = 0;
    for c in &run.logical {
        let merges: Vec<_> = c
            .dag
            .nodes
            .iter()
            .filter(|n| matches!(n.payload, LogicalASAPOperatorPayload::SummaryMerge))
            .collect();
        let [merge] = merges.as_slice() else {
            assert!(merges.is_empty(), "{}", c.id);
            continue;
        };
        tumbling += 1;
        let panes = c.dag.producers(merge.id);
        assert_eq!(panes.len(), 5, "{}", c.id);
        let mut covered: Vec<_> = panes
            .iter()
            .map(|&pane| {
                assert_eq!(
                    window_form(&c.dag, pane),
                    WindowForm::Tumbling {
                        length_ms: PATTERN_B_INTERVAL_MS
                    }
                );
                let node = c.dag.nodes.iter().find(|n| n.id == pane).unwrap();
                match &node.coverage.as_ref().unwrap().regions[..] {
                    [region] => match &region.time_ms {
                        Some(CoverageTime::RelativeToEvaluation(range)) => range.clone(),
                        other => panic!("{}: {other:?}", c.id),
                    },
                    other => panic!("{}: {other:?}", c.id),
                }
            })
            .collect();
        covered.sort_by_key(|r| r.start);
        let minute = PATTERN_B_INTERVAL_MS as i64;
        assert_eq!(
            covered,
            (0..5)
                .rev()
                .map(|i| -(i + 1) * minute..-i * minute)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            merge.coverage.as_ref().unwrap().regions[0].time_ms,
            Some(CoverageTime::RelativeToEvaluation(
                -(PATTERN_B_WINDOW_MS as i64)..0
            ))
        );
        let estimates: Vec<_> = c
            .dag
            .consumers(merge.id)
            .into_iter()
            .filter_map(|e| match c.dag.payload(e) {
                LogicalASAPOperatorPayload::SummaryEstimate { query } => Some(query.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(estimates, [SketchStatistic::Quantile { q: 0.99 }]);
    }
    assert_eq!(tumbling, 2, "KLL and DDSketch");
}

// ── Pattern B: Stage 3 ───────────────────────────────────────────────────

/// Stage 3 selects one Pattern B candidate, explains the rest, and picks the cheapest valid one.
#[test]
fn stage3_b_selects_cheapest_valid() {
    let run = run_b();
    assert_selects_one_and_explains_the_rest(&run);
    assert_selects_cheapest_valid(&run);
    assert_each_node_charged_once(&run);
}

/// Merged KLL and DDSketch panes keep the family's guarantee, so Stage 3
/// finds every tumbling candidate valid and prices it.
#[test]
fn stage3_b_tumbling_candidates_are_valid() {
    let run = run_b();
    let invalid = run.invalid();
    let mut tumbling = 0;
    for p in &run.physical {
        let merged = p
            .dag
            .nodes
            .iter()
            .any(|n| matches!(n.payload, LogicalASAPOperatorPayload::SummaryMerge));
        if merged {
            tumbling += 1;
            assert!(
                !invalid.contains_key(p.id.as_str()),
                "{}: {:?}",
                p.id,
                invalid
            );
            assert!(run.cost(&p.id).is_some(), "{}", p.id);
        }
    }
    assert_eq!(tumbling, 2);
}

// ── Pattern B: runtime ───────────────────────────────────────────────────

/// Run `p` in the executor at evaluation time [`EVALUATION_MS`]. Each raw
/// `TimeRange` a summary reads is a deployment input holding the samples in
/// its own window: `[T − offset − range, T − offset)`, so a pane gets its
/// minute and the whole-window build gets all five.
fn execute_b(p: &Physical) -> Vec<String> {
    use asap_executor::physical_planner::InputContract;
    use asap_executor::physical_planner::{compile, promql_rows::encode_series_identity};
    use asap_executor::runtime::Scope;
    use asap_executor::values::{Batch, Value};
    use asap_types::ir::export::NonASAPOpKind;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    let relational = |id| match p.dag.payload(id) {
        LogicalASAPOperatorPayload::Relational { operator } => Some(operator),
        _ => None,
    };
    let mut inputs = BTreeMap::new();
    for node in &p.dag.nodes {
        let Some(NonASAPOpKind::TimeRange { range, .. }) = relational(node.id) else {
            continue;
        };
        let offset = p
            .dag
            .producers(node.id)
            .into_iter()
            .find_map(|input| match relational(input) {
                Some(NonASAPOpKind::TimeShift { shift }) => Some(shift.offset_ms),
                _ => None,
            })
            .unwrap_or(0);
        let end = EVALUATION_MS - offset;
        let window = end - range.as_millis() as i64..end;
        let schema = Arc::new(node.output_schema.clone());
        let rows = samples()
            .into_iter()
            .filter(|(_, ts, _)| window.contains(ts))
            .map(|(series, ts, value)| {
                schema
                    .fields
                    .iter()
                    .map(|field| match field.name.as_str() {
                        "ts" => Value::Timestamp(ts),
                        "value" => Value::Float64(value),
                        _ => Value::Utf8(
                            encode_series_identity(&BTreeMap::from([
                                ("__name__".to_string(), "latency_ms".to_string()),
                                ("series".to_string(), series.to_string()),
                            ]))
                            .unwrap()
                            .into(),
                        ),
                    })
                    .collect()
            })
            .collect();
        inputs.insert(
            u64::from(node.id.0),
            Batch::try_new(schema, rows).expect("input batch"),
        );
    }
    let contracts = inputs
        .iter()
        .map(|(&id, batch)| (id, InputContract::bounded(batch.schema().clone())))
        .collect();
    let root = u64::from(p.dag.roots[0].0);
    let plan = compile(&p.dag, contracts, &[root]).unwrap_or_else(|e| panic!("{}: {e}", p.id));
    let scope = Scope::Query {
        evaluation_time_ms: EVALUATION_MS,
        revision: 1,
    };
    let mut rows: Vec<String> = physical_common::execute(&plan, inputs, scope)
        .remove(0)
        .iter()
        .flat_map(|batch| batch.rows().iter().map(|row| render(row)))
        .collect();
    rows.sort();
    rows
}

/// One output row as `ts=<ms> <value> <series identity>`.
fn render(row: &[asap_executor::values::Value]) -> String {
    use asap_executor::values::Value;
    row.iter()
        .map(|value| match value {
            Value::Timestamp(ms) => format!("ts={ms}"),
            Value::Float64(v) => format!("{v}"),
            Value::Utf8(s) => s.to_string(),
            other => panic!("unexpected output value {other:?}"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

const EVALUATION_MS: i64 = 5 * MINUTE_MS as i64;

/// Two series, four samples per minute over the 5 minutes before the
/// evaluation: (series, ts, value).
fn samples() -> Vec<(&'static str, i64, f64)> {
    let mut rows = vec![];
    for (s, series) in ["a", "b"].into_iter().enumerate() {
        for minute in 0..5 {
            for j in 0..4 {
                let ts = minute * MINUTE_MS as i64 + j * 15_000;
                rows.push((series, ts, (s * 100) as f64 + (minute * 4 + j) as f64));
            }
        }
    }
    rows
}

/// Runtime (Pattern B): the KLL candidate in 1-min tumbling panes compiles
/// in the executor and returns what the whole-window KLL returns: one p99
/// per series at the evaluation time. 20 samples per series are below k, so
/// both are exact.
#[test]
fn runtime_b_tumbling_kll_matches_the_whole_window() {
    let run = run_b();
    let kll = |merged: bool| {
        run.physical
            .iter()
            .find(|p| {
                let nodes = || p.dag.nodes.iter().map(|n| &n.payload);
                nodes().any(|n| {
                    matches!(n, LogicalASAPOperatorPayload::SummaryAgg {
                        family: asap_types::ir::schema::FieldDataType::Sketch(kind, _), ..
                    } if *kind.algorithm() == SketchAlgorithm::Kll)
                }) && nodes().any(|n| matches!(n, LogicalASAPOperatorPayload::SummaryMerge))
                    == merged
            })
            .expect("KLL candidate")
    };
    let whole = execute_b(kll(false));
    assert_eq!(whole.len(), 2, "{whole:?}");
    assert_eq!(
        whole,
        [
            format!("ts={EVALUATION_MS} 119 {{\"series\":\"b\"}}"),
            format!("ts={EVALUATION_MS} 19 {{\"series\":\"a\"}}"),
        ]
    );
    assert_eq!(execute_b(kll(true)), whole);
}
