//! `column_signs`, `depends_on_evaluation_time` and `scanned_sources`,
//! derived once per memo group from its input groups (concepts doc, section
//! 3, "Value sign" and `GroupProperties`; `aggregate_coverage` is in
//! `memo_aggregate_coverage.rs`). Every example reads one of three sources:
//!
//! ```text
//! latency         [ts, value, host]   value declared non-negative
//! requests_total  [ts, value, host]   a counter: value non-negative
//! gauge           [ts, value, host]   nothing declared
//! ```
use std::collections::{BTreeMap, BTreeSet};

use asap_types::ir::operator::arguments::{
    AtModifier, BinaryOpKind, GroupKeys, JoinKind, Reduction, Source,
};
use asap_types::ir::operator::AggIntent;

use asap_types::ir::scalar::expr_ir::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use asap_types::ir::scalar::{ColumnRef, ProjectItem};
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, OperatorResultKind, Predicate,
    ScalarExpr,
};
use asap_types::stages::{Memo, MemoGroupId, ValueSign};
use asap_types::summary_type::{SketchAlgorithm, SketchStatistic};
use asap_types::test_support::memo_fixtures::{filter, range, scan, shift, state};
use asap_types::workload::{DataWorkload, PrometheusMetricType, SourceFacts};

use ValueSign::{NonNegative, NonPositive, Unknown};

const TS: usize = 0;
const VALUE: usize = 1;
const HOST: usize = 2;
const MINUTE_MS: i64 = 60_000;

/// The memo of every example: `latency.value` declared non-negative, and
/// `requests_total` a declared counter, whose `value` is non-negative.
fn memo() -> Memo {
    Memo::new(&DataWorkload {
        sources: BTreeMap::from([
            (
                "latency".to_string(),
                SourceFacts {
                    column_signs: BTreeMap::from([("value".to_string(), NonNegative)]),
                    ..SourceFacts::default()
                },
            ),
            (
                "requests_total".to_string(),
                SourceFacts {
                    metric_type: Some(PrometheusMetricType::Counter),
                    ..SourceFacts::default()
                },
            ),
        ]),
        ..DataWorkload::default()
    })
}

fn add(memo: &mut Memo, expression: Operator<MemoGroupId>) -> MemoGroupId {
    memo.find_or_add_group(expression).unwrap()
}

fn signs(memo: &Memo, group: MemoGroupId) -> &[ValueSign] {
    &memo.groups()[group.0 as usize]
        .group_properties
        .column_signs
}

fn depends(memo: &Memo, group: MemoGroupId) -> bool {
    memo.groups()[group.0 as usize]
        .group_properties
        .depends_on_evaluation_time
}

/// `Scan metric`: `[ts, value, host]`.
fn series(metric: &str) -> Operator<MemoGroupId> {
    scan(metric, &[("host", DataType::Utf8)], vec![])
}

fn compare(
    left: ScalarExpr<MemoGroupId>,
    op: CompareOpKind,
    right: ScalarExpr<MemoGroupId>,
) -> ScalarExpr<MemoGroupId> {
    ScalarExpr::Compare {
        left: Box::new(left),
        op,
        right: Box::new(right),
        semantics: ExprSemantics::Sql,
    }
}

/// `Project(ts, expr AS value, host)`.
fn map_value(expr: ScalarExpr<MemoGroupId>, child: MemoGroupId) -> Operator<MemoGroupId> {
    let item = |expr, alias: &str| ProjectItem {
        alias: Some(alias.into()),
        expr,
    };
    Operator::NonASAP(NonASAPOp::Project {
        cols: vec![
            item(ScalarExpr::Column(TS), "ts"),
            item(expr, "value"),
            item(ScalarExpr::Column(HOST), "host"),
        ],
        qualifier: None,
        child,
    })
}

fn aggregate(
    reduction: Reduction,
    measure: AggIntent,
    child: MemoGroupId,
) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::Aggregate {
        reduction,
        measures: vec![measure],
        output_names: vec![],
        filters: vec![],
        having: None,
        child,
    })
}

fn by_host() -> Reduction {
    Reduction::by(vec![HOST])
}

fn readout(query: SketchStatistic, state: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: state,
        query,
    })
}

/// Rows of the per-operator table that keep or set a column's sign:
///
/// | Group | Expression | `column_signs` |
/// |---|---|---|
/// | G1 | `Scan latency` | `[Unknown, NonNegative, Unknown]` (declared) |
/// | G2 | `Scan gauge` | `[Unknown, Unknown, Unknown]` (nothing declared) |
/// | G3 | `Filter(value > 1)(G1)` | G1's |
/// | G4 | `TimeRange[1m](G3)` | G1's |
/// | G5 | `TimeShift(1m)(G4)` | G1's |
/// | G6 | `Project(abs(value))(G2)` | value `NonNegative` |
/// | G7 | `Project(-value)(G1)` | value `NonPositive` (flipped) |
/// | G8 | `Project(-value)(G7)` | value `NonNegative` (flipped back) |
/// | G9 | `Project(value * 2)(G1)` | value `NonNegative` (same signs multiply) |
/// | G10 | `Project(value - value)(G1)` | value `Unknown` (a difference) |
#[test]
fn scans_read_declared_signs_and_row_operators_keep_or_set_them() {
    let mut memo = memo();
    let g1 = add(&mut memo, series("latency"));
    let g2 = add(&mut memo, series("gauge"));
    let g3 = add(
        &mut memo,
        filter(
            compare(
                ScalarExpr::Column(VALUE),
                CompareOpKind::Gt,
                ScalarExpr::Literal(ScalarValue::Float64(1.0)),
            ),
            g1,
        ),
    );
    let g4 = add(&mut memo, range(MINUTE_MS, g3));
    let g5 = add(&mut memo, shift(MINUTE_MS, None, g4));
    let value = || Box::new(ScalarExpr::Column(VALUE));
    let negated = || ScalarExpr::Negative {
        expr: value(),
        semantics: ExprSemantics::Promql,
    };
    let g6 = add(
        &mut memo,
        map_value(
            ScalarExpr::FunctionCall {
                name: "promql_abs".into(),
                args: vec![ScalarExpr::Column(VALUE)],
            },
            g2,
        ),
    );
    let g7 = add(&mut memo, map_value(negated(), g1));
    let g8 = add(&mut memo, map_value(negated(), g7));
    let g9 = add(
        &mut memo,
        map_value(
            ScalarExpr::Arithmetic {
                op: ArithmeticOpKind::Mul,
                left: value(),
                right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(2.0))),
                semantics: ExprSemantics::Promql,
            },
            g1,
        ),
    );
    let g10 = add(
        &mut memo,
        map_value(
            ScalarExpr::Arithmetic {
                op: ArithmeticOpKind::Sub,
                left: value(),
                right: value(),
                semantics: ExprSemantics::Promql,
            },
            g1,
        ),
    );
    let declared = [Unknown, NonNegative, Unknown];
    assert_eq!(signs(&memo, g1), declared);
    assert_eq!(signs(&memo, g2), [Unknown; 3]);
    for group in [g3, g4, g5] {
        assert_eq!(signs(&memo, group), declared, "{group:?}");
    }
    for (group, value) in [
        (g6, NonNegative),
        (g7, NonPositive),
        (g8, NonNegative),
        (g9, NonNegative),
        (g10, Unknown),
    ] {
        assert_eq!(signs(&memo, group), [Unknown, value, Unknown], "{group:?}");
    }
}

/// Aggregates: keys keep their sign, sum / max keep the value's, counts are
/// non-negative, and a rate is non-negative over a non-negative value only:
///
/// | Group | Expression | `column_signs` |
/// |---|---|---|
/// | G2 | `sum by (host)(G1)`, G1 = `Scan latency` | `[Unknown, NonNegative]` |
/// | G3 | `max by (host)(G1)` | `[Unknown, NonNegative]` |
/// | G5 | `sum by (host)(G4)`, G4 = `Scan gauge` | `[Unknown, Unknown]` |
/// | G6 | `count by (host)(G4)` | `[Unknown, NonNegative]` |
/// | G7 | `count(distinct value) by (host)(G4)` | `[Unknown, NonNegative]` |
/// | G9 | `rate(G8)`, G8 = `requests_total[1m]` | value `NonNegative` |
/// | G11 | `rate(G10)`, G10 = `gauge[1m]` | value `Unknown` |
#[test]
fn aggregates_keep_their_keys_and_derive_their_values() {
    let mut memo = memo();
    let g1 = add(&mut memo, series("latency"));
    let sum = || AggIntent::Sum { col: Some(VALUE) };
    let g2 = add(&mut memo, aggregate(by_host(), sum(), g1));
    let g3 = add(
        &mut memo,
        aggregate(by_host(), AggIntent::Max { col: Some(VALUE) }, g1),
    );
    let g4 = add(&mut memo, series("gauge"));
    let g5 = add(&mut memo, aggregate(by_host(), sum(), g4));
    let g6 = add(&mut memo, aggregate(by_host(), AggIntent::Count, g4));
    let g7 = add(
        &mut memo,
        aggregate(by_host(), AggIntent::Cardinality { cols: vec![VALUE] }, g4),
    );
    let requests = add(&mut memo, series("requests_total"));
    let g8 = add(&mut memo, range(MINUTE_MS, requests));
    let g9 = add(
        &mut memo,
        aggregate(Reduction::PerEntity, AggIntent::Rate, g8),
    );
    let g10 = add(&mut memo, range(MINUTE_MS, g4));
    let g11 = add(
        &mut memo,
        aggregate(Reduction::PerEntity, AggIntent::Rate, g10),
    );
    for (group, value) in [
        (g2, NonNegative),
        (g3, NonNegative),
        (g5, Unknown),
        (g6, NonNegative),
        (g7, NonNegative),
    ] {
        assert_eq!(signs(&memo, group), [Unknown, value], "{group:?}");
    }
    assert_eq!(signs(&memo, g9), [Unknown, NonNegative, Unknown]);
    assert_eq!(signs(&memo, g11), [Unknown; 3]);
}

/// A state records the sign of the values it is updated with, a merge
/// keeps the sign its inputs agree on, and a readout reads the state's, a
/// count-like statistic being non-negative whatever the values:
///
/// | Group | Expression | `column_signs` |
/// |---|---|---|
/// | G3 | `SummaryAgg[KLL](G2)`, G2 = `latency[1m]` | state `NonNegative` |
/// | G6 | `SummaryAgg[KLL](G5)`, G5 = `(latency offset 1m)[1m]` | state `NonNegative` |
/// | G7 | `SummaryMerge(G3, G6)` | state `NonNegative` (agreed) |
/// | G8 | `SummaryEstimate[p99](G7)` | quantile `NonNegative` |
/// | G11 | `SummaryAgg[HLL](G10)`, G10 = `gauge[1m]` | state `Unknown` |
/// | G12 | `SummaryEstimate[cardinality](G11)` | cardinality `NonNegative` |
#[test]
fn states_record_their_input_sign_and_readouts_read_it() {
    let mut memo = memo();
    let g1 = add(&mut memo, series("latency"));
    let g2 = add(&mut memo, range(MINUTE_MS, g1));
    let g3 = add(
        &mut memo,
        state(FieldDataType::Sketch(SketchAlgorithm::Kll), g2),
    );
    let g4 = add(&mut memo, shift(MINUTE_MS, None, g1));
    let g5 = add(&mut memo, range(MINUTE_MS, g4));
    let g6 = add(
        &mut memo,
        state(FieldDataType::Sketch(SketchAlgorithm::Kll), g5),
    );
    let g7 = add(
        &mut memo,
        Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![g3, g6],
            group_by: None,
        }),
    );
    let g8 = add(
        &mut memo,
        readout(SketchStatistic::Quantile { q: 0.99 }, g7),
    );
    let g9 = add(&mut memo, series("gauge"));
    let g10 = add(&mut memo, range(MINUTE_MS, g9));
    let g11 = add(
        &mut memo,
        state(FieldDataType::Sketch(SketchAlgorithm::Hll), g10),
    );
    let g12 = add(&mut memo, readout(SketchStatistic::Cardinality, g11));
    // [ts, state, host]
    for group in [g3, g6, g7] {
        assert_eq!(
            signs(&memo, group),
            [Unknown, NonNegative, Unknown],
            "{group:?}"
        );
    }
    assert_eq!(signs(&memo, g8), [Unknown, NonNegative, Unknown]);
    assert_eq!(signs(&memo, g11), [Unknown; 3]);
    assert_eq!(signs(&memo, g12), [Unknown, NonNegative, Unknown]);
}

/// The concepts doc's example: `topk(5, sum by (host)(gauge))` ranks values
/// of `Unknown` sign (Stage 2 then prunes the Count-Min heap builds), while
/// `topk(5, sum by (host)(rate(requests_total[1m])))` ranks `NonNegative`
/// ones.
#[test]
fn a_top_k_ranks_values_of_its_inputs_sign() {
    let mut memo = memo();
    let top_5 = |memo: &mut Memo, child| {
        add(
            memo,
            aggregate(
                Reduction::Reduce(GroupKeys::by(vec![])),
                AggIntent::TopK { k: 5 },
                child,
            ),
        )
    };
    let sum = AggIntent::Sum { col: Some(VALUE) };
    let gauge = add(&mut memo, series("gauge"));
    let gauge_sum = add(&mut memo, aggregate(by_host(), sum.clone(), gauge));
    let gauge_top = top_5(&mut memo, gauge_sum);
    let requests = add(&mut memo, series("requests_total"));
    let ranged = add(&mut memo, range(MINUTE_MS, requests));
    let rate = add(
        &mut memo,
        aggregate(Reduction::PerEntity, AggIntent::Rate, ranged),
    );
    let rate_sum = add(&mut memo, aggregate(by_host(), sum, rate));
    let rate_top = top_5(&mut memo, rate_sum);
    // [host, sum]: the ranked value is column 1 of the ranked rows.
    assert_eq!(signs(&memo, gauge_sum), [Unknown, Unknown]);
    assert_eq!(signs(&memo, rate_sum), [Unknown, NonNegative]);
    let ranked = |group| {
        let schema = &memo.groups()[group as usize].group_properties.schema;
        let value = schema.column_id("topk_5").unwrap();
        signs(&memo, MemoGroupId(group))[value]
    };
    assert_eq!(ranked(gauge_top.0), Unknown);
    assert_eq!(ranked(rate_top.0), NonNegative);
}

/// `depends_on_evaluation_time`: a `TimeRange` reads the window before the
/// evaluation time, `now()` reads it directly, and everything above them
/// depends on it, unless `@ T` pins the subtree; `@ end()` does not pin it
/// to a fixed time.
///
/// | Group | Expression | Depends |
/// |---|---|---|
/// | G1 | `Scan latency` | no |
/// | G2 | `sum by (host)(G1)` | no |
/// | G3 | `latency[1m]` | yes |
/// | G4 | `SummaryAgg[KLL](G3)` | yes |
/// | G5 | `TimeShift(@ T, offset 1m)(G3)` | no |
/// | G6 | `SummaryAgg[KLL](G5)` | no |
/// | G7 | `TimeShift(@ end(), offset 1m)(G3)` | yes |
/// | G8 | `Filter(ts > now())(G1)` | yes |
#[test]
fn a_result_depends_on_the_evaluation_time_under_a_time_range_or_now() {
    let mut memo = memo();
    let g1 = add(&mut memo, series("latency"));
    let g2 = add(
        &mut memo,
        aggregate(by_host(), AggIntent::Sum { col: Some(VALUE) }, g1),
    );
    let g3 = add(&mut memo, range(MINUTE_MS, g1));
    let g4 = add(
        &mut memo,
        state(FieldDataType::Sketch(SketchAlgorithm::Kll), g3),
    );
    let g5 = add(
        &mut memo,
        shift(
            MINUTE_MS,
            Some(AtModifier::Timestamp(1_700_000_000_000)),
            g3,
        ),
    );
    let g6 = add(
        &mut memo,
        state(FieldDataType::Sketch(SketchAlgorithm::Kll), g5),
    );
    let g7 = add(&mut memo, shift(MINUTE_MS, Some(AtModifier::End), g3));
    let g8 = add(
        &mut memo,
        filter(
            compare(
                ScalarExpr::Column(TS),
                CompareOpKind::Gt,
                ScalarExpr::CurrentTimestamp,
            ),
            g1,
        ),
    );
    for (group, expected) in [
        (g1, false),
        (g2, false),
        (g3, true),
        (g4, true),
        (g5, false),
        (g6, false),
        (g7, true),
        (g8, true),
    ] {
        assert_eq!(depends(&memo, group), expected, "{group:?}");
    }
}

/// `scanned_sources`, derived once per group: a `Scan` its source, any other
/// group the union over its input groups. A PromQL `Scan` of a time series
/// and a `TimeRange` over it scan only time series
/// (`Source::is_time_series`); a SQL table `Scan` does not, nor a `Join`
/// reading two (a SQL join reads relations only, so a time series cannot be
/// one of its inputs).
#[test]
fn a_group_scans_the_sources_of_the_scans_below_it() {
    let mut memo = memo();
    let series = add(&mut memo, series("latency"));
    let window = add(&mut memo, range(MINUTE_MS, series));
    let table = |name: &str| {
        Operator::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: name.into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![
                Field::plain("host", DataType::Utf8, false),
                Field::plain("bytes", DataType::Float64, false),
            ]),
        })
    };
    let requests = add(&mut memo, table("requests"));
    let hosts = add(&mut memo, table("hosts"));
    let join = add(
        &mut memo,
        Operator::NonASAP(NonASAPOp::Join {
            kind: JoinKind::Inner,
            pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
            left: requests,
            right: hosts,
        }),
    );
    let properties = |group: MemoGroupId| &memo.groups()[group.0 as usize].group_properties;
    let table_source = |name: &str| Source::Table {
        table_ref: name.into(),
    };
    assert_eq!(
        properties(join).scanned_sources,
        BTreeSet::from([table_source("hosts"), table_source("requests")])
    );
    assert_eq!(
        properties(window).scanned_sources,
        BTreeSet::from([Source::TimeSeries {
            metric: "latency".into()
        }])
    );
    let reads = |group: MemoGroupId| {
        properties(group)
            .scanned_sources
            .iter()
            .all(Source::is_time_series)
    };
    assert!(reads(series));
    assert!(reads(window));
    assert!(!reads(requests));
    assert!(!reads(join));
}

/// `count_values without (host) ("version", latency)`: a `without`
/// aggregate's layout adds no label column for `count_values`
/// (`aggregate_output_schema`), so the group has one sign per output
/// column, the count's non-negative.
#[test]
fn count_values_without_has_one_sign_per_output_column() {
    let mut memo = memo();
    let g1 = add(&mut memo, series("latency"));
    let g2 = add(
        &mut memo,
        aggregate(
            Reduction::Reduce(GroupKeys::without(vec![HOST])),
            AggIntent::CountValues {
                label: "version".into(),
            },
            g1,
        ),
    );
    let group = &memo.groups()[g2.0 as usize].group_properties;
    assert_eq!(group.column_signs.len(), group.schema.fields.len());
    assert_eq!(group.column_signs.last(), Some(&NonNegative));
}

/// A group's properties come from its operator and its input groups'
/// properties: the scan's schema, the state's kind and state column, the
/// readout's value column, and whether equal copies may be one group.
#[test]
fn group_properties_come_from_input_groups() {
    let mut memo = memo();
    let g1 = add(&mut memo, series("latency"));
    let api = compare(
        ScalarExpr::Column(HOST),
        CompareOpKind::Eq,
        ScalarExpr::Literal(ScalarValue::Utf8("api".into())),
    );
    let g2 = add(&mut memo, filter(api, g1));
    let g3 = add(&mut memo, range(MINUTE_MS, g2));
    let g4 = add(
        &mut memo,
        state(FieldDataType::Sketch(SketchAlgorithm::Kll), g3),
    );
    let readout = memo
        .find_or_add_group(Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: g4,
            query: SketchStatistic::Quantile { q: 0.99 },
        }))
        .unwrap();
    let properties = |group: MemoGroupId| &memo.groups()[group.0 as usize].group_properties;
    assert_eq!(properties(g1).schema.fields.len(), 3);
    assert_eq!(properties(g1).aggregate_coverage, None);
    assert_eq!(properties(g4).result_kind, OperatorResultKind::State);
    assert!(properties(g4)
        .schema
        .fields
        .iter()
        .any(|field| field.name == "state" && !field.is_plain()));
    assert!(properties(g4).aggregate_coverage.is_some());
    assert_eq!(
        properties(readout).result_kind,
        OperatorResultKind::InstantVector
    );
    assert!(properties(readout)
        .schema
        .fields
        .iter()
        .any(|field| field.name == "quantile"
            && field.dtype == FieldDataType::Plain(DataType::Float64)));
    assert!(properties(g4).shareable_when_equal);
}

/// A point count read from a Count Sketch is a median of signed counters,
/// which can be negative over non-negative weights: `Unknown`; one read
/// from a Count-Min state, an overestimate of a sum of non-negative
/// weights, keeps the weights' sign.
#[test]
fn a_count_sketch_point_count_has_no_sign_a_count_min_one_keeps_it() {
    let mut memo = memo();
    let g1 = add(&mut memo, series("latency"));
    let g2 = add(&mut memo, range(MINUTE_MS, g1));
    let point_count = |memo: &mut Memo, algorithm| {
        let state = add(memo, state(FieldDataType::Sketch(algorithm), g2));
        add(
            memo,
            readout(
                SketchStatistic::PointCount {
                    key: ColumnRef::Named("host".into()),
                    value: Some("a".into()),
                },
                state,
            ),
        )
    };
    let count_sketch = point_count(&mut memo, SketchAlgorithm::CountSketch);
    let count_min = point_count(&mut memo, SketchAlgorithm::Cms);
    // [ts, count, host]: the count is the state's column.
    let value = |group: MemoGroupId| {
        let schema = &memo.groups()[group.0 as usize].group_properties.schema;
        signs(&memo, group)[schema.column_id("count").unwrap()]
    };
    assert_eq!(value(count_sketch), Unknown);
    assert_eq!(value(count_min), NonNegative);
}

/// PromQL's `quantile(φ, v)` with φ outside [0, 1] is ±Inf, so a quantile
/// keeps its input's sign only for 0 ≤ φ ≤ 1, as a measure and as a
/// sketch readout.
#[test]
fn a_quantile_outside_the_unit_interval_has_no_sign() {
    let mut memo = memo();
    let g1 = add(&mut memo, series("latency"));
    let quantile = |memo: &mut Memo, q| {
        add(
            memo,
            aggregate(
                by_host(),
                AggIntent::Quantile {
                    col: Some(VALUE),
                    q,
                },
                g1,
            ),
        )
    };
    let inside = quantile(&mut memo, 0.5);
    let below = quantile(&mut memo, -0.5);
    let g2 = add(&mut memo, range(MINUTE_MS, g1));
    let kll = add(
        &mut memo,
        state(FieldDataType::Sketch(SketchAlgorithm::Kll), g2),
    );
    let read = add(
        &mut memo,
        readout(SketchStatistic::Quantile { q: 1.5 }, kll),
    );
    let value = |group: MemoGroupId| *signs(&memo, group).last().unwrap();
    assert_eq!(value(inside), NonNegative);
    assert_eq!(value(below), Unknown);
    assert_eq!(value(read), Unknown);
}

fn arithmetic(op: ArithmeticOpKind, lhs: MemoGroupId, rhs: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::BinaryOp {
        operator: BinaryOperator {
            checked_relative_division: false,
            checked_finite_division: false,
            kind: BinaryOpKind::Arithmetic(op),
            vector_match: None,
        },
        return_bool: false,
        lhs,
        rhs,
    })
}

/// Arithmetic between two vectors: a sum of non-negatives, and a product or
/// quotient of same-signed values, is non-negative; a difference has no
/// sign. A `vector(s)` value has `s`'s sign:
///
/// | Group | Expression | value sign |
/// |---|---|---|
/// | G3 | `quantile_over_time(0.9, G2)`, G2 = `latency[1h]` | `NonNegative` |
/// | G4 | `quantile_over_time(0.5, G2)` | `NonNegative` |
/// | G5 | `G3 / G4` | `NonNegative` |
/// | G6 | `G3 + G4` | `NonNegative` |
/// | G7 | `G3 - G4` | `Unknown` |
/// | G8 | `vector(1000)` | `NonNegative` |
/// | G9 | `vector(-1)` | `NonPositive` |
/// | G10 | `G3 * G8` | `NonNegative` |
/// | G11 | `G3 * G9` | `NonPositive` |
/// | G12 | `G9 * G9` | `NonNegative` |
#[test]
fn vector_arithmetic_follows_its_operands_signs() {
    let mut memo = memo();
    let g1 = add(&mut memo, series("latency"));
    let g2 = add(&mut memo, range(60 * MINUTE_MS, g1));
    let quantile = |q| AggIntent::Quantile {
        col: Some(VALUE),
        q,
    };
    let g3 = add(
        &mut memo,
        aggregate(Reduction::PerEntity, quantile(0.9), g2),
    );
    let g4 = add(
        &mut memo,
        aggregate(Reduction::PerEntity, quantile(0.5), g2),
    );
    let g5 = add(&mut memo, arithmetic(ArithmeticOpKind::Div, g3, g4));
    let g6 = add(&mut memo, arithmetic(ArithmeticOpKind::Add, g3, g4));
    let g7 = add(&mut memo, arithmetic(ArithmeticOpKind::Sub, g3, g4));
    let vector = |s: f64| {
        Operator::NonASAP(NonASAPOp::PromqlVectorFromScalar(ScalarExpr::Literal(
            ScalarValue::Float64(s),
        )))
    };
    let g8 = add(&mut memo, vector(1000.0));
    let g9 = add(&mut memo, vector(-1.0));
    let g10 = add(&mut memo, arithmetic(ArithmeticOpKind::Mul, g3, g8));
    let g11 = add(&mut memo, arithmetic(ArithmeticOpKind::Mul, g3, g9));
    let g12 = add(&mut memo, arithmetic(ArithmeticOpKind::Mul, g9, g9));
    let value = |memo: &Memo, group: MemoGroupId| {
        let schema = &memo.groups()[group.0 as usize].group_properties.schema;
        signs(memo, group)[schema.column_id("value").unwrap()]
    };
    for (group, sign) in [
        (g3, NonNegative),
        (g4, NonNegative),
        (g5, NonNegative),
        (g6, NonNegative),
        (g7, Unknown),
        (g8, NonNegative),
        (g9, NonPositive),
        (g10, NonNegative),
        (g11, NonPositive),
        (g12, NonNegative),
    ] {
        assert_eq!(value(&memo, group), sign, "{group:?}");
    }
}

/// A vector-scalar operation lowers to a `Project` of an arithmetic
/// expression (PromQL `x / 1000`, `x * 8`), as does SQL's `value * 8`; the
/// expression's sign follows the same rule as vector arithmetic:
///
/// | Group | Expression | value sign |
/// |---|---|---|
/// | G2 | `G1 / 1000`, G1 = `Scan latency` (PromQL) | `NonNegative` |
/// | G3 | `G1 * 8` (PromQL) | `NonNegative` |
/// | G4 | `SELECT value * 8 FROM G1` (SQL) | `NonNegative` |
/// | G5 | `G1 - G1` | `Unknown` |
/// | G6 | `G1 * -8` | `NonPositive` |
/// | G7 | `G1 / -0` | `NonPositive` (`-0` has its sign bit set: the quotient is `-inf`, `-0` or NaN) |
#[test]
fn scalar_arithmetic_follows_its_operands_signs() {
    let mut memo = memo();
    let g1 = add(&mut memo, series("latency"));
    let expr = |op, right: ScalarExpr<MemoGroupId>, semantics| ScalarExpr::Arithmetic {
        op,
        left: Box::new(ScalarExpr::Column(VALUE)),
        right: Box::new(right),
        semantics,
    };
    let number = |n: f64| ScalarExpr::Literal(ScalarValue::Float64(n));
    let promql = ExprSemantics::Promql;
    let g2 = add(
        &mut memo,
        map_value(expr(ArithmeticOpKind::Div, number(1000.0), promql), g1),
    );
    let g3 = add(
        &mut memo,
        map_value(expr(ArithmeticOpKind::Mul, number(8.0), promql), g1),
    );
    let g4 = add(
        &mut memo,
        map_value(
            expr(
                ArithmeticOpKind::Mul,
                ScalarExpr::Literal(ScalarValue::Int64(8)),
                ExprSemantics::Sql,
            ),
            g1,
        ),
    );
    let g5 = add(
        &mut memo,
        map_value(
            expr(ArithmeticOpKind::Sub, ScalarExpr::Column(VALUE), promql),
            g1,
        ),
    );
    let g6 = add(
        &mut memo,
        map_value(expr(ArithmeticOpKind::Mul, number(-8.0), promql), g1),
    );
    let g7 = add(
        &mut memo,
        map_value(expr(ArithmeticOpKind::Div, number(-0.0), promql), g1),
    );
    for (group, value) in [
        (g2, NonNegative),
        (g3, NonNegative),
        (g4, NonNegative),
        (g5, Unknown),
        (g6, NonPositive),
        (g7, NonPositive),
    ] {
        assert_eq!(signs(&memo, group), [Unknown, value, Unknown], "{group:?}");
    }
}
