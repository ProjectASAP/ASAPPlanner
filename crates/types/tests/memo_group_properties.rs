//! `column_sign` and `depends_on_evaluation_time`, derived once per memo
//! group from its input groups (concepts doc, section 3, "Value sign" and
//! `GroupProperties`). Every example reads one of three sources:
//!
//! ```text
//! latency         [ts, value, host]   value declared non-negative
//! requests_total  [ts, value, host]   a counter: value non-negative
//! gauge           [ts, value, host]   nothing declared
//! ```
use std::collections::BTreeMap;
use std::time::Duration;

use asap_types::ir::operator::operator_properties::{
    AtModifier, GroupKeys, Reduction, Source, SourceColumn, TimeShift,
};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::expr_ir::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use asap_types::ir::scalar::{ColumnRef, ProjectItem};
use asap_types::ir::schema::{
    DataType, Field, FieldDataType, Schema, SketchAlgorithm, SketchStatistic, SummaryUpdate,
};
use asap_types::ir::{
    ASAPOp, ExprSemantics, NonASAPOp, Operator, Predicate, ScalarExpr, TimeRangeKind,
};
use asap_types::stages::{Memo, MemoGroupId, ValueSign};

use ValueSign::{NonNegative, NonPositive, Unknown};

const TS: usize = 0;
const VALUE: usize = 1;
const HOST: usize = 2;
const MINUTE_MS: u64 = 60_000;

/// The memo of every example: `latency.value` declared non-negative, and
/// `requests_total.value` as Stage 0 declares a counter's.
fn memo() -> Memo {
    let non_negative = |source: &str| {
        (
            SourceColumn {
                source: source.into(),
                column: "value".into(),
            },
            NonNegative,
        )
    };
    Memo::new(BTreeMap::from([
        non_negative("latency"),
        non_negative("requests_total"),
    ]))
}

fn add(memo: &mut Memo, expression: Operator<MemoGroupId>) -> MemoGroupId {
    memo.find_or_add_group(expression).unwrap()
}

fn signs(memo: &Memo, group: MemoGroupId) -> &[ValueSign] {
    &memo.groups()[group.0 as usize].group_properties.column_sign
}

fn depends(memo: &Memo, group: MemoGroupId) -> bool {
    memo.groups()[group.0 as usize]
        .group_properties
        .depends_on_evaluation_time
}

/// `Scan metric`: `[ts, value, host]`.
fn scan(metric: &str) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: metric.into(),
        },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
                Field::plain("host", DataType::Utf8, false),
            ],
            0,
            vec![],
        ),
    })
}

fn range(child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(MINUTE_MS),
        kind: TimeRangeKind::Range,
        child,
    })
}

fn shift(at: Option<AtModifier>, child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::TimeShift {
        shift: TimeShift {
            offset_ms: 60_000,
            at,
        },
        child,
    })
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

fn filter(pred: ScalarExpr<MemoGroupId>, child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::Filter {
        pred: Predicate(pred),
        child,
    })
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

fn state(algorithm: SketchAlgorithm, child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        summary_type: FieldDataType::Sketch(algorithm),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
    })
}

fn readout(query: SketchStatistic, state: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: state,
        query,
    })
}

/// Rows of the per-operator table that keep or set a column's sign:
///
/// | Group | Expression | `column_sign` |
/// |---|---|---|
/// | G1 | `Scan latency` | `[Unknown, NonNegative, Unknown]` (declared) |
/// | G2 | `Scan gauge` | `[Unknown, Unknown, Unknown]` (nothing declared) |
/// | G3 | `Filter(value > 1)(G1)` | G1's |
/// | G4 | `TimeRange[1m](G3)` | G1's |
/// | G5 | `TimeShift(1m)(G4)` | G1's |
/// | G6 | `Project(abs(value))(G2)` | value `NonNegative` |
/// | G7 | `Project(-value)(G1)` | value `NonPositive` (flipped) |
/// | G8 | `Project(-value)(G7)` | value `NonNegative` (flipped back) |
/// | G9 | `Project(value * 2)(G1)` | value `Unknown` (another expression) |
#[test]
fn scans_read_declared_signs_and_row_operators_keep_or_set_them() {
    let mut memo = memo();
    let g1 = add(&mut memo, scan("latency"));
    let g2 = add(&mut memo, scan("gauge"));
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
    let g4 = add(&mut memo, range(g3));
    let g5 = add(&mut memo, shift(None, g4));
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
        (g9, Unknown),
    ] {
        assert_eq!(signs(&memo, group), [Unknown, value, Unknown], "{group:?}");
    }
}

/// Aggregates: keys keep their sign, sum / max keep the value's, counts are
/// non-negative, and a rate is non-negative over a non-negative value only:
///
/// | Group | Expression | `column_sign` |
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
    let g1 = add(&mut memo, scan("latency"));
    let sum = || AggIntent::Sum { col: Some(VALUE) };
    let g2 = add(&mut memo, aggregate(by_host(), sum(), g1));
    let g3 = add(
        &mut memo,
        aggregate(by_host(), AggIntent::Max { col: Some(VALUE) }, g1),
    );
    let g4 = add(&mut memo, scan("gauge"));
    let g5 = add(&mut memo, aggregate(by_host(), sum(), g4));
    let g6 = add(&mut memo, aggregate(by_host(), AggIntent::Count, g4));
    let g7 = add(
        &mut memo,
        aggregate(by_host(), AggIntent::Cardinality { cols: vec![VALUE] }, g4),
    );
    let requests = add(&mut memo, scan("requests_total"));
    let g8 = add(&mut memo, range(requests));
    let g9 = add(
        &mut memo,
        aggregate(Reduction::PerEntity, AggIntent::Rate, g8),
    );
    let g10 = add(&mut memo, range(g4));
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
/// | Group | Expression | `column_sign` |
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
    let g1 = add(&mut memo, scan("latency"));
    let g2 = add(&mut memo, range(g1));
    let g3 = add(&mut memo, state(SketchAlgorithm::Kll, g2));
    let g4 = add(&mut memo, shift(None, g1));
    let g5 = add(&mut memo, range(g4));
    let g6 = add(&mut memo, state(SketchAlgorithm::Kll, g5));
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
    let g9 = add(&mut memo, scan("gauge"));
    let g10 = add(&mut memo, range(g9));
    let g11 = add(&mut memo, state(SketchAlgorithm::Hll, g10));
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
    let gauge = add(&mut memo, scan("gauge"));
    let gauge_sum = add(&mut memo, aggregate(by_host(), sum.clone(), gauge));
    let gauge_top = top_5(&mut memo, gauge_sum);
    let requests = add(&mut memo, scan("requests_total"));
    let ranged = add(&mut memo, range(requests));
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
    let g1 = add(&mut memo, scan("latency"));
    let g2 = add(
        &mut memo,
        aggregate(by_host(), AggIntent::Sum { col: Some(VALUE) }, g1),
    );
    let g3 = add(&mut memo, range(g1));
    let g4 = add(&mut memo, state(SketchAlgorithm::Kll, g3));
    let g5 = add(
        &mut memo,
        shift(Some(AtModifier::Timestamp(1_700_000_000_000)), g3),
    );
    let g6 = add(&mut memo, state(SketchAlgorithm::Kll, g5));
    let g7 = add(&mut memo, shift(Some(AtModifier::End), g3));
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
