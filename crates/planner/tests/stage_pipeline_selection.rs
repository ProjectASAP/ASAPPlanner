//! The stage pipeline's dynamic program selects the exhaustive minimum
//! (#572): on #509 Example 1 and on small nested, top-k and SQL workloads,
//! the combination it picks is the one that building and pricing every
//! combination picks.

use asap_aware_mapping::logical_candidates::{
    enumerate_local_logical_candidates, LocalLogicalCandidates,
};
use asap_aware_mapping::pass::PlanningModels;
use asap_aware_mapping::plan_selection::{
    select_exhaustive, select_plan, SelectionMethod, MAX_ENUMERATED_CANDIDATES,
};
use asap_frontend_sql::{lower_sql_dialect, SqlCatalog};
use asap_planner::{e2e_plan, FrontendInput, UserInput};
use asap_types::ir::cse::share_common_sub_dags;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::schema_support::with_promql_series_identity;
use asap_types::ir::QueryRoot;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataArrival, DataDistribution, DataWorkload, DurationMs,
    Evidence, EvidenceSource, LatencyRequirement, PlanningWorkload, Predictability, Query,
    QueryLanguage, QueryRequirements, QueryTimeScope, QueryWorkload, Rate, RepeatedDemand,
    RepeatingEntry, RepetitionInterval, SqlDialect, TimeSelection,
};

type Inventory = LocalLogicalCandidates<usize>;

fn declared<T>(value: T) -> Evidence<T> {
    Evidence {
        value: Some(value),
        source: EvidenceSource::Declared,
        ..Default::default()
    }
}

/// #509 Example 1 over its shared data workload, as `stage_pipeline` builds it.
fn example1() -> PlanningWorkload {
    let panel = |query: &str, accuracy, response_latency| RepeatingEntry {
        query: Query(query.into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
        requirements: QueryRequirements {
            accuracy: AccuracyRequirement::Explicit(accuracy),
            response_latency,
        },
        predictability: Predictability::Predictable { known_at: None },
        time_selection: TimeSelection {
            scope: QueryTimeScope::RealTime,
            lookback: Some(DurationMs(60_000)),
            as_of: None,
        },
    };
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![
                panel(
                    "sum by (job) (rate(http_requests_total[1m]))",
                    AccuracyTarget::Exact,
                    LatencyRequirement::Unspecified,
                ),
                panel(
                    "topk by (job) (10, sum_over_time(http_requests_total[1m]))",
                    AccuracyTarget::EpsilonDelta {
                        epsilon: 0.01,
                        delta: 0.001,
                    },
                    LatencyRequirement::ExplicitMaxMs(100.0),
                ),
            ]),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            data_ingestion_interval: declared(DurationMs(15_000)),
            ingestion_volume: Evidence::default(),
            ingestion_rate: declared(Rate(1_000_000.0 / 15.0)),
            input_cardinality: declared(1_000_000),
            distribution: declared(DataDistribution::Zipf),
        }),
    }
}

fn batch(query: &str, accuracy: AccuracyTarget) -> BatchEntry {
    BatchEntry {
        query: Query(query.into()),
        requirements: QueryRequirements {
            accuracy: AccuracyRequirement::Explicit(accuracy),
            ..Default::default()
        },
        predictability: Predictability::Unknown,
        invocations: 1,
        execute_at: None,
        time_selection: TimeSelection::default(),
    }
}

fn promql(queries: &[&str], series: u64) -> PlanningWorkload {
    let accuracy = AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.001,
    };
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(queries.iter().map(|q| batch(q, accuracy.clone())).collect()),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: declared(DurationMs(15_000)),
            ingestion_rate: declared(Rate(10.0)),
            input_cardinality: declared(series),
            ..Default::default()
        }),
    }
}

/// Stage 1's inventory as the stage pipeline builds it: series identity,
/// then identical sub-DAGs merged.
fn promql_inventory(workload: &PlanningWorkload) -> Inventory {
    let roots = asap_frontend_promql::lower_promql_query_workload(workload, 0)
        .expect("lowers")
        .into_iter()
        .enumerate()
        .map(|(index, root)| match root {
            QueryRoot::Operator(node) => (index, with_promql_series_identity(&node).unwrap()),
            QueryRoot::Scalar(_) => panic!("operator roots"),
        })
        .collect();
    let roots = share_common_sub_dags(roots)
        .into_iter()
        .map(|(index, node)| (index, QueryRoot::Operator(node)))
        .collect();
    enumerate_local_logical_candidates(roots).expect("Stage 1")
}

fn targets(workload: &PlanningWorkload) -> Vec<Option<AccuracyTarget>> {
    workload
        .query_workload
        .entries()
        .map(|entry| Some(entry.requirements.accuracy.target()))
        .collect()
}

/// The dynamic program's choice is the exhaustive winner's; returns that
/// winner's id.
fn assert_dp_matches_exhaustive(
    inventory: &Inventory,
    workload: &PlanningWorkload,
    combinations: usize,
) -> String {
    let targets = targets(workload);
    let data = workload.data_workload.clone().unwrap_or_default();
    let models = PlanningModels::builtin();
    let exhaustive = select_exhaustive(
        inventory,
        &targets,
        &data,
        models,
        MAX_ENUMERATED_CANDIDATES,
    )
    .expect("exhaustive selection");
    assert_eq!(exhaustive.combinations, combinations);
    assert_eq!(exhaustive.candidates.len(), combinations);
    let winner = exhaustive
        .candidates
        .iter()
        .find(|c| {
            c.physical
                .as_ref()
                .is_some_and(|p| p.id == exhaustive.selection.selected)
        })
        .expect("winner was built");

    let plan = select_plan(inventory, &targets, &data, models).expect("selects");
    assert_eq!(plan.selection.method, SelectionMethod::TreeDp);
    assert_eq!(plan.choice, winner.choice);
    assert_eq!(plan.selection.selected, exhaustive.selection.selected);
    exhaustive.selection.selected
}

/// #509 Example 1: the dynamic program picks the cheapest of its 24 combinations.
#[test]
fn example1_dp_equals_exhaustive() {
    let workload = example1();
    let selected = assert_dp_matches_exhaustive(&promql_inventory(&workload), &workload, 24);
    assert_eq!(selected, "P20");
}

/// Nested targets (`sum` over `rate`) select the exhaustive minimum.
#[test]
fn nested_sum_over_rate_dp_equals_exhaustive() {
    let workload = promql(&["sum by (job) (rate(x[1m]))"], 1_000);
    assert_dp_matches_exhaustive(&promql_inventory(&workload), &workload, 4);
}

/// An aggregate over a top-k, with inputs both below and above k × groups
/// rows, selects the exhaustive minimum over all 30 combinations.
#[test]
fn count_over_topk_dp_equals_exhaustive() {
    for series in [3, 1_000_000] {
        let workload = promql(&["count(topk by (job) (10, sum_over_time(m[1m])))"], series);
        assert_dp_matches_exhaustive(&promql_inventory(&workload), &workload, 30);
    }
}

/// Two PromQL queries sharing a source select the exhaustive minimum.
#[test]
fn two_query_promql_dp_equals_exhaustive() {
    let workload = promql(
        &[
            "sum by (job) (rate(x[1m]))",
            "topk by (job) (10, sum_over_time(x[1m]))",
        ],
        1_000,
    );
    assert_dp_matches_exhaustive(&promql_inventory(&workload), &workload, 24);
}

/// A SQL workload (distinct count and percentile) selects the exhaustive minimum.
#[tokio::test]
async fn sql_dp_equals_exhaustive() {
    let accuracy = AccuracyTarget::Epsilon(0.01);
    let queries = [
        "SELECT COUNT(DISTINCT l_orderkey) FROM lineitem",
        "SELECT approx_percentile_cont(l_extendedprice, 0.99) FROM lineitem",
    ];
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
            query_batch: Some(queries.iter().map(|q| batch(q, accuracy.clone())).collect()),
            repeating_queries: None,
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
    let mut roots = Vec::new();
    for (index, query) in queries.iter().enumerate() {
        let root = lower_sql_dialect(query, &catalog, SqlDialect::DataFusionSQL, accuracy.clone())
            .await
            .expect("lowers");
        roots.push((index, QueryRoot::Operator(root)));
    }
    let inventory = enumerate_local_logical_candidates(roots).expect("Stage 1");
    assert_dp_matches_exhaustive(&inventory, &workload, 15);
}

/// Through the facade, Example 1 selects the exhaustive winner, P20: both
/// queries exact, Q1's rate and sum and Q2's sum as exact accumulators.
#[tokio::test]
async fn facade_selects_the_example1_exhaustive_winner() {
    let workload = example1();
    let output = e2e_plan(UserInput::new(
        &workload,
        FrontendInput::Promql {
            now_ms: 0,
            histograms: None,
        },
        PlanningModels::builtin(),
    ))
    .await
    .expect("plans");
    let selection = output.selection.as_ref().expect("selection");
    assert_eq!(selection.selected, "P20");
    assert_eq!(selection.method, SelectionMethod::TreeDp);
    assert_eq!(output.entry_indices(), vec![0, 1]);
    // The plans are already timed at query time; exporting them again
    // re-times nothing.
    let dag = output.execution_timed_dag().expect("exports");
    assert_eq!(dag.roots.len(), 2);
}
