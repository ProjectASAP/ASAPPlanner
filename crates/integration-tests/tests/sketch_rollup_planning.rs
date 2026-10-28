//! Sketch rollup in planning: the memo's rollup rule offers answering a
//! coarse sketch grouping from the finest build of the same sketch over the
//! same rows, and plan selection selects it when one build and a merge cost
//! less than a build per query over the raw rows. Selection prices fixed
//! plans made of the memo's candidates, so no assertion depends on a timed
//! search.
use std::rc::Rc;

use asap_executor::physical_planner::promql_rows;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::{
    cheapest_of, compose_memo_plan, expanded_memo, select_among,
};
use asap_logical_asap_optimization::memo::candidates::{
    CandidateMemo, CandidateOrigin, ImplementationCandidate,
};
use asap_types::ir::properties::{AccuracyMetric, AccuracyScope, AccuracyTarget};
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};
use asap_types::workload::{
    DataWorkload, Predictability, QueryRecurrence, QueryRequirements, QueryWorkloadEntry,
    RepeatedDemand, RepetitionInterval,
};
use std::collections::BTreeMap;

fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "http",
        Schema::new(vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("region", DataType::Utf8, false),
            Field::plain("service", DataType::Utf8, false),
            Field::plain("user_id", DataType::Float64, false),
            Field::plain("latency", DataType::Float64, false),
        ]),
    )
}

/// A predictable query repeating every `interval_ms` at `accuracy`.
fn entry(accuracy: &AccuracyTarget, interval_ms: u32) -> QueryWorkloadEntry {
    QueryWorkloadEntry {
        query: Default::default(),
        requirements: QueryRequirements {
            accuracy: accuracy.clone(),
            max_latency_ms: None,
        },
        predictability: Predictability::Predictable { known_at: None },
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            interval_ms,
        ))),
        range_evaluation: None,
    }
}

/// `http` arrives at 2e6 rows/s (W-RollupHeavy's table), 10,000 series,
/// a million distinct users at most per evaluation, and holds a minute of
/// rows (the catalog has no time index, so a query's time predicate is a
/// filter over every row held).
fn data() -> DataWorkload {
    serde_json::from_value(serde_json::json!({
        "arrival": {"ingesting": {"rows_per_second": 2_000_000.0}},
        "sources": {"http": {
            "series_count": 10_000,
            "sample_interval": 5,
            "values_non_negative": true,
            "max_distinct_per_evaluation": 1_000_000,
            "rows_held": 120_000_000,
        }},
    }))
    .unwrap()
}

/// The SQL `queries` lowered at `accuracy`.
async fn sql_roots(queries: &[&str], accuracy: &AccuracyTarget) -> Vec<(usize, QueryRoot)> {
    let mut roots = Vec::new();
    for (i, query) in queries.iter().enumerate() {
        let root = lower_sql(query, &catalog(), accuracy.clone())
            .await
            .unwrap();
        roots.push((i, QueryRoot::Operator(root)));
    }
    roots
}

/// Whether `node` builds a summary.
fn is_build(node: &Rc<OperatorNode>) -> bool {
    matches!(
        node.asap(),
        Some(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
    )
}

/// `(group, candidate index)` of query `q`'s own groups' (read by `q`
/// alone) candidates `keep` accepts; those of the topmost group first.
fn own_choices(
    memo: &CandidateMemo,
    q: usize,
    keep: impl Fn(&ImplementationCandidate) -> bool,
) -> Vec<(usize, usize)> {
    let mut found: Vec<_> = memo
        .candidates
        .iter()
        .enumerate()
        .filter(|(group, _)| memo.groups[*group].queries == [q])
        .flat_map(|(group, candidates)| {
            candidates
                .iter()
                .enumerate()
                .filter(|(_, c)| keep(c))
                .map(move |(index, _)| (group, index))
                .collect::<Vec<_>>()
        })
        .collect();
    // Children come before parents in the memo.
    found.sort_by_key(|(group, _)| std::cmp::Reverse(*group));
    found
}

/// The selected plan's roots among fixed plans of `memo`'s queries, and
/// the selection's lowest prices of `separate` and of the `shared` plans:
/// - `separate`: each query reads its own summary build (its topmost own
///   group's first Pass 1 candidate that builds one);
/// - `shared{g}`: every query reads the summary state group `g`, its own
///   estimate or a regrouping merge of it, when every query can.
fn select_rollup(
    memo: &CandidateMemo,
    queries: usize,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
) -> (Vec<Rc<OperatorNode>>, f64, f64) {
    let builds = |node: &Rc<OperatorNode>| OperatorNode::reachable(node).iter().any(is_build);
    let separate: BTreeMap<usize, usize> = (0..queries)
        .map(|q| {
            own_choices(memo, q, |c| {
                c.origin == CandidateOrigin::Pass1 && builds(&c.node)
            })
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("Q{} has a summary candidate", q + 1))
        })
        .collect();
    let mut plans = vec![(
        "separate".to_string(),
        compose_memo_plan(memo, queries, &separate),
    )];
    for (state, group) in memo.groups.iter().enumerate() {
        if !is_build(&group.node) || group.queries.len() < queries {
            continue;
        }
        let reading: Option<BTreeMap<usize, usize>> = (0..queries)
            .map(|q| {
                own_choices(memo, q, |c| {
                    c.children.iter().any(|child| child.0 as usize == state)
                })
                .into_iter()
                .next()
            })
            .collect();
        if let Some(choice) = reading {
            plans.push((
                format!("shared{state}"),
                compose_memo_plan(memo, queries, &choice),
            ));
        }
    }
    assert!(
        plans.len() > 1,
        "a plan in which every query reads one build"
    );
    let named: Vec<(&str, Vec<QueryRoot>)> = plans
        .iter()
        .map(|(name, roots)| {
            (
                name.as_str(),
                roots.iter().cloned().map(QueryRoot::Operator).collect(),
            )
        })
        .collect();
    let (physical, selection) = select_among(&named, entries, data);
    let selected = physical
        .iter()
        .find(|plan| plan.id == selection.selected)
        .expect("the selected plan");
    let roots = plans
        .iter()
        .find(|(name, _)| *name == selected.from_logical)
        .map(|(_, roots)| roots.clone())
        .unwrap();
    let shared = plans[1..]
        .iter()
        .map(|(name, _)| cheapest_of(&selection, name))
        .fold(f64::INFINITY, f64::min);
    (roots, cheapest_of(&selection, "separate"), shared)
}

/// The summary builds and rollups (regrouping merges) `roots` read, each
/// once.
fn builds_and_rollups(roots: &[Rc<OperatorNode>]) -> (usize, usize) {
    let mut seen = std::collections::HashSet::new();
    let (mut builds, mut rollups) = (0, 0);
    for node in roots.iter().flat_map(OperatorNode::reachable) {
        if !seen.insert(Rc::as_ptr(&node)) {
            continue;
        }
        match node.asap() {
            _ if is_build(&node) => builds += 1,
            Some(ASAPOp::SummaryMerge {
                group_by: Some(_), ..
            }) => rollups += 1,
            _ => {}
        }
    }
    (builds, rollups)
}

/// Distinct users by (region, service) and by service, both at relative
/// ε = 0.05 per value over the same window (W-RollupHeavy's targets): the
/// memo offers the coarse KMV rolled up from the finer one, and plan
/// selection selects one build and a rollup, cheaper than a build per
/// query from the raw rows.
#[tokio::test]
async fn stage3_selects_a_distinct_count_rollup_when_it_is_cheaper() {
    let accuracy = AccuracyTarget::max_error(0.05)
        .failure_probability(0.05)
        .accuracy_metric(AccuracyMetric::Relative)
        .accuracy_scope(AccuracyScope::PerValue);
    let window = "WHERE ts >= now() - INTERVAL '5 minutes'";
    let roots = sql_roots(
        &[
            &format!(
                "SELECT region, service, COUNT(DISTINCT user_id) AS value FROM http {window} \
                 GROUP BY region, service"
            ),
            &format!(
                "SELECT service, COUNT(DISTINCT user_id) AS value FROM http {window} \
                 GROUP BY service"
            ),
        ],
        &accuracy,
    )
    .await;
    let entries = vec![entry(&accuracy, 60_000); 2];
    let memo = expanded_memo(roots, &entries);
    let (selected, separate, rolled_up) = select_rollup(&memo, 2, &entries, &data());
    assert!(rolled_up < separate, "{rolled_up} vs {separate}");
    assert_eq!(builds_and_rollups(&selected), (1, 1));
}

/// p99 latency by (region, service), by region and by service at rank
/// ε = 0.05 (template 13's shape): both coarse groupings roll up from one
/// KLL, and plan selection selects that plan.
#[tokio::test]
async fn coarse_quantile_groupings_roll_up_from_one_kll() {
    let accuracy = AccuracyTarget::max_error(0.05)
        .failure_probability(0.05)
        .accuracy_metric(AccuracyMetric::Rank)
        .accuracy_scope(AccuracyScope::PerValue);
    let window = "WHERE ts >= now() - INTERVAL '5 minutes'";
    let query = |keys: &str| {
        format!(
            "SELECT {keys}, approx_percentile_cont(latency, 0.99) AS value FROM http {window} \
             GROUP BY {keys}"
        )
    };
    let roots = sql_roots(
        &[
            &query("region, service"),
            &query("region"),
            &query("service"),
        ],
        &accuracy,
    )
    .await;
    let entries = vec![entry(&accuracy, 60_000); 3];
    let memo = expanded_memo(roots, &entries);
    let (selected, separate, rolled_up) = select_rollup(&memo, 3, &entries, &data());
    assert!(rolled_up < separate, "{rolled_up} vs {separate}");
    assert_eq!(builds_and_rollups(&selected), (1, 2));
}

/// #840's `quantile by (host, job)` and `quantile by (job)` over 10,000
/// series, every 10 s at rank ε = 0.01, δ = 0.001: the memo offers the
/// coarse KLL rolled up from the finer one, and its guarantee meets the
/// target, so plan selection selects it.
#[test]
fn promql_quantile_by_job_rolls_up_from_by_host_job() {
    let accuracy = AccuracyTarget::max_error(0.01)
        .failure_probability(0.001)
        .accuracy_metric(AccuracyMetric::Rank);
    let roots: Vec<(usize, QueryRoot)> = [
        "quantile by (host, job) (0.5, data)",
        "quantile by (job) (0.5, data)",
    ]
    .iter()
    .enumerate()
    .map(|(i, query)| {
        let root = lower_promql(query, accuracy.clone()).unwrap();
        (
            i,
            QueryRoot::Operator(promql_rows::with_series_identity(&root).unwrap()),
        )
    })
    .collect();
    let data: DataWorkload = serde_json::from_value(serde_json::json!({
        "arrival": {"ingesting": {"rows_per_second": 666.67}},
        "sources": {"data": {"series_count": 10_000, "metric_type": "gauge",
                             "sample_interval": 15_000,
                             // 15 minutes of raw rows.
                             "rows_held": 600_000}},
    }))
    .unwrap();
    let entry = entry(&accuracy, 10_000);
    let entries = [entry.clone(), entry];
    let memo = expanded_memo(roots, &entries);
    let (selected, separate, rolled_up) = select_rollup(&memo, 2, &entries, &data);
    assert!(rolled_up < separate, "{rolled_up} vs {separate}");
    assert_eq!(builds_and_rollups(&selected), (1, 1));
}
