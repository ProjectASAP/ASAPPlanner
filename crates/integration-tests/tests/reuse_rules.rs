//! Reuse in Stage 1 (#680): the nested-sum merge (a Pass 1 candidate) is
//! selected over the nested sums and answers the same; the rollup and exact
//! top-k reuse candidates (Pass 2 memo rules) are selected once the finer
//! computation they read is materialized for all its readers, and answer as
//! the computation they replace.
mod physical_common;

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;

use asap_devtools::planner_layering_examples::{shared_data_workload, SHARED_ROWS_PER_SECOND};
use asap_executor::physical_planner::{compile, promql_rows};
use asap_executor::runtime::Scope;
use asap_executor::values::{Batch, Value};
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::expanded_memo;
use asap_logical_asap_optimization::memo::candidates::{CandidateMemo, CandidateOrigin};
use asap_plan_selection::{PlanningModels, Stage1ToStage3Run};
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

/// `queries` lowered at `accuracy`, with series identity, as the planner
/// receives them.
fn roots(queries: &[(&str, AccuracyTarget)]) -> Vec<(usize, QueryRoot)> {
    queries
        .iter()
        .enumerate()
        .map(|(i, (query, accuracy))| {
            let root = lower_promql(query, accuracy.clone()).unwrap();
            let root = promql_rows::with_series_identity(&root).unwrap();
            (i, QueryRoot::Operator(root))
        })
        .collect()
}

fn exact_queries<'a>(queries: &[&'a str]) -> Vec<(&'a str, AccuracyTarget)> {
    queries
        .iter()
        .map(|query| (*query, AccuracyTarget::exact()))
        .collect()
}

/// The memo search's plan for `queries`, exact, each with `recurrence`.
fn plan(queries: &[&str], recurrence: QueryRecurrence) -> Stage1ToStage3Run<usize> {
    asap_plan_selection::run_stage1_to_stage3(
        roots(&exact_queries(queries)),
        &vec![exact(recurrence); queries.len()],
        &data(),
        PlanningModels::builtin(),
    )
    .unwrap()
}

/// The memo Stage 1 builds for `queries`, each with `recurrence`.
fn memo(queries: &[(&str, AccuracyTarget)], recurrence: QueryRecurrence) -> CandidateMemo {
    expanded_memo(roots(queries), &vec![exact(recurrence); queries.len()])
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

/// Query `q`'s root in the selected logical plan of `run`.
fn selected_root(run: &Stage1ToStage3Run<usize>, q: usize) -> Rc<OperatorNode> {
    match &run.selected_logical().expect("a plan")[q] {
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

fn summary_builds(root: &Rc<OperatorNode>) -> usize {
    OperatorNode::reachable(root)
        .iter()
        .filter(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
        .count()
}

/// Every candidate of every group of `memo` with origin in `origins`, with
/// the group it implements; groups that answer a query or aggregate only.
fn candidates_of<'a>(
    memo: &'a CandidateMemo,
    origins: &'a [CandidateOrigin],
) -> impl Iterator<Item = (usize, &'a Rc<OperatorNode>)> + 'a {
    memo.candidates
        .iter()
        .enumerate()
        .filter(|(group, _)| {
            let group = &memo.groups[*group];
            !group.roots_of.is_empty()
                || matches!(group.node.non_asap(), Some(NonASAPOp::Aggregate { .. }))
        })
        .flat_map(move |(group, candidates)| {
            candidates
                .iter()
                .filter(move |c| origins.contains(&c.origin))
                .map(move |c| (group, &c.node))
        })
}

/// `sum by (job) (sum_over_time(m[5m]))`: the nested sums and their merge
/// into one accumulator over the raw data samples. Plan selection picks the
/// merge, which builds no per-series state, and every candidate answers the
/// same per-job sums.
#[test]
fn nested_sum_merge_is_selected_and_answers_as_the_nested_sums() {
    // Evaluated once, so no tumbling-pane candidates.
    let query = "sum by (job) (sum_over_time(m[5m]))";
    let run = plan(&[query], QueryRecurrence::Unknown);
    assert_eq!(summary_builds(&selected_root(&run, 0)), 1);
    let answer = execute(&selected_root(&run, 0));
    assert_eq!(answer.len(), 2, "one sum per job: {answer:?}");
    every_candidate_answers_alike(query);
}

/// Kinds of the nodes, other than the source chain, that query 0's and
/// query 1's selected roots share: what one query reads of the other's
/// computation.
fn shared_kinds(run: &Stage1ToStage3Run<usize>) -> BTreeSet<String> {
    let first: BTreeSet<_> = OperatorNode::reachable(&selected_root(run, 0))
        .iter()
        .map(Rc::as_ptr)
        .collect();
    OperatorNode::reachable(&selected_root(run, 1))
        .into_iter()
        .filter(|node| first.contains(&Rc::as_ptr(node)))
        .map(|node| node.operator.kind_name().to_string())
        .filter(|kind| !["Scan", "TimeRange", "TimeShift", "Project"].contains(&kind.as_str()))
        .collect()
}

/// The nodes the selected plan of `queries` shares between its two
/// queries. Stage 2 offers each computation two queries read built once per
/// evaluation (`MatAtQuery`); the selected reuse candidate takes it.
fn selected_reuse(queries: &[&str]) -> BTreeSet<String> {
    let run = plan(queries, repeating());
    let selected = run.selected().unwrap();
    assert!(
        selected
            .materialization
            .values()
            .any(|m| matches!(m, Materialization::MatAtQuery(_))),
        "{queries:?}: {} computes its shared state once",
        selected.id
    );
    shared_kinds(&run)
}

/// `sum by (job)` beside `sum by (job, instance)`: with the finer sums
/// computed once, plan selection answers the coarse sum from them.
#[test]
fn rollup_is_selected_when_the_finer_sums_are_computed_once() {
    let shared = selected_reuse(&["sum by (job) (m)", "sum by (job, instance) (m)"]);
    // The coarse sum reads the finer sums' state or finalized values.
    assert!(
        shared.contains("Aggregate") || shared.contains("SummaryAgg"),
        "{shared:?}"
    );
}

/// Two exact top-k of the per-series rates: with the shared ranking
/// computed once, plan selection reads both from one ranking instead of
/// sorting every series twice.
#[test]
fn exact_topk_reuse_is_selected_when_the_ranking_is_computed_once() {
    let shared = selected_reuse(&[
        "topk by (job) (5, rate(m[1m]))",
        "topk by (job) (10, rate(m[1m]))",
    ]);
    assert!(shared.contains("Sort"), "{shared:?}");
}

/// `node` as Stage 2 implements it (its first materialization), executed.
fn execute_implemented(node: &Rc<OperatorNode>) -> Vec<String> {
    let plans = asap_physical_asap_optimization::optimize_physical_asap(
        "candidate",
        &[QueryRoot::Operator(Rc::clone(node))],
        &[exact(repeating())],
        &data(),
        PlanningModels::builtin().deployment_capabilities,
    )
    .expect("Stage 2 implements the candidate");
    match &plans[0].roots[0] {
        QueryRoot::Operator(root) => execute(root),
        QueryRoot::Scalar(_) => panic!("operator root"),
    }
}

/// Every memo candidate of `query`'s aggregate and root groups executes to
/// the same rows as the group's own logical node.
fn every_candidate_answers_alike(query: &str) {
    let memo = memo(&exact_queries(&[query]), QueryRecurrence::Unknown);
    let mut checked = 0;
    for (group, node) in candidates_of(&memo, &[CandidateOrigin::Pass1]) {
        assert_eq!(
            execute_implemented(node),
            execute_implemented(&memo.groups[group].node),
            "{query}: a candidate of group {group} differs"
        );
        checked += 1;
    }
    assert!(checked > 1, "{query}");
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

/// Every rollup or top-k reuse candidate in `memo`, of a group only
/// `exact` queries read, executes to the rows of the computation it replaces
/// (its group's logical node).
fn reuse_answers_alike_in(memo: &CandidateMemo, queries: &[(&str, AccuracyTarget)], exact: usize) {
    let mut checked = 0;
    for (group, node) in candidates_of(memo, &[CandidateOrigin::Rollup, CandidateOrigin::TopKReuse])
    {
        if memo.groups[group].queries.iter().any(|&q| q >= exact) {
            continue;
        }
        assert_eq!(
            execute_implemented(node),
            execute_implemented(&memo.groups[group].node),
            "{queries:?}: a reuse candidate of group {group} differs"
        );
        checked += 1;
    }
    assert!(checked > 0, "{queries:?}: some reuse candidate");
}

/// [`reuse_answers_alike_in`] the memo of the exact `queries`.
fn reuse_answers_alike(queries: &[(&str, AccuracyTarget)]) {
    reuse_answers_alike_in(&memo(queries, repeating()), queries, queries.len());
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
        reuse_answers_alike(&exact_queries(&queries));
    }
}

/// Top-k read from one larger ranking answer as computed alone.
#[test]
fn exact_topk_reuse_answers_as_each_topk() {
    reuse_answers_alike(&exact_queries(&[
        "topk by (job) (1, sum_over_time(m[5m]))",
        "topk by (job) (2, sum_over_time(m[5m]))",
    ]));
}

/// A fine and a coarse sum beside two differently grouped quantiles: in the
/// memo, the quantiles gain a shared summary build (summary-capability
/// rule) and the coarse sum a rollup of the fine one; the rollup answers as
/// the coarse sum computed from the raw data samples. The memo chooses each
/// group's candidate on its own, so the rollup's answer does not depend on
/// the quantiles' choice.
#[test]
fn a_rollup_over_a_shared_summary_candidate_answers_as_without_it() {
    let quantile = AccuracyTarget::max_error(0.05).failure_probability(0.3);
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
            quantile.clone(),
        ),
        ("avg by (job) (quantile_over_time(0.95, m[5m]))", quantile),
    ];
    let memo = memo(&queries, repeating());
    assert!(
        memo.groups.iter().any(|group| group.queries.len() > 1
            && group.queries.iter().all(|&q| q >= 2)
            && matches!(group.node.asap(), Some(ASAPOp::SummaryAgg { .. }))),
        "the quantiles share a summary build"
    );
    reuse_answers_alike_in(&memo, &queries, 2);
}
