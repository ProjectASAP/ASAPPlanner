//! Tier 3 — Range / streaming function queries (e2e test suite #13–17).
//!
//! All range functions lower to `Aggregate { child: TimeRange { range, child: Scan } }`.
//! The temporal range lives on the `TimeRange` node, not in the `AggIntent`.
//! `rate` / `increase` use `AggIntent::Rate` / `AggIntent::Increase` (no window field).
//! `*_over_time` functions reuse the corresponding cross-series intents
//! (`Count`, `Sum`, `Quantile`, …) — the `TimeRange` child is what marks them
//! as per-series reductions.

use std::time::Duration;

use asap_control_core::intent_algebra::{AggIntent, QueryExpr, Source};
use asap_control_core::types::AccuracyTarget;
use asap_control_lower::lower_promql;
use asap_e2e::fixtures::metric_schema;

fn lower(q: &str) -> QueryExpr {
    lower_promql(q, AccuracyTarget::Exact).unwrap_or_else(|e| panic!("lower failed for {q:?}: {e}"))
}

fn scan(metric: &str) -> QueryExpr {
    QueryExpr::Scan {
        source: Source::TimeSeries {
            metric: metric.into(),
        },
        predicates: vec![],
        schema: metric_schema(&[]),
    }
}

fn range_agg(range_secs: u64, intent: AggIntent, metric: &str) -> QueryExpr {
    QueryExpr::Aggregate {
        by: vec![],
        aggs: vec![intent],
        output_names: vec!["".into()],
        having: None,
        child: Box::new(QueryExpr::TimeRange {
            range: Duration::from_secs(range_secs),
            child: Box::new(scan(metric)),
        }),
    }
}

// #13 — rate: counter-reset-aware per-second rate; range on TimeRange node
#[test]
fn q13_rate() {
    assert_eq!(
        lower("rate(http_requests_total[5m])"),
        range_agg(300, AggIntent::Rate, "http_requests_total"),
    );
}

// #14 — increase: cumulative increase over the range window
#[test]
fn q14_increase() {
    assert_eq!(
        lower("increase(http_requests_total[1h])"),
        range_agg(3600, AggIntent::Increase, "http_requests_total"),
    );
}

// #15 — count_over_time: sample count per series over the window
#[test]
fn q15_count_over_time() {
    assert_eq!(
        lower("count_over_time(http_requests_total[5m])"),
        range_agg(
            300,
            AggIntent::Count {
                accuracy: AccuracyTarget::Exact
            },
            "http_requests_total"
        ),
    );
}

// #16 — sum_over_time: sum of samples per series over the window
#[test]
fn q16_sum_over_time() {
    assert_eq!(
        lower("sum_over_time(http_requests_total[5m])"),
        range_agg(300, AggIntent::Sum { col: None }, "http_requests_total"),
    );
}

// #17 — quantile_over_time: per-series quantile over the window
#[test]
fn q17_quantile_over_time() {
    assert_eq!(
        lower("quantile_over_time(0.99, http_requests_total[5m])"),
        range_agg(
            300,
            AggIntent::Quantile {
                q: 0.99,
                accuracy: AccuracyTarget::Exact
            },
            "http_requests_total",
        ),
    );
}
