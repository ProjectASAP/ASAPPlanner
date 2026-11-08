//! Structurally identical summary producers chosen by different queries are
//! shared: one node of the plan across their roots, planned by Pass 2
//! (ASAP-aware CSE). Some older cost-selection tests stay ignored: with the built-in cost model,
//! plan selection still selects an exact plan for at least one of their
//! queries (#580).

use asap_types::ir::{ASAPOp, OperatorNode};
use std::rc::Rc;

use asap_frontend_promql::promql_metric_names;
use asap_frontend_sql::SqlCatalog;

use asap_planner::{e2e_plan, FrontendInput, UserInput};
use asap_planner::{PlanOutput, PlanningModels};
use asap_types::ir::QueryRoot;

use asap_types::ir::properties::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::schema::{DataType, Field, Schema};

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
                max_out_of_order_ms: None,
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
    let input = UserInput::new(&workload, FrontendInput::Promql, PlanningModels::builtin());
    e2e_plan(input).await.expect("workload plans")
}

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

/// Each operator query's root of `output`'s plan, as IR, in entry order.
fn operator_roots(output: &PlanOutput) -> Vec<Rc<OperatorNode>> {
    output
        .plan
        .query_roots()
        .unwrap()
        .into_iter()
        .filter_map(|root| match root {
            QueryRoot::Operator(node) => Some(node),
            QueryRoot::Scalar(_) => None,
        })
        .collect()
}

/// Every summary state (`SummaryAgg`) a query's root reaches, in traversal
/// order.
fn plan_states(root: &Rc<OperatorNode>) -> Vec<Rc<OperatorNode>> {
    OperatorNode::reachable(root)
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
    operator_roots(output)
        .iter()
        .map(|root| {
            let states = plan_states(root);
            assert!(!states.is_empty(), "{root:?}");
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
    for root in operator_roots(output) {
        for state in plan_states(&root) {
            let ptr = Rc::as_ptr(&state);
            if !seen.contains(&ptr) {
                seen.push(ptr);
            }
        }
    }
    seen.len()
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
    let names: Vec<_> = operator_roots(&output)
        .iter()
        .map(|root| {
            root.schema
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

/// Front ends build structurally identical references as separate nodes.
/// The selected graph may reuse a producer or choose separate physical
/// candidates; the scalar root reads at least the distinct ones.
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
        let references: std::collections::HashSet<_> = output.plan.scalar_roots[0]
            .1
            .operator_refs()
            .into_iter()
            .copied()
            .collect();
        assert!(references.len() >= distinct, "{query}");
    }
}
