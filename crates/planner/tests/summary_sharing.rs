//! Structurally identical summary producers chosen by different queries are
//! shared: one `Rc<OperatorNode>` across their plans, planned by Pass 2
//! (ASAP-aware CSE). Some older cost-selection tests stay ignored: with the built-in cost model,
//! plan selection still selects an exact plan for at least one of their
//! queries (#580).

use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};
use std::rc::Rc;

use asap_frontend_promql::{lower_promql_query_workload, promql_metric_names};
#[cfg(feature = "sql")]
use asap_frontend_sql::SqlCatalog;
use asap_logical_asap_optimization::realization::{default_size_params, DEFAULT_DELTA};
use asap_plan_selection::pass::{PlanOutput, PlanningModels, QueryPlan};
use asap_plan_selection::RejectionCause;
use asap_planner::{e2e_plan, FrontendInput, UserInput};
use asap_types::ir::operator::agg_intent::default_quantile;
use asap_types::ir::properties::{AccuracyMetric, AccuracyTarget};
#[cfg(feature = "sql")]
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SketchParams};
#[cfg(feature = "sql")]
use asap_types::workload::SqlDialect;
use asap_types::workload::{
    DataArrival, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload, RepeatedDemand, RepeatingEntry, RepetitionInterval,
};

/// `epsilon` as a query's requested accuracy; a quantile query's target
/// names its metric (a rank error).
fn requested(query: &str, epsilon: f64) -> AccuracyTarget {
    let requested = AccuracyTarget::max_error(epsilon);
    if query.contains("quantile") {
        requested.accuracy_metric(AccuracyMetric::Rank)
    } else {
        requested
    }
}

fn requirements(query: &str, epsilon: f64) -> QueryRequirements {
    QueryRequirements {
        accuracy: requested(query, epsilon),
        max_latency_ms: None,
    }
}

/// Every query repeats every ten minutes: six reads each over the horizon.
fn repeating(query: &str, epsilon: f64) -> RepeatingEntry {
    RepeatingEntry {
        query: Query(query.into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(600_000)),
        requirements: requirements(query, epsilon),
        predictability: Predictability::Unknown,
        range_evaluation: None,
    }
}

fn promql_workload(queries: &[(&str, f64)]) -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(
                queries
                    .iter()
                    .map(|(query, epsilon)| repeating(query, *epsilon))
                    .collect(),
            ),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 1.0,
            },
            ..DataWorkload::sampled_every(
                queries
                    .iter()
                    .flat_map(|(query, _)| promql_metric_names(query).unwrap_or_default()),
                DurationMs(15_000),
            )
            .with_series_count(1_000)
        }),
    }
}

async fn plan_promql(queries: &[(&str, f64)]) -> PlanOutput {
    let workload = promql_workload(queries);
    let input = UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin(),
    );
    e2e_plan(input).await.expect("workload plans")
}

/// Explicit exact, all-KLL, and all-DDS assignments for composition checks.
/// These fixtures test candidate support and rejection independently of which
/// candidates a bounded search happens to visit.
fn composed_summary_candidates(
    queries: &[(&str, f64)],
) -> (
    Vec<asap_physical_asap_optimization::PhysicalWorkloadPlan>,
    asap_plan_selection::Selection,
) {
    use asap_logical_asap_optimization::per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates, ComputationCandidate,
    };
    let workload = promql_workload(queries);
    let roots = lower_promql_query_workload(&workload).expect("lowers");
    let inventory =
        generate_per_computation_candidates(roots.into_iter().enumerate().collect()).unwrap();
    let mut assignments = vec![vec![0; inventory.targets.len()]];
    for family in [SketchAlgorithm::Kll, SketchAlgorithm::DDSketch] {
        let choices: Vec<_> = inventory
            .targets
            .iter()
            .map(|target| {
                target
                    .choices
                    .candidates
                    .iter()
                    .position(|candidate| {
                        *candidate == ComputationCandidate::Summary(family.clone())
                    })
                    .unwrap_or(0)
            })
            .collect();
        assert_eq!(
            choices.iter().filter(|choice| **choice != 0).count(),
            2,
            "both quantile references must explicitly use {family:?}"
        );
        assignments.push(choices);
    }
    let entries = workload.query_workload.entries().collect::<Vec<_>>();
    let data = workload.data_workload.as_ref().unwrap();
    let models = PlanningModels::builtin();
    let candidates: Vec<_> = assignments
        .into_iter()
        .enumerate()
        .map(|(index, choices)| {
            let roots: Vec<_> = compose_logical_candidate(&inventory, &choices)
                .unwrap()
                .into_iter()
                .map(|(_, root)| root)
                .collect();
            let mut candidate = asap_physical_asap_optimization::optimize_physical_asap_chosen(
                "explicit-composition",
                &roots,
                &entries,
                data,
                models.deployment_capabilities,
                &mut |_, _, _| asap_types::physical::Materialization::NotMaterialized,
            )
            .unwrap()
            .expect("supported physical computation");
            candidate.id = format!("composition-{index}");
            candidate
        })
        .collect();
    let selection = asap_plan_selection::select_plan(&candidates, &entries, data, models)
        .expect("the exact candidate meets the requested accuracy");
    (candidates, selection)
}

/// Construct a legal shared-KLL candidate explicitly. Sharing capability must
/// not depend on which prefix a time-limited optimization search visits.
fn shared_kll_candidate(
    workload: &PlanningWorkload,
) -> asap_physical_asap_optimization::PhysicalWorkloadPlan {
    use asap_logical_asap_optimization::deployment_capabilities::DeploymentCapabilities;
    use asap_logical_asap_optimization::per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates,
    };
    use asap_types::physical::Materialization;
    let roots = lower_promql_query_workload(workload).unwrap();
    let inventory =
        generate_per_computation_candidates(roots.into_iter().enumerate().collect()).unwrap();
    let mut choices = vec![0; inventory.targets.len()];
    for index in 0..choices.len() {
        choices[index] = (1..inventory.targets[index].choice_count()).find(|choice| {
            let mut trial = vec![0; choices.len()];
            trial[index] = *choice;
            compose_logical_candidate(&inventory, &trial).unwrap().iter().any(|(_, root)| {
                root_operators(root).into_iter().flat_map(OperatorNode::reachable).any(|node| {
                    matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. }) if matches!(kind.params(), SketchParams::Kll { .. }))
                })
            })
        }).expect("KLL implementation exists");
    }
    let roots: Vec<_> = compose_logical_candidate(&inventory, &choices)
        .unwrap()
        .into_iter()
        .map(|(_, root)| root)
        .collect();
    // Every identical sub-DAG of the two queries made one node.
    let roots: Vec<_> = asap_types::ir::cse::share_common_sub_dags(
        roots
            .into_iter()
            .enumerate()
            .map(|(id, root)| match root {
                QueryRoot::Operator(node) => (id, node),
                QueryRoot::Scalar(_) => panic!("operator root"),
            })
            .collect(),
    )
    .into_iter()
    .map(|(_, node)| QueryRoot::Operator(node))
    .collect();
    let mut physical = asap_physical_asap_optimization::optimize_physical_asap_chosen(
        "shared-kll",
        &roots,
        &workload.query_workload.entries().collect::<Vec<_>>(),
        workload.data_workload.as_ref().unwrap(),
        &DeploymentCapabilities::default(),
        &mut |_, _, _| Materialization::NotMaterialized,
    )
    .unwrap()
    .unwrap();
    physical.id = "shared-kll".into();
    asap_plan_selection::select_plan(
        std::slice::from_ref(&physical),
        &workload.query_workload.entries().collect::<Vec<_>>(),
        workload.data_workload.as_ref().unwrap(),
        PlanningModels::builtin(),
    )
    .expect("shared candidate meets both accuracy targets");
    physical
}

fn shared_kll_output(workload: &PlanningWorkload) -> PlanOutput {
    let candidate = shared_kll_candidate(workload);
    PlanOutput::new(
        candidate
            .roots
            .into_iter()
            .enumerate()
            .map(|(entry_index, root)| {
                let QueryRoot::Operator(root) = root else {
                    panic!("operator fixture")
                };
                QueryPlan { entry_index, root }
            })
            .collect(),
    )
}

/// Whether `root` reads a sketch state.
fn reads_sketch(root: &Rc<OperatorNode>) -> bool {
    OperatorNode::reachable(root).into_iter().any(|node| {
        matches!(
            node.asap(),
            Some(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(..),
                ..
            })
        )
    })
}

/// The operator sub-DAGs a query root reads.
fn root_operators(root: &QueryRoot) -> Vec<&Rc<OperatorNode>> {
    match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    }
}

#[cfg(feature = "sql")]
async fn plan_sql(queries: &[&str]) -> PlanOutput {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
            query_batch: None,
            repeating_queries: Some(queries.iter().map(|query| repeating(query, 0.01)).collect()),
        },
        // `lineitem` at rest, holding a million rows.
        data_workload: Some(DataWorkload {
            arrival: DataArrival::AtRest,
            sources: [(
                "lineitem".to_string(),
                asap_types::workload::SourceFacts {
                    rows_held: Some(1_000_000),
                    ..Default::default()
                },
            )]
            .into(),
        }),
    };
    let catalog = SqlCatalog::new().with_table(
        "lineitem",
        Schema::new(vec![
            Field::plain("l_orderkey", DataType::Int64, false),
            Field::plain("l_extendedprice", DataType::Float64, false),
        ]),
    );
    let input = UserInput::new(
        &workload,
        FrontendInput::Sql { catalog: &catalog },
        PlanningModels::builtin(),
    );
    e2e_plan(input).await.expect("workload plans")
}

/// Every summary state (`SummaryAgg`) each plan reaches, in traversal order.
fn plan_states(plan: &QueryPlan) -> Vec<Rc<OperatorNode>> {
    OperatorNode::reachable(&plan.root)
        .into_iter()
        .filter(|node| {
            matches!(
                node.operator,
                asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg { .. })
            )
        })
        .collect()
}

/// Every summary state each plan reaches; each plan selects at least one.
fn states(output: &PlanOutput) -> Vec<Vec<Rc<OperatorNode>>> {
    output
        .plans
        .iter()
        .map(|plan| {
            let states = plan_states(plan);
            assert!(!states.is_empty(), "{:?}", plan.root);
            states
        })
        .collect()
}

/// Whether the two plans deploy exactly the same states, by pointer.
fn same_states(states: &[Vec<Rc<OperatorNode>>]) -> bool {
    states[0].len() == states[1].len()
        && states[0]
            .iter()
            .zip(&states[1])
            .all(|(left, right)| Rc::ptr_eq(left, right))
}

/// The summary states a consumer would run, deduplicated by pointer.
fn unique_deployments(output: &PlanOutput) -> usize {
    let mut seen: Vec<*const OperatorNode> = Vec::new();
    for plan in &output.plans {
        for state in plan_states(plan) {
            let ptr = Rc::as_ptr(&state);
            if !seen.contains(&ptr) {
                seen.push(ptr);
            }
        }
    }
    seen.len()
}

/// An explicitly connected KLL candidate shares one physical producer and
/// satisfies both outputs' accuracy contracts.
#[test]
fn quantiles_with_equal_params_share_one_producer() {
    let workload = promql_workload(&[
        ("quantile_over_time(0.5, lat[5m])", 0.01),
        ("quantile_over_time(0.99, lat[5m])", 0.01),
    ]);
    let cheapest = shared_kll_candidate(&workload);
    let states: Vec<Vec<Rc<OperatorNode>>> = cheapest
        .roots
        .iter()
        .map(|root| {
            root_operators(root)
                .into_iter()
                .flat_map(OperatorNode::reachable)
                .filter(|node| matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. })))
                .collect()
        })
        .collect();
    assert_eq!(states[0].len(), 1, "{}", cheapest.id);
    assert!(same_states(&states), "{}", cheapest.id);
}

/// Shared producer identity survives physicalization for a high-volume source;
/// this checks feasibility, independently of the cost optimizer's winner.
#[tokio::test]
async fn quantiles_with_equal_params_deploy_one_producer() {
    let mut workload = promql_workload(&[
        ("quantile_over_time(0.5, lat[5m])", 0.01),
        ("quantile_over_time(0.99, lat[5m])", 0.01),
    ]);
    let data = workload.data_workload.as_mut().unwrap();
    data.arrival = DataArrival::Ingesting {
        rows_per_second: 666_666.67,
    };
    let lat = data.sources.get_mut("lat").unwrap();
    lat.series_count = Some(1_000_000);
    lat.sample_interval = Some(DurationMs(1_500));
    let output = shared_kll_output(&workload);
    assert!(same_states(&states(&output)));
    assert!(!Rc::ptr_eq(&output.plans[0].root, &output.plans[1].root));
    assert_eq!(unique_deployments(&output), 1);
}

/// A different window or label selector is a different producer, even when
/// one query asks for a stricter accuracy than the other.
#[tokio::test]
#[ignore = "plan selection still selects an exact plan for at least one query: with the built-in cost model its query-time summary costs more than exact (#580)"]
async fn different_producers_are_not_shared() {
    for queries in [
        [
            ("quantile_over_time(0.5, lat[5m])", 0.01),
            ("quantile_over_time(0.99, lat[10m])", 0.01),
        ],
        [
            ("quantile_over_time(0.5, lat[5m])", 0.01),
            ("quantile_over_time(0.99, lat[10m])", 0.001),
        ],
        [
            ("quantile_over_time(0.5, lat{job=\"a\"}[5m])", 0.01),
            ("quantile_over_time(0.99, lat{job=\"b\"}[5m])", 0.001),
        ],
        [
            ("quantile_over_time(0.5, lat{job=\"a\"}[5m])", 0.01),
            ("quantile_over_time(0.99, lat{job=\"b\"}[5m])", 0.01),
        ],
    ] {
        let output = plan_promql(&queries).await;
        assert!(!same_states(&states(&output)), "{queries:?}");
        assert_eq!(unique_deployments(&output), 2, "{queries:?}");
        for (plan, (_, epsilon)) in output.plans.iter().zip(queries) {
            assert_eq!(kll_k(plan), kll_k_for(epsilon), "{queries:?}");
        }
    }
}

/// The KLL `k` of the one state a plan deploys.
fn kll_k(plan: &QueryPlan) -> u32 {
    let states = plan_states(plan);
    let [deployment] = states.as_slice() else {
        panic!("one state: {:?}", states.len());
    };
    let asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg {
        family: FieldDataType::Sketch(kind, _),
        ..
    }) = &deployment.operator
    else {
        panic!("sketch state: {:?}", deployment.operator);
    };
    let SketchParams::Kll { k, .. } = kind.params() else {
        panic!("KLL state: {kind:?}");
    };
    *k
}

/// The KLL `k` sized for `epsilon`.
fn kll_k_for(epsilon: f64) -> u32 {
    let SketchParams::Kll { k, .. } = default_size_params(
        SketchAlgorithm::Kll,
        &default_quantile(0.5),
        epsilon,
        DEFAULT_DELTA,
    ) else {
        unreachable!()
    };
    k
}

/// p50 at ε=0.01 and p99 at ε=0.001 over the same input share one KLL sized
/// for the strictest consumer; each reader's guarantee meets its own target.
#[tokio::test]
#[ignore = "plan selection still selects an exact plan for at least one query: with the built-in cost model its query-time summary costs more than exact (#580)"]
async fn quantiles_share_one_producer_sized_for_the_strictest_consumer() {
    let p50 = ("quantile_over_time(0.5, lat[5m])", 0.01);
    let p99 = ("quantile_over_time(0.99, lat[5m])", 0.001);
    assert!(kll_k_for(0.001) > kll_k_for(0.01));

    let output = plan_promql(&[p50, p99]).await;
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 1);
    for (plan, (_, epsilon)) in output.plans.iter().zip([p50, p99]) {
        assert_eq!(kll_k(plan), kll_k_for(0.001));
        let guarantee = plan.root.guarantee.as_ref().expect("certified");
        assert!(
            guarantee.spec.error.evaluate().unwrap() <= epsilon,
            "{guarantee:?}"
        );
    }

    // Alone, the looser query keeps its own, smaller KLL.
    let alone = plan_promql(&[p50]).await;
    assert_eq!(kll_k(&alone.plans[0]), kll_k_for(0.01));
}

/// Cross-series quantiles name their KLL state after the input column, not the
/// quantile, so p50 and p99 over one selector share it.
#[tokio::test]
async fn cross_series_p50_and_p99_share_one_producer() {
    let output = shared_kll_output(&promql_workload(&[
        ("quantile(0.5, lat)", 0.01),
        ("quantile(0.99, lat)", 0.01),
    ]));
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 1);
}

/// Two identical ungrouped queries: their identical producers (rate, then
/// sum) are shared.
#[tokio::test]
async fn identical_ungrouped_queries_share_their_producers() {
    let query = ("sum(rate(x[5m]))", 0.01);
    let output = plan_promql(&[query, query]).await;
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 2);
}

/// The SQL frontend reaches the same sharing for two copies of one filtered
/// percentile.
#[cfg(feature = "sql")]
#[tokio::test]
async fn identical_sql_percentiles_share_one_producer() {
    let query =
        "SELECT approx_percentile_cont(l_extendedprice, 0.5) FROM lineitem WHERE l_orderkey > 10";
    let output = plan_sql(&[query, query]).await;
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 1);
}

/// The quantile is a evaluation parameter: SQL p50 and p99 over one filtered
/// column build one KLL, named after its input, while each query keeps its
/// own output column.
#[cfg(feature = "sql")]
#[tokio::test]
#[ignore = "plan selection still selects an exact plan for at least one query: with the built-in cost model its query-time summary costs more than exact (#580)"]
async fn sql_p50_and_p99_share_one_producer() {
    let p50 =
        "SELECT approx_percentile_cont(l_extendedprice, 0.5) FROM lineitem WHERE l_orderkey > 10";
    let p99 =
        "SELECT approx_percentile_cont(l_extendedprice, 0.99) FROM lineitem WHERE l_orderkey > 10";
    let output = plan_sql(&[p50, p99]).await;
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 1);
    let names: Vec<_> = output
        .plans
        .iter()
        .map(|plan| {
            plan.root
                .schema
                .fields
                .iter()
                .map(|field| field.name.clone())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        names,
        [
            ["approx_percentile_cont(lineitem.l_extendedprice,Float64(0.5))"],
            ["approx_percentile_cont(lineitem.l_extendedprice,Float64(0.99))"],
        ]
    );

    for queries in [
        [
            p50,
            "SELECT approx_percentile_cont(l_extendedprice, 0.99) FROM lineitem WHERE l_orderkey > 20",
        ],
        [
            p50,
            "SELECT approx_percentile_cont(l_orderkey, 0.99) FROM lineitem WHERE l_orderkey > 10",
        ],
    ] {
        let output = plan_sql(&queries).await;
        assert!(!same_states(&states(&output)), "{queries:?}");
        assert_eq!(unique_deployments(&output), 2, "{queries:?}");
    }
}

/// The operator sub-DAG a standalone scalar query reads is planned like an
/// operator root (#511 §2.3): the scalar root is rebuilt over its selected
/// sub-DAG, and entries keep their order around a literal scalar query.
#[tokio::test]
async fn scalar_queries_read_planned_sub_dags() {
    let output = plan_promql(&[
        ("1", 0.01),
        ("scalar(quantile(0.99, rate(x[5m])))", 0.01),
        ("quantile(0.99, rate(x[5m]))", 0.01),
    ])
    .await;
    assert_eq!(output.entry_indices(), vec![0, 1, 2]);
    let scalar_entries: Vec<_> = output.scalar_roots.iter().map(|(i, _)| *i).collect();
    assert_eq!(scalar_entries, vec![0, 1]);
    assert_eq!(output.plans.len(), 1);
    assert_eq!(output.plans[0].entry_index, 2);

    assert_eq!(output.scalar_plans.len(), 1);
    let read = &output.scalar_plans[0];
    assert_eq!(read.entry_index, 1);
    let refs = output.scalar_roots[1].1.operator_refs();
    assert_eq!(refs.len(), 1);
    assert!(Rc::ptr_eq(refs[0], &read.root));

    // The export keys each scalar query by its entry and points the scalar
    // read at an exported node.
    let dag = output.physical_asap_dag().unwrap();
    let keys: Vec<_> = dag.scalar_roots.iter().map(|(i, _)| *i).collect();
    assert_eq!(keys, vec![0, 1]);
    assert_eq!(dag.roots.len(), 1);
    let [read] = dag.scalar_roots[1].1.operator_refs()[..] else {
        panic!("one read");
    };
    assert!(dag.nodes.iter().any(|node| node.id == *read));
}

/// A combination of approximate values keeps its summary candidates: each
/// reference of `scalar(q99) - scalar(q50)` is planned against the entry's
/// target, so a candidate reads a sketch in both. A difference of rank (KLL)
/// or relative (DDSketch) errors composes to no guarantee in the MVP (#669)
/// and the built-in models have no empirical output-level evidence, so plan
/// selection rejects it with that reason.
#[test]
fn combined_scalar_references_keep_summary_candidates() {
    let (candidates, selection) = composed_summary_candidates(&[(
        "scalar(quantile(0.99, rate(x[5m]))) - scalar(quantile(0.5, rate(x[5m])))",
        0.01,
    )]);
    for both in candidates.iter().skip(1) {
        let refs = root_operators(&both.roots[0]);
        assert_eq!(refs.len(), 2);
        assert!(
            refs.into_iter().all(reads_sketch),
            "both references use the explicit sketch family"
        );
        let rejection = selection
            .rejected
            .iter()
            .find(|r| r.id == both.id)
            .expect("not selected");
        assert_eq!(
            rejection.cause,
            RejectionCause::NoAccuracyGuarantee,
            "{rejection:?}"
        );
        assert!(
            rejection.reason.contains("no guarantee"),
            "{}",
            rejection.reason
        );
    }
    assert_eq!(
        selection.selected, candidates[0].id,
        "the exact assignment is selected"
    );
}

/// Candidate generation supports summary implementations of the vector
/// operands, but a difference or a comparison of
/// estimates composes to no guarantee in the MVP (#669; a comparison's band
/// guarantee is later work) and the built-in models have no empirical
/// output-level evidence (plan selection, #576), so it is not selected
/// automatically: the exact plan is.
#[tokio::test]
async fn combined_vector_quantiles_keep_unselected_summary_candidates() {
    for query in [
        "quantile(0.99, rate(x[5m])) - quantile(0.5, rate(x[5m]))",
        "quantile(0.99, rate(x[5m])) > quantile(0.5, rate(x[5m]))",
    ] {
        let (candidates, selection) = composed_summary_candidates(&[(query, 0.01)]);
        for summary in candidates.iter().skip(1) {
            let QueryRoot::Operator(root) = &summary.roots[0] else {
                panic!("{query}: operator root");
            };
            assert!(reads_sketch(root), "{query}: explicit summary candidate");
            assert!(root.guarantee.is_none(), "{query}");
            let rejection = selection
                .rejected
                .iter()
                .find(|rejection| rejection.id == summary.id)
                .expect("uncertified vector computation is rejected");
            assert_eq!(
                rejection.cause,
                RejectionCause::NoAccuracyGuarantee,
                "{query}: {rejection:?}"
            );
            assert!(rejection.reason.contains("no guarantee"), "{rejection:?}");
        }
        assert_eq!(
            selection.selected, candidates[0].id,
            "{query}: exact assignment selected"
        );

        let output = plan_promql(&[(query, 0.01)]).await;
        assert!(!reads_sketch(&output.plans[0].root), "{query}");
    }
}

/// Front ends build structurally identical references as separate nodes.
/// The selected graph may reuse a producer or choose separate physical
/// candidates. Every distinct selected reference must be exported exactly once.
#[tokio::test]
async fn identical_scalar_references_satisfy_the_plan_contract() {
    for (query, distinct) in [
        ("scalar(rate(x[5m])) - scalar(rate(x[5m]))", 1),
        (
            "scalar(sum(rate(x[5m]))) / scalar(sum(rate(x[5m]))) + scalar(count(x))",
            2,
        ),
    ] {
        let output = plan_promql(&[(query, 0.01)]).await;
        let references: std::collections::HashSet<_> = output.scalar_roots[0]
            .1
            .operator_refs()
            .into_iter()
            .map(Rc::as_ptr)
            .collect();
        assert_eq!(output.scalar_plans.len(), references.len(), "{query}");
        assert!(references.len() >= distinct, "{query}");
        let (_, expr) = &output.scalar_roots[0];
        for node in expr.operator_refs() {
            assert!(
                output
                    .scalar_plans
                    .iter()
                    .any(|read| Rc::ptr_eq(&read.root, node)),
                "{query}"
            );
        }
    }
}

/// A root whose selected candidate is an exact state is a query result: it
/// is read through `FinalizeExactAccumulator`, so its output is plain values,
/// not accumulator state.
#[tokio::test]
async fn exact_state_roots_are_finalized() {
    for query in [
        "sum_over_time(m[1m])",
        "sum(rate(x[5m]))",
        "sum(sum_over_time(m[1m]) + sum_over_time(n[1m]))",
    ] {
        let output = plan_promql(&[(query, 0.01)]).await;
        let root = &output.plans[0].root;
        assert!(
            matches!(root.asap(), Some(ASAPOp::FinalizeExactAccumulator { .. })),
            "{query}: {root:#?}"
        );
        let dag = output.physical_asap_dag().unwrap();
        let root = dag
            .nodes
            .iter()
            .find(|node| node.id == dag.roots[0])
            .unwrap();
        assert!(
            root.output_schema
                .fields
                .iter()
                .all(|field| field.is_plain()),
            "{query}: {:?}",
            root.output_schema
        );
        // With nothing materialized, the root and the exact state it reads
        // run at query time.
        use asap_types::ir::properties::{DataPrimitive, ExecutionDataState, ExecutionTiming};
        assert_eq!(root.output_state, ExecutionDataState::QUERY_ROWS, "{query}");
        let state = dag
            .edges
            .iter()
            .find(|edge| edge.consumer == root.id)
            .map(|edge| edge.producer)
            .unwrap();
        let state = dag.nodes.iter().find(|node| node.id == state).unwrap();
        assert_eq!(
            state.output_state,
            ExecutionDataState {
                timing: ExecutionTiming::QueryTime,
                primitive: DataPrimitive::SummaryState,
            },
            "{query}"
        );
    }
}
