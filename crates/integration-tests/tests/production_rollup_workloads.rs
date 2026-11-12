//! Sketch rollup in planning: Pass 2 offers answering a coarse sketch
//! grouping from the finest build of the same sketch over the same rows,
//! and Stage 3 selects it when one build and a merge cost less than two
//! builds over the raw rows.
use std::rc::Rc;

use asap_cost::{CostInputs, CostModel, ResourceUnitCost};
use asap_executor::physical_planner::promql_rows;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_integration_tests::fixtures::lower_promql;
use asap_planner::{run_planning, PlanSearchSettings, PlanningModels, PlanningRun};
use asap_types::accuracy::{AccuracyMetric, AccuracyScope, AccuracyTarget};
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};
use asap_types::workload::{
    DataWorkload, Predictability, QueryRecurrence, QueryRequirements, QueryWorkloadEntry,
    RepeatedDemand, RepetitionInterval, TimestampMs,
};
use std::collections::HashSet;

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
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
            interval: RepetitionInterval(interval_ms),
            evaluation_phase: TimestampMs(0),
        }),
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
            "column_signs": {"value": "non_negative"},
            "max_distinct_per_evaluation": 1_000_000,
            "rows_held": 120_000_000,
        }},
    }))
    .unwrap()
}

fn production(
    roots: Vec<QueryRoot>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
) -> PlanningRun {
    let mut settings = CostInputs::default();
    settings.cost_model = CostModel::UsageOverTime(ResourceUnitCost::aws_fargate_us_east_1(
        settings.resource_speed,
    ));
    run_planning(
        roots,
        entries,
        data,
        PlanningModels::builtin().with_cost_inputs(&settings),
        PlanSearchSettings::default(),
    )
    .unwrap()
}

fn check(run: &PlanningRun, queries: usize, expected_rollups: usize, family: &str) {
    let (plan, selection) = run
        .selection
        .as_ref()
        .expect("validated production plan within 30s");
    let accepted = &selection.accepted_accuracy_per_query;
    assert_eq!(accepted.len(), queries);
    assert!(
        accepted
            .iter()
            .all(|a| a.sketches.iter().any(|s| s == family)),
        "{accepted:?}"
    );
    let mut seen = HashSet::new();
    let mut builds = 0;
    let mut rollups = 0;
    for root in &plan.query_roots().unwrap() {
        let QueryRoot::Operator(root) = root else {
            panic!("operator output")
        };
        for node in OperatorNode::reachable(root) {
            if !seen.insert(Rc::as_ptr(&node)) {
                continue;
            }
            match node.asap() {
                Some(ASAPOp::SummaryAgg { .. } | ASAPOp::SummaryTumblingWindowChain { .. }) => {
                    builds += 1
                }
                Some(ASAPOp::SummaryMerge {
                    group_by: Some(_), ..
                }) => rollups += 1,
                _ => {}
            }
        }
    }
    assert_eq!(
        (builds, rollups),
        (1, expected_rollups),
        "selected plan must share the finest grouping"
    );
}

async fn sql(queries: &[String], accuracy: AccuracyTarget) -> PlanningRun {
    let mut roots = Vec::new();
    for query in queries {
        roots.push(QueryRoot::Operator(
            lower_sql(query, &catalog()).await.unwrap(),
        ));
    }
    production(
        roots,
        &vec![entry(&accuracy, 60_000); queries.len()],
        &data(),
    )
}

#[tokio::test]
async fn production_three_sql_groupings_share_kll() {
    let accuracy = AccuracyTarget::max_error(0.05)
        .failure_probability(0.05)
        .accuracy_metric(AccuracyMetric::Rank)
        .accuracy_scope(AccuracyScope::PerValue);
    let queries: Vec<_> = ["region, service", "region", "service"].iter().map(|keys| format!("SELECT {keys}, approx_percentile_cont(latency, 0.99) AS value FROM http WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY {keys}")).collect();
    check(&sql(&queries, accuracy).await, 3, 2, "Kll");
}

#[tokio::test]
async fn production_sql_distinct_region_service_rollup() {
    let accuracy = AccuracyTarget::max_error(0.05)
        .failure_probability(0.05)
        .accuracy_metric(AccuracyMetric::Relative)
        .accuracy_scope(AccuracyScope::PerValue);
    let queries: Vec<_> = ["region, service", "service"].iter().map(|keys| format!("SELECT {keys}, COUNT(DISTINCT user_id) AS value FROM http WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY {keys}")).collect();
    check(&sql(&queries, accuracy).await, 2, 1, "Kmv");
}

// Exercise multi-input operators through the production route under more than
// three heterogeneous query schedules, not just the local pricing kernel.
#[test]
fn production_binary_queries_use_joint_prices_with_mixed_schedules() {
    use asap_types::workload::{DemandEstimate, Rate};
    let accuracy = AccuracyTarget::exact();
    let queries = ["m + n", "m + m", "m or n", "m * n", "m - n"];
    let roots = queries
        .iter()
        .map(|query| {
            let root = lower_promql(query, accuracy.clone()).unwrap();
            QueryRoot::Operator(promql_rows::with_series_identity(&root).unwrap())
        })
        .collect();
    let mut entries: Vec<_> = [59_000, 61_000, 67_000, 71_000, 73_000]
        .into_iter()
        .map(|interval| entry(&accuracy, interval))
        .collect();
    entries[3].recurrence =
        QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(DemandEstimate {
            expected_rate: Rate(0.137),
            observed_at: None,
            valid_for: None,
        }));
    entries[4].recurrence = QueryRecurrence::Unknown;
    let data: DataWorkload = serde_json::from_value(serde_json::json!({
        "arrival":{"ingesting":{"rows_per_second":16.0}},
        "sources":{"m":{"rows_held":80,"series_count":8,"sample_interval":1000},
                   "n":{"rows_held":80,"series_count":8,"sample_interval":1000}}
    }))
    .unwrap();
    let run = production(roots, &entries, &data);
    let (_, selection) = run.selection.as_ref().expect("validated binary workload");
    assert_eq!(selection.accepted_accuracy_per_query.len(), queries.len());
}
