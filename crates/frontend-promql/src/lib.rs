//! PromQL front end: parse (via `promql-parser`) → the name-based
//! [`UnresolvedOp`](asap_frontend_common::UnresolvedOp) tree, built directly
//! in canonical shape (issue #179) → [`resolve_root`].
//!
//! `resolve_root` runs the
//! [`SchemaResolver`](asap_frontend_common::SchemaResolver) for positional
//! name resolution and returns the unified
//! [`OperatorNode`](asap_types::ir::OperatorNode) DAG. Depends on the PromQL
//! parser only — never on the SQL / DataFusion stack.

pub mod error;
pub mod histogram;
pub mod promql;

use std::rc::Rc;

use asap_types::ir::OperatorNode;
use asap_types::workload::{DurationMs, PlanningWorkload, QueryLanguage};

pub use error::PromqlError;
pub use histogram::{HistogramCatalog, HistogramKind};

/// Lower every normalized PromQL workload entry to a plan-ready operator DAG.
///
/// A bare instant selector reads the latest sample within the data
/// workload's `sample_interval` (declared by an `Ingesting` or `Mixed`
/// [`DataArrival`](asap_types::workload::DataArrival)); a query with one
/// fails with `WorkloadError::MissingSampleInterval` when none is declared.
/// Explicit range selectors keep their query-specified range and need no
/// interval.
pub fn lower_promql_workload(
    workload: &PlanningWorkload,
) -> Result<Vec<Rc<OperatorNode>>, PromqlError> {
    lower_promql_workload_inner(workload)
}

/// Like [`lower_promql_workload`], but uses `histograms` to distinguish classic
/// bucket interpolation from generic sketchable quantiles.
pub fn lower_promql_workload_with_histograms(
    workload: &PlanningWorkload,
    histograms: HistogramCatalog,
) -> Result<Vec<Rc<OperatorNode>>, PromqlError> {
    let _guard = histogram::CatalogGuard::install(histograms);
    lower_promql_workload_inner(workload)
}

/// Lower scalar and vector query roots without introducing constant operators.
pub fn lower_promql_query_workload(
    workload: &PlanningWorkload,
) -> Result<Vec<asap_types::ir::QueryRoot>, PromqlError> {
    lower_promql_query_workload_inner(workload)
}

pub fn lower_promql_query_workload_with_histograms(
    workload: &PlanningWorkload,
    histograms: HistogramCatalog,
) -> Result<Vec<asap_types::ir::QueryRoot>, PromqlError> {
    let _guard = histogram::CatalogGuard::install(histograms);
    lower_promql_query_workload_inner(workload)
}

fn lower_promql_workload_inner(
    workload: &PlanningWorkload,
) -> Result<Vec<Rc<OperatorNode>>, PromqlError> {
    lower_promql_query_workload_inner(workload)?
        .into_iter()
        .map(|root| match root {
            asap_types::ir::QueryRoot::Operator(node) => Ok(node),
            asap_types::ir::QueryRoot::Scalar(_) => Err(PromqlError::UnsupportedFeature(
                "scalar root: use lower_promql_query_workload".into(),
            )),
        })
        .collect()
}

fn lower_promql_query_workload_inner(
    workload: &PlanningWorkload,
) -> Result<Vec<asap_types::ir::QueryRoot>, PromqlError> {
    if !matches!(workload.query_workload.language, QueryLanguage::PromQL) {
        return Err(PromqlError::WrongLanguage(format!(
            "{:?}",
            workload.query_workload.language
        )));
    }
    workload.validate()?;
    let sample_interval = workload
        .sample_interval()
        .map(|DurationMs(ms)| std::time::Duration::from_millis(ms));
    workload
        .query_workload
        .entries()
        .map(|entry| {
            let root = promql::PromqlLowerer::lower_query_with_sample_interval(
                &entry.query.0,
                &entry.requirements.accuracy.clone(),
                sample_interval,
            )?;
            Ok(root)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use asap_types::ir::{NonASAPOp, TimeRangeKind};
    use asap_types::workload::{
        BatchEntry, DataArrival, DataWorkload, PlanningWorkload, Query, QueryRequirements,
        QueryWorkload, WorkloadError,
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
                }]),
                repeating_queries: None,
            },
            data_workload: Some(DataWorkload {
                arrival: DataArrival::Ingesting {
                    rows_per_second: 1_000.0,
                    sample_interval: Some(DurationMs(1_000)),
                },
                ..Default::default()
            }),
        }
    }

    // A bare instant selector reads the latest sample within the declared
    // sample interval: an `Instant` lookback of that length.
    #[test]
    fn instant_selector_uses_declared_sample_interval() {
        let query = lower_promql_workload(&workload("sum by (job) (data)")).unwrap();
        let NonASAPOp::Aggregate { child, .. } = query[0].expect_non_asap() else {
            panic!("expected aggregate")
        };
        assert!(
            matches!(child.expect_non_asap(), NonASAPOp::TimeRange { range, kind, child }
            if *range == Duration::from_secs(1)
                && *kind == TimeRangeKind::Instant
                && matches!(child.expect_non_asap(), NonASAPOp::Scan { .. }))
        );
    }

    // An explicit `m[5m]` keeps its own window as a `Range` selection.
    #[test]
    fn explicit_range_selector_keeps_its_query_range() {
        let query = lower_promql_workload(&workload("sum_over_time(data[5m])")).unwrap();
        let NonASAPOp::Aggregate { child, .. } = query[0].expect_non_asap() else {
            panic!("expected aggregate")
        };
        assert!(
            matches!(child.expect_non_asap(), NonASAPOp::TimeRange { range, kind, child }
            if *range == Duration::from_secs(300)
                && *kind == TimeRangeKind::Range
                && matches!(child.expect_non_asap(), NonASAPOp::Scan { .. }))
        );
    }

    // Data at rest, an unknown arrival, no data workload, or an ingesting
    // arrival without `sample_interval` declare no interval: an instant
    // selector (bare, under an aggregation, or inside a subquery) fails,
    // with and without histograms, while range-only queries still lower.
    #[test]
    fn instant_selector_needs_a_sample_interval() {
        let arrivals = [
            Some(DataArrival::AtRest),
            Some(DataArrival::Unknown),
            Some(DataArrival::Ingesting {
                rows_per_second: 1.0,
                sample_interval: None,
            }),
            None,
        ];
        for arrival in arrivals {
            let with = |query: &str| {
                let mut input = workload(query);
                input.data_workload = arrival.map(|arrival| DataWorkload {
                    arrival,
                    ..Default::default()
                });
                input
            };
            for query in ["data", "sum(data)", "max_over_time(data[5m:1m])"] {
                assert!(
                    matches!(
                        lower_promql_workload(&with(query)),
                        Err(PromqlError::InvalidWorkload(
                            WorkloadError::MissingSampleInterval
                        ))
                    ),
                    "{arrival:?} {query}"
                );
                assert!(matches!(
                    lower_promql_workload_with_histograms(
                        &with(query),
                        HistogramCatalog::default()
                    ),
                    Err(PromqlError::InvalidWorkload(
                        WorkloadError::MissingSampleInterval
                    ))
                ));
            }
            lower_promql_workload(&with("quantile_over_time(0.99, data[5y] offset 1y)")).unwrap();
        }
    }
}
