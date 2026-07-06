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

use asap_ir::intent_algebra::{
    AggIntent, ArithOp, BinaryOpKind, CompareOp, L3Expr, L3Scalar, Predicate, QueryExpr, Source,
    VectorMatch, VectorMatchKind,
};
use asap_ir::types::AccuracyTarget;
use asap_frontend_promql::lower_promql;
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

// #27 — outer cross-series reduction over a nested per-group reduction over a
//   per-series rate: `max(sum by (job) (rate(m[5m])))`. Three stacked levels —
//   the arbitrary function nesting the old two-level template could not express.
//   The inner `sum by (job)` resolves job at col 2 against rate's
//   label-preserving output schema; the outer `max` has no grouping.
#[test]
fn q27_max_over_sum_by_job_over_rate() {
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
    let sum_by_job = agg(vec![2], AggIntent::Sum { col: None }, inner_rate);
    let expected = agg(vec![], AggIntent::Max { col: None }, sum_by_job);
    assert_eq!(
        lower("max(sum by (job) (rate(http_requests_total[5m])))"),
        expected
    );
}

// #53 — outer group key provably absent from the nested aggregate's output.
//   The inner `sum by (group)` freezes the schema to the closed `[group, sum]`,
//   which lacks `job`; PromQL groups every series under the empty label value
//   and omits it from the output, so the outer `by (job)` lowers as a global
//   aggregate (the absent key is dropped, not rejected).
//   Scan schema: [ts(0), value(1), group(2), job(3)] (labels alphabetical).
#[test]
fn q53_outer_group_key_absent_from_nested_aggregate() {
    let scan = QueryExpr::Scan {
        source: Source::TimeSeries {
            metric: "http_requests".into(),
        },
        predicates: vec![Predicate(L3Expr::Compare {
            left: Box::new(L3Expr::Column(3)),
            op: CompareOp::Eq,
            right: Box::new(L3Expr::Literal(L3Scalar::Utf8("api-server".into()))),
        })],
        schema: metric_schema(&["group", "job"]),
    };
    let inner = agg(vec![2], AggIntent::Sum { col: None }, scan);
    let expected = agg(vec![], AggIntent::Sum { col: None }, inner);
    assert_eq!(
        lower(r#"sum(sum by (group)(http_requests{job="api-server"})) by (job)"#),
        expected
    );
}

// #52 — an outer group key referenced by neither binary-op side (`__name__`)
//   still resolves. Each `or` side is bound independently against its own
//   sub-tree, so `__name__` is seeded as an inherited column on both. Each side
//   references only `env` (its matcher), so its schema is [ts, value, env,
//   __name__] (referenced `env` first, inherited `__name__` appended) → the
//   outer `by (__name__)` resolves to col 3 on both sides. The `or` carries the
//   parser's default `ignoring([])` match modifier.
#[test]
fn q52_outer_name_label_over_binary_op() {
    let side = |metric: &str, env: &str| QueryExpr::Scan {
        source: Source::TimeSeries {
            metric: metric.into(),
        },
        predicates: vec![Predicate(L3Expr::Compare {
            left: Box::new(L3Expr::Column(2)), // env
            op: CompareOp::Eq,
            right: Box::new(L3Expr::Literal(L3Scalar::Utf8(env.into()))),
        })],
        schema: metric_schema(&["env", "__name__"]),
    };
    let expected = agg(
        vec![3], // __name__
        AggIntent::Sum { col: None },
        QueryExpr::BinaryOp {
            op: BinaryOpKind::Or,
            lhs: Box::new(side("metric_a", "1")),
            rhs: Box::new(side("metric_b", "2")),
            vector_match: Some(VectorMatch {
                kind: VectorMatchKind::Ignoring,
                labels: vec![],
                grouping: None,
            }),
        },
    );
    assert_eq!(
        lower(r#"sum by (__name__)(metric_a{env="1"} or metric_b{env="2"})"#),
        expected,
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

// #27 — the nested sub-query example from the Prometheus docs
//   (https://prometheus.io/docs/prometheus/latest/querying/examples/):
//     max_over_time(deriv(rate(distance_covered_total[5s])[30s:5s])[10m:])
//   Two stacked sub-queries feeding range functions; the outer `[10m:]` uses
//   the default resolution (None). Every level is a per-series reduction, so
//   the whole spine survives verbatim and the schema stays label-preserving.
#[test]
fn q27_nested_subquery_prometheus_docs_example() {
    let scan = QueryExpr::Scan {
        source: Source::TimeSeries {
            metric: "distance_covered_total".into(),
        },
        predicates: vec![],
        schema: metric_schema(&[]),
    };
    let rate = agg(
        vec![],
        AggIntent::Rate,
        QueryExpr::TimeRange {
            range: Duration::from_secs(5),
            child: Box::new(scan),
        },
    );
    let deriv = agg(
        vec![],
        AggIntent::Deriv,
        QueryExpr::Subquery {
            range: Duration::from_secs(30),
            resolution: Some(Duration::from_secs(5)),
            child: Box::new(rate),
        },
    );
    let expected = agg(
        vec![],
        AggIntent::Max { col: None },
        QueryExpr::Subquery {
            range: Duration::from_secs(600),
            resolution: None,
            child: Box::new(deriv),
        },
    );
    assert_eq!(
        lower("max_over_time(deriv(rate(distance_covered_total[5s])[30s:5s])[10m:])"),
        expected,
    );
}
