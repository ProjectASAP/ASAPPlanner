use asap_frontend_promql::{
    lower_promql_workload, lower_promql_workload_with_histograms, HistogramCatalog, PromqlError,
};
use asap_types::pre_asap::QueryExpr;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence, PlanningWorkload,
    Predictability, Query, QueryLanguage, QueryRequirements, QueryWorkload, TimeSelection,
};

fn workload(query: &str, accuracy: AccuracyTarget) -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(accuracy),
                    ..Default::default()
                },
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
                time_selection: TimeSelection::default(),
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(1_000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    }
}

pub fn lower_promql(query: &str, accuracy: AccuracyTarget) -> Result<QueryExpr, PromqlError> {
    let mut lowered = lower_promql_workload(&workload(query, accuracy), 0)?;
    Ok(lowered.remove(0))
}

#[allow(dead_code)]
pub fn lower_promql_with_histograms(
    query: &str,
    accuracy: AccuracyTarget,
    histograms: HistogramCatalog,
) -> Result<QueryExpr, PromqlError> {
    let mut lowered =
        lower_promql_workload_with_histograms(&workload(query, accuracy), histograms, 0)?;
    Ok(lowered.remove(0))
}
