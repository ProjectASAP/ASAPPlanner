//! Controlled plans for the viewer's actual distinct-count rollup workload.
use asap_cost::{CostInputs, CostModel, ResourceUnitCost};

use asap_planner::PlanningModels;

use asap_types::{
    ir::{
        schema::{DataType, Field, FieldDataType, Schema},
        ASAPOp, OperatorNode, QueryRoot,
    },
    summary_type::SketchAlgorithm,
    workload::{DurationMs, PlanningWorkload},
};
use std::rc::Rc;

fn fixture() -> (PlanningWorkload, Vec<QueryRoot>) {
    let workload: PlanningWorkload = serde_json::from_str(include_str!(
        "../../../tools/dag-viewer/memo-examples/sql/rollupheavy-t11-service-endpoint-5m.json"
    ))
    .unwrap();
    let mut schema = Schema::new(vec![
        Field::plain("ts", DataType::Timestamp, false),
        Field::plain("region", DataType::Utf8, false),
        Field::plain("service", DataType::Utf8, false),
        Field::plain("endpoint", DataType::Utf8, false),
        Field::plain("status", DataType::Utf8, false),
        Field::plain("user_id", DataType::Float64, false),
        Field::plain("latency", DataType::Float64, false),
    ]);
    schema.time_index = Some(0);
    let catalog = asap_frontend_sql::SqlCatalog::new().with_table("http", schema);
    let roots = asap_devtools::planning_stages::lower_sql_roots(&workload, &catalog).unwrap();
    (workload, roots)
}
fn nodes(roots: &[QueryRoot]) -> Vec<Rc<OperatorNode>> {
    roots
        .iter()
        .flat_map(|r| match r {
            QueryRoot::Operator(n) => OperatorNode::reachable(n),
            _ => panic!("operator root"),
        })
        .collect()
}

/// The viewer's distinct-count rollup workload, `COUNT(DISTINCT user_id)`
/// by service and by (service, endpoint) over 5 min every 60 s, with each
/// of its 10,000 series sampled every `sample_interval_ms`: at 5 ms,
/// 2,000,000 rows per second and 600,000,000 rows per evaluation, 10,000
/// groups for each grouping and 1,000,000 users.
/// Returns the sketches each query's accepted plan reads and how many KMV
/// builds the selected plan holds.
fn plan_rollup(sample_interval_ms: u64) -> (Vec<Vec<String>>, usize) {
    let (mut workload, roots) = fixture();
    let data = workload.data_workload.as_mut().unwrap();
    let http = data.sources.get_mut("http").unwrap();
    http.sample_interval = Some(DurationMs(sample_interval_ms));
    let rows_per_second = http.series_count.unwrap() as f64 * 1_000.0 / sample_interval_ms as f64;
    data.arrival = serde_json::from_value(serde_json::json!({
        "ingesting": {"rows_per_second": rows_per_second}
    }))
    .unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.as_ref().unwrap();
    let mut settings = CostInputs::default();
    settings.cost_model = CostModel::UsageOverTime(ResourceUnitCost::aws_fargate_us_east_1(
        settings.resource_speed,
    ));
    let run = asap_planner::run_planning(
        roots,
        &entries,
        data,
        PlanningModels::builtin().with_cost_inputs(&settings),
        asap_planner::PlanSearchSettings::default(),
    )
    .unwrap();
    let (plan, selection) = run
        .selection
        .as_ref()
        .expect("validated plan within budget");
    let sketches = selection
        .accepted_accuracy_per_query
        .iter()
        .map(|accuracy| accuracy.sketches.clone())
        .collect();
    let roots = plan.query_roots().unwrap();
    let kmv_builds = nodes(&roots)
        .iter()
        .filter(|node| {
            matches!(
                node.asap(),
                Some(ASAPOp::SummaryAgg {
                    summary_type: FieldDataType::Sketch(SketchAlgorithm::Kmv),
                    ..
                })
            )
        })
        .count();
    (sketches, kmv_builds)
}

/// The better plan for the rollup depends on the rows each KMV state folds.
/// Measured on the executor, both plans over the same data of 1,000 groups
/// by (service, endpoint), scaled to one evaluation:
/// - 60,000,000 rows (60,000 rows per state, ×10): exact 609.6 s, KMV
///   (k = 4,323) 483.6 s. KMV is 1.26x faster.
/// - 12,000,000 rows (12,000 rows per state, ×10): exact 127.9 s, KMV
///   178.4 s. Exact is 1.39x faster.
///
/// At 2,000,000 rows per second (a sample every 5 ms) the planned shape (600,000,000 rows over
/// 10,000 groups) has 60,000 rows per state, as the 60,000,000-row run
/// does, so the planner builds a KMV.
#[test]
fn production_distinct_rollup_builds_kmv_at_60k_rows_per_state() {
    let (sketches, kmv_builds) = plan_rollup(5);
    assert_eq!(sketches.len(), 2);
    assert!(
        sketches.iter().all(|sketches| sketches == &["Kmv"]),
        "both queries read a KMV: {sketches:?}"
    );
    assert!(kmv_builds > 0, "the selected plan builds a KMV");
}

/// At 400,000 rows per second (a sample every 25 ms) the planned shape (120,000,000 rows over
/// 10,000 groups) has 12,000 rows per state, as the 12,000,000-row run
/// above does, where exact measured 1.39x faster than KMV: the planner
/// counts both queries exactly.
#[test]
fn production_distinct_rollup_counts_exactly_at_12k_rows_per_state() {
    let (sketches, kmv_builds) = plan_rollup(25);
    assert_eq!(sketches.len(), 2);
    assert!(
        sketches.iter().all(|sketches| sketches.is_empty()),
        "both queries count exactly: {sketches:?}"
    );
    assert_eq!(kmv_builds, 0, "the selected plan builds no KMV");
}
