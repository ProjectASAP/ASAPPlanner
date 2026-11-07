//! Group properties and summary coverage, derived once per memo group from
//! its input groups (`docs/design_docs/proposals/asap-primitive-schema.md`
//! §4.2.2). The memo of the worked example:
//!
//! ```text
//! G1  Scan latency
//! G2  Filter(job = "api")(G1);  G3 TimeRange[1m](G2);  G4 SummaryAgg[KLL](G3)
//! G5  Filter(job = "web")(G1);  G6 TimeRange[1m](G5);  G7 SummaryAgg[KLL](G6)
//! G8  Filter(job = "api" ∧ a + b > 10)(G1);  G9 TimeRange[1m](G8);  G10 SummaryAgg[KLL](G9)
//! ```
use std::ops::Bound;
use std::time::Duration;

use asap_types::ir::operator::operator_properties::{GroupKeys, Reduction, Source};
use asap_types::ir::properties::column_lineage::{ColumnLineage, SourceColumn};
use asap_types::ir::properties::summary_coverage::{Constraint, CoverageError, SummaryCoverage};
use asap_types::ir::scalar::expr_ir::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{
    DataType, Field, FieldDataType, GroupingStrategy, MergeInputRule, Schema, SketchAlgorithm,
    SketchStatistic, SummaryUpdate,
};
use asap_types::ir::{
    ASAPOp, ExprSemantics, NonASAPOp, Operator, OperatorResultKind, Predicate, ScalarExpr,
    SchemaDerivationError, TimeRangeKind,
};
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::test_support::definition_id;

const JOB: usize = 2;
const A: usize = 3;
const B: usize = 4;
const MINUTE_MS: i64 = 60_000;

/// `Scan latency{predicates}`: `[ts, value, job, a, b]`.
fn scan(predicates: Vec<ScalarExpr<MemoGroupId>>) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "latency".into(),
        },
        predicates: predicates.into_iter().map(Predicate).collect(),
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
                Field::plain("job", DataType::Utf8, false),
                Field::plain("a", DataType::Float64, false),
                Field::plain("b", DataType::Float64, false),
            ],
            0,
            vec![],
        ),
    })
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

fn filter(pred: ScalarExpr<MemoGroupId>, child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::Filter {
        pred: Predicate(pred),
        child,
    })
}

fn range(width_ms: i64, child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(width_ms as u64),
        kind: TimeRangeKind::Range,
        child,
    })
}

/// `SummaryAgg[KLL]` of the sample value per series.
fn kll(child: MemoGroupId) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        family: FieldDataType::Sketch(SketchAlgorithm::Kll, GroupingStrategy::default()),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        grouping: GroupingStrategy::default(),
        filter: None,
    })
}

fn merge(children: Vec<MemoGroupId>) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryMerge {
        children,
        newest_tumbling_windows: None,
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

fn coverage(memo: &Memo, group: MemoGroupId) -> &SummaryCoverage {
    memo.groups()[group.0 as usize]
        .group_properties
        .summary_coverage
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

fn last_minute() -> Option<(Bound<i64>, Bound<i64>)> {
    Some((Bound::Excluded(-MINUTE_MS), Bound::Included(0)))
}

/// A group's properties come from its operator and its input groups'
/// properties: the scan's schema, the state's kind and state column, the
/// readout's value column, and whether equal copies may be one group.
#[test]
fn group_properties_come_from_input_groups() {
    let mut memo = Memo::default();
    let g1 = memo.find_or_add_group(scan(vec![])).unwrap();
    let g4 = kll_of_last_minute_where(&mut memo, g1, job_is("api"));
    let readout = memo
        .find_or_add_group(Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: g4,
            query: SketchStatistic::Quantile { q: 0.99 },
        }))
        .unwrap();
    let limit = memo
        .find_or_add_group(Operator::NonASAP(NonASAPOp::Limit {
            n: Some(1),
            offset: 0,
            partition_by: GroupKeys::default(),
            child: g1,
        }))
        .unwrap();
    let properties = |group: MemoGroupId| &memo.groups()[group.0 as usize].group_properties;
    assert_eq!(properties(g1).schema.fields.len(), 5);
    assert_eq!(properties(g1).summary_coverage, None);
    assert_eq!(properties(g4).result_kind, OperatorResultKind::State);
    assert!(properties(g4)
        .schema
        .fields
        .iter()
        .any(|field| field.name == "state" && !field.is_plain()));
    assert!(properties(g4).summary_coverage.is_some());
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
    assert!(!properties(limit).shareable_when_equal);
}

/// G4: `job = "api"` and the minute are lifted; the definition is the KLL
/// over G1.
#[test]
fn a_summary_agg_over_a_job_filter_lifts_it_into_the_selection() {
    let mut memo = Memo::default();
    let g1 = memo.find_or_add_group(scan(vec![])).unwrap();
    let g4 = kll_of_last_minute_where(&mut memo, g1, job_is("api"));
    let coverage = coverage(&memo, g4);
    assert_eq!(coverage.definition().input_group, Some(g1));
    assert_eq!(
        coverage.definition().unlifted_operators,
        vec![kll_unlifted()]
    );
    let [selection] = coverage.selection() else {
        panic!("one box: {coverage:?}")
    };
    assert_eq!(
        selection.columns()[&SourceColumn::new("latency", "job")],
        jobs(&["api"])
    );
    assert_eq!(selection.relative_time(), last_minute());
}

/// G7 has G4's definition and id and a disjoint selection, so
/// `SummaryMerge(G4, G7)` is a valid group covering job ∈ {api, web}.
#[test]
fn states_with_equal_definitions_and_disjoint_selections_merge() {
    let mut memo = Memo::default();
    let g1 = memo.find_or_add_group(scan(vec![])).unwrap();
    let g4 = kll_of_last_minute_where(&mut memo, g1, job_is("api"));
    let g7 = kll_of_last_minute_where(&mut memo, g1, job_is("web"));
    assert_eq!(
        coverage(&memo, g4).definition(),
        coverage(&memo, g7).definition()
    );
    assert_eq!(
        definition_id(coverage(&memo, g4)),
        definition_id(coverage(&memo, g7))
    );
    let merged = memo.find_or_add_group(merge(vec![g4, g7])).unwrap();
    let coverage = coverage(&memo, merged);
    assert_eq!(coverage.definition().input_group, Some(g1));
    let [selection] = coverage.selection() else {
        panic!("one box: {coverage:?}")
    };
    assert_eq!(
        selection.columns()[&SourceColumn::new("latency", "job")],
        jobs(&["api", "web"])
    );
}

/// Two states that select the same rows do not merge: KLL counts a row
/// once per input.
#[test]
fn states_with_overlapping_selections_do_not_merge() {
    let mut memo = Memo::default();
    let g1 = memo.find_or_add_group(scan(vec![])).unwrap();
    let g4 = kll_of_last_minute_where(&mut memo, g1, job_is("api"));
    let groups = memo.groups().len();
    assert!(matches!(
        memo.find_or_add_group(merge(vec![g4, g4])),
        Err(SchemaDerivationError::Coverage(
            CoverageError::InputsOverlap {
                required: MergeInputRule::Disjoint
            }
        ))
    ));
    assert_eq!(memo.groups().len(), groups);
}

/// G10: `a + b > 10` spans two columns, so it stays as a `Filter` in the
/// definition; G10 does not merge with G4 although both select job = api.
#[test]
fn a_filter_that_is_no_selection_stays_unlifted() {
    let mut memo = Memo::default();
    let g1 = memo.find_or_add_group(scan(vec![])).unwrap();
    let g4 = kll_of_last_minute_where(&mut memo, g1, job_is("api"));
    let g10 = kll_of_last_minute_where(
        &mut memo,
        g1,
        ScalarExpr::BoolAnd(vec![job_is("api"), a_plus_b_above_10()]),
    );
    let coverage_of_g10 = coverage(&memo, g10);
    assert_eq!(coverage_of_g10.definition().input_group, Some(g1));
    assert_eq!(
        coverage_of_g10.definition().unlifted_operators,
        vec![
            kll_unlifted(),
            filter(a_plus_b_above_10(), MemoGroupId(0)).map_children(|_| None),
        ]
    );
    assert_eq!(
        coverage_of_g10.selection()[0].columns()[&SourceColumn::new("latency", "job")],
        jobs(&["api"])
    );
    assert!(matches!(
        memo.find_or_add_group(merge(vec![g4, g10])),
        Err(SchemaDerivationError::Coverage(
            CoverageError::DefinitionMismatch
        ))
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
    let mut memo = Memo::default();
    let g1 = memo.find_or_add_group(scan(vec![])).unwrap();
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
    assert_eq!(definition.input_group, Some(g1));
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
        coverage(&memo, merged).selection()[0].columns()[&SourceColumn::new("latency", "job")],
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
    let mut memo = Memo::default();
    let state_of_job = |memo: &mut Memo, job: &str| {
        let series = memo.find_or_add_group(scan(vec![job_is(job)])).unwrap();
        assert_eq!(
            memo.groups()[series.0 as usize]
                .group_properties
                .column_lineage[JOB],
            ColumnLineage::SourceColumn(SourceColumn::new("latency", "job"))
        );
        let last_minute = memo.find_or_add_group(range(MINUTE_MS, series)).unwrap();
        memo.find_or_add_group(kll(last_minute)).unwrap()
    };
    let api = state_of_job(&mut memo, "api");
    let web = state_of_job(&mut memo, "web");
    let definition = coverage(&memo, api).definition();
    assert_eq!(definition.input_group, None);
    assert_eq!(
        definition.unlifted_operators,
        vec![kll_unlifted(), scan(vec![]).map_children(|_| None)]
    );
    assert_eq!(
        coverage(&memo, api).selection()[0].columns()[&SourceColumn::new("latency", "job")],
        jobs(&["api"])
    );
    let merged = memo.find_or_add_group(merge(vec![api, web])).unwrap();
    assert_eq!(
        coverage(&memo, merged).selection()[0].columns()[&SourceColumn::new("latency", "job")],
        jobs(&["api", "web"])
    );
}

/// A tumbling-window chain of five minutes covers what one build over the
/// five-minute range does: the same definition and selection.
#[test]
fn a_tumbling_window_chain_covers_its_whole_window() {
    let mut memo = Memo::default();
    let g1 = memo.find_or_add_group(scan(vec![])).unwrap();
    let chain = memo
        .find_or_add_group(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: g1,
            family: FieldDataType::Sketch(SketchAlgorithm::Kll, GroupingStrategy::default()),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            grouping: GroupingStrategy::default(),
            filter: None,
            tumbling_window_width_ms: MINUTE_MS as u64,
            tumbling_window_count: 5,
        }))
        .unwrap();
    let five_minutes = memo.find_or_add_group(range(5 * MINUTE_MS, g1)).unwrap();
    let direct = memo.find_or_add_group(kll(five_minutes)).unwrap();
    assert_eq!(coverage(&memo, chain), coverage(&memo, direct));
    assert_eq!(
        coverage(&memo, chain).selection()[0].relative_time(),
        Some((Bound::Excluded(-5 * MINUTE_MS), Bound::Included(0)))
    );
}
