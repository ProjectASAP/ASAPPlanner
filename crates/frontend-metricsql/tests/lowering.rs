use std::rc::Rc;
use std::time::Duration;

use asap_frontend_metricsql::test_support::{
    canonical_metricsql, lower_metricsql, parse_metricsql,
};
use asap_frontend_metricsql::MetricsqlError;
use asap_types::ir::operator::{
    AggIntent, GroupSide, Reduction, Source, VectorGrouping, VectorMatch, VectorMatchKind,
};
use asap_types::ir::{NonASAPOp, OperatorNode, TimeRangeKind};

/// Every metric's sample interval, the lookback of a bare selector.
const INTERVAL: Duration = Duration::from_secs(15);

fn lower(query: &str) -> Rc<OperatorNode> {
    lower_metricsql(query, INTERVAL).unwrap()
}

#[test]
fn selector_range_aggregate_and_call_share_the_canonical_shape() {
    let query = r#"sum by (job) (rate(http_requests_total{status=~"5.."}[5m]))"#;
    let dag = lower(query);
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = dag.expect_non_asap()
    else {
        panic!("expected outer aggregate");
    };
    assert_eq!(reduction, &Reduction::by(vec![2]));
    assert!(matches!(measures.as_slice(), [AggIntent::Sum { .. }]));
    let NonASAPOp::Aggregate {
        measures, child, ..
    } = child.expect_non_asap()
    else {
        panic!("expected rate aggregate");
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Rate]));
    let NonASAPOp::TimeRange { range, child, .. } = child.expect_non_asap() else {
        panic!("expected range");
    };
    assert_eq!(*range, Duration::from_secs(300));
    assert!(
        matches!(child.expect_non_asap(), NonASAPOp::Scan { source: Source::TimeSeries { metric }, predicates, .. } if metric == "http_requests_total" && predicates.len() == 1)
    );
}

#[test]
fn default_rollup_with_explicit_range_is_last_over_time() {
    let dag = lower("default_rollup(cpu_usage[5m])");
    let NonASAPOp::Aggregate {
        reduction,
        measures,
        child,
        ..
    } = dag.expect_non_asap()
    else {
        panic!("expected aggregate");
    };
    assert_eq!(reduction, &Reduction::PerEntity);
    assert!(matches!(measures.as_slice(), [AggIntent::LastOverTime]));
    assert!(
        matches!(child.expect_non_asap(), NonASAPOp::TimeRange { range, kind, .. }
        if *range == Duration::from_secs(300) && *kind == TimeRangeKind::Range)
    );
}

#[test]
fn keep_metric_names_is_preserved_in_ast_and_rejected_without_lineage() {
    let ast = parse_metricsql("rate(requests_total[5m]) keep_metric_names").unwrap();
    assert!(ast.keep_metric_names());
    let error =
        lower_metricsql("rate(requests_total[5m]) keep_metric_names", INTERVAL).unwrap_err();
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
    ];
    for query in cases {
        let ast = parse_metricsql(query)
            .unwrap_or_else(|error| panic!("native MetricsQL parser rejected {query:?}: {error}"));
        assert!(!ast.to_string().is_empty());
        let result = lower_metricsql(query, INTERVAL);
        assert!(
            result.is_err(),
            "extension semantics must be represented or routed to exact fallback: {query}"
        );
    }

    assert_eq!(
        lower(r#"WITH (prefix="http_") {__name__=prefix+"requests_total"}"#),
        lower("http_requests_total"),
        "a fully expanded WITH selector has ordinary selector semantics"
    );
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
        let error = lower_metricsql(query, INTERVAL).unwrap_err();
        assert!(
            matches!(error, MetricsqlError::UnsupportedFeature(message) if message.contains("requires exactly 1")),
            "{query} must not silently discard an aggregate input"
        );
    }
}

#[test]
fn supported_parameterized_functions_require_their_exact_arity() {
    let quantile = lower("quantile(0.9, requests_total)");
    assert!(matches!(
        quantile.expect_non_asap(),
        NonASAPOp::Aggregate { .. }
    ));
    let rollup = lower("quantile_over_time(0.9, requests_total[5m])");
    assert!(matches!(
        rollup.expect_non_asap(),
        NonASAPOp::Aggregate { .. }
    ));
}

/// The vector-vector `BinaryOp` at the root of `query`: its matching and
/// whether it returns 0/1.
fn root_binary(query: &str) -> (Option<VectorMatch>, bool) {
    let dag = lower(query);
    let NonASAPOp::BinaryOp {
        operator,
        return_bool,
        ..
    } = dag.expect_non_asap()
    else {
        panic!("{query}: expected a binary operator, got {dag:?}");
    };
    (operator.vector_match.clone(), *return_bool)
}

/// `on`/`ignoring` choose the matching labels and `group_left`/`group_right`
/// the many side with its extra labels, as in PromQL. A set operator
/// without a written modifier matches on every shared label, which is
/// `ignoring()` (the PromQL frontend's form); arithmetic without one keeps
/// the default one-to-one matching.
#[test]
fn binary_matching_modifiers_lower_to_vector_match() {
    let matched = |kind, labels: &[&str], grouping: Option<(GroupSide, &[&str])>| {
        Some(VectorMatch {
            kind,
            labels: labels.iter().map(|l| l.to_string()).collect(),
            grouping: grouping.map(|(side, labels)| VectorGrouping {
                side,
                labels: labels.iter().map(|l| l.to_string()).collect(),
            }),
        })
    };
    let cases = [
        (
            "a / on(job) group_left(team) b",
            matched(
                VectorMatchKind::On,
                &["job"],
                Some((GroupSide::Left, &["team"])),
            ),
        ),
        (
            "a - ignoring(instance) group_right b",
            matched(
                VectorMatchKind::Ignoring,
                &["instance"],
                Some((GroupSide::Right, &[])),
            ),
        ),
        (
            "a * on(job) b",
            matched(VectorMatchKind::On, &["job"], None),
        ),
        ("a and b", matched(VectorMatchKind::Ignoring, &[], None)),
        ("a or b", matched(VectorMatchKind::Ignoring, &[], None)),
        (
            "a unless on(job) b",
            matched(VectorMatchKind::On, &["job"], None),
        ),
        ("a + b", None),
    ];
    for (query, expected) in cases {
        assert_eq!(root_binary(query), (expected, false), "{query}");
    }
}

/// `bool` makes a comparison answer 0 or 1 instead of filtering.
#[test]
fn bool_modifier_sets_return_bool() {
    let (_, return_bool) = root_binary("a > bool b");
    assert!(return_bool);
    let (_, return_bool) = root_binary("a > bool on(job) b");
    assert!(return_bool);
    assert!(lower_metricsql("a > bool 10", INTERVAL).is_ok());
}

/// `keep_metric_names` on a binary operator needs metric-name lineage,
/// which lowering does not track, so it is still refused.
#[test]
fn keep_metric_names_on_a_binary_operator_is_rejected() {
    let error = lower_metricsql("a + b keep_metric_names", INTERVAL).unwrap_err();
    assert!(
        matches!(&error, MetricsqlError::UnsupportedFeature(message) if message.contains("keep_metric_names")),
        "{error:?}"
    );
}

/// The MetricsQL-only operators `if`, `ifnot` and `default` have no IR form
/// yet and stay refused.
#[test]
fn metricsql_only_binary_operators_are_refused() {
    for query in ["a if b", "a ifnot b", "a default b"] {
        let error = lower_metricsql(query, INTERVAL).unwrap_err();
        assert!(
            matches!(&error, MetricsqlError::UnsupportedFeature(message) if message.contains("MetricsQL operator")),
            "{query}: {error:?}"
        );
    }
}
