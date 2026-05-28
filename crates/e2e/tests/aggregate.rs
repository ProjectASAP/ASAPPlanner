//! `QueryExpr::Aggregate` — cross-series aggregation tests.
//!
//! topk/bottomk are omitted — dispatch is deferred.
//!
//! Cross-series aggregates lower to a single `Aggregate` node with no
//! `TimeRange` child (range functions use `TimeRange` — see `time_range.rs`).
//! Group keys land on `Aggregate.by` as positional `ColumnId`s.
//! Single-stat PromQL aggregates always get `output_names: [""]` (no alias)
//! and `having: None`.

use asap_control_core::intent_algebra::{AggIntent, QueryExpr, Source};
use asap_control_core::types::AccuracyTarget;
use asap_control_lower::lower_promql;
use asap_e2e::fixtures::metric_schema;

fn lower(q: &str) -> QueryExpr {
    lower_promql(q, AccuracyTarget::Exact).unwrap_or_else(|e| panic!("lower failed for {q:?}: {e}"))
}

fn scan(metric: &str, labels: &[&str]) -> QueryExpr {
    QueryExpr::Scan {
        source: Source::TimeSeries {
            metric: metric.into(),
        },
        predicates: vec![],
        schema: metric_schema(labels),
    }
}

fn agg(by: Vec<usize>, intent: AggIntent, child: QueryExpr) -> QueryExpr {
    QueryExpr::Aggregate {
        by: by.into(),
        aggs: vec![intent],
        output_names: vec!["".into()],
        having: None,
        child: Box::new(child),
    }
}

// #5 — sum with no group keys
#[test]
fn q05_sum_no_group() {
    assert_eq!(
        lower("sum(http_requests_total)"),
        agg(
            vec![],
            AggIntent::Sum { col: None },
            scan("http_requests_total", &[])
        ),
    );
}

// #6 — sum grouped by job; job is col 2 in [ts, value, job]
#[test]
fn q06_sum_by_job() {
    assert_eq!(
        lower("sum by (job) (http_requests_total)"),
        agg(
            vec![2],
            AggIntent::Sum { col: None },
            scan("http_requests_total", &["job"])
        ),
    );
}

// #7 — PromQL `count` is cross-series cardinality, not per-sample Count
#[test]
fn q07_count_is_cardinality() {
    assert_eq!(
        lower("count(http_requests_total)"),
        agg(
            vec![],
            AggIntent::Cardinality {
                accuracy: AccuracyTarget::Exact
            },
            scan("http_requests_total", &[]),
        ),
    );
}

// #8 — avg grouped by datacenter; datacenter is col 2
#[test]
fn q08_avg_by_datacenter() {
    assert_eq!(
        lower("avg by (datacenter) (http_requests_total)"),
        agg(
            vec![2],
            AggIntent::Avg { col: None },
            scan("http_requests_total", &["datacenter"])
        ),
    );
}

// multiple group keys — sum by (job, status); alphabetical: job(2), status(3)
#[test]
fn q_sum_by_job_and_status() {
    assert_eq!(
        lower("sum by (job, status) (http_requests_total)"),
        agg(
            vec![2, 3],
            AggIntent::Sum { col: None },
            scan("http_requests_total", &["job", "status"])
        ),
    );
}

// min — cross-series minimum
#[test]
fn q_min_no_group() {
    assert_eq!(
        lower("min(http_requests_total)"),
        agg(
            vec![],
            AggIntent::Min { col: None },
            scan("http_requests_total", &[])
        ),
    );
}

// max — cross-series maximum, grouped by job
#[test]
fn q_max_by_job() {
    assert_eq!(
        lower("max by (job) (http_requests_total)"),
        agg(
            vec![2],
            AggIntent::Max { col: None },
            scan("http_requests_total", &["job"])
        ),
    );
}

// stddev — cross-series standard deviation; PromQL stddev uses population=false in the lowering
#[test]
fn q_stddev_no_group() {
    assert_eq!(
        lower("stddev(http_requests_total)"),
        agg(
            vec![],
            AggIntent::StdDev {
                col: None,
                population: true
            },
            scan("http_requests_total", &[])
        ),
    );
}

// stdvar — cross-series variance
#[test]
fn q_stdvar_no_group() {
    assert_eq!(
        lower("stdvar(http_requests_total)"),
        agg(
            vec![],
            AggIntent::Variance {
                col: None,
                population: true
            },
            scan("http_requests_total", &[])
        ),
    );
}

// #10 — cross-series quantile; no TimeRange node (no range window)
#[test]
fn q10_quantile_cross_series() {
    assert_eq!(
        lower("quantile(0.99, http_requests_total)"),
        agg(
            vec![],
            AggIntent::Quantile {
                q: 0.99,
                accuracy: AccuracyTarget::Exact
            },
            scan("http_requests_total", &[]),
        ),
    );
}
