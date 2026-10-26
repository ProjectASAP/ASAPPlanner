//! Controlled plans for the viewer's actual distinct-count rollup workload.
use asap_logical_asap_optimization::{
    asap_aware_cse::{
        rollup_candidates, rollup_pass2_candidates, sharing_candidates, with_shared_candidates,
    },
    per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates, ComputationCandidate,
    },
};
use asap_plan_selection::{CostModel, PlanSelectionSettings, PlanningModels, ResourceUnitCost};
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
    let generated =
        generate_per_computation_candidates(roots.into_iter().enumerate().collect()).unwrap();
    let mut settings = PlanSelectionSettings::default();
    settings.cost_model = CostModel::UsageOverTime(ResourceUnitCost::aws_fargate_us_east_1(
        settings.resource_speed,
    ));
    let models = PlanningModels::builtin().with_plan_selection(&settings);
    let mut accepted = Vec::new();
    for algorithm in [None, Some(SketchAlgorithm::Hll), Some(SketchAlgorithm::Kmv)] {
        let choices: Vec<_> = generated
            .targets
            .iter()
            .map(|target| match &algorithm {
                None => 0,
                Some(a) => target
                    .choices
                    .candidates
                    .iter()
                    .position(|c| matches!(c,ComputationCandidate::Summary(found) if found==a))
                    .expect("family generated"),
            })
            .collect();
        let roots: Vec<_> = compose_logical_candidate(&generated, &choices)
            .unwrap()
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        let sharing = sharing_candidates(&roots);
        let roots = with_shared_candidates(&roots, &sharing, &vec![true; sharing.len()]);
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
            let mut plan = asap_physical_asap_optimization::optimize_physical_asap_chosen(
                &id,
                &roots,
                &entries,
                data,
                models.deployment_capabilities,
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
            let result = asap_plan_selection::select_plan(&[plan.clone()], &entries, data, models);
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
    let chosen = asap_plan_selection::select_plan(&accepted, &entries, data, models).unwrap();
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
    let mut settings = PlanSelectionSettings::default();
    settings.cost_model = CostModel::UsageOverTime(ResourceUnitCost::aws_fargate_us_east_1(
        settings.resource_speed,
    ));
    let run = asap_plan_selection::run_stage1_to_stage3_with(
        roots.into_iter().enumerate().collect::<Vec<_>>(),
        &entries,
        data,
        PlanningModels::builtin().with_plan_selection(&settings),
        asap_plan_selection::Stage1ToStage3Search {
            milp_time_limit_ms: Some(30_000),
            ..Default::default()
        },
    )
    .unwrap();
    let selection = run
        .stage3_plan_selection
        .as_ref()
        .expect("validated plan within budget");
    println!(
        "production cost={:?} report={:?}",
        selection.costs[&selection.selected].cost_per_second,
        selection.search.candidate_connections
    );
    let report = selection.search.candidate_connections.as_ref().unwrap();
    assert!(
        report.direct_cost_validated_plans > 0,
        "production must validate a plan priced directly by MILP: {report:?}"
    );
    assert!(
        report
            .direct_cost_best_validated_cost
            .is_some_and(|cost| cost < 2.0e-6),
        "direct MILP must price a plan cheaper than independent KMV"
    );
    let accepted = &selection.accepted[&selection.selected];
    assert_eq!(accepted.len(), 2);
    assert!(accepted
        .iter()
        .all(|accuracy| !accuracy.accepted_regardless_of_guarantee
            && accuracy.sketches.iter().any(|sketch| sketch == "Kmv")));
    assert!(
        selection.costs[&selection.selected]
            .cost_per_second
            .unwrap()
            < 2.0e-6
    );
    let plan = run.selected().unwrap();
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
