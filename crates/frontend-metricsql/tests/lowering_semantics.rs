//! MetricsQL lowering semantics: counts, rollup ranges, instant lookback.
use std::time::Duration;

use asap_frontend_metricsql::{lower_metricsql_query_workload, MetricsqlError};
use asap_types::ir::{NonASAPOp, OperatorNode, QueryRoot, TimeRangeKind};
use asap_types::pre_asap::AggIntent;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    BatchEntry, DataWorkload, DurationMs, Evidence, PlanningWorkload, Query, QueryLanguage,
    QueryRequirements, QueryWorkload, TimeSelection,
};

const INTERVAL: Duration = Duration::from_secs(15);

fn lower_metricsql(
    query: &str,
    accuracy: AccuracyTarget,
) -> Result<std::rc::Rc<OperatorNode>, MetricsqlError> {
    asap_frontend_metricsql::lower_metricsql(query, accuracy, INTERVAL)
}

fn workload(language: QueryLanguage, query: &str) -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language,
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
                value: Some(DurationMs(15_000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    }
}

/// The `TimeRange` directly under the first aggregate.
fn time_range_under_aggregate(node: &OperatorNode) -> (Duration, TimeRangeKind) {
    let NonASAPOp::Aggregate { child, .. } = node.expect_non_asap() else {
        panic!("expected an aggregate, got {node:?}");
    };
    let NonASAPOp::TimeRange { range, kind, child } = child.expect_non_asap() else {
        panic!("expected a TimeRange, got {child:?}");
    };
    assert!(matches!(child.expect_non_asap(), NonASAPOp::Scan { .. }));
    (*range, *kind)
}

/// `count(v)` counts series; it is not a distinct count of sample values.
#[test]
fn count_counts_series() {
    let node = lower_metricsql("count(up)", AccuracyTarget::exact()).unwrap();
    let NonASAPOp::Aggregate { measures, .. } = node.expect_non_asap() else {
        panic!("expected an aggregate, got {node:?}");
    };
    assert!(
        matches!(measures.as_slice(), [AggIntent::Count { .. }]),
        "{measures:?}"
    );
}

/// The implicit rollup window is the evaluation step, which lowering does not
/// know, so a rollup without an explicit range is rejected.
#[test]
fn rollups_without_an_explicit_range_are_rejected() {
    for query in [
        "rate(m)",
        "increase(m)",
        "max_over_time(m)",
        "default_rollup(m)",
    ] {
        let error = lower_metricsql(query, AccuracyTarget::exact()).unwrap_err();
        assert!(
            matches!(&error, MetricsqlError::UnsupportedFeature(message) if message.contains("evaluation step")),
            "{query}: {error:?}"
        );
    }
    assert!(lower_metricsql("rate(m[5m])", AccuracyTarget::exact()).is_ok());
}

/// A bare instant selector reads the latest sample within the ingestion
/// interval, as in PromQL; an explicit range keeps its own window.
#[test]
fn instant_selectors_look_back_one_ingestion_interval() {
    let node = lower_metricsql("sum(up)", AccuracyTarget::exact()).unwrap();
    assert_eq!(
        time_range_under_aggregate(&node),
        (INTERVAL, TimeRangeKind::Instant)
    );
    let node = lower_metricsql("rate(m[5m])", AccuracyTarget::exact()).unwrap();
    assert_eq!(
        time_range_under_aggregate(&node),
        (Duration::from_secs(300), TimeRangeKind::Range)
    );
}

/// The workload entry takes the interval from the workload and checks its
/// language and evidence, as `lower_promql_query_workload` does.
#[test]
fn workload_lowering_uses_the_declared_ingestion_interval() {
    let roots =
        lower_metricsql_query_workload(&workload(QueryLanguage::MetricsQL, "sum(up)"), 0).unwrap();
    let [QueryRoot::Operator(node)] = roots.as_slice() else {
        panic!("expected one operator root, got {roots:?}");
    };
    assert_eq!(
        time_range_under_aggregate(node),
        (INTERVAL, TimeRangeKind::Instant)
    );

    assert!(matches!(
        lower_metricsql_query_workload(&workload(QueryLanguage::PromQL, "sum(up)"), 0),
        Err(MetricsqlError::WrongLanguage(_))
    ));
    let mut missing = workload(QueryLanguage::MetricsQL, "sum(up)");
    missing.data_workload = Some(DataWorkload::default());
    assert!(matches!(
        lower_metricsql_query_workload(&missing, 0),
        Err(MetricsqlError::InvalidWorkload(_))
    ));
}
