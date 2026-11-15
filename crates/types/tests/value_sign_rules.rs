//! The value-sign transfer rules, one table row per rule (sign test plan,
//! sections T1 and T4). Each row builds one operator over a `Scan` of a
//! source whose `value` sign is known, adds it to a memo and reads the sign
//! the memo derives for the column under test.
//!
//! Every expected sign is derived by hand from IEEE 754 and from what the
//! executor computes, never from the code under test. A sign is the IEEE
//! sign bit: `NonNegative` (N) holds for `+0.0`, positives and `+inf`;
//! `NonPositive` (P) for `-0.0`, negatives and `-inf`; NaN and NULL are
//! excluded; `Unknown` (U) claims nothing. An `Int64` value counts as
//! `v as f64`, so an int `0` is N, and an int column is P only when every
//! value is `< 0`.
//!
//! The sources every row reads:
//!
//! ```text
//! pos      gauge [ts, value, job]   value declared N
//! neg      gauge [ts, value, job]   value declared P
//! mixed    gauge [ts, value, job]   nothing declared
//! c_total  counter                  value N by the counter default
//! negc     counter                  value declared P (the declaration wins)
//! h        histogram family         nothing declared: h_sum, h_count, h_bucket
//! hd       histogram family         value declared N
//! flows    SQL table [ts, bytes Int64 N, p Float64 U, debt Int64 P,
//!          delta Int64 U, gain Float64 N, loss Float64 P]
//! t_pos    SQL table [v Float64 N]
//! t_neg    SQL table [v Float64 P]
//! ```
//!
//! A row whose expectation depends on a provisional answer to a question of
//! the plan says so with `provisional answer QN`. A test collects every
//! wrong row and fails once, listing them all.
use std::collections::BTreeMap;
use std::time::Duration;

use asap_types::ir::operator::arguments::{
    AtModifier, BinaryOpKind, GroupKeys, GroupSide, JoinKind, PromQLVectorSetOpKind, Reduction,
    RelationalSetOpKind, SampleKind, Source, VectorGrouping, VectorMatch, VectorMatchKind,
    WindowFuncKind,
};
use asap_types::ir::operator::maintained_population::{
    CurrentSeriesInput, MaintainedPopulation, PopulationInput,
};
use asap_types::ir::operator::{AggIntent, MathFunc, TimeFunc};
use asap_types::ir::scalar::expr_ir::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use asap_types::ir::scalar::scalar_type_rules::{math_function_of_call, time_function_of_call};
use asap_types::ir::scalar::{ColumnRef, ProjectItem, SortKey};
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, Predicate, ScalarExpr,
    TimeRangeKind,
};
use asap_types::stages::{ColumnLineage, Memo, MemoGroupId, ValueSign};
use asap_types::summary_type::{
    ExactKind, SketchAlgorithm, SketchStatistic, SummaryInputExpr, SummaryUpdate,
};
use asap_types::test_support::memo_fixtures::{aggregate, range, readout, shift};
use asap_types::workload::{DataWorkload, PrometheusMetricType, SourceFacts};

use ValueSign::{NonNegative as N, NonPositive as P, Unknown as U};

const TS: usize = 0;
const VALUE: usize = 1;
const JOB: usize = 2;
const MINUTE_MS: i64 = 60_000;

// flows' columns.
const BYTES: usize = 1;
const P_COL: usize = 2;
const DEBT: usize = 3;
const DELTA: usize = 4;
const GAIN: usize = 5;
const LOSS: usize = 6;

/// Every wrong row of one table, reported together.
#[derive(Default)]
struct Rows {
    wrong: Vec<String>,
    checked: usize,
}

impl Rows {
    fn check(&mut self, row: impl std::fmt::Display, got: ValueSign, expected: ValueSign) {
        self.checked += 1;
        if got != expected {
            self.wrong
                .push(format!("{row}: derived {got:?}, expected {expected:?}"));
        }
    }

    fn fail(&mut self, row: impl std::fmt::Display, why: impl std::fmt::Display) {
        self.checked += 1;
        self.wrong.push(format!("{row}: {why}"));
    }

    fn finish(self, table: &str) {
        assert!(
            self.wrong.is_empty(),
            "{table}: {} of {} rows wrong:\n  {}",
            self.wrong.len(),
            self.checked,
            self.wrong.join("\n  ")
        );
    }
}

fn facts(sign: Option<ValueSign>, metric_type: Option<PrometheusMetricType>) -> SourceFacts {
    SourceFacts {
        metric_type,
        column_signs: sign
            .map(|sign| BTreeMap::from([("value".to_string(), sign)]))
            .unwrap_or_default(),
        ..SourceFacts::default()
    }
}

fn table_facts(signs: &[(&str, ValueSign)]) -> SourceFacts {
    SourceFacts {
        column_signs: signs
            .iter()
            .map(|(column, sign)| (column.to_string(), *sign))
            .collect(),
        ..SourceFacts::default()
    }
}

/// The memo every row is added to (the module doc's sources).
fn memo() -> Memo {
    use PrometheusMetricType::{Counter, Gauge, Histogram};
    Memo::new(&DataWorkload {
        sources: BTreeMap::from([
            ("pos".to_string(), facts(Some(N), Some(Gauge))),
            ("neg".to_string(), facts(Some(P), Some(Gauge))),
            ("mixed".to_string(), facts(None, Some(Gauge))),
            ("c_total".to_string(), facts(None, Some(Counter))),
            ("negc".to_string(), facts(Some(P), Some(Counter))),
            ("h".to_string(), facts(None, Some(Histogram))),
            ("hd".to_string(), facts(Some(N), Some(Histogram))),
            (
                "flows".to_string(),
                table_facts(&[("bytes", N), ("debt", P), ("gain", N), ("loss", P)]),
            ),
            ("t_pos".to_string(), table_facts(&[("v", N)])),
            ("t_neg".to_string(), table_facts(&[("v", P)])),
        ]),
        ..DataWorkload::default()
    })
}

fn add(memo: &mut Memo, expression: Operator<MemoGroupId>) -> MemoGroupId {
    let shown = format!("{expression:?}");
    memo.find_or_add_group(expression)
        .unwrap_or_else(|error| panic!("cannot add {shown}: {error:?}"))
}

fn try_add(memo: &mut Memo, expression: Operator<MemoGroupId>) -> Result<MemoGroupId, String> {
    memo.find_or_add_group(expression)
        .map_err(|error| format!("{error:?}"))
}

fn properties(memo: &Memo, group: MemoGroupId) -> &asap_types::stages::GroupProperties {
    &memo.groups()[group.0 as usize].group_properties
}

fn signs(memo: &Memo, group: MemoGroupId) -> &[ValueSign] {
    &properties(memo, group).column_signs
}

fn sign_of(memo: &Memo, group: MemoGroupId, column: &str) -> ValueSign {
    let schema = &properties(memo, group).schema;
    let index = schema
        .column_id(column)
        .unwrap_or_else(|| panic!("no column {column} in {schema:?}"));
    signs(memo, group)[index]
}

/// The one numeric column `group` computes itself (its measure, readout or
/// expression): lineage `Computed` by `group`, a plain numeric type.
fn computed_value_sign(memo: &Memo, group: MemoGroupId) -> ValueSign {
    let properties = properties(memo, group);
    let computed: Vec<usize> = properties
        .column_lineage
        .iter()
        .zip(&properties.schema.fields)
        .enumerate()
        .filter(|(_, (lineage, field))| {
            matches!(lineage, ColumnLineage::Computed { group: g, .. } if *g == group)
                && matches!(
                    field.dtype,
                    FieldDataType::Plain(DataType::Float64 | DataType::Int64)
                )
        })
        .map(|(index, _)| index)
        .collect();
    let [column] = computed[..] else {
        panic!(
            "expected one computed numeric column, found {computed:?} in {:?}",
            properties.schema
        );
    };
    properties.column_signs[column]
}

/// `Scan metric`: `[ts, value, job]`.
fn series(metric: &str) -> Operator<MemoGroupId> {
    asap_types::test_support::memo_fixtures::scan(metric, &[("job", DataType::Utf8)], vec![])
}

fn table(name: &str, columns: &[(&str, DataType)]) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: name.into(),
        },
        predicates: vec![],
        schema: Schema::new(
            columns
                .iter()
                .map(|(name, dtype)| Field::plain(*name, dtype.clone(), false))
                .collect(),
        ),
    })
}

fn flows() -> Operator<MemoGroupId> {
    table(
        "flows",
        &[
            ("ts", DataType::Timestamp),
            ("bytes", DataType::Int64),
            ("p", DataType::Float64),
            ("debt", DataType::Int64),
            ("delta", DataType::Int64),
            ("gain", DataType::Float64),
            ("loss", DataType::Float64),
        ],
    )
}

/// The gauge whose `value` has `sign`.
fn gauge_of(sign: ValueSign) -> &'static str {
    match sign {
        N => "pos",
        P => "neg",
        U => "mixed",
    }
}

const SIGNS: [ValueSign; 3] = [N, P, U];

fn f64_literal(value: f64) -> ScalarExpr<MemoGroupId> {
    ScalarExpr::Literal(ScalarValue::Float64(value))
}

fn column(index: usize) -> Box<ScalarExpr<MemoGroupId>> {
    Box::new(ScalarExpr::Column(index))
}

/// `Project(ts, expr AS value, job)` over a series.
fn map_value(expr: ScalarExpr<MemoGroupId>, child: MemoGroupId) -> Operator<MemoGroupId> {
    let item = |expr, alias: &str| ProjectItem {
        alias: Some(alias.into()),
        expr,
    };
    Operator::NonASAP(NonASAPOp::Project {
        cols: vec![
            item(ScalarExpr::Column(TS), "ts"),
            item(expr, "value"),
            item(ScalarExpr::Column(JOB), "job"),
        ],
        child,
    })
}

/// `SELECT expr AS v` over a table: the sign of `v`.
fn projected_sign(memo: &mut Memo, expr: ScalarExpr<MemoGroupId>, child: MemoGroupId) -> ValueSign {
    let group = add(
        memo,
        Operator::NonASAP(NonASAPOp::Project {
            cols: vec![ProjectItem {
                alias: Some("v".into()),
                expr,
            }],
            child,
        }),
    );
    signs(memo, group)[0]
}

/// The sign of `value` after `expr` maps it, over the gauge of sign `input`.
fn mapped_value_sign(
    memo: &mut Memo,
    input: ValueSign,
    expr: ScalarExpr<MemoGroupId>,
) -> ValueSign {
    let scan = add(memo, series(gauge_of(input)));
    let group = add(memo, map_value(expr, scan));
    signs(memo, group)[VALUE]
}

fn binary(
    kind: BinaryOpKind,
    vector_match: Option<VectorMatch>,
    return_bool: bool,
    lhs: MemoGroupId,
    rhs: MemoGroupId,
) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::BinaryOp {
        operator: BinaryOperator {
            checked_relative_division: false,
            checked_finite_division: false,
            kind,
            vector_match,
        },
        return_bool,
        lhs,
        rhs,
    })
}

fn vector_of(expr: ScalarExpr<MemoGroupId>) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::PromqlVectorFromScalar(expr))
}

// ── T1.1 ────────────────────────────────────────────────────────────────

/// T1.1: a `Scan` reads the declared signs; a counter's `value` is N without
/// a declaration, and a declaration wins over that default. An undeclared
/// histogram's `_sum` is U: a histogram may observe negative values, so
/// its `_sum` decreases (B14). A label is U.
///
/// Fails today: `h_sum` is N, because `series_facts` types a family's
/// `_sum` as a counter.
#[test]
fn scan_signs_follow_declarations_and_metric_types() {
    let mut memo = memo();
    let mut rows = Rows::default();
    for (metric, expected, note) in [
        ("pos", N, "declared N"),
        ("neg", P, "declared P"),
        ("mixed", U, "undeclared gauge"),
        ("c_total", N, "a counter"),
        // provisional answer Q8: the declaration wins over the counter default.
        ("negc", P, "a counter declared P"),
        // B14 (user decision Q3): an undeclared `_sum` has no sign.
        ("h_sum", U, "undeclared histogram _sum"),
        ("h_count", N, "histogram _count, a count"),
        ("h_bucket", U, "histogram _bucket"),
        // provisional answer Q8: "declared" means the family's `value` entry.
        ("hd_sum", N, "histogram _sum, family declares N"),
    ] {
        let group = add(&mut memo, series(metric));
        rows.check(
            format!("Scan {metric} value ({note})"),
            signs(&memo, group)[VALUE],
            expected,
        );
    }
    let pos = add(&mut memo, series("pos"));
    rows.check("Scan pos label job", signs(&memo, pos)[JOB], U);
    let flows = add(&mut memo, flows());
    rows.check("Scan flows.bytes", signs(&memo, flows)[BYTES], N);
    rows.check("Scan flows.p", signs(&memo, flows)[P_COL], U);
    rows.check("Scan flows.debt", signs(&memo, flows)[DEBT], P);
    rows.finish("T1.1");
}

/// T4 b14: the undeclared histogram `_sum` alone.
#[test]
fn b14_an_undeclared_histogram_sum_has_no_sign() {
    let mut memo = memo();
    let group = add(&mut memo, series("h_sum"));
    assert_eq!(signs(&memo, group)[VALUE], U);
}

/// T4 b04: a gauge can be declared non-negative.
#[test]
fn b04_a_gauge_can_be_declared_non_negative() {
    let mut memo = memo();
    let group = add(&mut memo, series("pos"));
    assert_eq!(signs(&memo, group)[VALUE], N);
}

// ── T1.2 ────────────────────────────────────────────────────────────────

/// T1.2: operators that keep their input's rows copy every column's sign;
/// a written or added column is U; set-like operators keep what their
/// inputs agree on.
#[test]
fn row_operators_copy_their_input_sign() {
    let mut memo = memo();
    let mut rows = Rows::default();
    for input in [N, P] {
        let scan = add(&mut memo, series(gauge_of(input)));
        let ranged = add(&mut memo, range(MINUTE_MS, scan));
        let instant = |child| {
            Operator::NonASAP(NonASAPOp::TimeRange {
                range: Duration::from_secs(300),
                kind: TimeRangeKind::Instant,
                child,
            })
        };
        let selector = add(&mut memo, instant(scan));
        let operators: Vec<(&str, Operator<MemoGroupId>)> = vec![
            (
                "Filter(value > 1)",
                Operator::NonASAP(NonASAPOp::Filter {
                    pred: Predicate(ScalarExpr::Compare {
                        left: column(VALUE),
                        op: CompareOpKind::Gt,
                        right: Box::new(f64_literal(1.0)),
                        semantics: ExprSemantics::Promql,
                    }),
                    child: scan,
                }),
            ),
            (
                "Sort(value)",
                Operator::NonASAP(NonASAPOp::Sort {
                    keys: vec![SortKey {
                        expr: ScalarExpr::Column(VALUE),
                        ascending: true,
                        nulls_first: false,
                    }],
                    partition_by: GroupKeys::default(),
                    child: scan,
                }),
            ),
            (
                "Limit 5",
                Operator::NonASAP(NonASAPOp::Limit {
                    n: Some(5),
                    offset: 0,
                    partition_by: GroupKeys::default(),
                    child: scan,
                }),
            ),
            (
                "Dedup(job)",
                Operator::NonASAP(NonASAPOp::Dedup {
                    cols: vec![TS, VALUE, JOB],
                    child: scan,
                }),
            ),
            ("TimeRange[1m]", range(MINUTE_MS, scan)),
            ("TimeShift(offset 1m)", shift(MINUTE_MS, None, selector)),
            (
                "TimeShift(@ 100)",
                shift(0, Some(AtModifier::Timestamp(100_000)), selector),
            ),
            (
                "TimeShift(@ end())",
                shift(0, Some(AtModifier::End), selector),
            ),
            (
                "PromqlSubquery[5m:30s]",
                Operator::NonASAP(NonASAPOp::PromqlSubquery {
                    range: Duration::from_secs(300),
                    resolution: Some(Duration::from_secs(30)),
                    child: selector,
                }),
            ),
            (
                "PromqlSeriesSample(limitk 2)",
                Operator::NonASAP(NonASAPOp::PromqlSeriesSample {
                    by: GroupKeys::default(),
                    kind: SampleKind::LimitK(2),
                    child: selector,
                }),
            ),
            (
                "PromqlInfoEnrich",
                Operator::NonASAP(NonASAPOp::PromqlInfoEnrich {
                    selector: vec![],
                    child: selector,
                }),
            ),
            (
                "MaintainPopulation",
                Operator::ASAP(ASAPOp::MaintainPopulation {
                    child: scan,
                    population: MaintainedPopulation {
                        input: PopulationInput::CurrentSeries(CurrentSeriesInput {
                            metric: gauge_of(input).into(),
                            matchers: vec![],
                            grouping: vec![],
                            without: false,
                            lookback_ms: 300_000,
                        }),
                        max_k: 1,
                        quantiles: false,
                    },
                }),
            ),
        ];
        let _ = ranged;
        for (name, operator) in operators {
            let row = format!("{name} over {}", gauge_of(input));
            match try_add(&mut memo, operator) {
                Ok(group) => rows.check(row, sign_of(&memo, group, "value"), input),
                Err(error) => rows.fail(row, format!("cannot build: {error}")),
            }
        }
        // A relabel writes its destination label; the value is copied.
        for dst in ["x", "job"] {
            let row = format!("PromqlRelabel(dst {dst}) over {}", gauge_of(input));
            let relabel = Operator::NonASAP(NonASAPOp::PromqlRelabel {
                dst: dst.into(),
                value: ScalarExpr::Literal(ScalarValue::Utf8("a".into())),
                child: selector,
            });
            match try_add(&mut memo, relabel) {
                Ok(group) => {
                    rows.check(
                        format!("{row}: value"),
                        sign_of(&memo, group, "value"),
                        input,
                    );
                    rows.check(format!("{row}: {dst}"), sign_of(&memo, group, dst), U);
                }
                Err(error) => rows.fail(row, format!("cannot build: {error}")),
            }
        }
    }
    // `Values` rows 1 and -1: no one sign.
    let values = Operator::NonASAP(NonASAPOp::Values {
        rows: vec![vec![f64_literal(1.0)], vec![f64_literal(-1.0)]],
        schema: Schema::new(vec![Field::plain("v", DataType::Float64, false)]),
    });
    match try_add(&mut memo, values) {
        Ok(group) => rows.check("Values(1, -1)", signs(&memo, group)[0], U),
        Err(error) => rows.fail("Values(1, -1)", format!("cannot build: {error}")),
    }
    // The window column `lag(p)`: p crosses zero, so no sign.
    let flows = add(&mut memo, flows());
    let window = Operator::NonASAP(NonASAPOp::SQLWindowFunc {
        func: WindowFuncKind::Lag,
        args: vec![ScalarExpr::Column(P_COL)],
        partition_by: GroupKeys::default(),
        order_by: vec![SortKey {
            expr: ScalarExpr::Column(TS),
            ascending: true,
            nulls_first: false,
        }],
        frame: None,
        output_name: "prev".into(),
        child: flows,
    });
    match try_add(&mut memo, window) {
        Ok(group) => {
            rows.check(
                "SQLWindowFunc lag(p): prev",
                sign_of(&memo, group, "prev"),
                U,
            );
            rows.check(
                "SQLWindowFunc lag(p): bytes",
                sign_of(&memo, group, "bytes"),
                N,
            );
        }
        Err(error) => rows.fail("SQLWindowFunc lag(p)", format!("cannot build: {error}")),
    }
    // Joins of t_pos (v N) and t_neg (v P).
    let t_pos = add(&mut memo, table("t_pos", &[("v", DataType::Float64)]));
    let t_neg = add(&mut memo, table("t_neg", &[("v", DataType::Float64)]));
    for (kind, expected) in [
        (JoinKind::Inner, vec![N, P]),
        (JoinKind::Semi, vec![N]),
        (JoinKind::Anti, vec![N]),
    ] {
        let join = Operator::NonASAP(NonASAPOp::Join {
            kind: kind.clone(),
            pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
            left: t_pos,
            right: t_neg,
        });
        match try_add(&mut memo, join) {
            Ok(group) => {
                let got = signs(&memo, group).to_vec();
                if got != expected {
                    rows.fail(
                        format!("Join {kind:?}(t_pos, t_neg)"),
                        format!("derived {got:?}, expected {expected:?}"),
                    );
                } else {
                    rows.checked += 1;
                }
            }
            Err(error) => rows.fail(format!("Join {kind:?}"), format!("cannot build: {error}")),
        }
    }
    for (right, expected) in [(t_pos, N), (t_neg, U)] {
        let name = if right == t_pos { "t_pos" } else { "t_neg" };
        let set_op = Operator::NonASAP(NonASAPOp::SetOp {
            kind: RelationalSetOpKind::Union,
            all: true,
            left: t_pos,
            right,
        });
        match try_add(&mut memo, set_op) {
            Ok(group) => rows.check(
                format!("SetOp union all(t_pos, {name})"),
                signs(&memo, group)[0],
                expected,
            ),
            Err(error) => rows.fail(
                format!("SetOp(t_pos, {name})"),
                format!("cannot build: {error}"),
            ),
        }
        let concat = Operator::NonASAP(NonASAPOp::Concat {
            children: vec![t_pos, right],
        });
        match try_add(&mut memo, concat) {
            Ok(group) => rows.check(
                format!("Concat(t_pos, {name})"),
                signs(&memo, group)[0],
                expected,
            ),
            Err(error) => rows.fail(
                format!("Concat(t_pos, {name})"),
                format!("cannot build: {error}"),
            ),
        }
    }
    rows.finish("T1.2");
}

// ── T1.3 ────────────────────────────────────────────────────────────────

/// T1.3: `vector(s)` has the sign bit of its literal: `-0` is P, NaN U,
/// `vector(time())` U (a time is a value the planner knows no sign of).
#[test]
fn vector_from_scalar_has_its_literals_sign() {
    let mut memo = memo();
    let mut rows = Rows::default();
    for (shown, expr, expected) in [
        ("vector(1)", f64_literal(1.0), N),
        ("vector(-1)", f64_literal(-1.0), P),
        ("vector(0)", f64_literal(0.0), N),
        ("vector(-0)", f64_literal(-0.0), P),
        ("vector(NaN)", f64_literal(f64::NAN), U),
        ("vector(+Inf)", f64_literal(f64::INFINITY), N),
        ("vector(-Inf)", f64_literal(f64::NEG_INFINITY), P),
        ("vector(time())", ScalarExpr::EvalTimestamp, U),
    ] {
        match try_add(&mut memo, vector_of(expr)) {
            Ok(group) => rows.check(shown, sign_of(&memo, group, "value"), expected),
            Err(error) => rows.fail(shown, format!("cannot build: {error}")),
        }
    }
    rows.finish("T1.3");
}

// ── T1.4 ────────────────────────────────────────────────────────────────

/// Every `AggIntent` variant, through an exhaustive match (no `_` arm): a
/// new variant fails to compile here until it is given a row. Each comes
/// with its expected sign over an N, P and U input (columns in that order).
fn every_measure() -> Vec<(AggIntent, [ValueSign; 3], &'static str)> {
    let col = Some(VALUE);
    let mut rows = Vec::new();
    // The exhaustive witness: each variant names the rows below that cover it.
    let witness = |measure: &AggIntent| -> &'static str {
        use AggIntent::*;
        match measure {
            Count
            | Cardinality { .. }
            | CountValues { .. }
            | Changes
            | Resets
            | Group
            | Absent { .. }
            | AbsentOverTime
            | PresentOverTime
            | StdDev { .. }
            | Variance { .. }
            | FrequencyL2 { .. } => "count-like: N",
            FrequencyEntropy { .. } => "entropy: U",
            Min { .. } | Max { .. } | FirstOverTime | LastOverTime => "keeps the sign",
            Sum { .. } | Avg { .. } => "keeps the sign (Q1)",
            Quantile { .. } => "keeps the sign inside [0, 1]",
            Rate | IRate | Increase => "N over N",
            Math(_) => "per math function",
            TimeFn(_) => "per time function",
            TsOfMinOverTime | TsOfMaxOverTime | TsOfFirstOverTime | TsOfLastOverTime => {
                "timestamps: U"
            }
            PearsonCorr { .. }
            | Delta
            | IDelta
            | Deriv
            | PredictLinear { .. }
            | DoubleExpSmoothing { .. }
            | HistogramCount
            | HistogramSum
            | HistogramAvg
            | HistogramStdDev
            | HistogramStdVar
            | HistogramFraction { .. }
            | HistogramQuantile { .. }
            | MadOverTime
            | TopK { .. }
            | Extension { .. } => "U",
        }
    };
    let all_n = [N, N, N];
    let keeps = [N, P, U];
    let none = [U, U, U];
    for measure in [
        AggIntent::Count,
        AggIntent::Cardinality { cols: vec![VALUE] },
        AggIntent::CountValues {
            label: "version".into(),
        },
        AggIntent::Changes,
        AggIntent::Resets,
        AggIntent::Group,
        AggIntent::Absent {
            labels: BTreeMap::new(),
        },
        AggIntent::AbsentOverTime,
        AggIntent::PresentOverTime,
        AggIntent::StdDev {
            col,
            population: true,
        },
        AggIntent::Variance {
            col,
            population: true,
        },
        AggIntent::FrequencyL2 { col },
    ] {
        rows.push((measure, all_n, "N whatever the input"));
    }
    // User decision Q2: entropy is U.
    rows.push((AggIntent::FrequencyEntropy { col }, none, "entropy"));
    for measure in [
        AggIntent::Min { col },
        AggIntent::Max { col },
        AggIntent::FirstOverTime,
        AggIntent::LastOverTime,
    ] {
        rows.push((measure, keeps, "a selected input value"));
    }
    // provisional answer Q1: sums start at -0.0, so a sum of P values is P
    // (the executor fix); without it a sum of only -0.0 is +0.0.
    rows.push((AggIntent::Sum { col }, keeps, "Q1"));
    rows.push((AggIntent::Avg { col }, keeps, "Q1"));
    for q in [0.0, 0.5, 1.0, -0.0] {
        rows.push((AggIntent::Quantile { col, q }, keeps, "q in [0, 1]"));
    }
    for q in [1.5, -0.5, f64::NAN] {
        rows.push((
            AggIntent::Quantile { col, q },
            none,
            "q outside [0, 1]: ±Inf",
        ));
    }
    for measure in [AggIntent::Rate, AggIntent::IRate, AggIntent::Increase] {
        rows.push((measure, [N, U, U], "a decrease reads as a reset"));
    }
    for (function, expected) in math_function_rows() {
        rows.push((AggIntent::Math(function), expected, "per T1.6"));
    }
    for (function, expected) in time_function_rows() {
        rows.push((AggIntent::TimeFn(function), expected, "per T1.7"));
    }
    for measure in [
        AggIntent::TsOfMinOverTime,
        AggIntent::TsOfMaxOverTime,
        AggIntent::TsOfFirstOverTime,
        AggIntent::TsOfLastOverTime,
    ] {
        rows.push((measure, none, "a timestamp"));
    }
    for measure in [
        AggIntent::PearsonCorr {
            left: VALUE,
            right: VALUE,
        },
        AggIntent::Delta,
        AggIntent::IDelta,
        AggIntent::Deriv,
        AggIntent::PredictLinear { seconds: 60.0 },
        AggIntent::DoubleExpSmoothing {
            smoothing: 0.5,
            trend: 0.5,
        },
        AggIntent::HistogramCount,
        AggIntent::HistogramSum,
        AggIntent::HistogramAvg,
        AggIntent::HistogramStdDev,
        AggIntent::HistogramStdVar,
        AggIntent::HistogramFraction {
            lower: 0.0,
            upper: 1.0,
        },
        AggIntent::HistogramQuantile { q: 0.5, le: JOB },
        AggIntent::MadOverTime,
        AggIntent::Extension {
            ext_kind: "custom".into(),
            payload: serde_json::Value::Null,
        },
    ] {
        rows.push((measure, none, "no rule"));
    }
    for (measure, _, _) in &rows {
        let _ = witness(measure);
    }
    rows
}

/// T1.4: one row per `AggIntent` variant over N, P and U inputs, so a rule
/// that returns a constant fails at least one row. A per-series range
/// function reads `X[1m]`; any other measure reads the instant `X` (the
/// first of the two the memo accepts). `TopK` alone ranks rows and is in
/// T1.2's sense a copy; as one of several measures it is U (below).
///
/// Fails today: entropy is N, every `Math(f)` but `Abs` is U and every
/// `TimeFn` is U.
#[test]
fn every_measure_has_a_sign_rule() {
    let mut memo = memo();
    let mut rows = Rows::default();
    for (measure, expected, why) in every_measure() {
        for (input, expected) in SIGNS.into_iter().zip(expected) {
            let scan = add(&mut memo, series(gauge_of(input)));
            let window = add(&mut memo, range(MINUTE_MS, scan));
            let row = format!("{measure:?} over {} ({why})", gauge_of(input));
            let reduction = match measure {
                AggIntent::Count
                | AggIntent::Cardinality { .. }
                | AggIntent::CountValues { .. }
                | AggIntent::Group
                | AggIntent::StdDev { .. }
                | AggIntent::Variance { .. }
                | AggIntent::FrequencyL2 { .. }
                | AggIntent::FrequencyEntropy { .. }
                | AggIntent::PearsonCorr { .. } => Reduction::by(vec![JOB]),
                _ => Reduction::PerEntity,
            };
            let group = try_add(
                &mut memo,
                aggregate(reduction.clone(), measure.clone(), window),
            )
            .or_else(|_| try_add(&mut memo, aggregate(reduction, measure.clone(), scan)));
            match group {
                Ok(group) => rows.check(row, computed_value_sign(&memo, group), expected),
                Err(error) => rows.fail(row, format!("cannot build: {error}")),
            }
        }
    }
    // TopK as one of several measures: U whatever the input.
    for input in SIGNS {
        let scan = add(&mut memo, series(gauge_of(input)));
        let row = format!("[Sum, TopK] over {}", gauge_of(input));
        let several = Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![JOB]),
            measures: vec![
                AggIntent::Sum { col: Some(VALUE) },
                AggIntent::TopK { k: 2 },
            ],
            output_names: vec![String::new(), String::new()],
            filters: vec![],
            having: None,
            child: scan,
        });
        match try_add(&mut memo, several) {
            Ok(group) => {
                let last = *signs(&memo, group).last().unwrap();
                rows.check(format!("{row}: topk"), last, U);
            }
            // The memo may reject a top-k among other measures: then the row
            // has nothing to derive.
            Err(error) => println!("{row}: not a valid aggregate ({error})"),
        }
    }
    rows.finish("T1.4");
}

/// T4 b07: entropy, as a measure and as a readout, has no sign (user
/// decision Q2): a one-item entropy evaluates `0.0 - (+0.0 * log2(1))`,
/// and an estimate (UnivMon) may give `-0.0`.
#[test]
fn b07_entropy_has_no_sign() {
    let mut memo = memo();
    let scan = add(&mut memo, series("pos"));
    let window = add(&mut memo, range(MINUTE_MS, scan));
    let measure = add(
        &mut memo,
        aggregate(
            Reduction::by(vec![JOB]),
            AggIntent::FrequencyEntropy { col: Some(VALUE) },
            scan,
        ),
    );
    let state = add(
        &mut memo,
        asap_types::test_support::memo_fixtures::state(
            FieldDataType::Sketch(SketchAlgorithm::UnivMon),
            window,
        ),
    );
    let read = add(&mut memo, readout(SketchStatistic::FrequencyEntropy, state));
    assert_eq!(computed_value_sign(&memo, measure), U, "measure");
    assert_eq!(computed_value_sign(&memo, read), U, "readout");
}

// ── T1.5 ────────────────────────────────────────────────────────────────

const OPERATORS: [ArithmeticOpKind; 7] = [
    ArithmeticOpKind::Add,
    ArithmeticOpKind::Sub,
    ArithmeticOpKind::Mul,
    ArithmeticOpKind::Div,
    ArithmeticOpKind::Mod,
    ArithmeticOpKind::Pow,
    ArithmeticOpKind::Atan2,
];

/// The sign of `l op r` over Float64 operands of signs `l` and `r`, derived
/// by hand from IEEE 754 (cells NN, NP, NU, PN, PP, PU, UN, UP, UU):
/// - `+`: `+0 + +0 = +0`, `-0 + -0 = -0`; mixed signs cancel.
/// - `-` is `l + (-r)`: `+0 - -0 = +0`, `-0 - +0 = -0`.
/// - `*`, `/`: the XOR of the sign bits (`+0 * -1 = -0`, `1 / -0 = -inf`).
/// - `%` (fmod): the dividend's sign, `fmod(-0, y) = -0`.
/// - `^` (powf): `powf(x ≥ +0, y) ≥ +0`, including `powf(+0, -1) = +inf`;
///   `powf(-2, 2) = 4` but `powf(-2, 3) = -8`.
/// - `atan2(l, r)`: the sign of `l`, including `atan2(±0, -1) = ±π`.
fn float_arithmetic(op: &ArithmeticOpKind) -> [ValueSign; 9] {
    match op {
        ArithmeticOpKind::Add => [N, U, U, U, P, U, U, U, U],
        ArithmeticOpKind::Sub => [U, N, U, P, U, U, U, U, U],
        ArithmeticOpKind::Mul | ArithmeticOpKind::Div => [N, P, U, P, N, U, U, U, U],
        ArithmeticOpKind::Mod => [N, N, N, P, P, P, U, U, U],
        ArithmeticOpKind::Pow => [N, N, N, U, U, U, U, U, U],
        // provisional answer Q10: atan2 has its left operand's sign.
        ArithmeticOpKind::Atan2 => [N, N, N, P, P, P, U, U, U],
    }
}

/// The same over Int64 operands, where an int `0` is N and an int P value
/// is `< 0` (provisional answer Q5):
/// - `-`: `0 - 0 = 0` and `1 - 2 = -1`, so NN is U; `0 - (-1) = 1` N;
///   `-1 - 0 = -1` P.
/// - `*`: `0 * -3 = 0` (N, not P), so NP and PN are U; `-2 * -3 = 6` N.
/// - `/`: `1 / -2 = 0` and `-1 / 2 = 0`, so NP and PN are U; `-1 / -2 = 0`
///   and `-4 / -2 = 2`, so PP is N.
/// - `%`: the dividend's sign, but `-4 % 2 = 0`, so a P dividend is U;
///   `5 % -3 = 2`, so an N dividend is N.
/// - `+`, `^` and `atan2` as for floats (`^` and `atan2` of ints are floats).
fn int_arithmetic(op: &ArithmeticOpKind) -> [ValueSign; 9] {
    match op {
        ArithmeticOpKind::Add => [N, U, U, U, P, U, U, U, U],
        ArithmeticOpKind::Sub => [U, N, U, P, U, U, U, U, U],
        ArithmeticOpKind::Mul => [N, U, U, U, N, U, U, U, U],
        ArithmeticOpKind::Div => [N, U, U, U, N, U, U, U, U],
        ArithmeticOpKind::Mod => [N, N, N, U, U, U, U, U, U],
        ArithmeticOpKind::Pow => [N, N, N, U, U, U, U, U, U],
        ArithmeticOpKind::Atan2 => [N, N, N, P, P, P, U, U, U],
    }
}

fn arithmetic_expr(
    op: ArithmeticOpKind,
    left: ScalarExpr<MemoGroupId>,
    right: ScalarExpr<MemoGroupId>,
    semantics: ExprSemantics,
) -> ScalarExpr<MemoGroupId> {
    ScalarExpr::Arithmetic {
        op,
        left: Box::new(left),
        right: Box::new(right),
        semantics,
    }
}

/// T1.5: 63 cells per operand type, each run through a vector `BinaryOp`
/// (`pos * neg`) and a scalar `Project` (`gain * loss` over `flows`); and
/// the Int64 cells through `Project` over `flows`' int columns.
///
/// Fails today: `-`, `%`, `^` and `atan2` are U, and the Int rows follow
/// the float rules (F5).
#[test]
fn arithmetic_signs_for_every_operator_and_operand_sign() {
    let mut memo = memo();
    let mut rows = Rows::default();
    let flows = add(&mut memo, flows());
    let float_column = |sign| match sign {
        N => GAIN,
        P => LOSS,
        U => P_COL,
    };
    let int_column = |sign| match sign {
        N => BYTES,
        P => DEBT,
        U => DELTA,
    };
    for op in OPERATORS {
        let float = float_arithmetic(&op);
        let int = int_arithmetic(&op);
        let mut cell = 0;
        for left in SIGNS {
            for right in SIGNS {
                let lhs = add(&mut memo, series(gauge_of(left)));
                let rhs = add(&mut memo, series(gauge_of(right)));
                let shown = format!("{} {op} {}", gauge_of(left), gauge_of(right));
                match try_add(
                    &mut memo,
                    binary(BinaryOpKind::Arithmetic(op.clone()), None, false, lhs, rhs),
                ) {
                    Ok(group) => rows.check(
                        format!("vector {shown}"),
                        sign_of(&memo, group, "value"),
                        float[cell],
                    ),
                    Err(error) => {
                        rows.fail(format!("vector {shown}"), format!("cannot build: {error}"))
                    }
                }
                let expr = arithmetic_expr(
                    op.clone(),
                    ScalarExpr::Column(float_column(left)),
                    ScalarExpr::Column(float_column(right)),
                    ExprSemantics::Sql,
                );
                rows.check(
                    format!("Float64 {left:?} {op} {right:?}"),
                    projected_sign(&mut memo, expr, flows),
                    float[cell],
                );
                let expr = arithmetic_expr(
                    op.clone(),
                    ScalarExpr::Column(int_column(left)),
                    ScalarExpr::Column(int_column(right)),
                    ExprSemantics::Sql,
                );
                // provisional answer Q5: Int64 results are type-aware.
                rows.check(
                    format!("Int64 {left:?} {op} {right:?}"),
                    projected_sign(&mut memo, expr, flows),
                    int[cell],
                );
                cell += 1;
            }
        }
    }
    rows.finish("T1.5");
}

/// T4 b06: `sum(rate(c_total[5m])) - min(neg)` is N - P = N.
///
/// Fails today: the difference is U (the arithmetic wildcard).
#[test]
fn b06_a_difference_of_opposite_signs_has_a_sign() {
    let mut memo = memo();
    let counter = add(&mut memo, series("c_total"));
    let window = add(&mut memo, range(5 * MINUTE_MS, counter));
    let rate = add(
        &mut memo,
        aggregate(Reduction::PerEntity, AggIntent::Rate, window),
    );
    let everything = Reduction::Reduce(GroupKeys::by(vec![]));
    let sum = add(
        &mut memo,
        aggregate(
            everything.clone(),
            AggIntent::Sum { col: Some(VALUE) },
            rate,
        ),
    );
    let neg = add(&mut memo, series("neg"));
    let min = add(
        &mut memo,
        aggregate(everything, AggIntent::Min { col: Some(VALUE) }, neg),
    );
    let difference = add(
        &mut memo,
        binary(
            BinaryOpKind::Arithmetic(ArithmeticOpKind::Sub),
            None,
            false,
            sum,
            min,
        ),
    );
    assert_eq!(computed_value_sign(&memo, sum), N, "sum(rate(c_total))");
    assert_eq!(computed_value_sign(&memo, min), P, "min(neg)");
    assert_eq!(*signs(&memo, difference).last().unwrap(), N, "difference");
}

/// T4 b05: a quantile outside [0, 1] has no sign; `pos / 1000` keeps N.
#[test]
fn b05_quantile_outside_the_unit_interval_and_a_scaled_value() {
    let mut memo = memo();
    let scan = add(&mut memo, series("pos"));
    let window = add(&mut memo, range(5 * MINUTE_MS, scan));
    let quantile = add(
        &mut memo,
        aggregate(
            Reduction::PerEntity,
            AggIntent::Quantile {
                col: Some(VALUE),
                q: 1.5,
            },
            window,
        ),
    );
    assert_eq!(computed_value_sign(&memo, quantile), U);
    let scaled = mapped_value_sign(
        &mut memo,
        N,
        arithmetic_expr(
            ArithmeticOpKind::Div,
            ScalarExpr::Column(VALUE),
            f64_literal(1000.0),
            ExprSemantics::Promql,
        ),
    );
    assert_eq!(scaled, N);
}

/// T4 f5: SQL `-bytes` over `bytes = 0` is the int `0`, which is N, not P:
/// the negation of an int N column has no sign (provisional answer Q5).
/// The same for `bytes * -1` and `bytes / -2`, and for `-COUNT(p)`.
///
/// Fails today: each is labelled P.
#[test]
fn f5_integer_negation_of_zero_has_no_sign() {
    let mut memo = memo();
    let flows = add(&mut memo, flows());
    let int = |value| ScalarExpr::Literal(ScalarValue::Int64(value));
    let mut rows = Rows::default();
    for (shown, expr) in [
        (
            "-bytes",
            ScalarExpr::Negative {
                expr: column(BYTES),
                semantics: ExprSemantics::Sql,
            },
        ),
        (
            "bytes * -1",
            arithmetic_expr(
                ArithmeticOpKind::Mul,
                ScalarExpr::Column(BYTES),
                int(-1),
                ExprSemantics::Sql,
            ),
        ),
        (
            "bytes / -2",
            arithmetic_expr(
                ArithmeticOpKind::Div,
                ScalarExpr::Column(BYTES),
                int(-2),
                ExprSemantics::Sql,
            ),
        ),
    ] {
        // provisional answer Q5
        rows.check(shown, projected_sign(&mut memo, expr, flows), U);
    }
    let count = add(
        &mut memo,
        aggregate(
            Reduction::Reduce(GroupKeys::by(vec![])),
            AggIntent::Count,
            flows,
        ),
    );
    let count_column = properties(&memo, count).schema.fields.len() - 1;
    let negated = ScalarExpr::Negative {
        expr: column(count_column),
        semantics: ExprSemantics::Sql,
    };
    let count_is_int = properties(&memo, count).schema.fields[count_column].dtype
        == FieldDataType::Plain(DataType::Int64);
    // A float count's negation is P (`-(+0.0) = -0.0`); an int count's is U.
    rows.check(
        format!("-COUNT(p) (count is Int64: {count_is_int})"),
        projected_sign(&mut memo, negated, count),
        if count_is_int { U } else { P },
    );
    rows.finish("f5");
}

/// T4 b15: `CAST(p AS BIGINT)` of a value in (-1, 0) is the int `0`, which
/// is N: an integer cast of P values has no sign.
///
/// Fails today: the cast keeps P.
#[test]
fn b15_an_integer_cast_of_non_positive_values_has_no_sign() {
    let mut memo = memo();
    let flows = add(&mut memo, flows());
    let cast = ScalarExpr::Cast {
        expr: column(LOSS),
        to: DataType::Int64,
        try_cast: false,
    };
    assert_eq!(projected_sign(&mut memo, cast, flows), U);
}

// ── T1.6 ────────────────────────────────────────────────────────────────

/// The sign of `f(v)` over v of sign N, P, U, by hand from IEEE 754 and the
/// executor's formulas:
/// - `acos(1) = acosh(1) = +0`; `exp`, `cosh` ≥ 1 or ≥ +0.
/// - `ceil(-0.5) = -0`, `floor(+0.5) = +0`, `sqrt(-0) = -0`,
///   `asinh` and `atanh(-0) = 0.5·ln_1p(-0) = -0` keep the sign bit.
/// - `round(v, to) = floor(v/to + 0.5)·to`: `round(-0.3) = +0`, so a P input
///   has no sign; `to < 0`: `round(+0, -1) = floor(+0.5)·-1 = -0`.
/// - clamp is `max(min, min(max, v))`, Go's signed zeros:
///   `max(+0, -0) = +0`, `min(+0, -0) = -0`.
fn math_function_rows() -> Vec<(MathFunc, [ValueSign; 3])> {
    use MathFunc::*;
    let all_n = [N, N, N];
    let keeps = [N, P, U];
    let none = [U, U, U];
    let mut rows = Vec::new();
    for f in [Abs, Exp, Acos, Cosh, Acosh] {
        rows.push((f, all_n));
    }
    for f in [
        Ceil, Floor, Sqrt, Asin, Atan, Sinh, Tanh, Asinh, Atanh, Deg, Rad,
    ] {
        rows.push((f, keeps));
    }
    // provisional answer Q2: the executor returns `x` for a zero, so
    // `sgn(-0) = -0` and Sgn keeps the sign.
    rows.push((Sgn, keeps));
    for f in [Ln, Log2, Log10, Sin, Cos, Tan] {
        rows.push((f, none));
    }
    for to_nearest in [1.0, 0.5] {
        rows.push((Round { to_nearest }, [N, U, U]));
    }
    for to_nearest in [-1.0, 0.0, f64::NAN] {
        rows.push((Round { to_nearest }, none));
    }
    // provisional answer Q3: every `*` cell of the plan's T1.6 table needs
    // the executor to resolve signed zeros as Go does.
    rows.push((
        Clamp {
            min: 0.0,
            max: 100.0,
        },
        all_n,
    ));
    rows.push((
        Clamp {
            min: -100.0,
            max: -1.0,
        },
        [P, P, P],
    ));
    rows.push((
        Clamp {
            min: -5.0,
            max: 5.0,
        },
        keeps,
    ));
    rows.push((
        Clamp {
            min: -0.0,
            max: 5.0,
        },
        keeps,
    ));
    rows.push((
        Clamp {
            min: 0.0,
            max: -0.0,
        },
        none,
    ));
    rows.push((ClampMin { min: 0.0 }, all_n));
    rows.push((ClampMin { min: 5.0 }, all_n));
    rows.push((ClampMin { min: -5.0 }, keeps));
    rows.push((ClampMin { min: -0.0 }, keeps));
    rows.push((ClampMax { max: -1.0 }, [P, P, P]));
    rows.push((ClampMax { max: -0.0 }, [P, P, P]));
    rows.push((ClampMax { max: 5.0 }, keeps));
    rows.push((ClampMax { max: 0.0 }, keeps));
    // The exhaustive witness: a new variant fails to compile here until it
    // is given a row above.
    for (function, _) in &rows {
        match function {
            Abs
            | Ceil
            | Floor
            | Exp
            | Ln
            | Log2
            | Log10
            | Sqrt
            | Sgn
            | Sin
            | Cos
            | Tan
            | Asin
            | Acos
            | Atan
            | Sinh
            | Cosh
            | Tanh
            | Asinh
            | Acosh
            | Atanh
            | Deg
            | Rad
            | Round { .. }
            | Clamp { .. }
            | ClampMin { .. }
            | ClampMax { .. } => {}
        }
    }
    rows
}

/// PromQL's name and literal arguments for `function`:
/// `ClampMin { min: 0 }` → `promql_clamp_min(value, 0)`.
fn promql_call(function: &MathFunc) -> (String, Vec<f64>) {
    use MathFunc::*;
    let (name, literals) = match function {
        Abs => ("abs", vec![]),
        Ceil => ("ceil", vec![]),
        Floor => ("floor", vec![]),
        Exp => ("exp", vec![]),
        Ln => ("ln", vec![]),
        Log2 => ("log2", vec![]),
        Log10 => ("log10", vec![]),
        Sqrt => ("sqrt", vec![]),
        Sgn => ("sgn", vec![]),
        Sin => ("sin", vec![]),
        Cos => ("cos", vec![]),
        Tan => ("tan", vec![]),
        Asin => ("asin", vec![]),
        Acos => ("acos", vec![]),
        Atan => ("atan", vec![]),
        Sinh => ("sinh", vec![]),
        Cosh => ("cosh", vec![]),
        Tanh => ("tanh", vec![]),
        Asinh => ("asinh", vec![]),
        Acosh => ("acosh", vec![]),
        Atanh => ("atanh", vec![]),
        Deg => ("deg", vec![]),
        Rad => ("rad", vec![]),
        Round { to_nearest } => ("round", vec![*to_nearest]),
        Clamp { min, max } => ("clamp", vec![*min, *max]),
        ClampMin { min } => ("clamp_min", vec![*min]),
        ClampMax { max } => ("clamp_max", vec![*max]),
    };
    (format!("promql_{name}"), literals)
}

fn call(name: &str, args: Vec<ScalarExpr<MemoGroupId>>) -> ScalarExpr<MemoGroupId> {
    ScalarExpr::FunctionCall {
        name: name.into(),
        args,
    }
}

fn promql_call_expr(function: &MathFunc) -> ScalarExpr<MemoGroupId> {
    let (name, literals) = promql_call(function);
    let args = std::iter::once(ScalarExpr::Column(VALUE))
        .chain(literals.into_iter().map(f64_literal))
        .collect();
    call(&name, args)
}

/// T1.6: every math function, built as PromQL lowers it,
/// `FunctionCall "promql_<f>"(value, literals…)`, so the row also exercises
/// `math_function_of_call`; plus SQL `abs`/`ABS`, a non-literal bound and
/// an unknown name.
///
/// Fails today: every function but `abs` is U.
#[test]
fn every_math_function_has_a_sign_rule() {
    let mut memo = memo();
    let mut rows = Rows::default();
    for (function, expected) in math_function_rows() {
        for (input, expected) in SIGNS.into_iter().zip(expected) {
            let expr = promql_call_expr(&function);
            let row = format!("{function:?} over {}", gauge_of(input));
            rows.check(row, mapped_value_sign(&mut memo, input, expr), expected);
        }
    }
    for name in ["abs", "ABS"] {
        let expr = call(name, vec![ScalarExpr::Column(VALUE)]);
        rows.check(
            format!("SQL {name}(mixed)"),
            mapped_value_sign(&mut memo, U, expr),
            N,
        );
    }
    let non_literal = call(
        "promql_clamp_min",
        vec![ScalarExpr::Column(VALUE), ScalarExpr::EvalTimestamp],
    );
    rows.check(
        "promql_clamp_min(mixed, time())",
        mapped_value_sign(&mut memo, U, non_literal),
        U,
    );
    // A registered function that is no math function (the memo rejects an
    // unregistered name such as `foo`): `nullif(value, 1)` over `mixed`.
    let other = call("nullif", vec![ScalarExpr::Column(VALUE), f64_literal(1.0)]);
    rows.check(
        "nullif(mixed, 1)",
        mapped_value_sign(&mut memo, U, other),
        U,
    );
    rows.finish("T1.6");
}

/// The date functions over v of sign N, P, U: minute … days_in_month are in
/// [0, 366] for any finite v; `year(v)` is negative for
/// `v < -62135596800` (before year 1), so it is N over N only
/// (provisional answer Q4). A non-finite v gives NaN.
fn time_function_rows() -> Vec<(TimeFunc, [ValueSign; 3])> {
    use TimeFunc::*;
    let witness = |function: &TimeFunc| match function {
        Timestamp | Minute | Hour | DayOfWeek | DayOfMonth | DayOfYear | Month | Year
        | DaysInMonth => {}
    };
    let rows = vec![
        (Minute, [N, N, N]),
        (Hour, [N, N, N]),
        (DayOfWeek, [N, N, N]),
        (DayOfMonth, [N, N, N]),
        (DayOfYear, [N, N, N]),
        (Month, [N, N, N]),
        (DaysInMonth, [N, N, N]),
        // provisional answer Q4
        (Year, [N, U, U]),
    ];
    rows.iter().for_each(|(f, _)| witness(f));
    rows
}

fn time_function_name(function: TimeFunc) -> &'static str {
    use TimeFunc::*;
    match function {
        Timestamp => "timestamp",
        Minute => "minute",
        Hour => "hour",
        DayOfWeek => "day_of_week",
        DayOfMonth => "day_of_month",
        DayOfYear => "day_of_year",
        Month => "month",
        Year => "year",
        DaysInMonth => "days_in_month",
    }
}

/// T1.7: every date function, built as `FunctionCall "promql_<f>"(value)`.
///
/// Fails today: each is U (only the year over P/U rows pass).
#[test]
fn every_time_function_has_a_sign_rule() {
    let mut memo = memo();
    let mut rows = Rows::default();
    for (function, expected) in time_function_rows() {
        for (input, expected) in SIGNS.into_iter().zip(expected) {
            let name = format!("promql_{}", time_function_name(function));
            let expr = call(&name, vec![ScalarExpr::Column(VALUE)]);
            rows.check(
                format!("{name} over {}", gauge_of(input)),
                mapped_value_sign(&mut memo, input, expr),
                expected,
            );
        }
    }
    rows.finish("T1.7");
}

/// T4 f4: `year(v)` at `v = -1e11` is year -1199, so `year(neg)` has no
/// sign (provisional answer Q4).
#[test]
fn f4_year_before_year_one_is_negative() {
    let mut memo = memo();
    let expr = call("promql_year", vec![ScalarExpr::Column(VALUE)]);
    assert_eq!(mapped_value_sign(&mut memo, P, expr), U);
}

// ── T1.8 ────────────────────────────────────────────────────────────────

/// T1.8: every `ScalarExpr` variant (an exhaustive match below), over
/// `flows`.
///
/// Fails today: a cast of P to an integer keeps P (B15) and the negation of
/// an int N column is P (F5).
#[test]
fn every_scalar_expression_has_a_sign_rule() {
    let mut memo = memo();
    let mut rows = Rows::default();
    let flows = add(&mut memo, flows());
    // The sub-query variants read a group: `t_pos`'s one column, and
    // `scalar(v)` the instant vector `pos`.
    let t_pos = add(&mut memo, table("t_pos", &[("v", DataType::Float64)]));
    let pos_series = add(&mut memo, series("pos"));
    let float_column = |sign| match sign {
        N => GAIN,
        P => LOSS,
        U => P_COL,
    };
    let int_column = |sign| match sign {
        N => BYTES,
        P => DEBT,
        U => DELTA,
    };
    let mut row = |rows: &mut Rows, shown: String, expr: ScalarExpr<MemoGroupId>, expected| {
        let witness = |expr: &ScalarExpr<MemoGroupId>| match expr {
            ScalarExpr::Column(_)
            | ScalarExpr::Literal(_)
            | ScalarExpr::Negative { .. }
            | ScalarExpr::Compare { .. }
            | ScalarExpr::BoolAnd(_)
            | ScalarExpr::BoolOr(_)
            | ScalarExpr::Not(_)
            | ScalarExpr::IsNull(_)
            | ScalarExpr::IsNotNull(_)
            | ScalarExpr::Cast { .. }
            | ScalarExpr::InList { .. }
            | ScalarExpr::FunctionCall { .. }
            | ScalarExpr::Arithmetic { .. }
            | ScalarExpr::Case { .. }
            | ScalarExpr::CurrentTimestamp
            | ScalarExpr::EvalTimestamp
            | ScalarExpr::PromqlScalarFromVector(_)
            | ScalarExpr::ScalarSubquery(_)
            | ScalarExpr::Exists { .. }
            | ScalarExpr::InSubquery { .. } => {}
        };
        witness(&expr);
        let project = Operator::NonASAP(NonASAPOp::Project {
            cols: vec![ProjectItem {
                alias: Some("v".into()),
                expr,
            }],
            child: flows,
        });
        match try_add(&mut memo, project) {
            Ok(group) => rows.check(shown, signs(&memo, group)[0], expected),
            Err(error) => rows.fail(shown, format!("cannot build: {error}")),
        }
    };
    for sign in SIGNS {
        row(
            &mut rows,
            format!("Column float {sign:?}"),
            ScalarExpr::Column(float_column(sign)),
            sign,
        );
        row(
            &mut rows,
            format!("Column int {sign:?}"),
            ScalarExpr::Column(int_column(sign)),
            sign,
        );
    }
    for (value, expected) in [
        (1.0, N),
        (-1.0, P),
        (0.0, N),
        (-0.0, P),
        (f64::NAN, U),
        (f64::INFINITY, N),
        (f64::NEG_INFINITY, P),
    ] {
        row(
            &mut rows,
            format!("Float64 literal {value:?}"),
            f64_literal(value),
            expected,
        );
    }
    for (value, expected) in [(0, N), (-3, P), (7, N)] {
        row(
            &mut rows,
            format!("Int64 literal {value}"),
            ScalarExpr::Literal(ScalarValue::Int64(value)),
            expected,
        );
    }
    for (shown, literal) in [
        ("Utf8", ScalarValue::Utf8("a".into())),
        ("Boolean", ScalarValue::Boolean(true)),
        ("Null", ScalarValue::Null),
        ("Timestamp", ScalarValue::Timestamp(0)),
    ] {
        row(
            &mut rows,
            format!("{shown} literal"),
            ScalarExpr::Literal(literal),
            U,
        );
    }
    let negative = |expr| ScalarExpr::Negative {
        expr: Box::new(expr),
        semantics: ExprSemantics::Sql,
    };
    for (sign, expected) in [(N, P), (P, N), (U, U)] {
        row(
            &mut rows,
            format!("Negative float {sign:?}"),
            negative(ScalarExpr::Column(float_column(sign))),
            expected,
        );
    }
    // provisional answer Q5: `-0` of an int is `0`, which is N.
    for (sign, expected) in [(N, U), (P, N), (U, U)] {
        row(
            &mut rows,
            format!("Negative int {sign:?}"),
            negative(ScalarExpr::Column(int_column(sign))),
            expected,
        );
    }
    let cast = |expr, to| ScalarExpr::Cast {
        expr: Box::new(expr),
        to,
        try_cast: false,
    };
    for sign in SIGNS {
        row(
            &mut rows,
            format!("Cast float {sign:?} to Float64"),
            cast(ScalarExpr::Column(float_column(sign)), DataType::Float64),
            sign,
        );
        row(
            &mut rows,
            format!("Cast int {sign:?} to Float64"),
            cast(ScalarExpr::Column(int_column(sign)), DataType::Float64),
            sign,
        );
        row(
            &mut rows,
            format!("Cast float {sign:?} to Utf8"),
            cast(ScalarExpr::Column(float_column(sign)), DataType::Utf8),
            U,
        );
    }
    // B15: `CAST(-0.5 AS BIGINT)` is the int 0, which is N.
    for (sign, expected) in [(N, N), (P, U), (U, U)] {
        row(
            &mut rows,
            format!("Cast float {sign:?} to Int64"),
            cast(ScalarExpr::Column(float_column(sign)), DataType::Int64),
            expected,
        );
    }
    let is_positive = |column| ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(column)),
        op: CompareOpKind::Gt,
        right: Box::new(f64_literal(0.0)),
        semantics: ExprSemantics::Sql,
    };
    let case = |branches: Vec<(ScalarExpr<MemoGroupId>, ScalarExpr<MemoGroupId>)>,
                else_expr: Option<ScalarExpr<MemoGroupId>>| ScalarExpr::Case {
        operand: None,
        branches,
        else_expr: else_expr.map(Box::new),
    };
    // provisional answer Q10: a Case has the sign its branches agree on;
    // provisional answer Q6: a missing ELSE is NULL, which is excluded.
    row(
        &mut rows,
        "CASE WHEN p > 0 THEN gain ELSE bytes END".into(),
        case(
            vec![(is_positive(P_COL), ScalarExpr::Column(GAIN))],
            Some(ScalarExpr::Column(BYTES)),
        ),
        N,
    );
    row(
        &mut rows,
        "CASE WHEN p > 0 THEN loss ELSE debt END".into(),
        case(
            vec![(is_positive(P_COL), ScalarExpr::Column(LOSS))],
            Some(ScalarExpr::Column(DEBT)),
        ),
        P,
    );
    row(
        &mut rows,
        "CASE WHEN p > 0 THEN gain ELSE loss END".into(),
        case(
            vec![(is_positive(P_COL), ScalarExpr::Column(GAIN))],
            Some(ScalarExpr::Column(LOSS)),
        ),
        U,
    );
    row(
        &mut rows,
        "CASE WHEN p > 0 THEN loss END".into(),
        case(vec![(is_positive(P_COL), ScalarExpr::Column(LOSS))], None),
        P,
    );
    let boolean = || is_positive(P_COL);
    for (shown, expr) in [
        ("Compare p > 0", boolean()),
        ("BoolAnd", ScalarExpr::BoolAnd(vec![boolean(), boolean()])),
        ("BoolOr", ScalarExpr::BoolOr(vec![boolean(), boolean()])),
        ("Not", ScalarExpr::Not(Box::new(boolean()))),
        ("IsNull", ScalarExpr::IsNull(column(GAIN))),
        ("IsNotNull", ScalarExpr::IsNotNull(column(GAIN))),
        (
            "InList",
            ScalarExpr::InList {
                expr: column(GAIN),
                list: vec![f64_literal(1.0)],
                negated: false,
            },
        ),
        ("CurrentTimestamp", ScalarExpr::CurrentTimestamp),
        ("EvalTimestamp", ScalarExpr::EvalTimestamp),
    ] {
        row(&mut rows, shown.into(), expr, U);
    }
    for (shown, expr) in [
        ("ScalarSubquery(t_pos)", ScalarExpr::ScalarSubquery(t_pos)),
        (
            "Exists(t_pos)",
            ScalarExpr::Exists {
                subquery: t_pos,
                negated: false,
            },
        ),
        (
            "InSubquery(gain, t_pos)",
            ScalarExpr::InSubquery {
                expr: column(GAIN),
                subquery: t_pos,
                negated: false,
            },
        ),
        (
            "PromqlScalarFromVector(pos)",
            ScalarExpr::PromqlScalarFromVector(pos_series),
        ),
    ] {
        row(&mut rows, shown.into(), expr, U);
    }
    rows.finish("T1.8");
}

// ── T1.9 ────────────────────────────────────────────────────────────────

/// T1.9: vector binary operators: a filter keeps the left side's values, a
/// union keeps what both agree on, a `bool` comparison is 0 or 1, and
/// `group_right` takes the right ("many") side.
#[test]
fn binary_op_values_and_labels() {
    let mut memo = memo();
    let mut rows = Rows::default();
    let pos = add(&mut memo, series("pos"));
    let neg = add(&mut memo, series("neg"));
    let mixed = add(&mut memo, series("mixed"));
    let one = add(&mut memo, vector_of(f64_literal(1.0)));
    let minus_one = add(&mut memo, vector_of(f64_literal(-1.0)));
    let zero = add(&mut memo, vector_of(f64_literal(0.0)));
    let set = |kind| BinaryOpKind::Set(kind);
    for (shown, operator, expected) in [
        (
            "pos > 1",
            binary(
                BinaryOpKind::Compare(CompareOpKind::Gt),
                None,
                false,
                pos,
                one,
            ),
            N,
        ),
        (
            "neg > -1",
            binary(
                BinaryOpKind::Compare(CompareOpKind::Gt),
                None,
                false,
                neg,
                minus_one,
            ),
            P,
        ),
        (
            "pos and neg",
            binary(set(PromQLVectorSetOpKind::And), None, false, pos, neg),
            N,
        ),
        (
            "neg unless pos",
            binary(set(PromQLVectorSetOpKind::Unless), None, false, neg, pos),
            P,
        ),
        (
            "pos or neg",
            binary(set(PromQLVectorSetOpKind::Or), None, false, pos, neg),
            U,
        ),
        (
            "pos or pos",
            binary(set(PromQLVectorSetOpKind::Or), None, false, pos, pos),
            N,
        ),
        // `a > bool b` between vectors lowers to `Compare` with
        // `return_bool`: its value is 0 or 1, so N whatever the left side
        // (`neg > bool -1` is `+0.0` or `1.0`, never P).
        (
            "mixed > bool 0",
            binary(
                BinaryOpKind::Compare(CompareOpKind::Gt),
                None,
                true,
                mixed,
                zero,
            ),
            N,
        ),
        (
            "neg > bool -1",
            binary(
                BinaryOpKind::Compare(CompareOpKind::Gt),
                None,
                true,
                neg,
                minus_one,
            ),
            N,
        ),
        (
            "CompareBool: mixed > 0",
            binary(
                BinaryOpKind::CompareBool(CompareOpKind::Gt),
                None,
                false,
                mixed,
                zero,
            ),
            N,
        ),
        (
            "neg * on(job) group_right pos",
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
                false,
                neg,
                pos,
            ),
            P,
        ),
    ] {
        match try_add(&mut memo, operator) {
            Ok(group) => {
                rows.check(
                    format!("{shown}: value"),
                    sign_of(&memo, group, "value"),
                    expected,
                );
                if let Some(job) = properties(&memo, group).schema.column_id("job") {
                    rows.check(format!("{shown}: job"), signs(&memo, group)[job], U);
                }
            }
            Err(error) => rows.fail(shown, format!("cannot build: {error}")),
        }
    }
    rows.finish("T1.9");
}

// ── T1.10 ───────────────────────────────────────────────────────────────

fn weighted_state(
    algorithm: SketchAlgorithm,
    weight: SummaryInputExpr,
    child: MemoGroupId,
) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        summary_type: FieldDataType::Sketch(algorithm),
        input: SummaryUpdate {
            item: Some(SummaryInputExpr::Column(ColumnRef::Named("job".into()))),
            weight,
        },
        reduction: Reduction::PerEntity,
        filter: None,
    })
}

/// The state column of `group`: its first non-plain field.
fn state_sign(memo: &Memo, group: MemoGroupId) -> ValueSign {
    let properties = properties(memo, group);
    let state = properties
        .schema
        .fields
        .iter()
        .position(|field| !field.is_plain())
        .expect("a state column");
    properties.column_signs[state]
}

/// The exhaustive witness over `FieldDataType` (provisional answer Q9): a
/// new state kind fails to compile here until a finalize/readout rule is
/// written for it.
fn field_data_type_witness(dtype: &FieldDataType) {
    match dtype {
        FieldDataType::Plain(_)
        | FieldDataType::ExactAggregate(..)
        | FieldDataType::Sketch(_)
        | FieldDataType::SummarySample(..)
        | FieldDataType::Wavelet(..)
        | FieldDataType::StatModel(..) => {}
    }
}

/// T1.10: a state records its weight's sign; readouts and finalized
/// accumulators derive theirs; merges keep what their inputs agree on.
///
/// Fails today: the entropy readout is N.
#[test]
fn summary_states_and_readouts() {
    let mut memo = memo();
    let mut rows = Rows::default();
    let windows: Vec<(ValueSign, MemoGroupId)> = SIGNS
        .into_iter()
        .map(|sign| {
            let scan = add(&mut memo, series(gauge_of(sign)));
            (sign, add(&mut memo, range(MINUTE_MS, scan)))
        })
        .collect();
    for &(sign, window) in &windows {
        let state = add(
            &mut memo,
            weighted_state(
                SketchAlgorithm::Cms,
                SummaryInputExpr::Column(ColumnRef::SampleValue),
                window,
            ),
        );
        rows.check(
            format!("Cms state weighted by value of {sign:?}"),
            state_sign(&memo, state),
            sign,
        );
    }
    let (_, pos_window) = windows[0];
    for (weight, expected) in [
        (SummaryInputExpr::Constant(1.0), N),
        (SummaryInputExpr::Constant(-1.0), P),
        (SummaryInputExpr::Constant(-0.0), P),
        (
            SummaryInputExpr::Tuple(vec![SummaryInputExpr::Constant(1.0)]),
            U,
        ),
    ] {
        let shown = format!("Cms state weighted by {weight:?}");
        match try_add(
            &mut memo,
            weighted_state(SketchAlgorithm::Cms, weight, pos_window),
        ) {
            Ok(state) => rows.check(shown, state_sign(&memo, state), expected),
            Err(error) => rows.fail(shown, format!("cannot build: {error}")),
        }
    }
    // Readouts over KLL / HLL / UnivMon states of each input sign.
    for &(sign, window) in &windows {
        let kll = add(
            &mut memo,
            asap_types::test_support::memo_fixtures::state(
                FieldDataType::Sketch(SketchAlgorithm::Kll),
                window,
            ),
        );
        for (q, expected) in [(0.9, sign), (1.5, U)] {
            let read = add(&mut memo, readout(SketchStatistic::Quantile { q }, kll));
            rows.check(
                format!("KLL quantile {q} over {sign:?}"),
                computed_value_sign(&memo, read),
                expected,
            );
        }
        let hll = add(
            &mut memo,
            asap_types::test_support::memo_fixtures::state(
                FieldDataType::Sketch(SketchAlgorithm::Hll),
                window,
            ),
        );
        let read = add(&mut memo, readout(SketchStatistic::Cardinality, hll));
        rows.check(
            format!("HLL cardinality over {sign:?}"),
            computed_value_sign(&memo, read),
            N,
        );
        let univmon = add(
            &mut memo,
            asap_types::test_support::memo_fixtures::state(
                FieldDataType::Sketch(SketchAlgorithm::UnivMon),
                window,
            ),
        );
        let read = add(&mut memo, readout(SketchStatistic::FrequencyL2, univmon));
        rows.check(
            format!("UnivMon L2 over {sign:?}"),
            computed_value_sign(&memo, read),
            N,
        );
        // User decision Q2.
        let read = add(
            &mut memo,
            readout(SketchStatistic::FrequencyEntropy, univmon),
        );
        rows.check(
            format!("UnivMon entropy over {sign:?}"),
            computed_value_sign(&memo, read),
            U,
        );
        for (algorithm, expected) in [
            (SketchAlgorithm::Cms, sign),
            (SketchAlgorithm::CountSketch, U),
        ] {
            let state = add(
                &mut memo,
                asap_types::test_support::memo_fixtures::state(
                    FieldDataType::Sketch(algorithm),
                    window,
                ),
            );
            let read = add(
                &mut memo,
                readout(
                    SketchStatistic::PointCount {
                        key: ColumnRef::Named("job".into()),
                        value: Some("a".into()),
                    },
                    state,
                ),
            );
            rows.check(
                format!("{algorithm:?} point count over {sign:?}"),
                computed_value_sign(&memo, read),
                expected,
            );
        }
    }
    // Finalized exact accumulators: the aggregate's sign over the state's.
    for &(sign, window) in &windows {
        for (kind, expected) in [
            (ExactKind::Sum, sign),
            (ExactKind::Min, sign),
            (ExactKind::Max, sign),
            (ExactKind::Count, N),
            (ExactKind::Rate, if sign == N { N } else { U }),
            (ExactKind::Increase, if sign == N { N } else { U }),
            (ExactKind::IRate, if sign == N { N } else { U }),
        ] {
            // provisional answer Q1 for the Sum over P row.
            let state = add(
                &mut memo,
                asap_types::test_support::memo_fixtures::state(FieldDataType::exact(kind), window),
            );
            field_data_type_witness(&properties(&memo, state).schema.fields[VALUE].dtype);
            let finalized = add(
                &mut memo,
                Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: state }),
            );
            rows.check(
                format!("Finalize {kind:?} over {sign:?}"),
                signs(&memo, finalized)[VALUE],
                expected,
            );
        }
    }
    // Merges of one definition (the memo rejects a merge of two sources,
    // `Coverage(DefinitionMismatch)`, so a merge of N with P states cannot be
    // built): N with N is N.
    let kll_of = |memo: &mut Memo, window| {
        add(
            memo,
            asap_types::test_support::memo_fixtures::state(
                FieldDataType::Sketch(SketchAlgorithm::Kll),
                window,
            ),
        )
    };
    let pos_kll = kll_of(&mut memo, windows[0].1);
    let pos_shifted = {
        let scan = add(&mut memo, series("pos"));
        let shifted = add(&mut memo, shift(MINUTE_MS, None, scan));
        let window = add(&mut memo, range(MINUTE_MS, shifted));
        kll_of(&mut memo, window)
    };
    for (shown, children, expected) in [(
        "SummaryMerge(pos, pos offset 1m)",
        vec![pos_kll, pos_shifted],
        N,
    )] {
        match try_add(
            &mut memo,
            Operator::ASAP(ASAPOp::SummaryMerge {
                children,
                group_by: None,
            }),
        ) {
            Ok(group) => rows.check(shown, state_sign(&memo, group), expected),
            Err(error) => rows.fail(shown, format!("cannot build: {error}")),
        }
    }
    rows.finish("T1.10");
}

/// T4 b09: a finalized rate over P values has no sign: samples -5 then -3
/// are an increase of 2, samples -3 then -5 a reset and an increase of -5.
#[test]
fn b09_a_finalized_rate_over_non_positive_values_has_no_sign() {
    let mut memo = memo();
    let scan = add(&mut memo, series("neg"));
    let window = add(&mut memo, range(MINUTE_MS, scan));
    let state = add(
        &mut memo,
        asap_types::test_support::memo_fixtures::state(
            FieldDataType::exact(ExactKind::Rate),
            window,
        ),
    );
    let finalized = add(
        &mut memo,
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: state }),
    );
    assert_eq!(signs(&memo, finalized)[VALUE], U);
}

/// T4 b03: a quantile read from KLL panes merged over `pos` keeps N.
#[test]
fn b03_a_quantile_of_merged_panes_keeps_its_sign() {
    let mut memo = memo();
    let scan = add(&mut memo, series("pos"));
    let window = add(&mut memo, range(MINUTE_MS, scan));
    let shifted = add(&mut memo, shift(MINUTE_MS, None, scan));
    let earlier = add(&mut memo, range(MINUTE_MS, shifted));
    let kll = |memo: &mut Memo, window| {
        add(
            memo,
            asap_types::test_support::memo_fixtures::state(
                FieldDataType::Sketch(SketchAlgorithm::Kll),
                window,
            ),
        )
    };
    let a = kll(&mut memo, window);
    let b = kll(&mut memo, earlier);
    let merged = add(
        &mut memo,
        Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![a, b],
            group_by: None,
        }),
    );
    let read = add(
        &mut memo,
        readout(SketchStatistic::Quantile { q: 0.9 }, merged),
    );
    assert_eq!(computed_value_sign(&memo, read), N);
}

/// T4 b01: `rate(mixed[5m])` has no sign: a gauge's decrease reads as a
/// reset.
#[test]
fn b01_rate_of_an_undeclared_gauge_has_no_sign() {
    let mut memo = memo();
    let scan = add(&mut memo, series("mixed"));
    let window = add(&mut memo, range(5 * MINUTE_MS, scan));
    let rate = add(
        &mut memo,
        aggregate(Reduction::PerEntity, AggIntent::Rate, window),
    );
    assert_eq!(computed_value_sign(&memo, rate), U);
}

/// T4 b10 (sign part): `-quantile_over_time(0.9, pos[5m])` is P and its
/// `sum by (job)` keeps P (provisional answer Q1).
#[test]
fn b10_negation_keeps_its_sign_through_a_sum() {
    let mut memo = memo();
    let scan = add(&mut memo, series("pos"));
    let window = add(&mut memo, range(5 * MINUTE_MS, scan));
    let quantile = add(
        &mut memo,
        aggregate(
            Reduction::PerEntity,
            AggIntent::Quantile {
                col: Some(VALUE),
                q: 0.9,
            },
            window,
        ),
    );
    let negated = add(
        &mut memo,
        map_value(
            ScalarExpr::Negative {
                expr: column(VALUE),
                semantics: ExprSemantics::Promql,
            },
            quantile,
        ),
    );
    let sum = add(
        &mut memo,
        aggregate(
            Reduction::by(vec![JOB]),
            AggIntent::Sum { col: Some(VALUE) },
            negated,
        ),
    );
    assert_eq!(signs(&memo, negated)[VALUE], P);
    assert_eq!(computed_value_sign(&memo, sum), P);
}

/// T4 b10 (key part): PromQL `abs`, `promql_abs` and SQL `ABS` all parse to
/// `MathFunc::Abs`, the key the error contract also matches on.
///
/// Fails today: `math_function_of_call` is not implemented.
#[test]
fn b10_abs_spellings_parse_to_one_key() {
    for name in ["abs", "ABS", "promql_abs"] {
        let args: Vec<ScalarExpr<MemoGroupId>> = vec![ScalarExpr::Column(VALUE)];
        assert_eq!(
            math_function_of_call(name, &args),
            Some(MathFunc::Abs),
            "{name}"
        );
    }
}

/// T4 b11 (sign part): `q(pos) / q(neg)` is P, the sign of the exact values.
#[test]
fn b11_quotient_sign_is_of_exact_values() {
    let mut memo = memo();
    let quantile = |memo: &mut Memo, metric| {
        let scan = add(memo, series(metric));
        let window = add(memo, range(5 * MINUTE_MS, scan));
        add(
            memo,
            aggregate(
                Reduction::PerEntity,
                AggIntent::Quantile {
                    col: Some(VALUE),
                    q: 0.9,
                },
                window,
            ),
        )
    };
    let numerator = quantile(&mut memo, "pos");
    let denominator = quantile(&mut memo, "neg");
    let quotient = add(
        &mut memo,
        binary(
            BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
            None,
            false,
            numerator,
            denominator,
        ),
    );
    assert_eq!(sign_of(&memo, quotient, "value"), P);
}

/// T4 b11 (precondition part): an estimated `+0.0` divisor where the exact
/// one is `-0.0` flips an infinite quotient's sign; the `/` contract must
/// require a checked division when the divisor reads an estimate.
#[test]
#[ignore = "S3: checked division when the divisor reads an estimate"]
fn b11_estimated_zero_divisor_needs_a_checked_division() {
    todo!("S3: BinaryOperator::checked_relative_division set when the divisor reads an estimate")
}

// ── T1.11 ───────────────────────────────────────────────────────────────

/// T1.11: `is_one_sign` and `state_column_sign`.
///
/// Fails today: both are new (`todo!()`).
#[test]
fn value_sign_one_sign_and_state_column_sign() {
    assert!(N.is_one_sign());
    assert!(P.is_one_sign());
    assert!(!U.is_one_sign());
    let mut memo = memo();
    let scan = add(&mut memo, series("pos"));
    let window = add(&mut memo, range(MINUTE_MS, scan));
    let ddsketch = add(
        &mut memo,
        asap_types::test_support::memo_fixtures::state(
            FieldDataType::Sketch(SketchAlgorithm::DDSketch),
            window,
        ),
    );
    assert_eq!(properties(&memo, ddsketch).state_column_sign(), Some(N));
    assert_eq!(properties(&memo, scan).state_column_sign(), None);
    // A chain's state column comes after its computed `window_end`
    // (column 0, U): the state's sign, not the first column's.
    let chain = add(
        &mut memo,
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: scan,
            summary_type: FieldDataType::Sketch(SketchAlgorithm::DDSketch),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
            tumbling_window_width_ms: 60_000,
        }),
    );
    assert_eq!(properties(&memo, chain).state_column_sign(), Some(N));
}

// ── T2.3 ────────────────────────────────────────────────────────────────

const COLUMN_PROPERTIES: &str = include_str!("../src/stages/logical/group/column_properties.rs");

/// T2.3: the sign module denies wildcard enum arms (user decision Q5), and
/// no tuple match in it ends in `_ =>`, which the lint cannot see
/// (provisional answer Q9).
///
/// Fails today: the attribute is absent, and `arithmetic_sign` matches
/// `(op, left, right)` with a `_ => Unknown` arm.
#[test]
fn the_sign_module_denies_wildcard_enum_arms() {
    let first_code_line = COLUMN_PROPERTIES
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("//"))
        .unwrap_or_default();
    assert_eq!(
        first_code_line, "#![deny(clippy::wildcard_enum_match_arm)]",
        "column_properties.rs must start with the lint"
    );
    let mut tuple_wildcards = Vec::new();
    let lines: Vec<&str> = COLUMN_PROPERTIES.lines().collect();
    for (start, line) in lines.iter().enumerate() {
        let Some(at) = line.find("match (") else {
            continue;
        };
        // The arms of this match: the lines up to the closing brace at the
        // match's own indentation.
        let indent = line.len() - line.trim_start().len();
        let _ = at;
        for (offset, arm) in lines[start + 1..].iter().enumerate() {
            let arm_indent = arm.len() - arm.trim_start().len();
            if arm.trim() == "}" && arm_indent == indent {
                break;
            }
            if arm.trim_start().starts_with("_ =>") {
                tuple_wildcards.push(format!("line {}: {}", start + offset + 2, arm.trim()));
            }
        }
    }
    assert!(
        tuple_wildcards.is_empty(),
        "tuple matches ending in a wildcard: {tuple_wildcards:?}"
    );
}

/// The parse functions return `None` for a call that is not a PromQL math
/// or date function.
#[test]
fn parse_functions_reject_other_calls() {
    let args: Vec<ScalarExpr<MemoGroupId>> = vec![ScalarExpr::Column(VALUE)];
    assert_eq!(math_function_of_call("foo", &args), None);
    assert_eq!(math_function_of_call("promql_hour", &args), None);
    assert_eq!(time_function_of_call("promql_abs"), None);
    assert_eq!(time_function_of_call("hour"), None);
}
