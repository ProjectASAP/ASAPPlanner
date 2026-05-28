//! Tier 4 — Binary op queries (e2e test suite #18–21).
//!
//! Each side of a `BinaryOp` is bound independently by the Binder, so each
//! gets its own scan schema derived from the labels it references.
//! `VectorMatch` labels (e.g. `on(job)`) are carried as strings on the node
//! and are NOT resolved to column ids — the Binder does not see them.

use std::time::Duration;

use asap_control_core::intent_algebra::{
    AggIntent, ArithOp, BinaryOpKind, QueryExpr, Source, VectorMatch, VectorMatchKind,
};
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

fn rate_agg(metric: &str) -> QueryExpr {
    QueryExpr::Aggregate {
        by: vec![],
        aggs: vec![AggIntent::Rate],
        output_names: vec!["".into()],
        having: None,
        child: Box::new(QueryExpr::TimeRange {
            range: Duration::from_secs(300),
            child: Box::new(scan(metric, &[])),
        }),
    }
}

fn sum_by_job(metric: &str) -> QueryExpr {
    QueryExpr::Aggregate {
        by: vec![2],
        aggs: vec![AggIntent::Sum { col: None }],
        output_names: vec!["".into()],
        having: None,
        child: Box::new(scan(metric, &["job"])),
    }
}

// #18 — arithmetic binary op between two bare scans; no vector match
#[test]
fn q18_div_bare_scans() {
    let expected = QueryExpr::BinaryOp {
        op: BinaryOpKind::Arith(ArithOp::Div),
        lhs: Box::new(scan("http_requests_total", &[])),
        rhs: Box::new(scan("http_requests_total", &[])),
        vector_match: None,
    };
    assert_eq!(lower("http_requests_total / http_requests_total"), expected);
}

// #19 — add with on(job) vector match; match labels are strings, not column ids
#[test]
fn q19_add_with_on_match() {
    let expected = QueryExpr::BinaryOp {
        op: BinaryOpKind::Arith(ArithOp::Add),
        lhs: Box::new(scan("http_requests_total", &[])),
        rhs: Box::new(scan("http_requests_total", &[])),
        vector_match: Some(VectorMatch {
            kind: VectorMatchKind::On,
            labels: vec!["job".into()],
            grouping: None,
        }),
    };
    assert_eq!(
        lower("http_requests_total + on(job) http_requests_total"),
        expected
    );
}

// #20 — divide two rate aggregates over different metrics
#[test]
fn q20_div_two_rates() {
    let expected = QueryExpr::BinaryOp {
        op: BinaryOpKind::Arith(ArithOp::Div),
        lhs: Box::new(rate_agg("http_requests_total")),
        rhs: Box::new(rate_agg("http_errors_total")),
        vector_match: None,
    };
    assert_eq!(
        lower("rate(http_requests_total[5m]) / rate(http_errors_total[5m])"),
        expected,
    );
}

// #21 — divide two sum-by-job aggregates over different metrics
//   each side: Aggregate{Sum, by=[2]} over Scan([ts, value, job])
#[test]
fn q21_div_two_sum_by_job() {
    let expected = QueryExpr::BinaryOp {
        op: BinaryOpKind::Arith(ArithOp::Div),
        lhs: Box::new(sum_by_job("http_requests_total")),
        rhs: Box::new(sum_by_job("http_errors_total")),
        vector_match: None,
    };
    assert_eq!(
        lower("sum by (job) (http_requests_total) / sum by (job) (http_errors_total)"),
        expected,
    );
}
