//! Reuse in Stage 1 (#680): the nested-sum merge (a Pass 1 candidate) is
//! selected over the nested sums and answers the same; the rollup and exact
//! top-k reuse candidates (Pass 2) are selected once the finer computation
//! they read is materialized for all its readers.
mod physical_common;

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use asap_aware_mapping::stage3_plan_selection::select_plan;
use asap_aware_mapping::{run_stage1_to_stage3, PlanningModels, Stage1ToStage3Run};
use asap_devtools::planner_layering_examples::shared_data_workload;
use asap_integration_tests::fixtures::lower_promql;
use asap_physical_operators::physical_planner::{compile, promql_rows, InputContract};
use asap_physical_operators::runtime::Scope;
use asap_physical_operators::values::{Batch, Value};
use asap_types::ir::physical_export::{
    compile_physical_asap_dag_with_node_ids, PhysicalASAPOperatorPayload,
};
use asap_types::ir::{
    apply_materialization_timings, ASAPOp, Materialization, MaterializationAssignment, Medium,
    NonASAPOp, Operator, OperatorNode, QueryRoot, TimingMemo,
};
use asap_types::post_asap::Schema;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    DataArrival, DataWorkload, QueryRecurrence, RepeatedDemand, RepetitionInterval,
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
    run_stage1_to_stage3(
        roots,
        &vec![Some(AccuracyTarget::exact()); queries.len()],
        &vec![recurrence; queries.len()],
        &data(),
        PlanningModels::builtin(),
    )
    .unwrap()
}

/// The #509 shared data workload: 1,000,000 series sampled every 15 s.
fn data() -> DataWorkload {
    shared_data_workload(DataArrival::ContinuouslyIngesting)
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

/// Execute `root` at query time, every raw `TimeRange` input fed `samples`.
fn execute(root: &Rc<OperatorNode>) -> Vec<String> {
    let timed = apply_materialization_timings(
        root,
        &MaterializationAssignment::default(),
        &mut TimingMemo::new(),
    )
    .unwrap();
    let compiled = compile_physical_asap_dag_with_node_ids(&timed).unwrap();
    let dag = compiled.dag;
    let inputs: BTreeMap<u64, Batch> = dag
        .nodes
        .iter()
        .filter(|node| {
            matches!(
                compiled.node_ids.operator_node(node.id).unwrap().operator,
                Operator::NonASAP(NonASAPOp::TimeRange { .. })
            )
        })
        .map(|node| {
            let schema: Arc<Schema> = Arc::new(node.output_schema.clone());
            let rows = samples()
                .into_iter()
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
    let contracts = inputs
        .iter()
        .map(|(&id, batch)| (id, InputContract::bounded(batch.schema().clone())))
        .collect();
    let plan = compile(&dag, contracts, &[dag.roots[0] as u64]).unwrap();
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
    .flat_map(|batch| batch.rows().to_vec())
    .map(|row: Vec<Value>| format!("{row:?}"))
    .collect();
    rows.sort();
    rows
}

/// `sum by (job) (sum_over_time(m[5m]))`: the nested sums (L1) and their
/// merge into one accumulator over the raw samples (L2). Plan selection
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

/// A stand-in for Stage 2 materialization (G8): the highest computed nodes
/// (not a raw scan or time range) that more than one query reads are
/// computed once per evaluation (`MatAtQuery`). Returns what the selected candidate
/// reuses.
fn materialize_shared(run: &Stage1ToStage3Run<usize>) -> Vec<String> {
    let mut candidates = run.stage2_physical_asap_optimization.clone();
    for candidate in &mut candidates {
        let dag = &mut candidate.dag;
        let mut readers: BTreeMap<_, Vec<usize>> = BTreeMap::new();
        for (query, &root) in dag.roots.iter().enumerate() {
            let mut stack = vec![root];
            while let Some(id) = stack.pop() {
                let queries = readers.entry(id).or_default();
                if queries.contains(&query) {
                    continue;
                }
                queries.push(query);
                stack.extend(
                    dag.edges
                        .iter()
                        .filter(|e| e.consumer == id)
                        .map(|e| e.producer),
                );
            }
        }
        let shared = |id| readers.get(&id).is_some_and(|q| q.len() > 1);
        // The highest shared nodes: read by a root or an unshared node.
        let highest: Vec<_> = dag
            .nodes
            .iter()
            .map(|node| node.id)
            .filter(|&id| {
                shared(id)
                    && (dag.roots.contains(&id)
                        || dag
                            .edges
                            .iter()
                            .any(|e| e.producer == id && !shared(e.consumer)))
            })
            .collect();
        for node in &mut dag.nodes {
            let raw = matches!(
                node.payload,
                PhysicalASAPOperatorPayload::NonASAP(
                    NonASAPOp::Scan { .. } | NonASAPOp::TimeRange { .. }
                )
            );
            if !raw && highest.contains(&node.id) {
                node.materialization = Materialization::MatAtQuery(Medium::Memory);
            }
        }
    }
    let queries = run.stage1_pass1_per_computation.roots.len();
    let selection = select_plan(
        &candidates,
        &vec![None; queries],
        &vec![repeating(); queries],
        &data(),
        PlanningModels::builtin(),
    )
    .unwrap();
    let from = &candidates
        .iter()
        .find(|c| c.id == selection.selected)
        .unwrap()
        .from_logical;
    run.stage1_pass2
        .iter()
        .find(|p| &p.id == from)
        .map_or_else(Vec::new, |p| p.reused.clone())
}

/// The reuse candidates of `queries`, and what the selected candidate
/// reuses when every shared computed node is computed once.
fn selected_reuse(queries: &[&str]) -> Vec<String> {
    let run = plan(queries, repeating());
    assert!(run.stage2_pruned.is_empty(), "{:?}", run.stage2_pruned);
    let reused: Vec<_> = run
        .stage1_pass2
        .iter()
        .filter(|p| !p.reused.is_empty())
        .collect();
    assert!(!reused.is_empty(), "{queries:?}");
    let selected = run.selected().unwrap();
    assert!(
        !reused.iter().any(|p| p.id == selected.from_logical),
        "{queries:?}: not cheaper while every query recomputes what it reads"
    );
    materialize_shared(&run)
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
/// the raw samples would group them under `m`. Every candidate answers as
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
    let reused: Vec<_> = run
        .stage1_pass2
        .iter()
        .filter(|p| !p.reused.is_empty())
        .collect();
    assert!(!reused.is_empty(), "{queries:?}");
    for candidate in reused {
        for q in 0..queries.len() {
            assert_eq!(
                execute(&physical_root(&run, &candidate.id, q)),
                execute(&physical_root(&run, &candidate.from_logical, q)),
                "{queries:?} Q{}: {:?}",
                q + 1,
                candidate.reused
            );
        }
    }
}

/// Coarse sums (over merged nested sums), minima and maxima rolled up from
/// the finer ones answer as computed from the raw samples.
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
