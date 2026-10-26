//! Sketch rollup in planning: Pass 2 offers answering a coarse sketch
//! grouping from the finest build of the same sketch over the same rows,
//! and Stage 3 selects it when one build and a merge cost less than two
//! builds over the raw rows.
use std::rc::Rc;

use asap_executor::physical_planner::promql_rows;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_integration_tests::fixtures::lower_promql;
use asap_plan_selection::{
    CostModel, PlanSelectionSettings, PlanningModels, ResourceUnitCost, Stage1ToStage3Run,
};
use asap_types::ir::properties::{AccuracyMetric, AccuracyScope, AccuracyTarget};
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};
use asap_types::workload::{
    DataWorkload, Predictability, QueryRecurrence, QueryRequirements, QueryWorkloadEntry,
    RepeatedDemand, RepetitionInterval,
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

fn production(
    roots: Vec<(usize, QueryRoot)>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
) -> Stage1ToStage3Run<usize> {
    let mut settings = PlanSelectionSettings::default();
    settings.cost_model = CostModel::UsageOverTime(ResourceUnitCost::aws_fargate_us_east_1(
        settings.resource_speed,
    ));
    asap_plan_selection::run_stage1_to_stage3_with(
        roots,
        entries,
        data,
        PlanningModels::builtin().with_plan_selection(&settings),
        asap_plan_selection::Stage1ToStage3Search {
            milp_time_limit_ms: Some(30_000),
            ..Default::default()
        },
    )
    .unwrap()
}

fn check(
    run: &Stage1ToStage3Run<usize>,
    queries: usize,
    expected_rollups: usize,
    family: &str,
    separate_cost: Option<f64>,
) {
    let selection = run
        .stage3_plan_selection
        .as_ref()
        .expect("validated production plan within 30s");
    let report = selection.search.candidate_connections.as_ref().unwrap();
    println!(
        "selected cost={:?}; report={report:?}",
        selection.costs[&selection.selected].cost_per_second
    );
    assert!(
        report.joint_price_solve_calls > 0,
        "joint supplier prices were not solved: {report:?}"
    );
    assert!(
        report.direct_cost_validated_plans > 0,
        "no directly priced validated plan: {report:?}"
    );
    assert!(report
        .direct_cost_best_validated_cost
        .is_some_and(|cost| cost.is_finite() && cost >= 0.0));
    let selected_cost = selection.costs[&selection.selected]
        .cost_per_second
        .unwrap();
    if let Some(separate) = separate_cost {
        // Whole-result direct configurations are conservatively sized; full
        // refinement may find a slightly smaller valid state. Both searches
        // must improve on independently built KLLs at the same accuracy target.
        assert!(report.direct_cost_best_validated_cost.unwrap() < separate);
        assert!(selected_cost < separate);
    } else {
        assert!(report.direct_cost_best_validated_cost.unwrap() <= selected_cost * (1.0 + 1e-9));
    }
    let accepted = &selection.accepted[&selection.selected];
    assert_eq!(accepted.len(), queries);
    assert!(accepted.iter().all(|a| !a.accepted_regardless_of_guarantee));
    if separate_cost.is_some() && accepted.iter().all(|a| a.sketches.is_empty()) {
        assert!(selected_cost <= report.direct_cost_best_validated_cost.unwrap() * (1.0 + 1e-9));
        println!("exact plan costs less than both the direct incumbent and independent KLLs");
        return;
    }
    assert!(
        accepted
            .iter()
            .all(|a| !a.accepted_regardless_of_guarantee && a.sketches.iter().any(|s| s == family)),
        "{accepted:?}"
    );
    let selected = run.selected().unwrap();
    let mut seen = HashSet::new();
    let mut builds = 0;
    let mut rollups = 0;
    for root in &selected.roots {
        let QueryRoot::Operator(root) = root else {
            panic!("operator output")
        };
        for node in OperatorNode::reachable(root) {
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
    }
    assert_eq!(
        (builds, rollups),
        (1, expected_rollups),
        "selected plan must share the finest grouping"
    );
}

async fn sql(queries: &[String], accuracy: AccuracyTarget) -> Stage1ToStage3Run<usize> {
    let mut roots = Vec::new();
    for (id, query) in queries.iter().enumerate() {
        roots.push((
            id,
            QueryRoot::Operator(
                lower_sql(query, &catalog(), accuracy.clone())
                    .await
                    .unwrap(),
            ),
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
    check(&sql(&queries, accuracy).await, 3, 2, "Kll", None);
}

#[tokio::test]
async fn production_sql_distinct_region_service_rollup() {
    let accuracy = AccuracyTarget::max_error(0.05)
        .failure_probability(0.05)
        .accuracy_metric(AccuracyMetric::Relative)
        .accuracy_scope(AccuracyScope::PerValue);
    let queries: Vec<_> = ["region, service", "service"].iter().map(|keys| format!("SELECT {keys}, COUNT(DISTINCT user_id) AS value FROM http WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY {keys}")).collect();
    check(&sql(&queries, accuracy).await, 2, 1, "Kmv", None);
}

#[test]
// The earlier controlled fixture omitted raw retention. This production
// workload explicitly holds one current sample for each of its 10,000 series.
fn production_promql_compares_rollup_with_exact() {
    let accuracy = AccuracyTarget::max_error(0.01)
        .failure_probability(0.001)
        .accuracy_metric(AccuracyMetric::Rank);
    let roots: Vec<_> = [
        "quantile by (host, job) (0.5, data)",
        "quantile by (job) (0.5, data)",
    ]
    .iter()
    .enumerate()
    .map(|(id, query)| {
        let root = lower_promql(query, accuracy.clone()).unwrap();
        (
            id,
            QueryRoot::Operator(promql_rows::with_series_identity(&root).unwrap()),
        )
    })
    .collect();
    let data: DataWorkload = serde_json::from_value(serde_json::json!({"arrival":{"ingesting":{"rows_per_second":666.67}},"sources":{"data":{"series_count":10000,"metric_type":"gauge","sample_interval":15000,"rows_held":10000}}})).unwrap();
    let entries = [entry(&accuracy, 10000), entry(&accuracy, 10000)];
    let separate = independent_kll_cost(roots.clone(), &entries, &data);
    println!("independent KLL cost={separate}");
    check(
        &production(roots, &entries, &data),
        2,
        1,
        "Kll",
        Some(separate),
    );
}

// Price the same workload with independently built, unmaterialized KLL states.
fn independent_kll_cost(
    roots: Vec<(usize, QueryRoot)>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
) -> f64 {
    use asap_logical_asap_optimization::per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates, ComputationCandidate,
    };
    let inventory = generate_per_computation_candidates(roots).unwrap();
    let choices: Vec<_> = inventory
        .targets
        .iter()
        .map(|target| {
            target
                .choices
                .candidates
                .iter()
                .position(|c| {
                    matches!(
                        c,
                        ComputationCandidate::Summary(asap_types::ir::schema::SketchAlgorithm::Kll)
                    )
                })
                .unwrap()
        })
        .collect();
    let roots: Vec<_> = compose_logical_candidate(&inventory, &choices)
        .unwrap()
        .into_iter()
        .map(|(_, root)| root)
        .collect();
    let mut settings = PlanSelectionSettings::default();
    settings.cost_model = CostModel::UsageOverTime(ResourceUnitCost::aws_fargate_us_east_1(
        settings.resource_speed,
    ));
    let models = PlanningModels::builtin().with_plan_selection(&settings);
    let plan = asap_physical_asap_optimization::optimize_physical_asap_chosen(
        "independent",
        &roots,
        entries,
        data,
        models.deployment_capabilities,
        &mut |_, _, _| asap_types::physical::Materialization::NotMaterialized,
    )
    .unwrap()
    .unwrap();
    let result = asap_plan_selection::select_plan(&[plan], entries, data, models).unwrap();
    result.costs[&result.selected].cost_per_second.unwrap()
}
