//! `histogram_quantile` and Prometheus summaries driven by the data
//! workload's declared metric types (issues #79, #1165).
//!
//! Float input to `histogram_quantile` is classic buckets, as the PromQL spec
//! reads it, whether declared a histogram or undeclared; no decision depends
//! on a `_bucket` name or a `by (le)` grouping. A declared native histogram
//! or summary is rejected. A declared histogram or summary family `foo` is
//! keyed by its family name and supplies its `_bucket`, `_sum` and `_count`
//! series.

use crate::support;
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::{NonASAPOp, OperatorNode};
use asap_types::workload::bounds::declared_non_negative;
use asap_types::workload::{DurationMs, PrometheusMetricType, SourceFacts};
use support::{lower_promql, lower_promql_with_metric_types};

use PrometheusMetricType::{Histogram, NativeHistogram, Summary};

/// The histogram/quantile intent kind in the lowered DAG: `"HQ"` for the
/// classic-bucket `HistogramQuantile`, `"Q"` for a `Quantile`, `None` for
/// neither.
fn quantile_kind(qe: &OperatorNode) -> Option<&'static str> {
    match qe.expect_non_asap() {
        NonASAPOp::Aggregate {
            measures, child, ..
        } => measures
            .iter()
            .find_map(|i| match i {
                AggIntent::HistogramQuantile { .. } => Some("HQ"),
                AggIntent::Quantile { .. } => Some("Q"),
                _ => None,
            })
            .or_else(|| quantile_kind(child)),
        NonASAPOp::TimeRange { child, .. }
        | NonASAPOp::Filter { child, .. }
        | NonASAPOp::Sort { child, .. }
        | NonASAPOp::Limit { child, .. }
        | NonASAPOp::PromqlSubquery { child, .. }
        | NonASAPOp::Project { child, .. } => quantile_kind(child),
        _ => None,
    }
}

/// `query`'s quantile intent with no declared metric type.
fn undeclared(query: &str) -> Option<&'static str> {
    quantile_kind(&lower_promql(query, AccuracyTarget::exact()).unwrap())
}

/// `query`'s quantile intent with `metric` declared of `metric_type`.
fn declared(query: &str, metric: &str, metric_type: PrometheusMetricType) -> Option<&'static str> {
    quantile_kind(
        &lower_promql_with_metric_types(query, AccuracyTarget::exact(), &[(metric, metric_type)])
            .unwrap(),
    )
}

/// The data workload of `query` declaring only `family`, of `metric_type`
/// and sampled every second: the query's series resolve to it by name.
fn over_family(
    query: &str,
    family: &str,
    metric_type: PrometheusMetricType,
) -> asap_types::workload::PlanningWorkload {
    let mut workload = support::workload(query, AccuracyTarget::exact());
    workload.data_workload.as_mut().unwrap().sources = [(
        family.to_string(),
        SourceFacts {
            metric_type: Some(metric_type),
            sample_interval: Some(DurationMs(1_000)),
            ..SourceFacts::default()
        },
    )]
    .into();
    workload
}

/// Whether `query` lowers over data declaring only `family` of `metric_type`.
fn lowers_over_family(query: &str, family: &str, metric_type: PrometheusMetricType) -> bool {
    asap_frontend_promql::lower_promql_query_workload(&over_family(query, family, metric_type))
        .is_ok()
}

/// A classic histogram without the `_bucket` suffix is classic buckets,
/// undeclared (as the PromQL spec reads float samples) or declared a histogram.
#[test]
fn suffixless_float_input_is_classic_buckets() {
    let q = "histogram_quantile(0.9, latency_seconds)";
    assert_eq!(undeclared(q), Some("HQ"));
    assert_eq!(declared(q, "latency_seconds", Histogram), Some("HQ"));
}

/// `_bucket` series and `by (le)` arguments stay classic buckets with no
/// declaration, now without a name check.
#[test]
fn bucket_named_input_is_classic_buckets_without_a_declaration() {
    for q in [
        "histogram_quantile(0.9, foo_bucket)",
        "histogram_quantile(0.9, sum by (le) (rate(x_bucket[5m])))",
        "histogram_quantile(0.9, sum by (le) (rate(http_request_duration_seconds_bucket[5m])))",
    ] {
        assert_eq!(undeclared(q), Some("HQ"), "{q}");
    }
}

/// A metric declared a native histogram is rejected, under
/// `histogram_quantile(s)` and as a bare read.
#[test]
fn declared_native_histogram_is_rejected() {
    for q in [
        "histogram_quantile(0.9, foo_bucket)",
        r#"histogram_quantiles(foo_bucket, "q", 0.5, 0.9)"#,
        "foo_bucket",
    ] {
        let lowered = lower_promql_with_metric_types(
            q,
            AccuracyTarget::exact(),
            &[("foo_bucket", NativeHistogram)],
        );
        assert!(
            matches!(&lowered, Err(e) if e.to_string().contains("native histogram")),
            "{q}: {lowered:?}"
        );
    }
}

/// A family `foo` declared a native histogram is rejected under
/// `histogram_quantile(0.9, foo)`, the form a user writes for one.
#[test]
fn declared_native_histogram_family_is_rejected() {
    let lowered = asap_frontend_promql::lower_promql_query_workload(&over_family(
        "histogram_quantile(0.9, foo)",
        "foo",
        NativeHistogram,
    ));
    assert!(
        matches!(&lowered, Err(e) if e.to_string().contains("native histogram")),
        "{lowered:?}"
    );
}

/// A declared summary family `foo`: a quantile series is a plain value read
/// (no quantile computed), `histogram_quantile(s)` over it is rejected, and
/// `foo_sum`/`foo_count` read the family's facts as counters.
#[test]
fn declared_summary_family() {
    assert!(lowers_over_family(
        r#"foo{quantile="0.99"}"#,
        "foo",
        Summary
    ));
    assert_eq!(
        quantile_kind(
            &asap_frontend_promql::lower_promql_query_workload(&over_family(
                r#"avg(foo{quantile="0.99"})"#,
                "foo",
                Summary
            ))
            .unwrap()[0]
                .as_operator()
                .cloned()
                .unwrap()
        ),
        None,
        "averaging quantile series computes no quantile"
    );
    for q in [
        "histogram_quantile(0.9, foo)",
        r#"histogram_quantiles(foo, "q", 0.5, 0.9)"#,
    ] {
        let lowered =
            asap_frontend_promql::lower_promql_query_workload(&over_family(q, "foo", Summary));
        assert!(
            matches!(&lowered, Err(e) if e.to_string().contains("summary")),
            "{q}: {lowered:?}"
        );
    }
    // `foo_sum` takes the family's sample interval and reads as a counter.
    assert!(lowers_over_family(
        "rate(foo_sum[5m]) / rate(foo_count[5m]) + foo_sum",
        "foo",
        Summary
    ));
    let data = over_family("", "foo", Summary).data_workload.unwrap();
    assert!(declared_non_negative(&data, "foo_sum"));
    assert!(declared_non_negative(&data, "foo_count"));
    assert!(!declared_non_negative(&data, "foo"));
}

/// A declared histogram family `foo`: `foo_sum` and `foo_count` read as
/// counters, and `histogram_quantile` over `foo_bucket` is classic buckets.
#[test]
fn declared_histogram_family() {
    let q = "histogram_quantile(0.9, sum by (le) (rate(foo_bucket[5m])))";
    let lowered =
        asap_frontend_promql::lower_promql_query_workload(&over_family(q, "foo", Histogram))
            .unwrap();
    assert_eq!(quantile_kind(lowered[0].as_operator().unwrap()), Some("HQ"));
    assert!(lowers_over_family(
        "rate(foo_sum[5m]) / rate(foo_count[5m]) + foo_bucket",
        "foo",
        Histogram
    ));
    let data = over_family("", "foo", Histogram).data_workload.unwrap();
    assert!(declared_non_negative(&data, "foo_sum"));
    assert!(declared_non_negative(&data, "foo_count"));
}
