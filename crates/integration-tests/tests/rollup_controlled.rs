//! Controlled plans for the viewer's actual distinct-count rollup workload.
use asap_cost::{CostInputs, CostModel, ResourceUnitCost};

use asap_planner::PlanningModels;

use asap_types::{
    ir::{
        schema::{DataType, Field, Schema},
        ASAPOp, OperatorNode, QueryRoot,
    },
    workload::PlanningWorkload,
};
use std::{collections::HashSet, rc::Rc};

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

// The production entrypoint must find a sketch rollup under the viewer budget.
#[test]
fn production_distinct_rollup_finds_shared_kmv() {
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
    assert!(accepted
        .iter()
        .all(|accuracy| accuracy.sketches.iter().any(|sketch| sketch == "Kmv")));
    assert!(
        asap_integration_tests::planning::plan_cost_per_second(plan, &entries, data, &settings)
            < 2.0e-6
    );
    let roots = plan.query_roots().unwrap();
    let builds: HashSet<_> = nodes(&roots)
        .iter()
        .filter(|node| matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. })))
        .map(Rc::as_ptr)
        .collect();
    assert_eq!(builds.len(), 1, "both queries reuse a single state build");
    let rollup = nodes(&roots)
        .iter()
        .any(|n| matches!(n.asap(), Some(ASAPOp::SummaryMerge { .. })));
    assert!(
        rollup,
        "production search must select the cheaper legal rollup"
    );
}
