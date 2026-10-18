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

use std::collections::BTreeSet;
use std::rc::Rc;

use asap_types::ir::OperatorNode;
use asap_types::workload::{DurationMs, PlanningWorkload, QueryLanguage};

pub use error::PromqlError;
pub use histogram::{HistogramCatalog, HistogramKind};

/// Lower every normalized PromQL workload entry to a plan-ready operator DAG.
///
/// A bare instant selector reads the latest sample within its metric's
/// `sample_interval`, declared in the data workload's
/// [`SourceFacts`](asap_types::workload::SourceFacts) whatever the arrival;
/// a query with one fails with `WorkloadError::MissingSampleInterval` when
/// that metric declares none. Explicit range selectors keep their
/// query-specified range and need no interval.
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

/// The metrics `query`'s selectors read, by name: the sources whose facts
/// (such as `sample_interval`) its lowering looks up. Selectors that
/// lowering rejects, such as one with no metric name, are skipped.
pub fn promql_metric_names(query: &str) -> Result<BTreeSet<String>, PromqlError> {
    promql::metric_names(query)
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
    let sample_interval_of = |metric: &str| {
        workload
            .sample_interval_of(metric)
            .map(|DurationMs(ms)| std::time::Duration::from_millis(ms))
    };
    workload
        .query_workload
        .entries()
        .map(|entry| {
            let root = promql::PromqlLowerer::lower_query_with_sample_intervals(
                &entry.query.0,
                &entry.requirements.accuracy.clone(),
                &sample_interval_of,
            )?;
            Ok(root)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use asap_types::ir::operator::Source;
    use asap_types::ir::{NonASAPOp, OperatorNode, TimeRangeKind};
    use asap_types::workload::{
        BatchEntry, DataArrival, DataWorkload, PlanningWorkload, Query, QueryRequirements,
        QueryWorkload, SourceFacts, WorkloadError,
    };

    use super::*;

    /// `query` over data whose metrics `data` and `other` are sampled every
    /// 1 s and 30 s.
    fn workload(query: &str) -> PlanningWorkload {
        let sampled = |ms: u64| SourceFacts {
            sample_interval: Some(DurationMs(ms)),
            ..SourceFacts::default()
        };
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
                arrival: DataArrival::Unknown,
                sources: [
                    ("data".to_string(), sampled(1_000)),
                    ("other".to_string(), sampled(30_000)),
                ]
                .into(),
            }),
        }
    }

    /// Each `Instant` lookback under `node`, with the metric it reads.
    fn instant_lookbacks(node: &Rc<OperatorNode>) -> Vec<(String, Duration)> {
        OperatorNode::reachable(node)
            .iter()
            .filter_map(|node| match node.non_asap()? {
                NonASAPOp::TimeRange {
                    range,
                    kind: TimeRangeKind::Instant,
                    child,
                } => match child.non_asap()? {
                    NonASAPOp::Scan {
                        source: Source::TimeSeries { metric },
                        ..
                    } => Some((metric.clone(), *range)),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    // A bare instant selector reads the latest sample within its metric's
    // declared sample interval: an `Instant` lookback of that length.
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

    // Two metrics sampled at different intervals: each selector looks back
    // its own metric's interval.
    #[test]
    fn each_instant_selector_uses_its_own_metrics_sample_interval() {
        let query = lower_promql_workload(&workload("sum(data) + sum(other)")).unwrap();
        let mut lookbacks = instant_lookbacks(&query[0]);
        lookbacks.sort();
        assert_eq!(
            lookbacks,
            [
                ("data".to_string(), Duration::from_secs(1)),
                ("other".to_string(), Duration::from_secs(30)),
            ]
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

    // A metric without a declared sample interval, whatever the arrival
    // (and with no data workload): an instant selector over it (bare, under
    // an aggregation, or inside a subquery) fails, with and without
    // histograms, even when another metric declares one; range-only
    // queries still lower.
    #[test]
    fn instant_selector_needs_its_metrics_sample_interval() {
        let arrivals = [
            Some(DataArrival::AtRest),
            Some(DataArrival::Unknown),
            Some(DataArrival::Ingesting {
                rows_per_second: 1.0,
            }),
            None,
        ];
        for arrival in arrivals {
            let with = |query: &str| {
                let mut input = workload(query);
                match arrival {
                    Some(arrival) => input.data_workload.as_mut().unwrap().arrival = arrival,
                    None => input.data_workload = None,
                }
                input
            };
            for query in [
                "undeclared",
                "sum(undeclared)",
                "max_over_time(undeclared[5m:1m])",
                "sum(data) + sum(undeclared)",
            ] {
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
            lower_promql_workload(&with("quantile_over_time(0.99, undeclared[5y] offset 1y)"))
                .unwrap();
            // The declared metric lowers whatever the arrival.
            if arrival.is_some() {
                lower_promql_workload(&with("sum(data)")).unwrap();
            }
        }
    }
}
