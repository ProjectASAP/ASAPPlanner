//! PromQL front end: parse (via `promql-parser`) → the canonical, unresolved
//! shape, built directly (issue #179) → [`resolve_root`].
//!
//! Emits [`UnresolvedQueryExpr`](asap_types::pre_asap::UnresolvedQueryExpr) itself — the
//! canonical `QueryExpr`, generic over an unresolved
//! [`ColumnRef`](asap_types::pre_asap::ColumnRef) — directly, rather than a
//! separate per-language relational tree; `resolve_root` runs the
//! [`SchemaResolver`](asap_types::pre_asap::SchemaResolver) for positional name resolution.
//! Depends on the PromQL parser only — never on the SQL / DataFusion stack.

pub mod error;
pub mod histogram;
pub mod promql;

use asap_types::pre_asap::resolve_root;
use asap_types::pre_asap::QueryExpr;
use asap_types::workload::{DurationMs, PlanningWorkload, QueryLanguage, WorkloadError};

pub use error::PromqlError;
pub use histogram::{HistogramCatalog, HistogramKind};

/// Lower every normalized PromQL workload entry to a plan-ready `QueryExpr`.
///
/// PromQL workloads must declare a non-zero `data_ingestion_interval`; it is
/// injected around each bare instant selector. Explicit range selectors keep
/// their query-specified range.
/// `now_ms` is the planning time in Unix milliseconds; cadence evidence must
/// be valid at that time, using the same clock as downstream planning.
pub fn lower_promql_workload(
    workload: &PlanningWorkload,
    now_ms: u64,
) -> Result<Vec<QueryExpr>, PromqlError> {
    lower_promql_workload_inner(workload, now_ms)
}

/// Like [`lower_promql_workload`], but uses `histograms` to distinguish classic
/// bucket interpolation from generic sketchable quantiles.
pub fn lower_promql_workload_with_histograms(
    workload: &PlanningWorkload,
    histograms: HistogramCatalog,
    now_ms: u64,
) -> Result<Vec<QueryExpr>, PromqlError> {
    let _guard = histogram::CatalogGuard::install(histograms);
    lower_promql_workload_inner(workload, now_ms)
}

fn lower_promql_workload_inner(
    workload: &PlanningWorkload,
    now_ms: u64,
) -> Result<Vec<QueryExpr>, PromqlError> {
    if !matches!(workload.query_workload.language, QueryLanguage::PromQL) {
        return Err(PromqlError::WrongLanguage(format!(
            "{:?}",
            workload.query_workload.language
        )));
    }
    workload.validate()?;
    let &DurationMs(interval_ms) = workload
        .data_workload
        .as_ref()
        .expect("validated PromQL workload has data_workload")
        .data_ingestion_interval
        .value_at(now_ms)
        .ok_or(WorkloadError::UnavailableDataIngestionInterval)?;
    workload
        .query_workload
        .entries()
        .map(|entry| {
            let unresolved = promql::PromqlLowerer::lower_with_ingestion_interval(
                &entry.query.0,
                &entry.requirements.accuracy.target(),
                std::time::Duration::from_millis(interval_ms),
            )?;
            Ok(resolve_root(&unresolved)?)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    // Expiring evidence without an observation timestamp is never usable.
    #[test]
    fn rejects_unusable_ingestion_evidence() {
        let mut input = workload("sum(data)");
        input
            .data_workload
            .as_mut()
            .unwrap()
            .data_ingestion_interval
            .valid_for_ms = Some(100);
        assert!(lower_promql_workload(&input, 0).is_err());
    }

    // Cadence expiry is inclusive; future and expired evidence cannot set a horizon.
    #[test]
    fn ingestion_evidence_respects_planning_time_with_and_without_histograms() {
        let mut input = workload("sum(data)");
        let evidence = &mut input
            .data_workload
            .as_mut()
            .unwrap()
            .data_ingestion_interval;
        evidence.observed_at_ms = Some(1_000);
        evidence.valid_for_ms = Some(100);
        for (now_ms, usable) in [(999, false), (1_000, true), (1_100, true), (1_101, false)] {
            assert_eq!(lower_promql_workload(&input, now_ms).is_ok(), usable);
            assert_eq!(
                lower_promql_workload_with_histograms(&input, HistogramCatalog::default(), now_ms)
                    .is_ok(),
                usable
            );
        }
        input
            .data_workload
            .as_mut()
            .unwrap()
            .data_ingestion_interval
            .observed_at_ms = None;
        assert!(
            lower_promql_workload_with_histograms(&input, HistogramCatalog::default(), 1_000)
                .is_err()
        );
    }
    use std::time::Duration;

    use asap_types::pre_asap::QueryExpr;
    use asap_types::workload::{
        BatchEntry, DataWorkload, Evidence, PlanningWorkload, Query, QueryRequirements,
        QueryWorkload, TimeSelection,
    };

    use super::*;

    fn workload(query: &str) -> PlanningWorkload {
        PlanningWorkload {
            query_workload: QueryWorkload {
                language: QueryLanguage::PromQL,
                query_batch: Some(vec![BatchEntry {
                    query: Query(query.into()),
                    requirements: QueryRequirements::default(),
                    predictability: Default::default(),
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

    #[test]
    fn instant_selector_uses_declared_ingestion_interval() {
        let query = lower_promql_workload(&workload("sum by (job) (data)"), 0).unwrap();
        let QueryExpr::Aggregate { child, .. } = &query[0] else {
            panic!("expected aggregate")
        };
        assert!(
            matches!(child.as_ref(), QueryExpr::TimeRange { range, child }
            if *range == Duration::from_secs(1) && matches!(child.as_ref(), QueryExpr::Scan { .. }))
        );
    }

    #[test]
    fn explicit_range_selector_keeps_its_query_range() {
        let query = lower_promql_workload(&workload("sum_over_time(data[5m])"), 0).unwrap();
        let QueryExpr::Aggregate { child, .. } = &query[0] else {
            panic!("expected aggregate")
        };
        assert!(
            matches!(child.as_ref(), QueryExpr::TimeRange { range, child }
            if *range == Duration::from_secs(300) && matches!(child.as_ref(), QueryExpr::Scan { .. }))
        );
    }

    #[test]
    fn workload_without_interval_fails_loudly() {
        let mut workload = workload("sum(data)");
        workload.data_workload = Some(DataWorkload::default());
        assert!(matches!(
            lower_promql_workload(&workload, 0),
            Err(PromqlError::InvalidWorkload(
                asap_types::workload::WorkloadError::MissingDataIngestionInterval
            ))
        ));
    }
}
