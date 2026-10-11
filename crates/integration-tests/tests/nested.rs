//! Multi-node pipeline tests — nested `Aggregate`, `TimeRange`, `BinaryOp`, and `Scan`.
//!
//! Key invariant: `rate`/`increase` are label-preserving (per-series), so an
//! outer `Aggregate` reduction resolves its group keys against the inner
//! aggregate's output schema, which still carries all label columns.
//!
//! Label column ordering is always alphabetical, so in a query that references
//! both `job` and `status`:
//!   schema = [ts(0), value(1), job(2), status(3)]

use std::rc::Rc;
use std::time::Duration;

use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::fixtures::metric_schema;
use asap_types::ir::operator::{
    AggIntent, AtModifier, BinaryOpKind, GroupKeys, PromQLVectorSetOpKind, Reduction, Source,
    TimeShift, VectorMatch, VectorMatchKind,
};
use asap_types::ir::scalar::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use asap_types::ir::{
    BinaryOperator, ExprSemantics, NonASAPOp, OperatorNode, Predicate, ScalarExpr, TimeRangeKind,
};
use asap_types::types::AccuracyTarget;

fn lower(q: &str) -> Rc<OperatorNode> {
    lower_promql(q, AccuracyTarget::Exact).unwrap_or_else(|e| panic!("lower failed for {q:?}: {e}"))
}

fn node(op: NonASAPOp) -> Rc<OperatorNode> {
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(op))
        .expect("fixture node derives its schema")
}

fn scan(metric: &str, predicates: Vec<Predicate>, labels: &[&str]) -> Rc<OperatorNode> {
    node(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: metric.into(),
        },
        predicates,
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

fn range(secs: u64, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    node(NonASAPOp::TimeRange {
        range: Duration::from_secs(secs),
        kind: TimeRangeKind::Range,
        child,
    })
}

fn eq_pred(col_id: usize, value: &str) -> Predicate {
    Predicate(ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(col_id)),
        op: CompareOpKind::Eq,
        right: Box::new(ScalarExpr::Literal(ScalarValue::Utf8(value.into()))),
        semantics: ExprSemantics::Promql,
    })
}

fn agg(by: Vec<usize>, intent: AggIntent, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    node(NonASAPOp::Aggregate {
        reduction: Reduction::by(by),
        measures: vec![intent],
        output_names: vec!["".into()],
        filters: vec![],
        having: None,
        child,
    })
}

fn agg_per_entity(intent: AggIntent, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    node(NonASAPOp::Aggregate {
        reduction: Reduction::PerEntity,
        measures: vec![intent],
        output_names: vec!["".into()],
        filters: vec![],
        having: None,
        child,
    })
}

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

// #22 — sum by job over rate; outer by=[2] resolves against rate's
//   label-preserving output schema [ts, value, job]
#[test]
fn q22_sum_by_job_over_rate() {
    let inner_rate = agg_per_entity(
        AggIntent::Rate,
        range(300, scan("http_requests_total", vec![], &["job"])),
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
    let scan = scan(
        "http_requests_total",
        vec![eq_pred(3, "200")],
        &["job", "status"],
    );
    let expected = agg(vec![2], AggIntent::Sum { col: None }, instant(scan));
    assert_eq!(
        lower(r#"sum by (job) (http_requests_total{status="200"})"#),
        expected
    );
}

// #25 — binary op over two complex sub-DAGs
//   LHS: sum by (job) over rate over filtered scan
//     schema [ts, value, job, status]; outer by=[2] (job)
//   RHS: sum by (job) over rate over bare scan
//     schema [ts, value, job]; outer by=[2] (job)
#[test]
fn q25_div_over_complex_sub_dags() {
    let lhs_scan = scan(
        "http_requests_total",
        vec![eq_pred(3, "200")],
        &["job", "status"],
    );
    let lhs = agg(
        vec![2],
        AggIntent::Sum { col: None },
        agg_per_entity(AggIntent::Rate, range(300, lhs_scan)),
    );

    let rhs_scan = scan("http_errors_total", vec![], &["job"]);
    let rhs = agg(
        vec![2],
        AggIntent::Sum { col: None },
        agg_per_entity(AggIntent::Rate, range(300, rhs_scan)),
    );

    let expected = binary(
        BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
        None,
        lhs,
        rhs,
    );
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
    let inner_rate = agg_per_entity(
        AggIntent::Rate,
        range(300, scan("http_requests_total", vec![], &["job"])),
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
    let scan = scan(
        "http_requests",
        vec![eq_pred(3, "api-server")],
        &["group", "job"],
    );
    let inner = agg(vec![2], AggIntent::Sum { col: None }, instant(scan));
    let expected = agg(vec![], AggIntent::Sum { col: None }, inner);
    assert_eq!(
        lower(r#"sum(sum by (group)(http_requests{job="api-server"})) by (job)"#),
        expected
    );
}

// #52 — an outer group key referenced by neither binary-op side (`__name__`)
//   still resolves. Each `or` side is bound independently against its own
//   sub-DAG, so `__name__` is seeded as an inherited column on both. Each side
//   references only `env` (its matcher), so its schema is [ts, value, env,
//   __name__] (referenced `env` first, inherited `__name__` appended) → the
//   outer `by (__name__)` resolves to col 3 on both sides. The `or` carries the
//   parser's default `ignoring([])` match modifier.
#[test]
fn q52_outer_name_label_over_binary_op() {
    let side = |metric: &str, env: &str| {
        instant(scan(
            metric,
            vec![eq_pred(2, env)], // env
            &["env", "__name__"],
        ))
    };
    let expected = agg(
        vec![3], // __name__
        AggIntent::Sum { col: None },
        binary(
            BinaryOpKind::Set(PromQLVectorSetOpKind::Or),
            Some(VectorMatch {
                kind: VectorMatchKind::Ignoring,
                labels: vec![],
                grouping: None,
            }),
            side("metric_a", "1"),
            side("metric_b", "2"),
        ),
    );
    assert_eq!(
        lower(r#"sum by (__name__)(metric_a{env="1"} or metric_b{env="2"})"#),
        expected,
    );
}

// #39 — `sum without (instance) (rate(m[5m]))`: a cross-series reduction over
//   the per-series rate, grouped by every label EXCEPT `instance`. The excluded
//   label is seeded (referenced) and stored positionally as the `without` form;
//   the inner rate is label-preserving. Scan schema [ts(0), value(1), instance(2)].
#[test]
fn q39_sum_without_instance_over_rate() {
    let inner_rate = agg_per_entity(
        AggIntent::Rate,
        range(300, scan("http_requests_total", vec![], &["instance"])),
    );
    let expected = node(NonASAPOp::Aggregate {
        reduction: Reduction::Reduce(GroupKeys::without(vec![2])), // exclude `instance`
        measures: vec![AggIntent::Sum { col: None }],
        output_names: vec!["".into()],
        filters: vec![],
        having: None,
        child: inner_rate,
    });
    assert_eq!(
        lower("sum without (instance) (rate(http_requests_total[5m]))"),
        expected,
    );
}

// #40 — week-over-week: `rate(m[5m]) - rate(m[5m] offset 1w)`. Only the RHS
//   selector is time-shifted, so its scan is wrapped in a `TimeShift` under the
//   `TimeRange`; the LHS is a bare rate. Both sides bound independently.
#[test]
fn q40_week_over_week_offset() {
    let rate_over = |shift: Option<i64>| {
        let scan = scan("m", vec![], &[]);
        let ranged = match shift {
            Some(ms) => node(NonASAPOp::TimeShift {
                shift: TimeShift {
                    offset_ms: ms,
                    at: None,
                },
                child: scan,
            }),
            None => scan,
        };
        agg_per_entity(AggIntent::Rate, range(300, ranged))
    };
    let expected = binary(
        BinaryOpKind::Arithmetic(ArithmeticOpKind::Sub),
        None,
        rate_over(None),
        rate_over(Some(604_800_000)), // 1w
    );
    assert_eq!(lower("rate(m[5m]) - rate(m[5m] offset 1w)"), expected,);
}

// #40 — `@` anchor: `up @ 1609746000` pins the eval time to an absolute instant
//   (seconds → ms); a bare selector wrapped in a `TimeShift` carrying the anchor.
#[test]
fn q40_at_modifier_absolute() {
    let expected = instant(node(NonASAPOp::TimeShift {
        shift: TimeShift {
            offset_ms: 0,
            at: Some(AtModifier::Timestamp(1_609_746_000_000)),
        },
        child: scan("up", vec![], &[]),
    }));
    assert_eq!(lower("up @ 1609746000"), expected);
}

// #24 — sum by job over rate over a filtered scan
//   same schema [ts, value, job, status]; rate is label-preserving,
//   so outer sum by job still finds job at col 2
#[test]
fn q24_sum_by_job_over_rate_over_filtered_scan() {
    let scan = scan(
        "http_requests_total",
        vec![eq_pred(3, "200")],
        &["job", "status"],
    );
    let inner_rate = agg_per_entity(AggIntent::Rate, range(300, scan));
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
    let rate = agg_per_entity(
        AggIntent::Rate,
        range(5, scan("distance_covered_total", vec![], &[])),
    );
    let deriv = agg_per_entity(
        AggIntent::Deriv,
        node(NonASAPOp::PromqlSubquery {
            range: Duration::from_secs(30),
            resolution: Some(Duration::from_secs(5)),
            child: rate,
        }),
    );
    let expected = agg_per_entity(
        AggIntent::Max { col: None },
        node(NonASAPOp::PromqlSubquery {
            range: Duration::from_secs(600),
            resolution: None,
            child: deriv,
        }),
    );
    assert_eq!(
        lower("max_over_time(deriv(rate(distance_covered_total[5s])[30s:5s])[10m:])"),
        expected,
    );
}
