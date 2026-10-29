//! Reuse in Stage 1 (#680): the nested-sum merge (a Pass 1 candidate) is
//! selected over the nested sums and answers the same; the rollup and exact
//! top-k reuse candidates (Pass 2 memo rules) are selected once the finer
//! computation they read is materialized for all its readers, and answer as
//! the computation they replace. Selection prices fixed plans made of the
//! memo's candidates, so no assertion depends on a timed search.
use crate::physical_common;

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use asap_devtools::planner_layering_examples::{shared_data_workload, SHARED_ROWS_PER_SECOND};
use asap_executor::physical_planner::{compile, promql_rows};
use asap_executor::runtime::Scope;
use asap_executor::values::Value;
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::{
    cheapest_of, compose_memo_plan, expanded_memo, select_among,
};
use asap_logical_asap_optimization::pass2::memo::candidates::{
    CandidateMemo, OriginOfCandidateForMemoGroup,
};
use asap_logical_asap_optimization::pass2::memo::{root_groups, MemoGroupId};
use asap_types::ir::physical_export::compile_physical_asap_dag;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::{
    apply_materialization_timings, ASAPOp, MaterializationAssignment, NodeToTimedNodeCache,
    NonASAPOp, OperatorNode, QueryRoot,
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

/// Execute `root` at query time over the `samples` bound as raw data at its
/// `Scan`; each `TimeRange` keeps its own window (a tumbling pane's moved
/// back by the `TimeShift` below it).
fn execute(root: &Rc<OperatorNode>) -> Vec<String> {
    let timed = apply_materialization_timings(
        root,
        &MaterializationAssignment::default(),
        &mut NodeToTimedNodeCache::new(),
    )
    .unwrap();
    // Tumbling panes run as their panes, each over its own window.
    let timed = asap_types::ir::operator::pane_chain::expand_pane_chains(&timed).unwrap();
    let dag = compile_physical_asap_dag(&timed).unwrap();
    let store = physical_common::raw_data_store(&dag, |_, schema| {
        samples()
            .into_iter()
            .map(|(job, series, ts, value)| {
                let labels = BTreeMap::from([
                    ("__name__".to_string(), "m".to_string()),
                    ("job".to_string(), job.to_string()),
                    ("series".to_string(), series.to_string()),
                ]);
                promql_rows::series_row(schema, &labels, ts, value).unwrap()
            })
            .collect()
    });
    let plan = compile(&dag, &[], &[dag.roots[0] as u64]).unwrap();
    let mut rows: Vec<String> = physical_common::execute(
        &plan,
        store,
        Scope::Query {
            evaluation_time_ms: EVALUATION_MS,
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
    origins: &'a [OriginOfCandidateForMemoGroup],
) -> impl Iterator<Item = (usize, &'a Rc<OperatorNode>)> + 'a {
    let root_groups = root_groups(&memo.query_roots);
    memo.candidates
        .iter()
        .enumerate()
        .filter(move |(group, _)| {
            let is_root = root_groups.contains(&MemoGroupId(*group as u32));
            let group = &memo.groups[*group];
            is_root
                || matches!(
                    group.logical_node.non_asap(),
                    Some(NonASAPOp::Aggregate { .. })
                )
        })
        .flat_map(move |(group, candidates)| {
            candidates
                .iter()
                .filter(move |c| origins.contains(&c.origin))
                .map(move |c| (group, &c.root_node))
        })
}

/// Plan selection's choice among fixed plans of `memo`'s `queries`: every
/// group on its first candidate (`group`), and `variant{i}` per entry of
/// `variants`, a group's candidate index each. Returns the selected plan's
/// name, its roots, whether it builds some computation once per evaluation
/// for all its readers (`MatAtQuery`), and the selection's lowest prices of
/// `group` and of every variant.
fn select(
    memo: &CandidateMemo,
    queries: usize,
    entry: QueryWorkloadEntry,
    variants: &[BTreeMap<usize, usize>],
) -> (String, Vec<Rc<OperatorNode>>, bool, f64, f64) {
    let mut plans = vec![(
        "group".to_string(),
        compose_memo_plan(memo, queries, &BTreeMap::new()),
    )];
    for (i, choice) in variants.iter().enumerate() {
        plans.push((
            format!("variant{i}"),
            compose_memo_plan(memo, queries, choice),
        ));
    }
    let named: Vec<(&str, Vec<QueryRoot>)> = plans
        .iter()
        .map(|(name, roots)| {
            (
                name.as_str(),
                roots.iter().cloned().map(QueryRoot::Operator).collect(),
            )
        })
        .collect();
    let (physical, selection) = select_among(&named, &vec![entry; queries], &data());
    let selected = physical
        .iter()
        .find(|plan| plan.id == selection.selected)
        .expect("the selected plan");
    let name = selected.from_logical.clone();
    let built_once = selected
        .materialization
        .values()
        .any(|m| matches!(m, Materialization::MatAtQuery(_)));
    let roots = plans
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, roots)| roots.clone())
        .unwrap();
    let variants = (0..variants.len())
        .map(|i| cheapest_of(&selection, &format!("variant{i}")))
        .fold(f64::INFINITY, f64::min);
    let own = cheapest_of(&selection, "group");
    // A rejected baseline would make "cheaper than the baseline" hold trivially.
    assert!(own.is_finite(), "the plan without reuse is valid");
    (name, roots, built_once, own, variants)
}

/// `(group, candidate index)` of every candidate [`candidates_of`] yields.
fn choices(memo: &CandidateMemo, origins: &[OriginOfCandidateForMemoGroup]) -> Vec<(usize, usize)> {
    candidates_of(memo, origins)
        .map(|(group, node)| {
            let index = memo.candidates[group]
                .iter()
                .position(|c| Rc::ptr_eq(&c.root_node, node))
                .unwrap();
            (group, index)
        })
        .collect()
}

/// [`candidates_of`], without the candidates that are their group's own
/// node.
fn own_candidates<'a>(
    memo: &'a CandidateMemo,
    origins: &'a [OriginOfCandidateForMemoGroup],
) -> impl Iterator<Item = (usize, &'a Rc<OperatorNode>)> + 'a {
    candidates_of(memo, origins)
        .filter(|(group, node)| !Rc::ptr_eq(node, &memo.groups[*group].logical_node))
}

/// `sum by (job) (sum_over_time(m[5m]))`: the nested sums and their merge
/// into one accumulator over the raw data samples. Plan selection picks the
/// merge, which builds no per-series state, and every candidate answers the
/// same per-job sums.
#[test]
fn nested_sum_merge_is_selected_and_answers_as_the_nested_sums() {
    // Evaluated once, so no tumbling-pane candidates.
    let query = "sum by (job) (sum_over_time(m[5m]))";
    let memo = memo(&exact_queries(&[query]), QueryRecurrence::Unknown);
    let variants: Vec<_> = choices(&memo, &[OriginOfCandidateForMemoGroup::Pass1])
        .into_iter()
        .map(|(group, index)| BTreeMap::from([(group, index)]))
        .collect();
    let (_, roots, ..) = select(&memo, 1, exact(QueryRecurrence::Unknown), &variants);
    assert_eq!(summary_builds(&roots[0]), 1);
    let answer = execute(&roots[0]);
    assert_eq!(answer.len(), 2, "one sum per job: {answer:?}");
    every_candidate_answers_alike(query);
}

/// Kinds of the nodes, other than the source chain, that query 0's and
/// query 1's roots share: what one query reads of the other's computation.
fn shared_kinds(roots: &[Rc<OperatorNode>]) -> BTreeSet<String> {
    let first: BTreeSet<_> = OperatorNode::reachable(&roots[0])
        .iter()
        .map(Rc::as_ptr)
        .collect();
    OperatorNode::reachable(&roots[1])
        .into_iter()
        .filter(|node| first.contains(&Rc::as_ptr(node)))
        .map(|node| node.operator.kind_name().to_string())
        .filter(|kind| !["Scan", "TimeRange", "TimeShift", "Project"].contains(&kind.as_str()))
        .collect()
}

/// The nodes the plan selected for `queries` shares between its two
/// queries, among the queries' own roots and the plans reading `origin`
/// candidates (the `i`-th of each group that has some, together). Stage 2
/// offers each computation two queries read built once per evaluation
/// (`MatAtQuery`); the reuse plan is selected and is cheaper than without it.
fn selected_reuse(queries: &[&str], origin: OriginOfCandidateForMemoGroup) -> BTreeSet<String> {
    let memo = memo(&exact_queries(queries), repeating());
    let mut by_group: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (group, index) in choices(&memo, std::slice::from_ref(&origin)) {
        by_group.entry(group).or_default().push(index);
    }
    let most = by_group.values().map(Vec::len).max().unwrap_or(0);
    assert!(most > 0, "{queries:?}: some {origin:?} candidate");
    let variants: Vec<BTreeMap<_, _>> = (0..most)
        .map(|i| {
            by_group
                .iter()
                .map(|(group, indices)| (*group, indices[i.min(indices.len() - 1)]))
                .collect()
        })
        .collect();
    let (name, roots, built_once, own, reuse) =
        select(&memo, queries.len(), exact(repeating()), &variants);
    assert!(name.starts_with("variant"), "{queries:?}: selected {name}");
    assert!(
        built_once,
        "{queries:?}: {name} computes its shared state once"
    );
    assert!(reuse < own, "{queries:?}: reuse {reuse} vs own {own}");
    shared_kinds(&roots)
}

/// `sum by (job)` beside `sum by (job, instance)`: with the finer sums
/// computed once, plan selection answers the coarse sum from them.
#[test]
fn rollup_is_selected_when_the_finer_sums_are_computed_once() {
    let shared = selected_reuse(
        &["sum by (job) (m)", "sum by (job, instance) (m)"],
        OriginOfCandidateForMemoGroup::Rollup,
    );
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
    let shared = selected_reuse(
        &[
            "topk by (job) (5, rate(m[1m]))",
            "topk by (job) (10, rate(m[1m]))",
        ],
        OriginOfCandidateForMemoGroup::TopKReuse,
    );
    assert!(shared.contains("Sort"), "{shared:?}");
}

/// `node` as Stage 2 implements it for `entry` (its first
/// materialization), executed.
fn execute_implemented(node: &Rc<OperatorNode>, entry: &QueryWorkloadEntry) -> Vec<String> {
    let plans = asap_physical_asap_optimization::optimize_physical_asap(
        "candidate",
        &[QueryRoot::Operator(Rc::clone(node))],
        std::slice::from_ref(entry),
        &data(),
        false,
    )
    .expect("Stage 2 implements the candidate");
    match &plans[0].roots[0] {
        QueryRoot::Operator(root) => execute(root),
        QueryRoot::Scalar(_) => panic!("operator root"),
    }
}

/// Every memo candidate of `query`'s aggregate and root groups other than
/// the group's own node executes to the same rows as that node.
fn every_candidate_answers_alike(query: &str) {
    let memo = memo(&exact_queries(&[query]), QueryRecurrence::Unknown);
    let entry = exact(QueryRecurrence::Unknown);
    let mut checked = 0;
    for (group, node) in own_candidates(&memo, &[OriginOfCandidateForMemoGroup::Pass1]) {
        assert_eq!(
            execute_implemented(node, &entry),
            execute_implemented(&memo.groups[group].logical_node, &entry),
            "{query}: a candidate of group {group} differs"
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "{query}: some candidate other than the group's node"
    );
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

/// Every rollup or top-k reuse candidate in `memo`, of a group only the
/// queries before `first_approximate_query` read, executes to the rows of
/// the computation it replaces (its group's logical node).
fn reuse_answers_alike_in(
    memo: &CandidateMemo,
    queries: &[(&str, AccuracyTarget)],
    first_approximate_query: usize,
) {
    let entry = exact(repeating());
    let mut checked = 0;
    for (group, node) in candidates_of(
        memo,
        &[
            OriginOfCandidateForMemoGroup::Rollup,
            OriginOfCandidateForMemoGroup::TopKReuse,
        ],
    ) {
        if memo.groups[group]
            .reached_by_queries()
            .iter()
            .any(|&q| q >= first_approximate_query)
        {
            continue;
        }
        assert_eq!(
            execute_implemented(node, &entry),
            execute_implemented(&memo.groups[group].logical_node, &entry),
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

/// Query `q`'s rows when Stage 2 implements `roots` together (its first
/// materialization) for `entries`.
fn execute_plan(
    roots: &[Rc<OperatorNode>],
    entries: &[QueryWorkloadEntry],
    q: usize,
) -> Vec<String> {
    let roots: Vec<_> = roots.iter().cloned().map(QueryRoot::Operator).collect();
    let plans = asap_physical_asap_optimization::optimize_physical_asap(
        "plan",
        &roots,
        entries,
        &data(),
        false,
    )
    .expect("Stage 2 implements the plan");
    match &plans[0].roots[q] {
        QueryRoot::Operator(root) => execute(root),
        QueryRoot::Scalar(_) => panic!("operator root"),
    }
}

/// A fine and a coarse sum beside two differently grouped quantiles: a plan
/// that shares the quantiles' summary build (summary-capability rule) and
/// rolls the coarse sum up from the fine one (rollup rule) answers each sum
/// as its own computation does (from the raw data samples), and each
/// quantile as the same shared build without the rollup.
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
    let shared = memo
        .groups
        .iter()
        .find(|group| {
            group.reached_by_queries().len() > 1
                && group.reached_by_queries().iter().all(|&q| q >= 2)
                && matches!(group.logical_node.asap(), Some(ASAPOp::SummaryAgg { .. }))
        })
        .map(|group| Rc::clone(&group.logical_node))
        .expect("the quantiles share a summary build");
    // Each quantile's own candidate that reads the shared build.
    let mut sharing: BTreeMap<usize, usize> = [2, 3]
        .into_iter()
        .map(|q| {
            memo.candidates
                .iter()
                .enumerate()
                .filter(|(group, _)| memo.groups[*group].reached_by_queries() == [q])
                .flat_map(|(group, candidates)| {
                    candidates
                        .iter()
                        .enumerate()
                        .map(move |(i, c)| (group, i, c))
                })
                .find(|(_, _, c)| {
                    OperatorNode::reachable(&c.root_node)
                        .iter()
                        .any(|node| Rc::ptr_eq(node, &shared))
                })
                .map(|(group, i, _)| (group, i))
                .unwrap_or_else(|| panic!("Q{} reads the shared build", q + 1))
        })
        .collect();
    let without = compose_memo_plan(&memo, queries.len(), &sharing);
    let (group, index) = choices(&memo, &[OriginOfCandidateForMemoGroup::Rollup])
        .into_iter()
        .find(|(group, _)| memo.groups[*group].reached_by_queries() == [0])
        .expect("a rollup of the coarse sum");
    sharing.insert(group, index);
    let with = compose_memo_plan(&memo, queries.len(), &sharing);
    let own = compose_memo_plan(&memo, queries.len(), &BTreeMap::new());
    assert!(
        OperatorNode::reachable(&with[2])
            .iter()
            .any(|node| OperatorNode::reachable(&with[3])
                .iter()
                .any(|other| Rc::ptr_eq(node, other)
                    && matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. })))),
        "both quantiles read one summary build"
    );
    let entries = vec![exact(repeating()); queries.len()];
    for q in 0..2 {
        assert_eq!(
            execute_plan(&with, &entries, q),
            execute_plan(&own, &entries, q),
            "Q{}",
            q + 1
        );
    }
    for q in 2..4 {
        assert_eq!(
            execute_plan(&with, &entries, q),
            execute_plan(&without, &entries, q),
            "Q{}",
            q + 1
        );
    }
}
