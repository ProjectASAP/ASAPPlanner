//! `Schema::closed` propagation — open/closed invariant tests.
//!
//! Verifies that `QueryExpr::output_schema()` propagates the open/closed
//! completeness flag correctly through a lowered query tree.
//!
//! Key invariant: a PromQL scan is always `closed: false` (open) because its
//! label set is runtime-only.  The schema freezes to `closed: true` exactly at
//! the first operator that fully enumerates its output columns: a cross-series
//! `Aggregate` or a `Project`.  Per-series reductions (`rate`, `*_over_time`)
//! are label-preserving and keep the schema open.

use asap_ir::types::AccuracyTarget;
use asap_frontend_promql::lower_promql;

fn lower(q: &str) -> asap_ir::intent_algebra::QueryExpr {
    lower_promql(q, AccuracyTarget::Exact).unwrap_or_else(|e| panic!("lower failed for {q:?}: {e}"))
}

// bare scan is open — the metric's full label set is unknown at plan time
#[test]
fn schema_bare_scan_is_open() {
    let s = lower("http_requests_total").output_schema().unwrap();
    assert!(!s.closed, "PromQL scan must be open");
}

// scan with label filter is also open — the listed labels are referenced, not exhaustive
#[test]
fn schema_filtered_scan_is_open() {
    let s = lower(r#"http_requests_total{job="api-server"}"#)
        .output_schema()
        .unwrap();
    assert!(!s.closed, "PromQL scan with predicates must remain open");
    assert_eq!(s.columns.len(), 3, "[ts, value, job]");
}

// per-series rate is label-preserving → output stays open
#[test]
fn schema_rate_stays_open() {
    let s = lower("rate(http_requests_total[5m])")
        .output_schema()
        .unwrap();
    assert!(!s.closed, "per-series rate is label-preserving; stays open");
}

// per-series count_over_time is also label-preserving → stays open
#[test]
fn schema_count_over_time_stays_open() {
    let s = lower("count_over_time(http_requests_total[5m])")
        .output_schema()
        .unwrap();
    assert!(!s.closed, "per-series count_over_time stays open");
}

// cross-series sum with no group keys freezes to closed
#[test]
fn schema_sum_freezes_to_closed() {
    let s = lower("sum(http_requests_total)").output_schema().unwrap();
    assert!(s.closed, "cross-series aggregate must freeze to closed");
}

// cross-series sum grouped by job also freezes to closed
#[test]
fn schema_sum_by_job_freezes_to_closed() {
    let s = lower("sum by (job) (http_requests_total)")
        .output_schema()
        .unwrap();
    assert!(
        s.closed,
        "grouped cross-series aggregate must freeze to closed"
    );
}

// open scan → per-series rate → cross-series sum: freezes at the outer aggregate
#[test]
fn schema_sum_over_rate_freezes_to_closed() {
    let s = lower("sum by (job) (rate(http_requests_total[5m]))")
        .output_schema()
        .unwrap();
    assert!(
        s.closed,
        "cross-series aggregate over rate must freeze to closed"
    );
}

// binary op between two open scans: output stays open (open && open → open)
#[test]
fn schema_binary_op_two_open_stays_open() {
    let s = lower("http_requests_total / http_errors_total")
        .output_schema()
        .unwrap();
    assert!(!s.closed, "binary op over two open scans must stay open");
}

// binary op between two closed aggregates: output is closed (closed && closed → closed)
#[test]
fn schema_binary_op_two_closed_is_closed() {
    let s = lower("sum by (job) (http_requests_total) / sum by (job) (http_errors_total)")
        .output_schema()
        .unwrap();
    assert!(
        s.closed,
        "binary op over two closed aggregates must be closed"
    );
}
