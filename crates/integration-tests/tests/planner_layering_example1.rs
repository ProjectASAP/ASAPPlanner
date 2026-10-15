//! Acceptance tests for #509 "Example 1: Aggregation over dimensions".
//!
//! Spec: `docs/design_docs/proposals/planner-layering-example1-acceptance.md`.
//! Written by a test designer before the stages existed, then run against
//! the real stages. Tests whose expectation needs a feature not in the stack
//! yet are `#[ignore]`d with the PR or issue that delivers it.
//!
//! Scope: logical ASAP-aware optimization = Pass 1 and Pass 2's
//! identical-expression rule (no window composition); physical ASAP-aware
//! optimization = operator implementation with every sub-DAG
//! `NotMaterialized`. Pass 1 gives 3 candidates; Pass 2 adds, for each, one
//! sharing the scan and one sharing the scan and the range selector: 9 → 9 →
//! 1, a subset of the doc's 1 → 54 → 156 → 1.

mod planner_layering_common;

use std::collections::{BTreeMap, BTreeSet, HashSet};

use asap_aware_mapping::stage3_plan_selection::select_plan;
use asap_aware_mapping::PlanningModels;
use asap_types::ir::flat::NodeId;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, QueryRoot, ScalarExpr, SortKey};
use asap_types::post_asap::{FieldDataType, GroupingStrategy, HydraKind, SketchAlgorithm};
use asap_types::post_asap::{Materialization, Medium};
use asap_types::types::AccuracyTarget;
use planner_layering_common::*;

// ── Example 1 workload ───────────────────────────────────────────────────

fn pipeline() -> Run {
    run_promql(&example1())
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Q2's per-computation option for its top-k, read off its summary build node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Q2Option {
    Exact,
    CountMinHeapPerJob,
    CountSketchHeapPerJob,
    Hydra,
}

const PASS1_Q2_OPTIONS: [Q2Option; 3] = [
    Q2Option::Exact,
    Q2Option::CountMinHeapPerJob,
    Q2Option::CountSketchHeapPerJob,
];

/// Sketch families built in `nodes`, as Example 1's Q2 options.
fn sketch_options(dag: &impl ExportedDag, nodes: &HashSet<NodeId>) -> BTreeSet<Q2Option> {
    nodes
        .iter()
        .filter_map(|&id| match dag.payload(id) {
            Operator::ASAP(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, grouping),
                ..
            }) => Some(match (grouping, kind.algorithm()) {
                (
                    GroupingStrategy::SharedMultiSubpopulation {
                        kind: HydraKind::HydraCms,
                        ..
                    },
                    _,
                ) => Q2Option::Hydra,
                (GroupingStrategy::PerSubpopulationInstance, SketchAlgorithm::CmsWithHeap) => {
                    Q2Option::CountMinHeapPerJob
                }
                (
                    GroupingStrategy::PerSubpopulationInstance,
                    SketchAlgorithm::CountSketchWithHeap,
                ) => Q2Option::CountSketchHeapPerJob,
                (other, algorithm) => {
                    panic!("summary family outside Example 1: {algorithm:?} {other:?}")
                }
            }),
            _ => None,
        })
        .collect()
}

fn roots(query_roots: &[NodeId]) -> (NodeId, NodeId) {
    assert_eq!(query_roots.len(), 2, "every candidate covers Q1 and Q2");
    (query_roots[0], query_roots[1])
}

/// Q2's option and whether Q1 and Q2 share any node, for one candidate.
fn classify(dag: &impl ExportedDag, query_roots: &[NodeId]) -> (Q2Option, bool) {
    let (q1, q2) = roots(query_roots);
    let (c1, c2) = (closure(dag, q1), closure(dag, q2));
    let options = sketch_options(dag, &c2);
    assert!(options.len() <= 1, "Q2 uses one option: {options:?}");
    let option = options.into_iter().next().unwrap_or(Q2Option::Exact);
    (option, !c1.is_disjoint(&c2))
}

/// The candidate with Q2 `option` whose queries share no node (Pass 2 adds
/// shared-input variants of each).
fn physical_with(run: &Run, option: Q2Option) -> &Physical {
    let mut found = run
        .physical
        .iter()
        .filter(|p| classify(p.dag(), &p.query_roots) == (option, false));
    let p = found.next().unwrap_or_else(|| panic!("no {option:?}"));
    assert!(found.next().is_none(), "one {option:?} candidate");
    p
}

/// Example 1's panels repeat every 10 s.
fn every_10s() -> asap_types::workload::QueryRecurrence {
    use asap_types::workload::{QueryRecurrence, RepeatedDemand, RepetitionInterval};
    QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(10_000)))
}

// ── Workload ─────────────────────────────────────────────────────────────

/// The encoded workload is valid and normalizes to Q1 then Q2.
#[test]
fn workload_encodes_example1() {
    let workload = example1();
    workload.validate().expect("valid workload");
    let entries: Vec<_> = workload.query_workload.entries().collect();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].query.0, EXAMPLE1_Q1);
    assert_eq!(entries[1].query.0, EXAMPLE1_Q2);
}

// ── Query-language frontends ──────────────────────────────────────────

/// The PromQL frontend lowers each query to the doc's chain.
#[test]
fn stage0_query_language_frontends_lower_each_query_to_doc_chain() {
    let roots = lower_promql(&example1());
    let chains: Vec<_> = roots.iter().map(frontend_operations).collect();
    assert_eq!(
        chains,
        [
            ["aggregate:rate", "aggregate:sum", "scan", "time_range"],
            ["aggregate:sum", "aggregate:top_k", "scan", "time_range"],
        ]
    );
}

/// The frontends yield one workload DAG with one root per query and no summaries.
#[test]
fn stage0_query_language_frontends_one_summary_free_workload_dag() {
    let stage0_query_language_frontends = pipeline().stage0_query_language_frontends;
    roots(&stage0_query_language_frontends.query_roots);
    assert!(stage0_query_language_frontends
        .dag
        .nodes
        .iter()
        .all(|n| !is_summary(&n.operator)));
}

/// The frontends keep Q1 and Q2 separate; sharing is a logical ASAP-aware optimization decision.
#[test]
fn stage0_query_language_frontends_queries_do_not_share_nodes() {
    let stage0_query_language_frontends = pipeline().stage0_query_language_frontends;
    assert!(!stage0_query_language_frontends.shared_input());
}

// ── Logical ASAP-aware optimization ──────────────────────────────────────

/// Pass 1 gives 1 × 3 = 3 candidates without a shared input: Q2's top-k is
/// exact, Count-Min + heap or CountSketch + heap. (Pass 2 adds shared-input
/// variants of each.)
#[test]
fn stage1_logical_asap_optimization_has_three_pass1_candidates() {
    let run = pipeline();
    let found: Vec<_> = run
        .logical
        .iter()
        .map(|c| classify(&c.dag, &c.query_roots))
        .filter(|(_, shared)| !shared)
        .collect();
    let expected: Vec<_> = PASS1_Q2_OPTIONS.into_iter().map(|o| (o, false)).collect();
    let mut sorted = found.clone();
    sorted.sort();
    assert_eq!(sorted, expected, "each Q2 option exactly once: {found:?}");
}

/// Q1 is exact in every candidate: no summary is reachable from its root.
#[test]
fn stage1_logical_asap_optimization_q1_is_always_exact() {
    let run = pipeline();
    for c in &run.logical {
        let (q1, _) = roots(&c.query_roots);
        assert!(
            closure(&c.dag, q1)
                .iter()
                .all(|&id| !is_summary(c.dag.payload(id))),
            "{}: Q1 reaches a summary",
            c.id
        );
    }
}

/// Every Pass 1 target's exact option is its exact accumulator: Q1's rate
/// and sum and Q2's sum_over_time. Q2's heaps are whole-expression: they
/// absorb its sum_over_time, which is then not computed.
#[test]
fn stage1_logical_asap_optimization_exact_targets_use_accumulators() {
    let run = pipeline();
    for c in &run.logical {
        let expected = match classify(&c.dag, &c.query_roots).0 {
            Q2Option::Exact => 3,
            _ => 2,
        };
        let accumulators = c
            .dag
            .nodes
            .iter()
            .filter(|n| {
                matches!(
                    n.operator,
                    Operator::ASAP(ASAPOp::FinalizeExactAccumulator { .. })
                )
            })
            .count();
        assert_eq!(accumulators, expected, "{}", c.id);
    }
}

/// Q2's summary families are Count-Min + heap and CountSketch + heap, one per job.
#[test]
fn stage1_logical_asap_optimization_q2_summary_families() {
    let run = pipeline();
    let families: BTreeSet<_> = run
        .logical
        .iter()
        .flat_map(|c| {
            let (_, q2) = roots(&c.query_roots);
            sketch_options(&c.dag, &closure(&c.dag, q2))
        })
        .collect();
    assert_eq!(
        families,
        BTreeSet::from([
            Q2Option::CountMinHeapPerJob,
            Q2Option::CountSketchHeapPerJob
        ])
    );
}

/// Pass 1 also offers Hydra (HydraCms over all (job, series) keys, a heap per job) for Q2's top-k.
#[test]
#[ignore = "needs Pass 1 Hydra for grouped approximate counts (#605)"]
fn stage1_logical_asap_optimization_q2_offers_hydra() {
    let run = pipeline();
    assert!(run
        .logical
        .iter()
        .any(|c| classify(&c.dag, &c.query_roots).0 == Q2Option::Hydra));
}

/// Pass 2's identical-expression rule adds a shared-input variant of every Q2 option and keeps the independent one.
#[test]
fn stage1_logical_asap_optimization_keeps_independent_and_shared_variants() {
    let run = pipeline();
    let found: Vec<_> = run
        .logical
        .iter()
        .map(|c| classify(&c.dag, &c.query_roots))
        .collect();
    for option in PASS1_Q2_OPTIONS {
        assert!(found.contains(&(option, false)), "{option:?} independent");
        assert!(found.contains(&(option, true)), "{option:?} shared");
    }
}

/// Only the raw input is ever shared between Q1 and Q2; no summary is shared.
#[test]
fn stage1_logical_asap_optimization_shares_input_but_never_a_summary() {
    let run = pipeline();
    for c in &run.logical {
        for id in cross_query_nodes(&c.dag, &c.query_roots) {
            let p = c.dag.payload(id);
            assert!(
                matches!(relational(p).as_deref(), Some("scan" | "time_range")),
                "{}: shared node {id:?} is not the range selector input: {p:?}",
                c.id
            );
        }
    }
}

/// Logical candidates are valid DAGs with unique ids.
#[test]
fn stage1_logical_asap_optimization_candidates_are_valid_and_uniquely_named() {
    assert_valid_and_uniquely_named(&pipeline());
}

// ── Physical ASAP-aware optimization ─────────────────────────────────────

/// No candidate is discarded before plan selection: the logical candidates
/// map one-to-one, 3 of them without a shared input.
#[test]
fn stage2_physical_asap_optimization_keeps_every_logical_candidate() {
    let run = pipeline();
    assert_one_physical_candidate_per_logical(&run);
    let unshared = run
        .physical
        .iter()
        .filter(|p| !classify(p.dag(), &p.query_roots).1)
        .count();
    assert_eq!(unshared, 3);
}

/// Each physical candidate keeps its logical Q2 option and input sharing.
#[test]
fn stage2_physical_asap_optimization_preserves_logical_choices() {
    let run = pipeline();
    for p in &run.physical {
        let source = run.logical.iter().find(|c| c.id == p.from_logical).unwrap();
        assert_eq!(
            classify(p.dag(), &p.query_roots),
            classify(&source.dag, &source.query_roots),
            "{} vs {}",
            p.id,
            source.id
        );
    }
}

/// Exact TopK is implemented as a sort followed by a limit.
#[test]
fn stage2_physical_asap_optimization_exact_topk_is_sort_then_limit() {
    let run = pipeline();
    let p = physical_with(&run, Q2Option::Exact);
    let sort_then_limit = p.dag().edges.iter().any(|e| {
        relational(p.dag().payload(e.producer)).as_deref() == Some("sort")
            && relational(p.dag().payload(e.consumer)).as_deref() == Some("limit")
    });
    assert!(sort_then_limit, "{}: no sort → limit", p.id);
}

/// Exact Q2 sorts each job's series by the ranked value, descending, and a
/// `Project` over the limit restores the logical `[job, topk_10]` columns.
#[test]
fn stage2_physical_asap_optimization_exact_topk_returns_job_and_topk_10() {
    let run = pipeline();
    let p = physical_with(&run, Q2Option::Exact);
    let QueryRoot::Operator(root) = &p.candidate.roots[1] else {
        panic!("{}: Q2 is an operator root", p.id)
    };
    let names: Vec<_> = root.schema.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["job", "topk_10"], "{}", p.id);
    let NonASAPOp::Project { child: limit, .. } = root.expect_non_asap() else {
        panic!("{}: Q2's root is a Project", p.id)
    };
    let NonASAPOp::Limit {
        n: Some(10),
        child: sort,
        ..
    } = limit.expect_non_asap()
    else {
        panic!("{}: a limit 10 under the Project", p.id)
    };
    let NonASAPOp::Sort {
        keys,
        partition_by,
        child,
    } = sort.expect_non_asap()
    else {
        panic!("{}: a sort under the limit", p.id)
    };
    let [SortKey {
        expr: ScalarExpr::Column(ranked),
        ascending: false,
        ..
    }] = keys.as_slice()
    else {
        panic!("{}: one descending column key: {keys:?}", p.id)
    };
    assert_eq!(child.schema.fields[*ranked].name, "value", "{}", p.id);
    assert_eq!(partition_by.len(), 1, "{}: partitioned by job", p.id);
}

/// A summary Q2 is a build node feeding a top-10 estimation node, with no merge.
#[test]
fn stage2_physical_asap_optimization_summary_topk_is_build_then_estimate() {
    let run = pipeline();
    for p in &run.physical {
        if classify(p.dag(), &p.query_roots).0 == Q2Option::Exact {
            continue;
        }
        assert!(
            !p.dag().nodes.iter().any(|n| is_merge(&n.payload)),
            "{}: no window summaries, so no merge",
            p.id
        );
        let build_to_estimate = p.dag().edges.iter().any(|e| {
            matches!(
                p.dag().payload(e.producer),
                Operator::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(..),
                    ..
                })
            ) && matches!(
                p.dag().payload(e.consumer),
                Operator::ASAP(ASAPOp::SummaryEstimate { .. })
            )
        });
        assert!(build_to_estimate, "{}: no build → estimate", p.id);
    }
}

/// Every sub-DAG is NotMaterialized, so every node runs at query time.
#[test]
fn stage2_physical_asap_optimization_nothing_is_materialized() {
    let run = pipeline();
    for p in &run.physical {
        for n in &p.dag().nodes {
            assert_eq!(
                materialization(p, n.id),
                MaterializationOption::NotMaterialized,
                "{}: {:?}",
                p.id,
                n.id
            );
            assert!(!runs_at_ingestion(p, n.id), "{}: {:?}", p.id, n.id);
        }
    }
}

/// Compile `p` in the physical planner (the runtime capability check), with
/// the samples each time range reads as its input.
fn compile_in_runtime(p: &Physical) -> Result<(), String> {
    use asap_physical_operators::physical_planner::{compile, InputContract};
    use std::sync::Arc;
    let dag = p.dag();
    let mut inputs = BTreeMap::new();
    let mut pending = dag.roots.clone();
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        if relational(dag.payload(id)).as_deref() == Some("time_range") {
            let node = dag.nodes.iter().find(|n| n.id == id).unwrap();
            let schema = Arc::new(node.output_schema.clone());
            inputs.insert(id as u64, InputContract::bounded(schema));
        } else {
            pending.extend(dag.producers(id));
        }
    }
    let roots: Vec<u64> = dag.roots.iter().map(|&id| id as u64).collect();
    compile(dag, inputs, &roots)
        .map(|_| ())
        .map_err(|e| format!("{} ({}): {e}", p.id, p.candidate.display_name))
}

/// Runtime capability check (added by the implementer, not part of the
/// spec): the physical planner compiles every physical candidate.
#[test]
#[ignore = "runtime gaps: a physical candidate's CMS+heap weights are an open requirement \
            that only the selected plan's confirmation answers; the heaps' \
            SummaryEstimate{TopK} output is not the keyed-evaluation shape the physical \
            planner builds (#579)"]
fn stage2_physical_asap_optimization_every_candidate_compiles_in_the_physical_planner() {
    let run = pipeline();
    let failures: Vec<_> = run
        .physical
        .iter()
        .filter_map(|p| compile_in_runtime(p).err())
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}

// ── Plan selection ───────────────────────────────────────────────────────

/// Plan selection selects one candidate and gives every other one a reason.
#[test]
fn stage3_plan_selection_selects_one_and_explains_the_rest() {
    assert_selects_one_and_explains_the_rest(&pipeline());
}

/// The exact Q2 is selected. Both heaps are invalid because a top-k output
/// has no guarantee and the deployment has no evidence for it. Count-Min's
/// weights are raw samples of `http_requests_total`, which the workload
/// declares a counter, so they are non-negative.
#[test]
fn stage3_plan_selection_selects_exact_q2_and_rejects_both_heaps() {
    let run = pipeline();
    assert_eq!(
        run.selection.selected,
        physical_with(&run, Q2Option::Exact).id
    );
    let invalid = run.invalid();
    // Only heap candidates are invalid (with or without a shared input).
    for id in invalid.keys() {
        let p = run.physical(id);
        assert_ne!(
            classify(p.dag(), &p.query_roots).0,
            Q2Option::Exact,
            "{invalid:?}"
        );
    }
    let cms = &physical_with(&run, Q2Option::CountMinHeapPerJob).id;
    assert!(
        invalid[cms.as_str()].contains("no guarantee"),
        "{invalid:?}"
    );
    let count_sketch = &physical_with(&run, Q2Option::CountSketchHeapPerJob).id;
    assert!(
        invalid[count_sketch.as_str()].contains("no guarantee"),
        "{invalid:?}"
    );
}

/// Without the counter declaration, Count-Min's weights are not proven
/// non-negative: plan selection reads no type from the `_total` name.
#[test]
fn stage3_plan_selection_count_min_needs_the_counter_declaration() {
    let mut workload = example1();
    workload
        .data_workload
        .as_mut()
        .unwrap()
        .metric_types
        .clear();
    let run = run_promql(&workload);
    let invalid = run.invalid();
    let cms = &physical_with(&run, Q2Option::CountMinHeapPerJob).id;
    assert!(
        invalid[cms.as_str()].contains("non-negative"),
        "{invalid:?}"
    );
}

/// A shared NotMaterialized input is charged once per consumer, so the
/// shared-input candidates cost what the independent one does; their totals
/// differ from it only by floating-point rounding (the same charges summed
/// in another grouping), which plan selection treats as a tie, so the
/// independent candidate, enumerated first, is selected.
#[test]
fn stage3_plan_selection_shared_input_ties_up_to_rounding() {
    let run = pipeline();
    let independent = physical_with(&run, Q2Option::Exact);
    let base = run.cost(&independent.id).unwrap();
    let shared: Vec<_> = run
        .physical
        .iter()
        .filter(|p| classify(p.dag(), &p.query_roots) == (Q2Option::Exact, true))
        .map(|p| (p.id.clone(), run.cost(&p.id).unwrap()))
        .collect();
    assert!(!shared.is_empty());
    assert!(
        shared
            .iter()
            .any(|(_, total)| total.to_bits() != base.to_bits()),
        "the rounding difference the tie tolerance absorbs: {base} vs {shared:?}"
    );
    for (id, total) in &shared {
        assert!(
            (total - base).abs() <= 1e-9 * base,
            "{id}: {total} vs {base}"
        );
        let rejection = run.selection.rejected.iter().find(|r| &r.id == id).unwrap();
        assert!(rejection.reason.contains("tied"), "{}", rejection.reason);
    }
    assert_eq!(run.selection.selected, independent.id);
}

/// The selected plan is the cheapest valid candidate for the whole workload.
#[test]
fn stage3_plan_selection_selects_cheapest_valid() {
    assert_selects_cheapest_valid(&pipeline());
}

/// Every node of a valid candidate is charged once per query that reads it: nothing is materialized, so a shared node is recomputed for each.
#[test]
fn stage3_plan_selection_charges_each_node_per_consuming_query() {
    assert_each_node_charged_per_consuming_query(&pipeline());
}

/// A hand-built candidate with a materialized node: Q1's plan read by two
/// queries, its `time_range` kept `MatAtQuery`. Plan selection charges the
/// two accumulators (rate, sum) and their finalizations twice, once per query; the walks from
/// both roots stop at the materialized `time_range`, which is computed once
/// and reads the scan once.
fn materialized_case() -> Run {
    let mut workload = example1();
    let repeating = workload.query_workload.repeating_queries.as_mut().unwrap();
    repeating.truncate(1);
    let run = run_promql(&workload);
    let mut p = run.physical[0].clone();
    p.candidate.roots = vec![p.candidate.roots[0].clone(); 2];
    let dag = &mut p.candidate.dag;
    dag.roots = vec![dag.roots[0]; 2];
    p.query_roots = dag.roots.clone();
    let time_range = dag
        .nodes
        .iter_mut()
        .find(|n| relational(&n.payload).as_deref() == Some("time_range"))
        .unwrap();
    time_range.materialization = Materialization::MatAtQuery(Medium::Memory);
    let selection = select_plan(
        std::slice::from_ref(&p.candidate),
        &[Some(AccuracyTarget::exact()), Some(AccuracyTarget::exact())],
        &[every_10s(), every_10s()],
        workload.data_workload.as_ref().unwrap(),
        PlanningModels::builtin(),
    )
    .unwrap();
    Run {
        physical: vec![p],
        selection,
        ..run
    }
}

/// The charging helper agrees with plan selection when a node is
/// materialized.
#[test]
fn stage3_plan_selection_charging_helper_follows_plan_selection_with_a_materialized_node() {
    let run = materialized_case();
    let p = &run.physical[0];
    let mut by_kind: Vec<_> = run.selection.costs[&p.id]
        .per_node
        .iter()
        .map(|(id, c)| (operation(p.dag().payload(*id)), c.multiplier))
        .collect();
    by_kind.sort();
    let expected = [
        ("finalize_exact_accumulator", 2),
        ("finalize_exact_accumulator", 2),
        ("scan", 1),
        ("summary_agg", 2),
        ("summary_agg", 2),
        ("time_range", 1),
    ];
    assert_eq!(
        by_kind,
        expected.map(|(kind, times)| (kind.to_string(), times))
    );
    assert_each_node_charged_per_consuming_query(&run);
}

/// Charging the scan under a materialized node once per query is caught.
#[test]
#[should_panic(expected = "multiplier of")]
fn stage3_plan_selection_charging_helper_rejects_recounting_below_a_materialized_node() {
    let mut run = materialized_case();
    let p = run.physical[0].clone();
    let scan = p
        .dag()
        .nodes
        .iter()
        .find(|n| relational(&n.payload).as_deref() == Some("scan"))
        .unwrap()
        .id;
    run.selection
        .costs
        .get_mut(&p.id)
        .unwrap()
        .per_node
        .get_mut(&scan)
        .unwrap()
        .multiplier = 2;
    assert_each_node_charged_per_consuming_query(&run);
}

/// The plan selection picks compiles in the physical planner (added by the
/// implementer, not part of the spec).
#[test]
fn stage3_plan_selection_selected_plan_compiles_in_the_physical_planner() {
    let run = pipeline();
    compile_in_runtime(run.physical(&run.selection.selected)).unwrap();
}

/// Sharing the input never costs more than reading it separately.
#[test]
fn stage3_plan_selection_shared_input_is_not_costlier() {
    let run = pipeline();
    let by_combo: BTreeMap<_, _> = run
        .physical
        .iter()
        .filter_map(|p| Some((classify(p.dag(), &p.query_roots), run.cost(&p.id)?)))
        .collect();
    let mut compared = 0;
    for option in PASS1_Q2_OPTIONS {
        if let (Some(shared), Some(separate)) = (
            by_combo.get(&(option, true)),
            by_combo.get(&(option, false)),
        ) {
            assert!(shared <= separate, "{option:?}");
            compared += 1;
        }
    }
    assert!(compared > 0);
}
