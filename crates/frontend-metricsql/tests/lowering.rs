use std::time::Duration;

use asap_frontend_metricsql::{lower_metricsql, parse_metricsql, MetricsqlError, MetricsqlExpr};
use asap_types::pre_asap::{AggIntent, QueryExpr, Reduction, Source};
use asap_types::types::AccuracyTarget;

fn lower(query: &str) -> QueryExpr {
    lower_metricsql(query, AccuracyTarget::Epsilon(0.01)).unwrap()
}

#[test]
fn selector_range_aggregate_and_call_share_the_canonical_shape() {
    let query = r#"sum by (job) (rate(http_requests_total{status=~"5.."}[5m]))"#;
    let tree = lower(query);
    let QueryExpr::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = tree
    else {
        panic!("expected outer aggregate");
    };
    assert_eq!(reduction, Reduction::by(vec![2]));
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    let QueryExpr::Aggregate {
        measures, child, ..
    } = child.as_ref()
    else {
        panic!("expected rate aggregate");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Rate]));
    let QueryExpr::TimeRange { range, child } = child.as_ref() else {
        panic!("expected range");
    };
    assert_eq!(*range, Duration::from_secs(300));
    assert!(
        matches!(child.as_ref(), QueryExpr::Scan { source: Source::TimeSeries { metric }, predicates, .. } if metric == "http_requests_total" && predicates.len() == 1)
    );
}

#[test]
fn default_rollup_with_explicit_range_is_last_over_time() {
    let tree = lower("default_rollup(cpu_usage[5m])");
    let QueryExpr::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = tree
    else {
        panic!("expected aggregate");
    };
    assert_eq!(reduction, Reduction::PerEntity);
    assert!(matches!(measures.as_slice(), [AggIntent::LastOverTime]));
    assert!(
        matches!(child.as_ref(), QueryExpr::TimeRange { range, .. } if *range == Duration::from_secs(300))
    );
}

#[test]
fn implicit_default_rollup_requires_runtime_step() {
    let error = lower_metricsql("default_rollup(cpu_usage)", AccuracyTarget::Exact).unwrap_err();
    assert!(
        matches!(error, MetricsqlError::UnsupportedFeature(message) if message.contains("evaluation step"))
    );
}

#[test]
fn keep_metric_names_is_preserved_in_ast_and_rejected_without_lineage() {
    let ast = parse_metricsql("rate(requests_total[5m]) keep_metric_names").unwrap();
    assert!(matches!(ast, MetricsqlExpr::KeepMetricNames(_)));
    let error = lower_metricsql(
        "rate(requests_total[5m]) keep_metric_names",
        AccuracyTarget::Exact,
    )
    .unwrap_err();
    assert!(
        matches!(error, MetricsqlError::UnsupportedFeature(message) if message.contains("metric-name lineage"))
    );
}
