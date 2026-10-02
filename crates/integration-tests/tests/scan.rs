//! `NonASAPOp::Scan` — label matcher / predicate tests.
//!
//! The Scan schema is always [ts(0), value(1), label_a(2), label_b(3), …]
//! where labels are appended alphabetically after dedup by the SchemaResolver.
//! Filter-only labels (not group keys) still land in the schema because the
//! predicate expression references them positionally.
//! Predicates are canonicalized alphabetically by label name at lowering time.

use std::rc::Rc;
use std::time::Duration;

use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::fixtures::metric_schema;
use asap_types::ir::{
    ExprSemantics, NonASAPOp, OperatorNode, Predicate, ScalarExpr, TimeRangeKind,
};
use asap_types::pre_asap::{CompareOpKind, ScalarValue, Source};
use asap_types::types::AccuracyTarget;

fn lower(q: &str) -> Rc<OperatorNode> {
    lower_promql(q, AccuracyTarget::Exact).unwrap_or_else(|e| panic!("lower failed for {q:?}: {e}"))
}

fn node(op: NonASAPOp) -> Rc<OperatorNode> {
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(op))
        .expect("fixture node derives its schema")
}

fn bare_scan(metric: &str, labels: &[&str]) -> Rc<OperatorNode> {
    node(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: metric.into(),
        },
        predicates: vec![],
        schema: metric_schema(labels),
    })
}

fn instant(child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    node(NonASAPOp::TimeRange {
        range: Duration::from_secs(1),
        kind: TimeRangeKind::Instant,
        child,
    })
}

fn label_pred(col_id: usize, op: CompareOpKind, value: &str) -> Predicate {
    Predicate(ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(col_id)),
        op,
        right: Box::new(ScalarExpr::Literal(ScalarValue::Utf8(value.into()))),
        semantics: ExprSemantics::Promql,
    })
}

fn eq_pred(col_id: usize, value: &str) -> Predicate {
    label_pred(col_id, CompareOpKind::Eq, value)
}

fn ne_pred(col_id: usize, value: &str) -> Predicate {
    label_pred(col_id, CompareOpKind::Ne, value)
}

fn regex_pred(col_id: usize, pattern: &str) -> Predicate {
    label_pred(col_id, CompareOpKind::Regex, pattern)
}

fn notregex_pred(col_id: usize, pattern: &str) -> Predicate {
    label_pred(col_id, CompareOpKind::NotRegex, pattern)
}

// #1 — bare metric name, no matchers
#[test]
fn q01_bare_scan() {
    assert_eq!(
        lower("http_requests_total"),
        instant(bare_scan("http_requests_total", &[]))
    );
}

// #2 — single equality matcher
//   schema: [ts(0), value(1), job(2)]
#[test]
fn q02_equality_predicate() {
    let expected = instant(node(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "http_requests_total".into(),
        },
        predicates: vec![eq_pred(2, "api-server")],
        schema: metric_schema(&["job"]),
    }));
    assert_eq!(lower(r#"http_requests_total{job="api-server"}"#), expected);
}

// #3 — single inequality matcher
//   schema: [ts(0), value(1), status(2)]
#[test]
fn q03_inequality_predicate() {
    let expected = instant(node(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "http_requests_total".into(),
        },
        predicates: vec![ne_pred(2, "500")],
        schema: metric_schema(&["status"]),
    }));
    assert_eq!(lower(r#"http_requests_total{status!="500"}"#), expected);
}

// #4 — regex matcher; RHS is the pattern string, op is Regex
//   schema: [ts(0), value(1), job(2)]
#[test]
fn q04_regex_predicate() {
    let expected = instant(node(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "http_requests_total".into(),
        },
        predicates: vec![regex_pred(2, "api.*")],
        schema: metric_schema(&["job"]),
    }));
    assert_eq!(lower(r#"http_requests_total{job=~"api.*"}"#), expected);
}

// negative regex matcher; RHS is the pattern string, op is NotRegex
//   schema: [ts(0), value(1), job(2)]
#[test]
fn q_notregex_predicate() {
    let expected = instant(node(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "http_requests_total".into(),
        },
        predicates: vec![notregex_pred(2, "internal.*")],
        schema: metric_schema(&["job"]),
    }));
    assert_eq!(lower(r#"http_requests_total{job!~"internal.*"}"#), expected);
}

// multiple matchers — two predicates, canonicalized alphabetically by label name
//   labels sorted: job(2) < status(3)  →  schema: [ts, value, job, status]
//   predicates in same alphabetical order: job first, then status
#[test]
fn q_multi_two_predicates() {
    let expected = instant(node(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "http_requests_total".into(),
        },
        predicates: vec![eq_pred(2, "api-server"), ne_pred(3, "500")],
        schema: metric_schema(&["job", "status"]),
    }));
    assert_eq!(
        lower(r#"http_requests_total{job="api-server",status!="500"}"#),
        expected,
    );
}
