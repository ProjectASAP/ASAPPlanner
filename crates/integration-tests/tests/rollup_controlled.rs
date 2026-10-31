//! Controlled plans for the viewer's actual distinct-count rollup workload.
use asap_cost::{CostInputs, CostModel, ResourceUnitCost};
use asap_integration_tests::planning::{builds_and_rollups, expanded_memo, select_rollup};
use asap_logical_asap_optimization::{
    test_support::{
        compose_logical_candidate, generate_per_computation_candidates, rollup_candidates,
        rollup_pass2_candidates,
    },
    HowToComputeAggregateMeasure,
};
use asap_planner::PlanningModels;
use asap_types::{
    ir::{
        schema::{DataType, Field, Schema, SketchAlgorithm},
        ASAPOp, OperatorNode, QueryRoot,
    },
    physical::{Materialization, Medium},
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

#[test]
fn controlled_distinct_rollup_prices_the_original_workload() {
    let (workload, roots) = fixture();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.as_ref().unwrap();
    let generated = generate_per_computation_candidates(
        &asap_logical_asap_optimization::DagOfLogicalMemoGroups::from_roots(&roots),
    )
    .unwrap();
    let mut settings = CostInputs::default();
    settings.cost_model = CostModel::UsageOverTime(ResourceUnitCost::aws_fargate_us_east_1(
        settings.resource_speed,
    ));
    let models = PlanningModels::builtin().with_cost_inputs(&settings);
    let mut accepted = Vec::new();
    for algorithm in [None, Some(SketchAlgorithm::Hll), Some(SketchAlgorithm::Kmv)] {
        let choices: Vec<_> = generated
            .targets
            .iter()
            .map(|target| match &algorithm {
                None => 0,
                Some(a) => target
                    .ways_to_compute
                    .iter()
                    .position(
                        |c| matches!(c,HowToComputeAggregateMeasure::Summary(found) if found==a),
                    )
                    .expect("family generated"),
            })
            .collect();
        let roots: Vec<_> = compose_logical_candidate(&generated, &choices).unwrap();
        // Identical sub-DAGs made one node, as the memo's groups are.
        let roots: Vec<_> = asap_logical_asap_optimization::test_support::share_common_sub_dags(
            roots
                .iter()
                .enumerate()
                .map(|(id, root)| match root {
                    QueryRoot::Operator(node) => (id, Rc::clone(node)),
                    QueryRoot::Scalar(_) => panic!("operator root"),
                })
                .collect(),
        )
        .into_iter()
        .map(|(_, node)| QueryRoot::Operator(node))
        .collect();
        let mut variants = vec![("separate", roots.clone())];
        if algorithm.is_some() {
            let rules = rollup_candidates(&roots);
            assert_eq!(rules.len(), 1, "one service rollup for {algorithm:?}");
            for rolled in rollup_pass2_candidates(&roots, &rules).unwrap() {
                variants.push(("rollup", rolled.roots.unwrap()));
            }
        }
        for (kind, roots) in variants {
            let family = algorithm
                .as_ref()
                .map_or("exact".to_string(), |a| format!("{a:?}"));
            let id = format!("{family}-{kind}");
            let builds: HashSet<_> = nodes(&roots)
                .iter()
                .filter(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
                .map(Rc::as_ptr)
                .collect();
            let mut plan =
                asap_physical_asap_optimization::test_support::optimize_physical_asap_chosen(
                    &id,
                    &roots,
                    &entries,
                    data,
                    models.deployment_capabilities.query_time_results_retained,
                    &mut |node, _, options| {
                        let mat = Materialization::MatAtQuery(Medium::Memory);
                        if builds.contains(&node) && options.contains(&mat) {
                            mat
                        } else {
                            Materialization::NotMaterialized
                        }
                    },
                )
                .unwrap()
                .expect("fixed query-time implementation");
            plan.id = id.clone();
            let result = asap_devtools::memo_pricing::reference_select(
                &[plan.clone()],
                &entries,
                data,
                models,
            );
            match result {
                Ok(selection) => {
                    println!(
                        "{id}: builds={} cost={:?} accepted={:?}",
                        builds.len(),
                        selection.costs[&id].cost_per_second,
                        selection.accepted
                    );
                    accepted.push(plan);
                }
                Err(error) => println!("{id}: REJECTED {error}"),
            }
            for n in nodes(&roots) {
                if let Some(ASAPOp::SummaryAgg { family, .. }) = n.asap() {
                    println!("{id} family={family:?}");
                }
            }
        }
    }
    let chosen =
        asap_devtools::memo_pricing::reference_select(&accepted, &entries, data, models).unwrap();
    println!("CONTROLLED WINNER {}", chosen.selected);
    assert_eq!(
        chosen.selected, "Kmv-rollup",
        "shared KMV must win the controlled cost comparison"
    );
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
        asap_planner::PlanSearchSettings {
            time_limit_ms: Some(30_000),
            inspect: false,
        },
    )
    .unwrap();
    let selection = run
        .selection
        .as_ref()
        .expect("validated plan within budget");
    println!(
        "production cost={:?} report={:?}",
        selection.cost.cost_per_second, selection.search_report
    );
    let accepted = &selection.accepted_accuracy_per_query;
    assert_eq!(accepted.len(), 2);
    assert!(accepted
        .iter()
        .all(|accuracy| !accuracy.accepted_regardless_of_guarantee
            && accuracy.sketches.iter().any(|sketch| sketch == "Kmv")));
    assert!(selection.cost.cost_per_second.unwrap() < 2.0e-6);
    let plan = &run.selected_physical_plan.as_ref().unwrap().plan;
    let builds: HashSet<_> = nodes(&plan.roots)
        .iter()
        .filter(|node| matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. })))
        .map(Rc::as_ptr)
        .collect();
    assert_eq!(builds.len(), 1, "both queries reuse a single state build");
    let rollup = nodes(&plan.roots)
        .iter()
        .any(|n| matches!(n.asap(), Some(ASAPOp::SummaryMerge { .. })));
    assert!(
        rollup,
        "production search must select the cheaper legal rollup"
    );
}

// The production shape without a timed search: among fixed plans of the
// memo (a KMV per query, or both reading one KMV state group), plan
// selection with the built-in models picks one build and one rollup, and
// it costs less than a valid build per query.
#[test]
fn memo_offers_one_kmv_build_and_a_rollup() {
    let (workload, roots) = fixture();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.as_ref().unwrap();
    let memo = expanded_memo(roots, &entries);
    let (selected, separate, shared) = select_rollup(&memo, 2, &entries, data);
    assert!(shared < separate, "{shared} vs {separate}");
    assert_eq!(builds_and_rollups(&selected), (1, 1));
}
