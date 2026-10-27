//! Reuse in Stage 1 (#680): the nested-sum merge (a Pass 1 candidate) is
//! selected over the nested sums and answers the same; the rollup and exact
//! top-k reuse candidates (Pass 2) are selected once the finer computation
//! they read is materialized for all its readers.
mod physical_common;

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;

use asap_devtools::candidate_labels::pass2_label;
use asap_devtools::planner_layering_examples::{shared_data_workload, SHARED_ROWS_PER_SECOND};
use asap_executor::physical_planner::{compile, promql_rows};
use asap_executor::runtime::Scope;
use asap_executor::values::{Batch, Value};
use asap_integration_tests::fixtures::lower_promql;
use asap_logical_asap_optimization::workload_candidates::Pass2Candidate;
use asap_plan_selection::Stage1ToStage3Run;
use asap_types::ir::physical_export::compile_physical_asap_dag_with_node_ids;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::schema::Schema;
use asap_types::ir::{
    apply_materialization_timings, ASAPOp, MaterializationAssignment, NonASAPOp, Operator,
    OperatorNode, QueryRoot, TimingMemo,
};
use asap_types::physical::Materialization;
use asap_types::workload::{
    DataArrival, DataWorkload, QueryRecurrence, QueryWorkloadEntry, RepeatedDemand,
    RepetitionInterval,
};

const EVALUATION_MS: i64 = 300_000;

/// Every stage's output for `queries`, exact, each with `recurrence`.
fn plan(queries: &[&str], recurrence: QueryRecurrence) -> Stage1ToStage3Run<usize> {
    let roots = queries
        .iter()
        .enumerate()
        .map(|(i, query)| {
            let root = lower_promql(query, AccuracyTarget::exact()).unwrap();
            let root = promql_rows::with_series_identity(&root).unwrap();
            (i, QueryRoot::Operator(root))
        })
        .collect();
    explicit_rule_plans(roots, &vec![exact(recurrence); queries.len()])
}

/// Price and execute explicit exact, fused, and shared-summary rule fixtures.
/// This catalog tests rewrite semantics independently of a timed search archive.
fn explicit_rule_plans(
    roots: Vec<(usize, QueryRoot)>,
    entries: &[QueryWorkloadEntry],
) -> Stage1ToStage3Run<usize> {
    use asap_logical_asap_optimization::per_computation_candidates::{
        generate_per_computation_candidates, ComputationCandidate,
    };
    use asap_types::ir::schema::SketchAlgorithm;
    let inventory = generate_per_computation_candidates(roots).unwrap();
    let exact = vec![0; inventory.targets.len()];
    let mut fused = exact.clone();
    for (index, target) in inventory.targets.iter().enumerate() {
        if target
            .fused
            .as_ref()
            .is_some_and(|fused| !fused.candidates.is_empty())
        {
            fused[index] = target.choices.candidates.len();
        }
    }
    for target in &inventory.targets {
        if let Some(fusion) = &target.fused {
            for &absorbed in &fusion.absorbs {
                fused[absorbed] = 0;
            }
        }
    }
    let mut summaries = fused.clone();
    for (index, target) in inventory.targets.iter().enumerate() {
        if let Some(choice) = target.choices.candidates.iter().position(|candidate| {
            matches!(
                candidate,
                ComputationCandidate::Summary(SketchAlgorithm::DDSketch)
            )
        }) {
            summaries[index] = choice;
        }
    }
    let mut assignments = vec![exact];
    for assignment in [fused, summaries] {
        if !assignments.contains(&assignment) {
            assignments.push(assignment);
        }
    }
    asap_integration_tests::planning::explicit_assignment_plans(
        inventory,
        assignments,
        entries,
        &data(),
    )
}

/// The workload entry of an exact query evaluated as `recurrence` says.
fn exact(recurrence: QueryRecurrence) -> QueryWorkloadEntry {
    QueryWorkloadEntry {
        query: Default::default(),
        requirements: Default::default(),
        predictability: Default::default(),
        recurrence,
        range_evaluation: None,
    }
}

/// The #509 shared data workload: 1,000,000 series sampled every 15 s.
fn data() -> DataWorkload {
    let mut data = shared_data_workload(
        DataArrival::Ingesting {
            rows_per_second: SHARED_ROWS_PER_SECOND,
        },
        "m",
    );
    // 100 jobs: each `by (job)` group holds many series, so a ranking
    // shared between two top-k saves work.
    data.sources.get_mut("m").unwrap().distinct_values_per_label =
        [("job".to_string(), 100)].into();
    data
}

/// Every minute.
fn repeating() -> QueryRecurrence {
    QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(60_000)))
}

fn logical(run: &Stage1ToStage3Run<usize>, id: &str) -> Rc<OperatorNode> {
    let candidate = run
        .stage1_logical_asap_optimization
        .iter()
        .find(|c| c.id == id)
        .unwrap();
    match &candidate.roots.as_ref().unwrap()[0] {
        QueryRoot::Operator(node) => Rc::clone(node),
        QueryRoot::Scalar(_) => panic!("operator root"),
    }
}

/// Two jobs of two series each, a sample every 15 s over [0, 5m).
fn samples() -> Vec<(&'static str, &'static str, i64, f64)> {
    let mut rows = vec![];
    for (s, (job, series)) in [("a", "a1"), ("a", "a2"), ("b", "b1"), ("b", "b2")]
        .into_iter()
        .enumerate()
    {
        for step in 0..20i64 {
            rows.push((
                job,
                series,
                step * 15_000,
                (s as i64 * 100 + step) as f64 + 0.5,
            ));
        }
    }
    rows
}

/// Execute `root` at query time, every raw `TimeRange` input fed the
/// `samples` in its window: `(end - range, end]`, where `end` is the
/// evaluation time moved back by a `TimeShift` directly below (a tumbling
/// pane's).
fn execute(root: &Rc<OperatorNode>) -> Vec<String> {
    let timed = apply_materialization_timings(
        root,
        &MaterializationAssignment::default(),
        &mut TimingMemo::new(),
    )
    .unwrap();
    // Tumbling panes run as their panes, each over its own window.
    let timed = asap_types::ir::operator::pane_chain::expand_pane_chains(&timed).unwrap();
    let compiled = compile_physical_asap_dag_with_node_ids(&timed).unwrap();
    let dag = compiled.dag;
    let inputs: BTreeMap<u64, Batch> = dag
        .nodes
        .iter()
        .filter_map(|node| {
            let Operator::NonASAP(NonASAPOp::TimeRange { range, child, .. }) =
                &compiled.node_ids.operator_node(node.id).unwrap().operator
            else {
                return None;
            };
            let back_ms = match child.non_asap() {
                Some(NonASAPOp::TimeShift { shift, .. }) => shift.offset_ms,
                _ => 0,
            };
            let end = EVALUATION_MS - back_ms;
            let start = end - range.as_millis() as i64;
            Some((node, start, end))
        })
        .map(|(node, start, end)| {
            let schema: Arc<Schema> = Arc::new(node.output_schema.clone());
            let rows = samples()
                .into_iter()
                .filter(|&(_, _, ts, _)| start < ts && ts <= end)
                .map(|(job, series, ts, value)| {
                    let labels = BTreeMap::from([
                        ("__name__".to_string(), "m".to_string()),
                        ("job".to_string(), job.to_string()),
                        ("series".to_string(), series.to_string()),
                    ]);
                    promql_rows::series_row(&schema, &labels, ts, value).unwrap()
                })
                .collect();
            (node.id as u64, Batch::try_new(schema, rows).unwrap())
        })
        .collect();
    let boundaries: BTreeSet<u64> = inputs.keys().copied().collect();
    let plan = compile(&dag, &boundaries, &[dag.roots[0] as u64]).unwrap();
    let mut rows: Vec<String> = physical_common::execute(
        &plan,
        inputs,
        Scope::Query {
            evaluation_time_ms: EVALUATION_MS,
            revision: 1,
        },
    )
    .remove(0)
    .iter()
    .flat_map(|batch| batch.to_rows().to_vec())
    .map(|row: Vec<Value>| format!("{row:?}"))
    .collect();
    rows.sort();
    rows
}

/// `sum by (job) (sum_over_time(m[5m]))`: the nested sums (L1) and their
/// merge into one accumulator over the raw data samples (L2). Plan selection
/// picks the merge, which builds no per-series state and answers the same
/// per-job sums.
#[test]
fn nested_sum_merge_is_selected_and_answers_as_the_nested_sums() {
    // Evaluated once, so no tumbling-pane candidates.
    let run = plan(
        &["sum by (job) (sum_over_time(m[5m]))"],
        QueryRecurrence::Unknown,
    );
    assert_eq!(run.stage1_logical_asap_optimization.len(), 2);
    let selected = run.selected().expect("a plan");
    assert_eq!(selected.from_logical, "L2");
    let (nested, merged) = (logical(&run, "L1"), logical(&run, "L2"));
    let builds = |root: &Rc<OperatorNode>| {
        OperatorNode::reachable(root)
            .iter()
            .filter(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
            .count()
    };
    assert_eq!((builds(&nested), builds(&merged)), (2, 1));
    let answer = execute(&merged);
    assert_eq!(answer.len(), 2, "one sum per job: {answer:?}");
    assert_eq!(answer, execute(&nested));
}

/// What the logical candidate `id` of `run` answers from another
/// computation: one devtools label per rollup or exact top-k reuse
/// candidate it applies (e.g. "Sum acc for Q1 rolled up from Q2"); empty
/// when it reuses nothing.
fn reused(run: &Stage1ToStage3Run<usize>, id: &str) -> Vec<String> {
    let logical = &run.stage1_logical_asap_optimization;
    let Some(pass2) = logical
        .iter()
        .find(|c| c.id == id)
        .and_then(|c| c.pass2.as_ref())
    else {
        return Vec::new();
    };
    let pass1_roots = logical[pass2.from_pass1_position]
        .roots
        .as_deref()
        .unwrap_or_default();
    let roots = logical
        .iter()
        .find(|c| c.id == id)
        .and_then(|c| c.roots.as_deref().ok())
        .unwrap_or_default();
    // A Pass 2 candidate with only one reuse candidate, labelled alone.
    let only = |rollup_candidates, topk_reuse_candidates| Pass2Candidate {
        shared_builds: Vec::new(),
        pane_candidates: Vec::new(),
        shared_pane_chains: Vec::new(),
        rollup_candidates,
        topk_reuse_candidates,
        sharing_candidates: Vec::new(),
        ..pass2.clone()
    };
    pass2
        .rollup_candidates
        .iter()
        .map(|c| only(vec![c.clone()], Vec::new()))
        .chain(
            pass2
                .topk_reuse_candidates
                .iter()
                .map(|c| only(Vec::new(), vec![c.clone()])),
        )
        .map(|p| {
            pass2_label(&p, pass1_roots, roots)
                .trim_start_matches(" · ")
                .to_string()
        })
        .collect()
}

/// Whether some Pass 2 candidate of `run` reuses a computation.
fn any_reuse(run: &Stage1ToStage3Run<usize>) -> bool {
    run.stage1_logical_asap_optimization
        .iter()
        .filter_map(|c| c.pass2.as_ref())
        .any(|p| !p.rollup_candidates.is_empty() || !p.topk_reuse_candidates.is_empty())
}

/// The reuse candidates of `queries`, and what the selected candidate
/// reuses. Stage 2 offers each computation two queries read built once per
/// evaluation (`MatAtQuery`); the selected reuse candidate takes it.
fn selected_reuse(queries: &[&str]) -> Vec<String> {
    let run = plan(queries, repeating());
    assert!(run.stage2_pruned.is_empty(), "{:?}", run.stage2_pruned);
    assert!(any_reuse(&run), "{queries:?}");
    let selected = run.selected().unwrap();
    assert!(
        selected
            .materialization
            .values()
            .any(|m| matches!(m, Materialization::MatAtQuery(_))),
        "{queries:?}: {} computes its shared state once",
        selected.id
    );
    reused(&run, &selected.from_logical)
}

/// `sum by (job)` beside `sum by (job, instance)`: with the finer sums
/// computed once, plan selection answers the coarse sum from them.
#[test]
fn rollup_is_selected_when_the_finer_sums_are_computed_once() {
    assert_eq!(
        selected_reuse(&["sum by (job) (m)", "sum by (job, instance) (m)"]),
        ["Sum acc for Q1 rolled up from Q2"]
    );
}

/// Two exact top-k of the per-series rates: with the shared ranking
/// computed once, plan selection reads both from one top-10 ranking
/// instead of sorting every series twice.
#[test]
fn exact_topk_reuse_is_selected_when_the_ranking_is_computed_once() {
    assert_eq!(
        selected_reuse(&[
            "topk by (job) (5, rate(m[1m]))",
            "topk by (job) (10, rate(m[1m]))",
        ]),
        ["top-k for Q1, Q2 from one top-10"]
    );
}

/// Query `q`'s root in the physical candidate Stage 2 made of `logical`.
fn physical_root(run: &Stage1ToStage3Run<usize>, logical: &str, q: usize) -> Rc<OperatorNode> {
    let candidate = run
        .stage2_physical_asap_optimization
        .iter()
        .find(|c| c.from_logical == logical)
        .unwrap_or_else(|| panic!("{logical} is implemented"));
    match &candidate.roots[q] {
        QueryRoot::Operator(node) => Rc::clone(node),
        QueryRoot::Scalar(_) => panic!("operator root"),
    }
}

/// Every Stage 1 candidate of `query` executes to the same rows.
fn every_candidate_answers_alike(query: &str) {
    let run = plan(&[query], QueryRecurrence::Unknown);
    assert!(run.stage2_pruned.is_empty(), "{:?}", run.stage2_pruned);
    let answers: Vec<(String, Vec<String>)> = run
        .stage1_logical_asap_optimization
        .iter()
        .map(|c| (c.id.clone(), execute(&physical_root(&run, &c.id, 0))))
        .collect();
    for (id, answer) in &answers[1..] {
        assert_eq!(answer, &answers[0].1, "{query}: {id} differs from L1");
    }
}

/// A per-series sum drops the metric name, so grouping by `__name__` over
/// the nested sums groups every series under the empty name; a merge over
/// the raw data samples would group them under `m`. Every candidate answers as
/// the nested sums.
#[test]
fn nested_sum_by_the_metric_name_answers_as_the_nested_sums() {
    every_candidate_answers_alike("sum by (__name__) (sum_over_time(m[5m]))");
    every_candidate_answers_alike("sum by (job) (sum_over_time(m[5m]))");
}

/// Every reuse candidate of `queries` executes, query by query, to the rows
/// of the candidate it reuses from.
fn reuse_answers_alike(queries: &[&str]) {
    let run = plan(queries, repeating());
    assert!(run.stage2_pruned.is_empty(), "{:?}", run.stage2_pruned);
    // `(reuse candidate id, the id of the Pass 1 candidate it derives from)`.
    let reusing: Vec<(String, String)> = run
        .stage1_logical_asap_optimization
        .iter()
        .filter(|c| !reused(&run, &c.id).is_empty())
        .map(|c| {
            let from = c.pass2.as_ref().unwrap().from_pass1_position;
            (
                c.id.clone(),
                run.stage1_logical_asap_optimization[from].id.clone(),
            )
        })
        .collect();
    assert!(!reusing.is_empty(), "{queries:?}");
    for (id, from) in &reusing {
        for q in 0..queries.len() {
            assert_eq!(
                execute(&physical_root(&run, id, q)),
                execute(&physical_root(&run, from, q)),
                "{queries:?} Q{}: {:?}",
                q + 1,
                reused(&run, id)
            );
        }
    }
}

/// Coarse sums (over merged nested sums), minima and maxima rolled up from
/// the finer ones answer as computed from the raw data samples.
#[test]
fn rollups_answer_as_the_coarse_computations() {
    for queries in [
        [
            "sum by (job) (sum_over_time(m[5m]))",
            "sum by (job, series) (sum_over_time(m[5m]))",
        ],
        ["min by (job) (m)", "min by (job, series) (m)"],
        ["max by (job) (m)", "max by (job, series) (m)"],
    ] {
        reuse_answers_alike(&queries);
    }
}

/// Top-k read from one larger ranking answer as computed alone.
#[test]
fn exact_topk_reuse_answers_as_each_topk() {
    reuse_answers_alike(&[
        "topk by (job) (1, sum_over_time(m[5m]))",
        "topk by (job) (2, sum_over_time(m[5m]))",
    ]);
}

/// A fine and a coarse sum beside two differently grouped quantiles: every
/// candidate that shares the quantiles' DDSketch (summary-capability rule)
/// and rolls the coarse sum up from the fine one (rollup rule) answers each
/// sum as its Pass 1 candidate does (computed from the raw data samples), and
/// each quantile as the candidate with the same shared build and no rollup.
#[test]
fn a_rollup_over_a_shared_summary_candidate_answers_as_without_it() {
    let queries = [
        (
            "sum by (job) (sum_over_time(m[5m]))",
            AccuracyTarget::exact(),
        ),
        (
            "sum by (job, series) (sum_over_time(m[5m]))",
            AccuracyTarget::exact(),
        ),
        (
            "avg by (job, series) (quantile_over_time(0.99, m[5m]))",
            AccuracyTarget::max_error(0.05).failure_probability(0.3),
        ),
        (
            "avg by (job) (quantile_over_time(0.95, m[5m]))",
            AccuracyTarget::max_error(0.05).failure_probability(0.3),
        ),
    ];
    let roots = queries
        .iter()
        .enumerate()
        .map(|(i, (query, accuracy))| {
            let root = lower_promql(query, accuracy.clone()).unwrap();
            let root = promql_rows::with_series_identity(&root).unwrap();
            (i, QueryRoot::Operator(root))
        })
        .collect();
    let run = explicit_rule_plans(roots, &vec![exact(repeating()); queries.len()]);
    let implemented = |id: &str| {
        run.stage2_physical_asap_optimization
            .iter()
            .any(|c| c.from_logical == id)
    };
    let logical = &run.stage1_logical_asap_optimization;
    let mut checked = 0;
    for candidate in logical.iter().filter(|c| implemented(&c.id)) {
        let Some(pass2) = &candidate.pass2 else {
            continue;
        };
        if pass2.shared_builds.is_empty() || pass2.rollup_candidates.is_empty() {
            continue;
        }
        let from = &logical[pass2.from_pass1_position].id;
        // For a candidate without panes: the candidate with the same shared
        // builds and nothing else.
        let unrolled = logical
            .iter()
            .filter(|c| implemented(&c.id) && pass2.pane_candidates.is_empty())
            .find(|c| {
                c.pass2.as_ref().is_some_and(|p| {
                    p.from_pass1_position == pass2.from_pass1_position
                        && p.shared_builds == pass2.shared_builds
                        && p.pane_candidates.is_empty()
                        && p.rollup_candidates.is_empty()
                        && p.topk_reuse_candidates.is_empty()
                        && p.sharing_candidates.is_empty()
                })
            });
        for q in 0..2 {
            assert_eq!(
                execute(&physical_root(&run, &candidate.id, q)),
                execute(&physical_root(&run, from, q)),
                "{} Q{}",
                candidate.id,
                q + 1
            );
        }
        if let Some(unrolled) = unrolled {
            for q in 2..4 {
                assert_eq!(
                    execute(&physical_root(&run, &candidate.id, q)),
                    execute(&physical_root(&run, &unrolled.id, q)),
                    "{} Q{} against {}",
                    candidate.id,
                    q + 1,
                    unrolled.id
                );
            }
        }
        checked += 1;
    }
    assert!(checked > 0, "some candidate shares a summary and rolls up");
}
