//! #1207 PR 7: Stage 3's objective over execution contexts against
//! `asap_cost::plan_cost` of the realized plan, end to end through the
//! facade. On small workloads whose plan meets none of the terms the
//! formulation leaves out (memory a query-time result holds while its
//! evaluation runs; see `SearchReport::inexact_objective`), the objective
//! equals `plan_cost`.

use asap_cost::{CostInputs, CostModel, ResourceUnitCost};
use asap_frontend_promql::promql_metric_names;
use asap_planner::{e2e_plan, FrontendInput, PlanOutput, PlanningModels, UserInput};
use asap_types::workload::{
    DataArrival, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload, RepeatedDemand, RepeatingEntry, RepetitionInterval,
    TimestampMs,
};

/// Exact PromQL queries, each `(query, interval in ms, phase in ms)`, over
/// stored samples (no ingestion pipeline): 1,000 series sampled every 15 s.
fn workload(queries: &[(&str, u32, u64)]) -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(
                queries
                    .iter()
                    .map(|&(query, interval_ms, phase_ms)| RepeatingEntry {
                        query: Query(query.into()),
                        demand: RepeatedDemand::FixedIntervalAt {
                            interval: RepetitionInterval(interval_ms),
                            evaluation_phase: TimestampMs(phase_ms),
                        },
                        requirements: QueryRequirements::default(),
                        predictability: Predictability::Unknown,
                        range_evaluation: None,
                    })
                    .collect(),
            ),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::AtRest,
            ..DataWorkload::sampled_every(
                queries
                    .iter()
                    .flat_map(|(query, ..)| promql_metric_names(query).unwrap_or_default()),
                DurationMs(15_000),
            )
            .with_series_count(1_000)
        }),
    }
}

async fn plan(queries: &[(&str, u32, u64)], cost_inputs: &CostInputs) -> PlanOutput {
    let workload = workload(queries);
    let mut models = PlanningModels::builtin();
    models.cost_inputs = cost_inputs;
    let input = UserInput::new(&workload, FrontendInput::Promql, models);
    e2e_plan(input).await.expect("workload plans")
}

/// Cost model 1 with the illustrative prices, memory free: the memory a
/// query-time result holds while its evaluation runs, which needs query
/// latency (out of scope), costs nothing.
fn memory_free() -> CostInputs {
    CostInputs {
        cost_model: CostModel::UsageOverTime(ResourceUnitCost {
            cost_per_memory_byte_second: 0.0,
            ..ResourceUnitCost::illustrative()
        }),
        ..CostInputs::default()
    }
}

/// One query; two queries of one context sharing a range; two queries in
/// two contexts (every 10 s and every 15 s) reading the same range, each
/// context running its own: in each, the objective equals `plan_cost`
/// (no `inexact_objective`) and the solver's bound is reported.
#[tokio::test]
async fn the_objective_equals_plan_cost_on_small_workloads() {
    let cost_inputs = memory_free();
    for queries in [
        vec![("sum(rate(latency[1m]))", 10_000, 0)],
        vec![
            ("sum(rate(latency[1m]))", 10_000, 0),
            ("max(rate(latency[5m]))", 15_000, 0),
        ],
    ] {
        let output = plan(&queries, &cost_inputs).await;
        let selection = output.selection.expect("a plan is selected");
        let report = &selection.search_report;
        assert_eq!(report.inexact_objective, None, "{queries:?}: {report:?}");
        assert!(
            report.cost_lower_bound_per_second.is_some(),
            "{queries:?}: {report:?}"
        );
    }
}

/// With memory priced, the memory a query-time result holds while its
/// evaluation runs is in `plan_cost` but not in the objective: the
/// selection reports the objective as inexact and names that term.
#[tokio::test]
async fn held_memory_is_reported_as_an_inexact_term() {
    let output = plan(
        &[("sum(rate(latency[1m]))", 10_000, 0)],
        &CostInputs::default(),
    )
    .await;
    let selection = output.selection.expect("a plan is selected");
    let inexact = selection
        .search_report
        .inexact_objective
        .expect("held memory is priced");
    assert!(inexact.objective_per_second < inexact.plan_cost_per_second);
    assert!(
        inexact
            .inexact_terms
            .keys()
            .any(|term| term.starts_with("memory a query-time result holds")),
        "{inexact:?}"
    );
}
