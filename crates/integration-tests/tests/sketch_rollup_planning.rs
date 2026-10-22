//! Sketch rollup in planning: Pass 2 offers answering a coarse sketch
//! grouping from the finest build of the same sketch over the same rows,
//! and Stage 3 selects it when one build and a merge cost less than two
//! builds over the raw rows.
use std::rc::Rc;

use asap_executor::physical_planner::promql_rows;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_integration_tests::fixtures::lower_promql;
use asap_plan_selection::{run_stage1_to_stage3_with, PlanningModels, Stage1ToStage3Run};
use asap_types::ir::properties::{AccuracyMetric, AccuracyScope, AccuracyTarget};
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};
use asap_types::workload::{
    DataWorkload, Predictability, QueryRecurrence, QueryRequirements, QueryWorkloadEntry,
    RepeatedDemand, RepetitionInterval,
};

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
/// a million distinct users at most per evaluation.
fn data() -> DataWorkload {
    serde_json::from_value(serde_json::json!({
        "arrival": {"ingesting": {"rows_per_second": 2_000_000.0}},
        "sources": {"http": {
            "series_count": 10_000,
            "sample_interval": 5,
            "values_non_negative": true,
            "max_distinct_per_evaluation": 1_000_000,
        }},
    }))
    .unwrap()
}

/// Every stage's output for the SQL `queries` at `accuracy`.
async fn plan_sql(queries: &[&str], accuracy: AccuracyTarget) -> Stage1ToStage3Run<usize> {
    let mut roots = Vec::new();
    for (i, query) in queries.iter().enumerate() {
        let root = lower_sql(query, &catalog(), accuracy.clone())
            .await
            .unwrap();
        roots.push((i, QueryRoot::Operator(root)));
    }
    run_stage1_to_stage3_with(
        roots,
        &vec![entry(&accuracy, 60_000); queries.len()],
        &data(),
        PlanningModels::builtin(),
        asap_plan_selection::Stage1ToStage3Search::exhaustive(),
    )
    .unwrap()
}

/// The logical candidate `id`'s roots.
fn logical_roots(run: &Stage1ToStage3Run<usize>, id: &str) -> Vec<Rc<OperatorNode>> {
    run.stage1_logical_asap_optimization
        .iter()
        .find(|c| c.id == id)
        .unwrap()
        .roots
        .as_ref()
        .unwrap()
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => Rc::clone(node),
            QueryRoot::Scalar(_) => panic!("operator root"),
        })
        .collect()
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
            Some(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. }) => builds += 1,
            Some(ASAPOp::SummaryMerge {
                group_by: Some(_), ..
            }) => rollups += 1,
            _ => {}
        }
    }
    (builds, rollups)
}

/// The cheapest valid candidate's cost per second among the Stage 2
/// candidates whose logical candidate does (`rolled_up`) or does not roll
/// up a sketch.
fn cheapest(run: &Stage1ToStage3Run<usize>, rolled_up: bool) -> Option<f64> {
    let selection = run.stage3_plan_selection.as_ref().unwrap();
    run.stage2_physical_asap_optimization
        .iter()
        .filter(|c| {
            let (_, rollups) = builds_and_rollups(&logical_roots(run, &c.from_logical));
            (rollups > 0) == rolled_up
        })
        .filter_map(|c| selection.costs.get(&c.id)?.cost_per_second)
        .min_by(f64::total_cmp)
}

/// Distinct users by (region, service) and by service, both at relative
/// ε = 0.05 per value over the same window (W-RollupHeavy's targets): Pass 2 offers the coarse KMV rolled up
/// from the finer one, and Stage 3 selects a plan with one build and a
/// rollup, cheaper than any plan that builds both from the raw rows.
#[tokio::test]
async fn stage3_selects_a_distinct_count_rollup_when_it_is_cheaper() {
    let accuracy = AccuracyTarget::max_error(0.05)
        .failure_probability(0.05)
        .accuracy_metric(AccuracyMetric::Relative)
        .accuracy_scope(AccuracyScope::PerValue);
    let window = "WHERE ts >= now() - INTERVAL '5 minutes'";
    let run = plan_sql(
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
        accuracy,
    )
    .await;
    let rolled_up = cheapest(&run, true).expect("a rollup candidate is priced");
    let separate = cheapest(&run, false).expect("separate builds are priced");
    assert!(rolled_up < separate, "{rolled_up} vs {separate}");
    let selected = run.selected().unwrap();
    assert_eq!(
        builds_and_rollups(&logical_roots(&run, &selected.from_logical)),
        (1, 1),
        "{}",
        selected.from_logical
    );
}

/// p99 latency by (region, service), by region and by service at rank
/// ε = 0.05 (template 13's shape): both coarse groupings roll up from one
/// KLL, in one Pass 2 candidate, and Stage 3 selects it.
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
    let run = plan_sql(
        &[
            &query("region, service"),
            &query("region"),
            &query("service"),
        ],
        accuracy,
    )
    .await;
    let selected = run.selected().unwrap();
    assert_eq!(
        builds_and_rollups(&logical_roots(&run, &selected.from_logical)),
        (1, 2),
        "{}",
        selected.from_logical
    );
}

/// #840's `quantile by (host, job)` and `quantile by (job)` over 10,000
/// series, every 10 s at rank ε = 0.01, δ = 0.001: Pass 2 offers the
/// coarse KLL rolled up from the finer one, and its guarantee meets the
/// target, so Stage 3 prices it.
#[test]
fn promql_quantile_by_job_rolls_up_from_by_host_job() {
    let accuracy = AccuracyTarget::max_error(0.01)
        .failure_probability(0.001)
        .accuracy_metric(AccuracyMetric::Rank);
    let roots = [
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
                             "sample_interval": 15_000}},
    }))
    .unwrap();
    let entry = entry(&accuracy, 10_000);
    let run = run_stage1_to_stage3_with(
        roots,
        &[entry.clone(), entry],
        &data,
        PlanningModels::builtin(),
        asap_plan_selection::Stage1ToStage3Search::exhaustive(),
    )
    .unwrap();
    let rolled: Vec<_> = run
        .stage1_logical_asap_optimization
        .iter()
        .filter(|c| {
            c.pass2
                .as_ref()
                .is_some_and(|p| !p.rollup_candidates.is_empty())
        })
        .collect();
    assert!(!rolled.is_empty(), "a rollup Pass 2 candidate");
    let selection = run.stage3_plan_selection.as_ref().unwrap();
    assert!(
        run.stage2_physical_asap_optimization
            .iter()
            .filter(|c| rolled.iter().any(|l| l.id == c.from_logical))
            .any(|c| selection.costs.contains_key(&c.id)),
        "a rollup plan is valid and priced"
    );
}
