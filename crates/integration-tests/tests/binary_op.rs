//! `NonASAPOp::BinaryOp` — arithmetic, comparison, and vector-match tests.
//!
//! Each side of a `BinaryOp` is bound independently by the SchemaResolver, so each
//! gets its own scan schema derived from the labels it references.
//! `VectorMatch` labels (e.g. `on(job)`) are carried as strings on the
//! operator and are NOT resolved to column ids — the SchemaResolver does not
//! see them.

use std::rc::Rc;
use std::time::Duration;

use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::fixtures::metric_schema;
use asap_types::ir::{BinaryOperator, NonASAPOp, OperatorNode, ScalarExpr, TimeRangeKind};
use asap_types::pre_asap::{
    AggIntent, ArithmeticOpKind, BinaryOpKind, CompareOpKind, GroupSide, Reduction, Source,
    VectorGrouping, VectorMatch, VectorMatchKind,
};
use asap_types::types::AccuracyTarget;

fn lower(q: &str) -> Rc<OperatorNode> {
    lower_promql(q, AccuracyTarget::Exact).unwrap_or_else(|e| panic!("lower failed for {q:?}: {e}"))
}

fn node(op: NonASAPOp) -> Rc<OperatorNode> {
    OperatorNode::non_asap_node(op).expect("fixture node derives its schema")
}

fn scan(metric: &str, labels: &[&str]) -> Rc<OperatorNode> {
    node(NonASAPOp::TimeRange {
        range: Duration::from_secs(1),
        kind: TimeRangeKind::Instant,
        child: source_scan(metric, labels),
    })
}

fn source_scan(metric: &str, labels: &[&str]) -> Rc<OperatorNode> {
    node(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: metric.into(),
        },
        predicates: vec![],
        schema: metric_schema(labels),
    })
}

fn rate_agg(metric: &str) -> Rc<OperatorNode> {
    node(NonASAPOp::Aggregate {
        reduction: Reduction::PerEntity,
        measures: vec![AggIntent::Rate],
        output_names: vec!["".into()],
        having: None,
        child: node(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: source_scan(metric, &[]),
        }),
    })
}

fn sum_by_job(metric: &str) -> Rc<OperatorNode> {
    node(NonASAPOp::Aggregate {
        reduction: Reduction::by(vec![2]),
        measures: vec![AggIntent::Sum { col: None }],
        output_names: vec!["".into()],
        having: None,
        child: scan(metric, &["job"]),
    })
}

/// A PromQL binary operator: no checked-division flags, no `bool` modifier.
fn binary(
    kind: BinaryOpKind,
    vector_match: Option<VectorMatch>,
    lhs: Rc<OperatorNode>,
    rhs: Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    node(NonASAPOp::BinaryOp {
        operator: BinaryOperator {
            checked_relative_division: false,
            checked_finite_division: false,
            kind,
            vector_match,
        },
        return_bool: false,
        lhs,
        rhs,
    })
}

/// A bare PromQL numeric literal at an operator position.
fn promql_scalar(v: f64) -> Rc<OperatorNode> {
    node(NonASAPOp::ScalarBridge(ScalarExpr::literal_f64(v)))
}

// #18 — arithmetic binary op between two bare scans; no vector match
#[test]
fn q18_div_bare_scans() {
    let expected = binary(
        BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
        None,
        scan("http_requests_total", &[]),
        scan("http_requests_total", &[]),
    );
    assert_eq!(lower("http_requests_total / http_requests_total"), expected);
}

// #19 — add with on(job) vector match; match labels are strings, not column ids
#[test]
fn q19_add_with_on_match() {
    let expected = binary(
        BinaryOpKind::Arithmetic(ArithmeticOpKind::Add),
        Some(VectorMatch {
            kind: VectorMatchKind::On,
            labels: vec!["job".into()],
            grouping: None,
        }),
        scan("http_requests_total", &[]),
        scan("http_requests_total", &[]),
    );
    assert_eq!(
        lower("http_requests_total + on(job) http_requests_total"),
        expected
    );
}

// #20 — divide two rate aggregates over different metrics
#[test]
fn q20_div_two_rates() {
    let expected = binary(
        BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
        None,
        rate_agg("http_requests_total"),
        rate_agg("http_errors_total"),
    );
    assert_eq!(
        lower("rate(http_requests_total[5m]) / rate(http_errors_total[5m])"),
        expected,
    );
}

// comparison ops — filter semantics; each op between two instant vectors
#[test]
fn q_gt_comparison() {
    assert_eq!(
        lower("http_requests_total > http_errors_total"),
        binary(
            BinaryOpKind::Compare(CompareOpKind::Gt),
            None,
            scan("http_requests_total", &[]),
            scan("http_errors_total", &[]),
        )
    );
}

#[test]
fn q_lt_comparison() {
    assert_eq!(
        lower("http_requests_total < http_errors_total"),
        binary(
            BinaryOpKind::Compare(CompareOpKind::Lt),
            None,
            scan("http_requests_total", &[]),
            scan("http_errors_total", &[]),
        )
    );
}

#[test]
fn q_ge_comparison() {
    assert_eq!(
        lower("http_requests_total >= http_errors_total"),
        binary(
            BinaryOpKind::Compare(CompareOpKind::Ge),
            None,
            scan("http_requests_total", &[]),
            scan("http_errors_total", &[]),
        )
    );
}

#[test]
fn q_le_comparison() {
    assert_eq!(
        lower("http_requests_total <= http_errors_total"),
        binary(
            BinaryOpKind::Compare(CompareOpKind::Le),
            None,
            scan("http_requests_total", &[]),
            scan("http_errors_total", &[]),
        )
    );
}

// ignoring(job) — match on all labels except job; labels are strings, not column ids
#[test]
fn q_add_with_ignoring() {
    assert_eq!(
        lower("http_requests_total + ignoring(job) http_errors_total"),
        binary(
            BinaryOpKind::Arithmetic(ArithmeticOpKind::Add),
            Some(VectorMatch {
                kind: VectorMatchKind::Ignoring,
                labels: vec!["job".into()],
                grouping: None,
            }),
            scan("http_requests_total", &[]),
            scan("http_errors_total", &[]),
        )
    );
}

// group_left — many-to-one: left side has higher cardinality
#[test]
fn q_mul_group_left() {
    assert_eq!(
        lower("http_requests_total * on(job) group_left() node_info"),
        binary(
            BinaryOpKind::Arithmetic(ArithmeticOpKind::Mul),
            Some(VectorMatch {
                kind: VectorMatchKind::On,
                labels: vec!["job".into()],
                grouping: Some(VectorGrouping {
                    side: GroupSide::Left,
                    labels: vec![],
                }),
            }),
            scan("http_requests_total", &[]),
            scan("node_info", &[]),
        )
    );
}

// group_right — one-to-many: right side has higher cardinality
#[test]
fn q_mul_group_right() {
    assert_eq!(
        lower("node_info * on(job) group_right() http_requests_total"),
        binary(
            BinaryOpKind::Arithmetic(ArithmeticOpKind::Mul),
            Some(VectorMatch {
                kind: VectorMatchKind::On,
                labels: vec!["job".into()],
                grouping: Some(VectorGrouping {
                    side: GroupSide::Right,
                    labels: vec![],
                }),
            }),
            scan("node_info", &[]),
            scan("http_requests_total", &[]),
        )
    );
}

// #21 — divide two sum-by-job aggregates over different metrics
//   each side: Aggregate{Sum, by=[2]} over Scan([ts, value, job])
#[test]
fn q21_div_two_sum_by_job() {
    let expected = binary(
        BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
        None,
        sum_by_job("http_requests_total"),
        sum_by_job("http_errors_total"),
    );
    assert_eq!(
        lower("sum by (job) (http_requests_total) / sum by (job) (http_errors_total)"),
        expected,
    );
}

// #36 — unary negation lowers as `expr * -1`: a Mul BinaryOp of the vector
//   against a `ScalarBridge(-1)` leaf, no vector match. The vector side keeps
//   its schema.
#[test]
fn q36_unary_negation_is_multiply_by_minus_one() {
    let expected = binary(
        BinaryOpKind::Arithmetic(ArithmeticOpKind::Mul),
        None,
        scan("some_metric", &[]),
        promql_scalar(-1.0),
    );
    assert_eq!(lower("-some_metric"), expected);
}

// #36 — negation nested inside an aggregate argument (issue #27 nesting):
//   `sum(-m)` → Aggregate{Sum} over the `m * -1` BinaryOp.
#[test]
fn q36_sum_of_negation_nests() {
    let expected = node(NonASAPOp::Aggregate {
        reduction: Reduction::by(vec![]),
        measures: vec![AggIntent::Sum { col: None }],
        output_names: vec!["".into()],
        having: None,
        child: binary(
            BinaryOpKind::Arithmetic(ArithmeticOpKind::Mul),
            None,
            scan("node_cpu_seconds_total", &[]),
            promql_scalar(-1.0),
        ),
    });
    assert_eq!(lower("sum(-node_cpu_seconds_total)"), expected);
}
