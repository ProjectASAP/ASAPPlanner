use asap_frontend_promql::lower_promql;
use asap_types::types::AccuracyTarget;

/// Prometheus treats these quantile parameters as valid queries returning special values.
#[test]
fn quantile_parameters_retain_prometheus_special_value_semantics() {
    for parameter in ["-0.1", "1.1", "NaN", "+Inf", "-Inf"] {
        for query in [
            format!("quantile({parameter}, smoke_gauge)"),
            format!("quantile_over_time({parameter}, smoke_gauge[5m])"),
            format!("histogram_quantile({parameter}, smoke_bucket)"),
        ] {
            assert!(
                lower_promql(&query, AccuracyTarget::Exact).is_ok(),
                "{query}"
            );
        }
    }
}

/// Instant rate and extrapolated rate need different execution kernels, including subqueries.
#[test]
fn irate_and_rate_have_distinct_canonical_intents() {
    for input in ["smoke_counter_total[5m]", "smoke_counter_total[5m:1m]"] {
        let rate = lower_promql(&format!("rate({input})"), AccuracyTarget::Exact).unwrap();
        let irate = lower_promql(&format!("irate({input})"), AccuracyTarget::Exact).unwrap();
        assert_ne!(rate, irate, "rate and irate must not collapse: {input}");
    }
}

/// PromQL count counts series even when two sample values are equal.
#[test]
fn count_is_row_count_not_distinct_sample_value_count() {
    use asap_types::pre_asap::{AggIntent, QueryExpr};
    let tree = lower_promql("count(smoke_gauge)", AccuracyTarget::Exact).unwrap();
    let QueryExpr::Aggregate { measures, .. } = tree else {
        panic!("expected aggregate")
    };
    assert!(matches!(measures.as_slice(), [AggIntent::Count { .. }]));
}
