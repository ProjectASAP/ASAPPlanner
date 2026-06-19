//! Multi-node pipeline tests — nested `Aggregate`, `TimeRange`, `BinaryOp`, and `Scan`.
//!
//! Key invariant: `rate`/`increase` are label-preserving (per-series), so an
//! outer `Aggregate.by` resolves its group keys against the inner aggregate's
//! output schema, which still carries all label columns.
//!
//! Label column ordering is always alphabetical, so in a query that references
//! both `job` and `status`:
//!   schema = [ts(0), value(1), job(2), status(3)]

use std::time::Duration;

use asap_control_core::intent_algebra::{
    AggIntent, ArithOp, BinaryOpKind, CompareOp, L3Expr, L3Scalar, Predicate, QueryExpr, Source,
};
use asap_control_core::types::AccuracyTarget;
use asap_control_lower::lower_promql;
use asap_e2e::fixtures::metric_schema;

fn lower(q: &str) -> QueryExpr {
    lower_promql(q, AccuracyTarget::Exact).unwrap_or_else(|e| panic!("lower failed for {q:?}: {e}"))
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

// #22 — sum by job over rate; outer by=[2] resolves against rate's
//   label-preserving output schema [ts, value, job]
#[test]
fn q22_sum_by_job_over_rate() {
    let scan = QueryExpr::Scan {
        source: Source::TimeSeries {
            metric: "http_requests_total".into(),
        },
        predicates: vec![],
        schema: metric_schema(&["job"]),
    };
    let inner_rate = agg(
        vec![],
        AggIntent::Rate,
        QueryExpr::TimeRange {
            range: Duration::from_secs(300),
            child: Box::new(scan),
        },
    );
    let expected = agg(vec![2], AggIntent::Sum { col: None }, inner_rate);
    assert_eq!(
        lower("sum by (job) (rate(http_requests_total[5m]))"),
        expected
    );
}

// #23 — sum by job over a filtered scan; status="200" is a filter-only label
//   labels sorted: job(2) < status(3)
//   predicate on status (col 3); group key job (col 2)
#[test]
fn q23_sum_by_job_over_filtered_scan() {
    let scan = QueryExpr::Scan {
        source: Source::TimeSeries {
            metric: "http_requests_total".into(),
        },
        predicates: vec![Predicate(L3Expr::Compare {
            left: Box::new(L3Expr::Column(3)),
            op: CompareOp::Eq,
            right: Box::new(L3Expr::Literal(L3Scalar::Utf8("200".into()))),
        })],
        schema: metric_schema(&["job", "status"]),
    };
    let expected = agg(vec![2], AggIntent::Sum { col: None }, scan);
    assert_eq!(
        lower(r#"sum by (job) (http_requests_total{status="200"})"#),
        expected
    );
}

// #25 — binary op over two complex subtrees
//   LHS: sum by (job) over rate over filtered scan
//     schema [ts, value, job, status]; outer by=[2] (job)
//   RHS: sum by (job) over rate over bare scan
//     schema [ts, value, job]; outer by=[2] (job)
#[test]
fn q25_div_over_complex_subtrees() {
    let lhs_scan = QueryExpr::Scan {
        source: Source::TimeSeries {
            metric: "http_requests_total".into(),
        },
        predicates: vec![Predicate(L3Expr::Compare {
            left: Box::new(L3Expr::Column(3)),
            op: CompareOp::Eq,
            right: Box::new(L3Expr::Literal(L3Scalar::Utf8("200".into()))),
        })],
        schema: metric_schema(&["job", "status"]),
    };
    let lhs = agg(
        vec![2],
        AggIntent::Sum { col: None },
        agg(
            vec![],
            AggIntent::Rate,
            QueryExpr::TimeRange {
                range: Duration::from_secs(300),
                child: Box::new(lhs_scan),
            },
        ),
    );

    let rhs_scan = QueryExpr::Scan {
        source: Source::TimeSeries {
            metric: "http_errors_total".into(),
        },
        predicates: vec![],
        schema: metric_schema(&["job"]),
    };
    let rhs = agg(
        vec![2],
        AggIntent::Sum { col: None },
        agg(
            vec![],
            AggIntent::Rate,
            QueryExpr::TimeRange {
                range: Duration::from_secs(300),
                child: Box::new(rhs_scan),
            },
        ),
    );

    let expected = QueryExpr::BinaryOp {
        op: BinaryOpKind::Arith(ArithOp::Div),
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
        vector_match: None,
    };
    assert_eq!(
        lower(
            r#"sum by (job) (rate(http_requests_total{status="200"}[5m])) / sum by (job) (rate(http_errors_total[5m]))"#
        ),
        expected
    );
}

// #24 — sum by job over rate over a filtered scan
//   same schema [ts, value, job, status]; rate is label-preserving,
//   so outer sum by job still finds job at col 2
#[test]
fn q24_sum_by_job_over_rate_over_filtered_scan() {
    let scan = QueryExpr::Scan {
        source: Source::TimeSeries {
            metric: "http_requests_total".into(),
        },
        predicates: vec![Predicate(L3Expr::Compare {
            left: Box::new(L3Expr::Column(3)),
            op: CompareOp::Eq,
            right: Box::new(L3Expr::Literal(L3Scalar::Utf8("200".into()))),
        })],
        schema: metric_schema(&["job", "status"]),
    };
    let inner_rate = agg(
        vec![],
        AggIntent::Rate,
        QueryExpr::TimeRange {
            range: Duration::from_secs(300),
            child: Box::new(scan),
        },
    );
    let expected = agg(vec![2], AggIntent::Sum { col: None }, inner_rate);
    assert_eq!(
        lower(r#"sum by (job) (rate(http_requests_total{status="200"}[5m]))"#),
        expected,
    );
}
