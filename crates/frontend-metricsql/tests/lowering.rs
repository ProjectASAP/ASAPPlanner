use std::time::Duration;

use asap_frontend_metricsql::{
    canonical_metricsql, lower_metricsql, parse_metricsql, MetricsqlError,
};
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
    assert!(ast.keep_metric_names());
    let error = lower_metricsql(
        "rate(requests_total[5m]) keep_metric_names",
        AccuracyTarget::Exact,
    )
    .unwrap_err();
    assert!(
        matches!(error, MetricsqlError::UnsupportedFeature(message) if message.contains("metric-name lineage"))
    );
}

/// Representative MetricsQL-only forms from VictoriaMetrics' parser/docs
/// corpus. Source: app/vmselect/vmui/assets/MetricsQL-*.md and
/// app/vmselect/promql/exec_test.go in VictoriaMetrics/VictoriaMetrics.
#[test]
fn victoria_metrics_extension_corpus_parses_natively_and_fails_closed() {
    let cases = [
        r#"rate({__name__=~"foo|bar"}[5m]) keep_metric_names"#,
        "time() ifnot time() > 1400 default -time()",
        "rate(foo[5i])",
        "sum(foo) by (job) limit 10",
        r#"foo{job="a" or job="b"}"#,
        r#"WITH (prefix="http_") {__name__=prefix+"requests_total"}"#,
    ];
    for query in cases {
        let ast = parse_metricsql(query)
            .unwrap_or_else(|error| panic!("native MetricsQL parser rejected {query:?}: {error}"));
        assert!(!ast.to_string().is_empty());
        let result = lower_metricsql(query, AccuracyTarget::Exact);
        assert!(
            result.is_err(),
            "extension semantics must be represented or routed to exact fallback: {query}"
        );
    }
}

#[test]
fn canonical_identity_ignores_compatible_formatting() {
    let compact = canonical_metricsql("sum by(job)(rate(requests_total[5m]))").unwrap();
    let spaced = canonical_metricsql("  sum by ( job ) ( rate( requests_total[5m] ) )  ").unwrap();
    assert_eq!(compact, spaced);
}

#[test]
fn canonical_identity_recursively_formats_metricsql_extensions() {
    let compact =
        canonical_metricsql("default_rollup(requests_total[5m]) keep_metric_names").unwrap();
    let spaced =
        canonical_metricsql(" default_rollup( requests_total[5m] )   keep_metric_names ").unwrap();
    assert_eq!(compact, spaced);
    assert_eq!(
        compact,
        "default_rollup(requests_total[5m]) keep_metric_names"
    );
}

#[test]
fn metricsql_multi_argument_aggregates_fail_closed() {
    for query in ["sum(foo, bar)", "avg(foo, bar)", "count(foo, bar)"] {
        parse_metricsql(query).expect("MetricsQL accepts multi-argument aggregates");
        let error = lower_metricsql(query, AccuracyTarget::Exact).unwrap_err();
        assert!(
            matches!(error, MetricsqlError::UnsupportedFeature(message) if message.contains("requires exactly 1")),
            "{query} must not silently discard an aggregate input"
        );
    }
}

#[test]
fn supported_parameterized_functions_require_their_exact_arity() {
    let quantile = lower("quantile(0.9, requests_total)");
    assert!(matches!(quantile, QueryExpr::Aggregate { .. }));
    let rollup = lower("quantile_over_time(0.9, requests_total[5m])");
    assert!(matches!(rollup, QueryExpr::Aggregate { .. }));
}
