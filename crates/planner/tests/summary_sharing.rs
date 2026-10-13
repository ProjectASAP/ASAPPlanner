//! Structurally identical summary producers chosen by different queries are
//! shared: one `Rc<OperatorNode>` across their plans. The planner pipeline
//! plans no such sharing until Pass 2 (ASAP-aware CSE), and plan selection
//! prefers raw plans while every summary runs at query time (#580); the
//! sharing tests are ignored until then.

use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};
use std::rc::Rc;

use asap_aware_mapping::pass::{
    run_stage1_to_stage3, PlanOutput, PlanningModels, QueryPlan, Stage1ToStage3Run,
};
use asap_aware_mapping::realization::{default_size_params, DEFAULT_DELTA};
use asap_frontend_promql::lower_promql_query_workload;
use asap_frontend_sql::SqlCatalog;
use asap_planner::{e2e_plan, FrontendInput, UserInput};
use asap_types::post_asap::{FieldDataType, SketchAlgorithm, SketchParams};
use asap_types::pre_asap::agg_intent::default_quantile;
use asap_types::pre_asap::schema::{DataType, Field, Schema};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, DataArrival, DataWorkload, DurationMs, Evidence, LatencyRequirement,
    PlanningWorkload, Predictability, Query, QueryLanguage, QueryRequirements, QueryWorkload, Rate,
    RepeatedDemand, RepeatingEntry, RepetitionInterval, SqlDialect, TimeSelection,
};

const NOW_MS: u64 = 1_700_000_000_000;

fn requirements(epsilon: f64) -> QueryRequirements {
    QueryRequirements {
        accuracy: AccuracyRequirement::Explicit(AccuracyTarget::Epsilon(epsilon)),
        response_latency: LatencyRequirement::Unspecified,
    }
}

/// Every query repeats every ten minutes: six reads each over the horizon.
fn repeating(query: &str, epsilon: f64) -> RepeatingEntry {
    RepeatingEntry {
        query: Query(query.into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(600_000)),
        requirements: requirements(epsilon),
        predictability: Predictability::Unknown,
        time_selection: TimeSelection::default(),
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
            arrival: DataArrival::ContinuouslyIngesting,
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(15_000)),
                ..Default::default()
            },
            ingestion_rate: Evidence {
                value: Some(Rate(1.0)),
                ..Default::default()
            },
            ..Default::default()
        }),
    }
}

async fn plan_promql(queries: &[(&str, f64)]) -> PlanOutput {
    let workload = promql_workload(queries);
    let input = UserInput::new(
        &workload,
        FrontendInput::Promql {
            now_ms: NOW_MS,
            histograms: None,
        },
        PlanningModels::builtin(),
    );
    e2e_plan(input).await.expect("workload plans")
}

/// Every planning stage's output for `queries`, run over the frontend's roots.
fn promql_stages(queries: &[(&str, f64)]) -> Stage1ToStage3Run<usize> {
    let workload = promql_workload(queries);
    let roots = lower_promql_query_workload(&workload, NOW_MS).expect("lowers");
    let targets: Vec<_> = queries
        .iter()
        .map(|(_, epsilon)| Some(AccuracyTarget::Epsilon(*epsilon)))
        .collect();
    run_stage1_to_stage3(
        roots.into_iter().enumerate().collect(),
        &targets,
        &workload
            .query_workload
            .entries()
            .map(|entry| entry.recurrence)
            .collect::<Vec<_>>(),
        &workload.data_workload.clone().unwrap(),
        PlanningModels::builtin(),
    )
    .expect("Pass 1 accepts the workload")
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

async fn plan_sql(queries: &[&str]) -> PlanOutput {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
            query_batch: None,
            repeating_queries: Some(queries.iter().map(|query| repeating(query, 0.01)).collect()),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::AtRest,
            ..Default::default()
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

/// p50 and p99 over the same window and accuracy read one KLL: the
/// equal-params subset of summary capability. Both plans hold the same `Rc`,
/// so a consumer maintains it once.
#[tokio::test]
#[ignore = "plan selection picks the exact plan while summaries run at query time; sharing needs Pass 2 (#580)"]
async fn quantiles_with_equal_params_share_one_producer() {
    let output = plan_promql(&[
        ("quantile_over_time(0.5, lat[5m])", 0.01),
        ("quantile_over_time(0.99, lat[5m])", 0.01),
    ])
    .await;
    assert!(same_states(&states(&output)));
    assert!(!Rc::ptr_eq(&output.plans[0].root, &output.plans[1].root));
    assert_eq!(unique_deployments(&output), 1);
}

/// A different window or label selector is a different producer, even when
/// one query asks for a stricter accuracy than the other.
#[tokio::test]
#[ignore = "plan selection picks the exact plan while summaries run at query time; sharing needs Pass 2 (#580)"]
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
    let SketchParams::Kll { k } = kind.params() else {
        panic!("KLL state: {kind:?}");
    };
    *k
}

/// The KLL `k` sized for `epsilon`.
fn kll_k_for(epsilon: f64) -> u32 {
    let SketchParams::Kll { k } = default_size_params(
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
#[ignore = "plan selection picks the exact plan while summaries run at query time; sharing needs Pass 2 (#580)"]
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
            guarantee.bound.evaluate().unwrap() <= epsilon,
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
#[ignore = "plan selection picks the exact plan while summaries run at query time; sharing needs Pass 2 (#580)"]
async fn cross_series_p50_and_p99_share_one_producer() {
    let output = plan_promql(&[("quantile(0.5, lat)", 0.01), ("quantile(0.99, lat)", 0.01)]).await;
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 1);
}

/// An ungrouped aggregate has no unique key, so pre-ASAP CSE keeps the two
/// copies apart; their identical producers (rate, then sum) are shared here.
#[tokio::test]
#[ignore = "plan selection picks the exact plan while summaries run at query time; sharing needs Pass 2 (#580)"]
async fn identical_ungrouped_queries_share_their_producers() {
    let query = ("sum(rate(x[5m]))", 0.01);
    let output = plan_promql(&[query, query]).await;
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 2);
}

/// The SQL frontend reaches the same sharing for two copies of one filtered
/// percentile.
#[tokio::test]
#[ignore = "plan selection picks the exact plan while summaries run at query time; sharing needs Pass 2 (#580)"]
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
#[tokio::test]
#[ignore = "plan selection picks the exact plan while summaries run at query time; sharing needs Pass 2 (#580)"]
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
    let dag = output.execution_timed_dag().unwrap();
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
/// target, so a candidate reads a sketch in both. Its output has no composed
/// guarantee (#669) and the built-in models have no empirical output-level
/// evidence, so plan selection rejects it with that reason.
#[test]
fn combined_scalar_references_keep_summary_candidates() {
    let run = promql_stages(&[(
        "scalar(quantile(0.99, rate(x[5m]))) - scalar(quantile(0.5, rate(x[5m])))",
        0.01,
    )]);
    let both = run
        .stage2_physical_asap_optimization
        .iter()
        .find(|candidate| {
            let refs = root_operators(&candidate.roots[0]);
            refs.len() == 2 && refs.into_iter().all(reads_sketch)
        })
        .expect("a candidate reads a sketch in both references");
    let selection = run.stage3_plan_selection.as_ref().unwrap();
    let rejection = selection
        .rejected
        .iter()
        .find(|r| r.id == both.id)
        .expect("not selected");
    assert!(!rejection.valid, "{rejection:?}");
    assert!(
        rejection.reason.contains("no guarantee"),
        "{}",
        rejection.reason
    );
}

/// The vector form of a combination of approximate values keeps its summary
/// candidates (nothing prunes them), but its output has no composed guarantee
/// (#669) and the built-in models have no empirical output-level evidence
/// (plan selection, #576), so it is not selected automatically: the exact plan is.
#[tokio::test]
async fn combined_vector_quantiles_keep_unselected_summary_candidates() {
    for query in [
        "quantile(0.99, rate(x[5m])) - quantile(0.5, rate(x[5m]))",
        "quantile(0.99, rate(x[5m])) > quantile(0.5, rate(x[5m]))",
    ] {
        let run = promql_stages(&[(query, 0.01)]);
        let summary = run
            .stage2_physical_asap_optimization
            .iter()
            .find(|candidate| {
                root_operators(&candidate.roots[0])
                    .into_iter()
                    .any(reads_sketch)
            })
            .unwrap_or_else(|| panic!("{query}: summary candidate expected"));
        let QueryRoot::Operator(root) = &summary.roots[0] else {
            panic!("{query}: operator root");
        };
        assert!(root.guarantee.is_none(), "{query}");
        assert_ne!(
            run.stage3_plan_selection.as_ref().unwrap().selected,
            summary.id,
            "{query}"
        );

        let output = plan_promql(&[(query, 0.01)]).await;
        assert!(!reads_sketch(&output.plans[0].root), "{query}");
    }
}

/// Front ends build structurally identical references as separate nodes.
/// Until Pass 2 merges them (#580), each is planned on its own; the plan
/// holds one plan per distinct reference and satisfies the plan contract.
#[tokio::test]
async fn identical_scalar_references_satisfy_the_plan_contract() {
    for (query, distinct) in [
        ("scalar(rate(x[5m])) - scalar(rate(x[5m]))", 2),
        (
            "scalar(sum(rate(x[5m]))) / scalar(sum(rate(x[5m]))) + scalar(count(x))",
            3,
        ),
    ] {
        let output = plan_promql(&[(query, 0.01)]).await;
        assert_eq!(output.scalar_plans.len(), distinct, "{query}");
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
        let dag = output.execution_timed_dag().unwrap();
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
    }
}
