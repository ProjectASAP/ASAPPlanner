//! Structurally identical summary producers chosen by different queries are
//! shared after Pass 1: one `Rc<SummaryNode>` across their plans, costed once.

use std::rc::Rc;

use asap_aware_mapping::cost_model::Cost;
use asap_aware_mapping::pass::{PlanOutput, PlanningModels};
use asap_aware_mapping::{
    CostModel, CostRate, DefaultCostModel, Horizon, LifecycleInput, SummaryMaintenanceCapabilities,
    SummaryMaintenanceLifecycleCapabilities, SummaryMaintenanceLifecycleCostInputs,
};
use asap_frontend_sql::SqlCatalog;
use asap_planner::{e2e_plan, FrontendInput, UserInput};
use asap_types::post_asap::{SketchAlgorithm, SummaryNode};
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::pre_asap::{AggIntent, QueryExpr};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, DataArrival, DataWorkload, DurationMs, Evidence, LatencyRequirement,
    PlanningWorkload, Predictability, Query, QueryLanguage, QueryRequirements, QueryWorkload, Rate,
    RepeatedDemand, RepeatingEntry, RepetitionInterval, SqlDialect, TimeSelection,
};

const NOW_MS: u64 = 1_700_000_000_000;
const HORIZON_S: f64 = 3_600.0;

/// A state costs `build` once however often it is read; raw recomputation
/// costs `raw_per_read` per read.
struct FixedCosts {
    build: f64,
    raw_per_read: f64,
}

impl CostModel for FixedCosts {
    fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm> {
        DefaultCostModel.rank_candidates(intent, candidates)
    }

    fn summary_maintenance_lifecycle_cost_inputs(
        &self,
        _summary: &SummaryNode,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        SummaryMaintenanceLifecycleCostInputs {
            build_cost: Some(Cost(self.build)),
            maintenance_cost_per_update: Some(Cost::ZERO),
            summary_read_cost: Some(Cost::ZERO),
            retention_cost_rate: Some(CostRate(0.0)),
            retirement_cost: Some(Cost::ZERO),
        }
    }

    fn summary_maintenance_capabilities(
        &self,
        _summary: &SummaryNode,
    ) -> SummaryMaintenanceCapabilities {
        SummaryMaintenanceCapabilities {
            incremental_update: true,
            merge: true,
            delete: true,
        }
    }

    fn raw_query_recompute_cost(&self, _target: &QueryExpr) -> Option<Cost> {
        Some(Cost(self.raw_per_read))
    }
}

/// Summaries are far cheaper than raw recomputation, so every query selects
/// one independently and only sharing is under test.
const CHEAP_SUMMARY: FixedCosts = FixedCosts {
    build: 1.0,
    raw_per_read: 1_000.0,
};

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

fn lifecycle() -> LifecycleInput {
    LifecycleInput::new(NOW_MS, SummaryMaintenanceLifecycleCapabilities::default())
        .with_horizon(Horizon(HORIZON_S))
}

async fn plan_promql(queries: &[(&str, f64)], costs: &FixedCosts) -> PlanOutput {
    let workload = PlanningWorkload {
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
    };
    let input = UserInput::new(
        &workload,
        FrontendInput::Promql {
            now_ms: NOW_MS,
            histograms: None,
        },
        PlanningModels::builtin().with_cost(costs),
        lifecycle(),
    );
    e2e_plan(input).await.expect("workload plans")
}

async fn plan_sql(queries: &[&str], costs: &FixedCosts) -> PlanOutput {
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
            Column::new("l_orderkey", DataType::Int64, false),
            Column::new("l_extendedprice", DataType::Float64, false),
        ]),
    );
    let input = UserInput::new(
        &workload,
        FrontendInput::Sql { catalog: &catalog },
        PlanningModels::builtin().with_cost(costs),
        lifecycle(),
    );
    e2e_plan(input).await.expect("workload plans")
}

/// Every summary state each plan deploys.
fn states(output: &PlanOutput) -> Vec<Vec<Rc<SummaryNode>>> {
    output
        .plans
        .iter()
        .map(|plan| {
            assert!(!plan.plan.selected_raw_recompute, "{:?}", plan.plan.root);
            assert!(!plan.plan.deployments.is_empty());
            plan.plan
                .deployments
                .iter()
                .map(|deployment| Rc::clone(&deployment.summary))
                .collect()
        })
        .collect()
}

/// Whether the two plans deploy exactly the same states, by pointer.
fn same_states(states: &[Vec<Rc<SummaryNode>>]) -> bool {
    states[0].len() == states[1].len()
        && states[0]
            .iter()
            .zip(&states[1])
            .all(|(left, right)| Rc::ptr_eq(left, right))
}

/// The deployments a consumer would run, deduplicated by pointer.
fn unique_deployments(output: &PlanOutput) -> usize {
    let mut seen: Vec<*const SummaryNode> = Vec::new();
    for plan in &output.plans {
        for deployment in &plan.plan.deployments {
            let ptr = Rc::as_ptr(&deployment.summary);
            if !seen.contains(&ptr) {
                seen.push(ptr);
            }
        }
    }
    seen.len()
}

/// p50 and p99 over the same window and accuracy read one KLL: the
/// equal-params subset of summary capability. Both plans hold the same `Rc`
/// with the same lifecycle, so a consumer maintains it once.
#[tokio::test]
async fn quantiles_with_equal_params_share_one_producer() {
    let output = plan_promql(
        &[
            ("quantile_over_time(0.5, lat[5m])", 0.01),
            ("quantile_over_time(0.99, lat[5m])", 0.01),
        ],
        &CHEAP_SUMMARY,
    )
    .await;
    assert!(same_states(&states(&output)));
    assert!(!Rc::ptr_eq(
        &output.plans[0].plan.root,
        &output.plans[1].plan.root
    ));
    assert_eq!(unique_deployments(&output), 1);
    let lifecycles: Vec<_> = output
        .plans
        .iter()
        .map(|plan| {
            plan.plan.deployments[0]
                .summary_maintenance_lifecycle_guarantee
                .clone()
        })
        .collect();
    assert_eq!(lifecycles[0], lifecycles[1]);
    assert!(lifecycles[0].is_some());
    // Each plan is planned against both queries' reads.
    for plan in &output.plans {
        assert_eq!(plan.plan.expected_reads, Some(12.0));
    }
}

/// A different window, a stricter accuracy that changes the sketch's
/// parameters, or a different label selector is a different producer.
#[tokio::test]
async fn different_producers_are_not_shared() {
    for queries in [
        [
            ("quantile_over_time(0.5, lat[5m])", 0.01),
            ("quantile_over_time(0.99, lat[10m])", 0.01),
        ],
        [
            ("quantile_over_time(0.5, lat[5m])", 0.01),
            ("quantile_over_time(0.99, lat[5m])", 0.001),
        ],
        [
            ("quantile_over_time(0.5, lat{job=\"a\"}[5m])", 0.01),
            ("quantile_over_time(0.99, lat{job=\"b\"}[5m])", 0.01),
        ],
    ] {
        let output = plan_promql(&queries, &CHEAP_SUMMARY).await;
        assert!(!same_states(&states(&output)), "{queries:?}");
        assert_eq!(unique_deployments(&output), 2, "{queries:?}");
        for plan in &output.plans {
            assert_eq!(plan.plan.expected_reads, Some(6.0), "{queries:?}");
        }
    }
}

/// Cross-series quantiles name their KLL state after the input column, not the
/// quantile, so p50 and p99 over one selector share it.
#[tokio::test]
async fn cross_series_p50_and_p99_share_one_producer() {
    let output = plan_promql(
        &[("quantile(0.5, lat)", 0.01), ("quantile(0.99, lat)", 0.01)],
        &CHEAP_SUMMARY,
    )
    .await;
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 1);
}

/// An ungrouped aggregate has no unique key, so pre-ASAP CSE keeps the two
/// copies apart; their identical producers (rate, then sum) are shared here.
#[tokio::test]
async fn identical_ungrouped_queries_share_their_producers() {
    let query = ("sum(rate(x[5m]))", 0.01);
    let output = plan_promql(&[query, query], &CHEAP_SUMMARY).await;
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 2);
}

/// The SQL frontend reaches the same sharing for two copies of one filtered
/// percentile.
#[tokio::test]
async fn identical_sql_percentiles_share_one_producer() {
    let query =
        "SELECT approx_percentile_cont(l_extendedprice, 0.5) FROM lineitem WHERE l_orderkey > 10";
    let output = plan_sql(&[query, query], &CHEAP_SUMMARY).await;
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 1);
}

/// The quantile is a readout parameter: SQL p50 and p99 over one filtered
/// column build one KLL, named after its input, while each query keeps its
/// own output column.
#[tokio::test]
async fn sql_p50_and_p99_share_one_producer() {
    let p50 =
        "SELECT approx_percentile_cont(l_extendedprice, 0.5) FROM lineitem WHERE l_orderkey > 10";
    let p99 =
        "SELECT approx_percentile_cont(l_extendedprice, 0.99) FROM lineitem WHERE l_orderkey > 10";
    let output = plan_sql(&[p50, p99], &CHEAP_SUMMARY).await;
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 1);
    let names: Vec<_> = output
        .plans
        .iter()
        .map(|plan| {
            plan.plan
                .root
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
        let output = plan_sql(&queries, &CHEAP_SUMMARY).await;
        assert!(!same_states(&states(&output)), "{queries:?}");
        assert_eq!(unique_deployments(&output), 2, "{queries:?}");
    }
}

/// A state costs 100 and recomputing a query costs 60 over its six reads:
/// alone, the query recomputes raw. Shared by p50 and p99, the state costs 50
/// per query, so both keep it.
#[tokio::test]
async fn shared_amortization_alone_can_beat_raw_recompute() {
    let costs = FixedCosts {
        build: 100.0,
        raw_per_read: 10.0,
    };
    let p50 = ("quantile_over_time(0.5, lat[5m])", 0.01);
    let p99 = ("quantile_over_time(0.99, lat[5m])", 0.01);

    let alone = plan_promql(&[p50], &costs).await;
    assert!(alone.plans[0].plan.selected_raw_recompute);

    let output = plan_promql(&[p50, p99], &costs).await;
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 1);
}
