//! Controlled plans for the viewer's actual distinct-count rollup workload.
use asap_cost::{CostInputs, CostModel, ResourceUnitCost};

use asap_planner::PlanningModels;

use asap_types::{
    ir::{
        schema::{DataType, Field, FieldDataType, Schema},
        ASAPOp, OperatorNode, QueryRoot,
    },
    summary_type::SketchAlgorithm,
    workload::PlanningWorkload,
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
/// by service and by (service, endpoint) over 5 min every 60 s, at 2,000,000
/// rows per second: 600,000,000 rows per evaluation, 10,000 groups for each
/// grouping and 1,000,000 users. The production entrypoint answers both
/// queries from a KMV (k = 4,323), the plan that measures faster at the
/// largest shape measured.
///
/// The better plan depends on the rows per state. Measured on the executor,
/// both plans over the same data:
/// - 60,000,000 rows (60,000 rows per state, scaled ×10 to one evaluation):
///   exact 609.6 s, KMV 483.6 s. KMV is 1.26x faster.
/// - 12,000,000 rows (12,000 rows per state): exact 12.79 s, KMV 17.84 s.
///   Exact is 1.39x faster.
///
/// So the crossover lies between 12,000 and 60,000 rows per state. The
/// planned shape (600,000,000 rows over 10,000 groups) has 60,000 rows per
/// state, as the 60,000,000-row run does, so the test expects KMV.
#[test]
fn production_distinct_rollup_builds_kmv_at_60k_rows_per_state() {
    let (workload, roots) = fixture();
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
    let accepted = &selection.accepted_accuracy_per_query;
    assert_eq!(accepted.len(), 2);
    assert!(
        accepted.iter().all(|accuracy| accuracy.sketches == ["Kmv"]),
        "both queries read a KMV: {accepted:?}"
    );
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
    assert!(kmv_builds > 0, "the selected plan builds a KMV");
}
