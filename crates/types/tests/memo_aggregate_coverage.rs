//! Aggregate coverage (`AggregateCoverage`), derived once per memo group
//! from its input groups (`docs/design_docs/proposals/asap-primitive-schema.md`
//! §4.2.2). The memo of the worked example:
//!
//! ```text
//! G1  Scan latency
//! G2  Filter(job = "api")(G1);  G3 TimeRange[1m](G2);  G4 SummaryAgg[KLL](G3)
//! G5  Filter(job = "web")(G1);  G6 TimeRange[1m](G5);  G7 SummaryAgg[KLL](G6)
//! G8  Filter(job = "api" ∧ a + b > 10)(G1);  G9 TimeRange[1m](G8);  G10 SummaryAgg[KLL](G9)
//! ```
use std::ops::Bound;

use asap_types::ir::operator::arguments::{AtModifier, Reduction, Source, SourceColumn};
use asap_types::ir::operator::AggIntent;

use asap_types::ir::properties::selection::{Constraint, IntervalConstraintLimit};
use asap_types::ir::scalar::expr_ir::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::{ASAPOp, ExprSemantics, NonASAPOp, Operator, Predicate, ScalarExpr};
use asap_types::stages::{
    view_coverage, AggregateCoverage, CandidateExpression, CandidateForMemoGroup, ColumnLineage,
    CoverageError, LogicalProperties, LogicalRequirements, Memo, MemoGroupError, MemoGroupId,
    OriginOfCandidateForMemoGroup,
};
use asap_types::summary_type::{ExactKind, MergeInputRule, SketchAlgorithm, SummaryUpdate};
use asap_types::test_support::memo_fixtures::{filter, range, scan, shift, state};
use asap_types::workload::DataWorkload;

const TS: usize = 0;
const JOB: usize = 2;
const A: usize = 3;
const B: usize = 4;
const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 60 * MINUTE_MS;

/// `Scan latency{predicates}`: `[ts, value, job, a, b]`.
fn latency(predicates: Vec<ScalarExpr<MemoGroupId>>) -> Operator<MemoGroupId> {
    scan(
        "latency",
        &[
            ("job", DataType::Utf8),
            ("a", DataType::Float64),
            ("b", DataType::Float64),
        ],
        predicates,
    )
}

fn job_is(job: &str) -> ScalarExpr<MemoGroupId> {
    ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(JOB)),
        op: CompareOpKind::Eq,
        right: Box::new(ScalarExpr::Literal(ScalarValue::Utf8(job.into()))),
        semantics: ExprSemantics::Sql,
    }
}

/// `a + b > 10`: it spans two columns, so it is no selection.
fn a_plus_b_above_10() -> ScalarExpr<MemoGroupId> {
    ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Add,
            left: Box::new(ScalarExpr::Column(A)),
            right: Box::new(ScalarExpr::Column(B)),
            semantics: ExprSemantics::Sql,
        }),
        op: CompareOpKind::Gt,
        right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(10.0))),
        semantics: ExprSemantics::Sql,
    }
}

fn kll_summary_type() -> FieldDataType {
    FieldDataType::Sketch(SketchAlgorithm::Kll)
}

/// `SummaryAgg[KLL]` of the sample value per series.
fn kll(child: MemoGroupId) -> Operator<MemoGroupId> {
    state(kll_summary_type(), child)
}

/// `SummaryTumblingWindowChain[KLL, width_ms]` of the sample value per series.
fn kll_chain(width_ms: i64, child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child,
        summary_type: kll_summary_type(),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
        tumbling_window_width_ms: width_ms as u64,
    })
}

fn merge(children: Vec<MemoGroupId>) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryMerge {
        children,
        group_by: None,
    })
}

/// KLL over the last minute of `G1` filtered by `pred`.
fn kll_of_last_minute_where(
    memo: &mut Memo,
    g1: MemoGroupId,
    pred: ScalarExpr<MemoGroupId>,
) -> MemoGroupId {
    let filtered = memo.find_or_add_group(filter(pred, g1)).unwrap();
    let last_minute = memo.find_or_add_group(range(MINUTE_MS, filtered)).unwrap();
    memo.find_or_add_group(kll(last_minute)).unwrap()
}

fn coverage(memo: &Memo, group: MemoGroupId) -> &AggregateCoverage {
    memo.groups()[group.0 as usize]
        .group_properties
        .aggregate_coverage
        .as_ref()
        .expect("a summary state")
}

/// `KLL` as an unlifted operator: its main input is `None`.
fn kll_unlifted() -> Operator<Option<MemoGroupId>> {
    kll(MemoGroupId(0)).map_children(|_| None)
}

fn jobs(values: &[&str]) -> Constraint {
    Constraint::In(
        values
            .iter()
            .map(|value| ScalarValue::Utf8((*value).into()))
            .collect(),
    )
}

/// `(t + lower_ms, t + upper_ms]` on the timestamp column, t the evaluation
/// time: left-open, right-closed like a PromQL range.
fn relative(lower_ms: i64, upper_ms: i64) -> Constraint {
    Constraint::Interval {
        lower: Bound::Excluded(IntervalConstraintLimit::EvaluationTimePlusMs(lower_ms)),
        upper: Bound::Included(IntervalConstraintLimit::EvaluationTimePlusMs(upper_ms)),
    }
}

fn last_minute() -> Constraint {
    relative(-MINUTE_MS, 0)
}

fn ts() -> SourceColumn {
    latency_column("ts")
}

/// The column `column` of the series `latency`.
fn latency_column(column: &str) -> SourceColumn {
    SourceColumn {
        source: Source::TimeSeries {
            metric: "latency".into(),
        },
        column: column.into(),
    }
}

/// The constraint on the timestamp column of `group`'s one selection box.
fn time_of(memo: &Memo, group: MemoGroupId) -> Constraint {
    let [selection] = coverage(memo, group).selection() else {
        panic!("one box: {:?}", coverage(memo, group))
    };
    selection.columns()[&ts()].clone()
}

/// G4: `job = "api"` and the minute are lifted; the definition is the KLL
/// over G1.
#[test]
fn a_summary_agg_over_a_job_filter_lifts_it_into_the_selection() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo.find_or_add_group(latency(vec![])).unwrap();
    let g4 = kll_of_last_minute_where(&mut memo, g1, job_is("api"));
    let coverage = coverage(&memo, g4);
    assert_eq!(coverage.definition().input, Some(g1));
    assert_eq!(
        coverage.definition().unlifted_operators,
        vec![kll_unlifted()]
    );
    let [selection] = coverage.selection() else {
        panic!("one box: {coverage:?}")
    };
    assert_eq!(selection.columns()[&latency_column("job")], jobs(&["api"]));
    assert_eq!(selection.columns()[&ts()], last_minute());
}

/// G7 has G4's definition (`==`) and a disjoint selection, so
/// `SummaryMerge(G4, G7)` is a valid group covering job ∈ {api, web}.
#[test]
fn states_with_equal_definitions_and_disjoint_selections_merge() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo.find_or_add_group(latency(vec![])).unwrap();
    let g4 = kll_of_last_minute_where(&mut memo, g1, job_is("api"));
    let g7 = kll_of_last_minute_where(&mut memo, g1, job_is("web"));
    assert_eq!(
        coverage(&memo, g4).definition(),
        coverage(&memo, g7).definition()
    );
    let merged = memo.find_or_add_group(merge(vec![g4, g7])).unwrap();
    let coverage = coverage(&memo, merged);
    assert_eq!(coverage.definition().input, Some(g1));
    let [selection] = coverage.selection() else {
        panic!("one box: {coverage:?}")
    };
    assert_eq!(
        selection.columns()[&latency_column("job")],
        jobs(&["api", "web"])
    );
}

/// Two states that select the same rows do not merge: KLL counts a row
/// once per input.
#[test]
fn states_with_overlapping_selections_do_not_merge() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo.find_or_add_group(latency(vec![])).unwrap();
    let g4 = kll_of_last_minute_where(&mut memo, g1, job_is("api"));
    let groups = memo.groups().len();
    assert!(matches!(
        memo.find_or_add_group(merge(vec![g4, g4])),
        Err(MemoGroupError::Coverage(CoverageError::InputsOverlap {
            required: MergeInputRule::Disjoint
        }))
    ));
    assert_eq!(memo.groups().len(), groups);
}

/// G10: `a + b > 10` spans two columns, so it stays as a `Filter` in the
/// definition; G10 does not merge with G4 although both select job = api.
#[test]
fn a_filter_that_is_no_selection_stays_unlifted() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo.find_or_add_group(latency(vec![])).unwrap();
    let g4 = kll_of_last_minute_where(&mut memo, g1, job_is("api"));
    let g10 = kll_of_last_minute_where(
        &mut memo,
        g1,
        ScalarExpr::BoolAnd(vec![job_is("api"), a_plus_b_above_10()]),
    );
    let coverage_of_g10 = coverage(&memo, g10);
    assert_eq!(coverage_of_g10.definition().input, Some(g1));
    assert_eq!(
        coverage_of_g10.definition().unlifted_operators,
        vec![
            kll_unlifted(),
            filter(a_plus_b_above_10(), MemoGroupId(0)).map_children(|_| None),
        ]
    );
    assert_eq!(
        coverage_of_g10.selection()[0].columns()[&latency_column("job")],
        jobs(&["api"])
    );
    assert!(matches!(
        memo.find_or_add_group(merge(vec![g4, g10])),
        Err(MemoGroupError::Coverage(CoverageError::DefinitionMismatch))
    ));
}

/// `a > (SELECT threshold)`: a scalar subquery reading group `threshold`.
fn a_above(threshold: MemoGroupId) -> ScalarExpr<MemoGroupId> {
    ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(A)),
        op: CompareOpKind::Gt,
        right: Box::new(ScalarExpr::ScalarSubquery(threshold)),
        semantics: ExprSemantics::Sql,
    }
}

/// A residual `Filter` that reads a scalar subquery names the subquery's
/// group (`Some`) next to its main input (`None`). Two states with that
/// equal definition and disjoint selections merge.
#[test]
fn states_whose_definitions_read_the_same_subquery_merge() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo.find_or_add_group(latency(vec![])).unwrap();
    let threshold = memo
        .find_or_add_group(Operator::NonASAP(NonASAPOp::Values {
            rows: vec![vec![ScalarExpr::Literal(ScalarValue::Float64(10.0))]],
            schema: Schema::new(vec![Field::plain("threshold", DataType::Float64, false)]),
        }))
        .unwrap();
    let state_of_job = |memo: &mut Memo, job: &str| {
        let pred = ScalarExpr::BoolAnd(vec![job_is(job), a_above(threshold)]);
        kll_of_last_minute_where(memo, g1, pred)
    };
    let api = state_of_job(&mut memo, "api");
    let web = state_of_job(&mut memo, "web");
    let definition = coverage(&memo, api).definition();
    assert_eq!(definition.input, Some(g1));
    assert_eq!(
        definition.unlifted_operators,
        vec![
            kll_unlifted(),
            Operator::NonASAP(NonASAPOp::Filter {
                pred: Predicate(a_above(threshold).map_operator_refs(&mut |group| Some(*group))),
                child: None,
            }),
        ]
    );
    assert_eq!(definition, coverage(&memo, web).definition());
    let merged = memo.find_or_add_group(merge(vec![api, web])).unwrap();
    assert_eq!(
        coverage(&memo, merged).selection()[0].columns()[&latency_column("job")],
        jobs(&["api", "web"])
    );
}

/// PromQL label matchers are the scan's predicates: they are lifted too, so
/// `latency{job="api"}` and `latency{job="web"}` states merge. The scan,
/// without them, is the last unlifted operator. Each scan's `job` column has
/// lineage `latency.job`, so the selections `latency.job ∈ {api}` and
/// `latency.job ∈ {web}` are disjoint selections on one column (concepts doc,
/// section 3, column lineage example 4).
#[test]
fn scan_predicates_lift_and_their_states_merge() {
    let mut memo = Memo::new(&DataWorkload::default());
    let state_of_job = |memo: &mut Memo, job: &str| {
        let series = memo.find_or_add_group(latency(vec![job_is(job)])).unwrap();
        assert_eq!(
            memo.groups()[series.0 as usize]
                .group_properties
                .column_lineage[JOB],
            ColumnLineage::SourceColumn(latency_column("job"))
        );
        let last_minute = memo.find_or_add_group(range(MINUTE_MS, series)).unwrap();
        memo.find_or_add_group(kll(last_minute)).unwrap()
    };
    let api = state_of_job(&mut memo, "api");
    let web = state_of_job(&mut memo, "web");
    let definition = coverage(&memo, api).definition();
    assert_eq!(definition.input, None);
    assert_eq!(
        definition.unlifted_operators,
        vec![kll_unlifted(), latency(vec![]).map_children(|_| None)]
    );
    assert_eq!(
        coverage(&memo, api).selection()[0].columns()[&latency_column("job")],
        jobs(&["api"])
    );
    let merged = memo.find_or_add_group(merge(vec![api, web])).unwrap();
    assert_eq!(
        coverage(&memo, merged).selection()[0].columns()[&latency_column("job")],
        jobs(&["api", "web"])
    );
}

/// A chain has the definition of a build over its input and leaves the
/// timestamp column unconstrained. `TimeRange` and `TimeShift` over it
/// select windows as over raw rows, so the merge over that selection covers
/// what the direct build does, and `add_candidate` accepts the merge in the
/// direct build's group:
///
/// ```text
/// G1 Scan latency
/// G2 TimeShift(1h)(G1);  G3 TimeRange[5m](G2);  G4 SummaryAgg[KLL](G3)   (the query)
/// G5 Chain[KLL, 1m](G1);  G6 TimeShift(1h)(G5);  G7 TimeRange[5m](G6)
/// G4 candidate: SummaryMerge(G7)
/// ```
#[test]
fn a_merge_over_a_chains_windows_covers_the_direct_build() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo.find_or_add_group(latency(vec![])).unwrap();
    let g2 = memo.find_or_add_group(shift(HOUR_MS, None, g1)).unwrap();
    let g3 = memo.find_or_add_group(range(5 * MINUTE_MS, g2)).unwrap();
    let g4 = memo.find_or_add_group(kll(g3)).unwrap();
    let g5 = memo.find_or_add_group(kll_chain(MINUTE_MS, g1)).unwrap();
    let g6 = memo.find_or_add_group(shift(HOUR_MS, None, g5)).unwrap();
    let g7 = memo.find_or_add_group(range(5 * MINUTE_MS, g6)).unwrap();

    let whole_chain = coverage(&memo, g5);
    assert_eq!(whole_chain.definition(), coverage(&memo, g4).definition());
    assert!(whole_chain.selection()[0].columns().is_empty());
    let five_minutes_an_hour_ago = relative(-HOUR_MS - 5 * MINUTE_MS, -HOUR_MS);
    assert_eq!(time_of(&memo, g7), five_minutes_an_hour_ago);
    assert_eq!(coverage(&memo, g7), coverage(&memo, g4));

    let expression = CandidateExpression {
        operator: merge(vec![g7]),
    };
    let merge_of_windows = CandidateForMemoGroup {
        logical_properties: LogicalProperties::new(&expression, None).unwrap(),
        expression,
        input_requirements: vec![LogicalRequirements::default()],
        fused_groups: vec![],
        origin: OriginOfCandidateForMemoGroup::Pass1,
    };
    memo.add_candidate(g4, merge_of_windows).unwrap();
    let merged = memo.find_or_add_group(merge(vec![g7])).unwrap();
    assert_eq!(coverage(&memo, merged), coverage(&memo, g4));
}

/// The range and every offset lift into one interval relative to the
/// evaluation time t; `@ T` fixes the interval in absolute time:
///
/// | Group | Expression | Timestamp column |
/// |---|---|---|
/// | G2 | `TimeRange[5m](G1)` (`m[5m]`) | `(t − 5m, t]` |
/// | G4 | `TimeRange[5m](TimeShift(1h)(G1))` (`m[5m] offset 1h`) | `(t − 65m, t − 60m]` |
/// | G6 | `TimeRange[5m](TimeShift(@ 1700000000)(G1))` | `(1699999700000, 1700000000000]` |
///
/// each under a `SummaryAgg[KLL]`.
#[test]
fn time_range_and_shifts_lift_into_one_interval_on_the_timestamp_column() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo.find_or_add_group(latency(vec![])).unwrap();
    let plain = kll_over_last_5m(&mut memo, g1);
    let offset = kll_over_last_5m_of(&mut memo, shift(HOUR_MS, None, g1));
    let at_t = kll_over_last_5m_of(
        &mut memo,
        shift(0, Some(AtModifier::Timestamp(1_700_000_000_000)), g1),
    );
    assert_eq!(time_of(&memo, plain), relative(-5 * MINUTE_MS, 0));
    assert_eq!(time_of(&memo, offset), relative(-3_900_000, -3_600_000));
    let timestamp = |ms| IntervalConstraintLimit::FixedValue(ScalarValue::Timestamp(ms));
    assert_eq!(
        time_of(&memo, at_t),
        Constraint::Interval {
            lower: Bound::Excluded(timestamp(1_699_999_700_000)),
            upper: Bound::Included(timestamp(1_700_000_000_000)),
        }
    );
    for state in [offset, at_t] {
        assert_eq!(
            coverage(&memo, state).definition(),
            coverage(&memo, plain).definition()
        );
    }
}

/// `SummaryAgg[KLL](TimeRange[5m](G1))`: `m[5m]`.
fn kll_over_last_5m(memo: &mut Memo, input: MemoGroupId) -> MemoGroupId {
    let ranged = memo.find_or_add_group(range(5 * MINUTE_MS, input)).unwrap();
    memo.find_or_add_group(kll(ranged)).unwrap()
}

/// [`kll_over_last_5m`] over the group of `expression`.
fn kll_over_last_5m_of(memo: &mut Memo, expression: Operator<MemoGroupId>) -> MemoGroupId {
    let input = memo.find_or_add_group(expression).unwrap();
    kll_over_last_5m(memo, input)
}

/// `@ end()` and `@ start()` are lifted above the subtree they anchor:
/// the front end lowers `m[5m] @ end()` as `TimeShift{@ end()}(G2)` over
/// G2 = `m[5m]`, the group an instant query's `m[5m]` reads too. The
/// `TimeShift` pins G2 to one end of the request, so it is not lifted: it
/// is the input group of the state over it, whose selection is empty.
///
/// ```text
/// G1 Scan latency;  G2 TimeRange[5m](G1);  G3 SummaryAgg[KLL](G2)   (instant query)
/// G4 TimeShift(@ end())(G2);  G5 SummaryAgg[KLL](G4)                 (range query)
/// ```
#[test]
fn at_end_and_at_start_share_the_subtree_they_anchor_and_stay_unlifted() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo.find_or_add_group(latency(vec![])).unwrap();
    let g2 = memo.find_or_add_group(range(5 * MINUTE_MS, g1)).unwrap();
    let g3 = memo.find_or_add_group(kll(g2)).unwrap();
    assert_eq!(time_of(&memo, g3), relative(-5 * MINUTE_MS, 0));
    for at in [AtModifier::End, AtModifier::Start] {
        let pinned = memo.find_or_add_group(shift(0, Some(at), g2)).unwrap();
        assert_eq!(
            memo.groups()[pinned.0 as usize].expression.children(),
            [&g2]
        );
        let state = memo.find_or_add_group(kll(pinned)).unwrap();
        let coverage = coverage(&memo, state);
        assert_eq!(coverage.definition().input, Some(pinned));
        assert_eq!(coverage.definition().unlifted_operators, [kll_unlifted()]);
        assert!(coverage.selection()[0].columns().is_empty());
    }
}

/// `ts >= <timestamp>` (a SQL timestamp filter) and `value > 10` lift as
/// intervals between fixed values: `G3 SummaryAgg[KLL](G2)`, `G2
/// Filter(ts >= 2026-11-01 ∧ value > 10)(G1)`.
#[test]
fn value_and_absolute_timestamp_filters_lift_with_fixed_value_limits() {
    const NOV_1_2026_MS: i64 = 1_793_491_200_000;
    let compare = |column, op, value| ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(column)),
        op,
        right: Box::new(ScalarExpr::Literal(value)),
        semantics: ExprSemantics::Sql,
    };
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo.find_or_add_group(latency(vec![])).unwrap();
    let g2 = memo
        .find_or_add_group(filter(
            ScalarExpr::BoolAnd(vec![
                compare(TS, CompareOpKind::Ge, ScalarValue::Timestamp(NOV_1_2026_MS)),
                compare(1, CompareOpKind::Gt, ScalarValue::Float64(10.0)),
            ]),
            g1,
        ))
        .unwrap();
    let g3 = memo.find_or_add_group(kll(g2)).unwrap();
    let fixed = |value| Bound::Included(IntervalConstraintLimit::FixedValue(value));
    assert_eq!(
        time_of(&memo, g3),
        Constraint::Interval {
            lower: fixed(ScalarValue::Timestamp(NOV_1_2026_MS)),
            upper: Bound::Unbounded,
        }
    );
    assert_eq!(
        coverage(&memo, g3).selection()[0].columns()[&latency_column("value")],
        Constraint::Interval {
            lower: Bound::Excluded(IntervalConstraintLimit::FixedValue(ScalarValue::Float64(
                10.0
            ))),
            upper: Bound::Unbounded,
        }
    );
    assert_eq!(
        coverage(&memo, g3).definition().unlifted_operators,
        vec![kll_unlifted()]
    );
}

/// Counter states (`Increase`) merge only over windows disjoint in time:
/// the last 5 minutes and the 5 before them merge into the last 10; two
/// copies of the last 5 minutes do not. A KLL over the last minute and one
/// over the minute before it are disjoint too.
///
/// ```text
/// G2 TimeRange[5m](G1);  G3 SummaryAgg[Increase](G2)
/// G4 TimeShift(5m)(G1);  G5 TimeRange[5m](G4);  G6 SummaryAgg[Increase](G5)
/// G7 SummaryMerge(G3, G6)
/// ```
#[test]
fn counter_states_merge_over_windows_disjoint_in_time() {
    let increase = || FieldDataType::exact(ExactKind::Increase);
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo.find_or_add_group(latency(vec![])).unwrap();
    let g2 = memo.find_or_add_group(range(5 * MINUTE_MS, g1)).unwrap();
    let g3 = memo.find_or_add_group(state(increase(), g2)).unwrap();
    let g4 = memo
        .find_or_add_group(shift(5 * MINUTE_MS, None, g1))
        .unwrap();
    let g5 = memo.find_or_add_group(range(5 * MINUTE_MS, g4)).unwrap();
    let g6 = memo.find_or_add_group(state(increase(), g5)).unwrap();
    let g7 = memo.find_or_add_group(merge(vec![g3, g6])).unwrap();
    assert_eq!(time_of(&memo, g7), relative(-10 * MINUTE_MS, 0));
    assert!(matches!(
        memo.find_or_add_group(merge(vec![g3, g3])),
        Err(MemoGroupError::Coverage(CoverageError::InputsOverlap {
            required: MergeInputRule::DisjointInTime
        }))
    ));
    let g8 = memo.find_or_add_group(kll(g2)).unwrap();
    let g9 = memo.find_or_add_group(kll(g5)).unwrap();
    let g10 = memo.find_or_add_group(merge(vec![g8, g9])).unwrap();
    assert_eq!(time_of(&memo, g10), relative(-10 * MINUTE_MS, 0));
}

/// An exact `Aggregate` gets coverage like a summary state: `G4
/// Aggregate[sum(value) by job](G3)` over the last minute of `job = "api"`
/// has the `Aggregate` as its definition and the job and minute as its
/// selection.
#[test]
fn an_exact_aggregate_has_coverage() {
    let sum_by_job = |child| {
        Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![JOB]),
            measures: vec![AggIntent::Sum { col: Some(1) }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child,
        })
    };
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = memo.find_or_add_group(latency(vec![])).unwrap();
    let g2 = memo.find_or_add_group(filter(job_is("api"), g1)).unwrap();
    let g3 = memo.find_or_add_group(range(MINUTE_MS, g2)).unwrap();
    let g4 = memo.find_or_add_group(sum_by_job(g3)).unwrap();
    let coverage = coverage(&memo, g4);
    assert_eq!(coverage.definition().input, Some(g1));
    assert_eq!(
        coverage.definition().unlifted_operators,
        vec![sum_by_job(MemoGroupId(0)).map_children(|_| None)]
    );
    let [selection] = coverage.selection() else {
        panic!("one box: {coverage:?}")
    };
    assert_eq!(selection.columns()[&latency_column("job")], jobs(&["api"]));
    assert_eq!(selection.columns()[&ts()], last_minute());
}

/// KLL over the last minute of `metric{job="api"}` (`scan` → `filter` →
/// `range` → `kll`), in `memo`: its scan group and its state group.
fn kll_of_last_minute_of(memo: &mut Memo, metric: &str) -> (MemoGroupId, MemoGroupId) {
    let scan = memo
        .find_or_add_group(scan(
            metric,
            &[
                ("job", DataType::Utf8),
                ("a", DataType::Float64),
                ("b", DataType::Float64),
            ],
            vec![],
        ))
        .unwrap();
    (scan, kll_of_last_minute_where(memo, scan, job_is("api")))
}

/// The plan's view of a state's coverage names no memo group: the same
/// state in two memos whose `Scan` groups have different ids has one
/// `definition_id` and one selection.
#[test]
fn a_view_coverage_is_equal_across_memos_whatever_the_group_ids() {
    let mut first = Memo::new(&DataWorkload::default());
    let (first_scan, first_state) = kll_of_last_minute_of(&mut first, "latency");
    let mut second = Memo::new(&DataWorkload::default());
    // An unrelated query first, so `latency`'s scan gets another id.
    kll_of_last_minute_of(&mut second, "requests");
    let (second_scan, second_state) = kll_of_last_minute_of(&mut second, "latency");
    assert_ne!(first_scan, second_scan);
    assert_eq!(
        coverage(&first, first_state).definition().input,
        Some(first_scan)
    );
    let first_view = view_coverage(&first, first_state).unwrap();
    let second_view = view_coverage(&second, second_state).unwrap();
    assert_eq!(first_view.definition_id(), second_view.definition_id());
    assert_eq!(first_view.selection(), second_view.selection());
}

/// The same shape over another source computes something else: another
/// `definition_id`.
#[test]
fn a_view_coverage_over_another_source_has_another_definition_id() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, over_latency) = kll_of_last_minute_of(&mut memo, "latency");
    let (_, over_requests) = kll_of_last_minute_of(&mut memo, "requests");
    assert_ne!(
        view_coverage(&memo, over_latency).unwrap().definition_id(),
        view_coverage(&memo, over_requests).unwrap().definition_id()
    );
}

/// A group without aggregate coverage (a scan) has no view coverage.
#[test]
fn a_group_without_aggregate_coverage_has_no_view_coverage() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (scan, _) = kll_of_last_minute_of(&mut memo, "latency");
    assert_eq!(view_coverage(&memo, scan), None);
}
