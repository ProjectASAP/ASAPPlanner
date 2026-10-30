//! PromQL front end: parse (via `promql-parser`) → the name-based
//! [`UnresolvedOp`](asap_frontend_common::UnresolvedOp) tree, built directly
//! in canonical shape (issue #179) → [`resolve_root`].
//!
//! `resolve_root` runs the frontend-common `SchemaResolver` for positional
//! name resolution and returns the unified
//! [`OperatorNode`](asap_types::ir::OperatorNode) DAG. Depends on the PromQL
//! parser only — never on the SQL / DataFusion stack.

mod error;
mod histogram;
mod promql;

use std::collections::BTreeSet;

use asap_types::ir::QueryRoot;
use asap_types::workload::{DataWorkload, DurationMs, MetricType, PlanningWorkload, QueryLanguage};

pub use error::PromqlError;
pub use histogram::{HistogramCatalog, HistogramKind};

/// The metrics `query`'s selectors read, by name: the sources whose facts
/// (such as `sample_interval`) its lowering looks up. Selectors that
/// lowering rejects, such as one with no metric name, are skipped.
pub fn promql_metric_names(query: &str) -> Result<BTreeSet<String>, PromqlError> {
    promql::metric_names(query)
}

/// One warning per `rate`, `increase` or `irate` call in `query` (one per
/// function and metric) whose argument reads a metric `data` declares a
/// gauge ([`MetricType::Gauge`]): those functions assume a counter, so a
/// gauge's drop reads as a reset. The query still plans as written; a
/// counter or a metric of undeclared type gets none.
pub fn counter_function_over_gauge_warnings(
    query: &str,
    data: &DataWorkload,
) -> Result<Vec<String>, PromqlError> {
    let declared_gauge = |metric: &str| {
        data.sources
            .get(metric)
            .is_some_and(|facts| facts.metric_type == Some(MetricType::Gauge))
    };
    Ok(promql::counter_function_metrics(query)?
        .into_iter()
        .filter(|(_, metric)| declared_gauge(metric))
        .map(|(function, metric)| {
            format!(
                "{function} over {metric}, which the data workload declares a gauge: \
                 {function} assumes a counter (a drop is a reset); planned as written"
            )
        })
        .collect())
}

/// Lower every normalized PromQL workload entry to its query root: an
/// operator DAG, or a scalar root that introduces no constant operator.
///
/// `histograms` distinguishes classic bucket interpolation from generic
/// sketchable quantiles; an empty catalog falls back to query structure.
///
/// A bare instant selector reads the latest sample within its metric's
/// `sample_interval`, declared in the data workload's
/// [`SourceFacts`](asap_types::workload::SourceFacts) whatever the arrival;
/// a query with one fails with `WorkloadError::MissingSampleInterval` when
/// that metric declares none. Explicit range selectors keep their
/// query-specified range and need no interval.
pub fn lower_promql_query_workload(
    workload: &PlanningWorkload,
    histograms: &HistogramCatalog,
) -> Result<Vec<QueryRoot>, PromqlError> {
    let _guard = histogram::CatalogGuard::install(histograms.clone());
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
    use std::rc::Rc;
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
                    range_evaluation: None,
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
        let query = lower_promql_query_workload(
            &workload("sum by (job) (data)"),
            &HistogramCatalog::default(),
        )
        .unwrap()
        .remove(0)
        .as_operator()
        .cloned()
        .expect("operator root");
        let NonASAPOp::Aggregate { child, .. } = query.expect_non_asap() else {
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
        let query = lower_promql_query_workload(
            &workload("sum(data) + sum(other)"),
            &HistogramCatalog::default(),
        )
        .unwrap()
        .remove(0)
        .as_operator()
        .cloned()
        .expect("operator root");
        let mut lookbacks = instant_lookbacks(&query);
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
        let query = lower_promql_query_workload(
            &workload("sum_over_time(data[5m])"),
            &HistogramCatalog::default(),
        )
        .unwrap()
        .remove(0)
        .as_operator()
        .cloned()
        .expect("operator root");
        let NonASAPOp::Aggregate { child, .. } = query.expect_non_asap() else {
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
    // an aggregation, or inside a subquery) fails, even when another metric
    // declares one; range-only queries still lower.
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
        let lower = |input: &PlanningWorkload| {
            lower_promql_query_workload(input, &HistogramCatalog::default())
        };
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
                        lower(&with(query)),
                        Err(PromqlError::InvalidWorkload(
                            WorkloadError::MissingSampleInterval
                        ))
                    ),
                    "{arrival:?} {query}"
                );
            }
            lower(&with("quantile_over_time(0.99, undeclared[5y] offset 1y)")).unwrap();
            // The declared metric lowers whatever the arrival.
            if arrival.is_some() {
                lower(&with("sum(data)")).unwrap();
            }
        }
    }

    // `rate`, `increase` and `irate` assume a counter: over a metric the
    // data workload declares a gauge each call warns once, naming the
    // function and the metric; a counter or an undeclared type does not.
    #[test]
    fn counter_functions_over_a_declared_gauge_warn() {
        use asap_types::workload::MetricType;
        let typed = |metric_type| SourceFacts {
            metric_type,
            ..SourceFacts::default()
        };
        let data = DataWorkload {
            arrival: DataArrival::Unknown,
            sources: [
                ("g".to_string(), typed(Some(MetricType::Gauge))),
                ("c".to_string(), typed(Some(MetricType::Counter))),
                ("u".to_string(), typed(None)),
            ]
            .into(),
        };
        let warnings = |query: &str| counter_function_over_gauge_warnings(query, &data).unwrap();
        assert_eq!(
            warnings("sum by (job) (rate(g[5m])) / sum(irate(g[1m])) + increase(c[5m])"),
            [
                "irate over g, which the data workload declares a gauge: irate assumes a \
                 counter (a drop is a reset); planned as written",
                "rate over g, which the data workload declares a gauge: rate assumes a \
                 counter (a drop is a reset); planned as written",
            ]
        );
        assert_eq!(
            warnings("rate(g[5m]) + rate(g[5m] offset 1h)").len(),
            1,
            "one warning per function and metric"
        );
        assert!(warnings("increase(c[5m]) + rate(u[5m]) + delta(g[5m]) + g").is_empty());
        assert!(warnings("rate(max_over_time(g[1m])[5m:1m])").len() == 1);
        assert!(counter_function_over_gauge_warnings("rate(", &data).is_err());
    }
}
