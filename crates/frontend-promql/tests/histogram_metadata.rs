//! Type-driven `histogram_quantile` discrimination (issue #79).
//!
//! The structural heuristic (`by (le)` / `_bucket` / `le=` matcher) proxies the
//! argument's sample type. A declared [`HistogramKind`] overrides it, fixing the
//! heuristic's false-positive and false-negative cases. Undeclared metrics still
//! fall back to the heuristic.

use asap_frontend_promql::{HistogramCatalog, HistogramKind};
mod support;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::{NonASAPOp, OperatorNode};
use asap_types::types::AccuracyTarget;
use support::{lower_promql, lower_promql_with_histograms};

/// The histogram/quantile intent kind in the lowered DAG: `"HQ"` for the
/// classic-bucket `HistogramQuantile`, `"Q"` for the sketch-able `Quantile`.
fn quantile_kind(qe: &OperatorNode) -> &'static str {
    fn walk(e: &OperatorNode) -> Option<&'static str> {
        match e.expect_non_asap() {
            NonASAPOp::Aggregate {
                measures, child, ..
            } => measures
                .iter()
                .find_map(|i| match i {
                    AggIntent::HistogramQuantile { .. } => Some("HQ"),
                    AggIntent::Quantile { .. } => Some("Q"),
                    _ => None,
                })
                .or_else(|| walk(child)),
            NonASAPOp::TimeRange { child, .. }
            | NonASAPOp::Filter { child, .. }
            | NonASAPOp::Sort { child, .. }
            | NonASAPOp::Limit { child, .. }
            | NonASAPOp::PromqlSubquery { child, .. }
            | NonASAPOp::Project { child, .. } => walk(child),
            _ => None,
        }
    }
    walk(qe).expect("a HistogramQuantile or Quantile intent")
}

fn heuristic(q: &str) -> &'static str {
    quantile_kind(&lower_promql(q, AccuracyTarget::Exact).unwrap())
}

fn with_meta(q: &str, catalog: HistogramCatalog) -> &'static str {
    quantile_kind(&lower_promql_with_histograms(q, AccuracyTarget::Exact, catalog).unwrap())
}

#[test]
fn heuristic_baseline_is_unchanged_without_a_catalog() {
    // Classic buckets are represented; undeclared native samples are rejected.
    assert_eq!(
        heuristic(
            "histogram_quantile(0.9, sum by (le) (rate(http_request_duration_seconds_bucket[5m])))"
        ),
        "HQ"
    );
    assert!(lower_promql(
        "histogram_quantile(0.9, native_latency)",
        AccuracyTarget::Exact
    )
    .is_err());
}

#[test]
fn declared_classic_bucket_fixes_the_false_negative() {
    // A classic histogram exposed WITHOUT the `_bucket` suffix and queried with
    // no `le` grouping/matcher requires an explicit sample-type declaration.
    let q = "histogram_quantile(0.9, latency_seconds)";
    assert!(lower_promql(q, AccuracyTarget::Exact).is_err());
    assert_eq!(
        with_meta(
            q,
            HistogramCatalog::new().with("latency_seconds", HistogramKind::ClassicBucket)
        ),
        "HQ",
        "metadata routes it to exact bucket interpolation"
    );
}

#[test]
fn declared_raw_extension_and_native_gap_override_the_heuristic() {
    // A metric merely NAMED `…_bucket` that actually holds raw samples / a native
    // histogram: the heuristic wrongly routes it to bucket interpolation.
    let q = "histogram_quantile(0.9, foo_bucket)";
    assert_eq!(
        heuristic(q),
        "HQ",
        "heuristic mis-routes on the `_bucket` name"
    );
    assert_eq!(
        with_meta(
            q,
            HistogramCatalog::new().with("foo_bucket", HistogramKind::RawSamples)
        ),
        "Q",
        "raw samples are sketch-able"
    );
    let catalog = HistogramCatalog::new().with("foo_bucket", HistogramKind::Native);
    assert!(lower_promql_with_histograms(q, AccuracyTarget::Exact, catalog.clone()).is_err());
    assert!(lower_promql_with_histograms("foo_bucket", AccuracyTarget::Exact, catalog).is_err());
}

#[test]
fn undeclared_metric_falls_back_to_the_heuristic() {
    // A catalog that doesn't mention the queried metric leaves the structural
    // decision in place.
    let catalog = HistogramCatalog::new().with("some_other_metric", HistogramKind::RawSamples);
    assert_eq!(
        with_meta(
            "histogram_quantile(0.9, sum by (le) (x_bucket))",
            catalog.clone()
        ),
        "HQ"
    );
    assert!(lower_promql_with_histograms(
        "histogram_quantile(0.9, native_thing)",
        AccuracyTarget::Exact,
        catalog
    )
    .is_err());
}

#[test]
fn the_catalog_does_not_leak_across_calls() {
    // The ambient catalog is scoped to the single `_with_histograms` call; a
    // subsequent plain `lower_promql` sees no metadata (guards against a
    // thread-local that isn't cleaned up).
    let _ = with_meta(
        "histogram_quantile(0.9, foo_bucket)",
        HistogramCatalog::new().with("foo_bucket", HistogramKind::RawSamples),
    );
    // `foo_bucket` would be sketch-able under that catalog, but with none it must
    // revert to the heuristic (the `_bucket` name → HistogramQuantile).
    assert_eq!(heuristic("histogram_quantile(0.9, foo_bucket)"), "HQ");
}
