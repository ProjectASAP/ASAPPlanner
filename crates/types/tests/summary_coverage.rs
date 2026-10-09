//! Summary coverage is `definition + selection`, derived from the sub-DAG
//! (`docs/design_docs/proposals/asap-primitive-schema.md` §4.2). Merges need
//! the same definition and disjoint selections.
use std::ops::Bound;
use std::rc::Rc;
use std::time::Duration;

use asap_types::ir::operator_properties::{Reduction, Source};
use asap_types::ir::summary_coverage::{ColumnIdentity, Constraint, CoverageError, SelectionBox};
use asap_types::ir::{
    ASAPOp, ExprSemantics, NonASAPOp, Operator, OperatorNode, Predicate, ProjectItem, ScalarExpr,
    SchemaDerivationError, TimeRangeKind,
};
use asap_types::post_asap::{
    ExecutionTiming, GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryUpdate,
};
use asap_types::pre_asap::expr_ir::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use asap_types::pre_asap::{ColumnRef, DataType, Field, FieldDataType, Schema, TimeShift};

const JOB: usize = 0;
const REGION: usize = 1;
const LATENCY: usize = 2;

fn table() -> Rc<OperatorNode> {
    table_with(vec![
        Field::plain("job", DataType::Utf8, false),
        Field::plain("region", DataType::Utf8, false),
        Field::plain("latency", DataType::Float64, false),
        Field::plain("size", DataType::Float64, false),
    ])
}

fn table_with(fields: Vec<Field>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "t".into(),
        },
        predicates: vec![],
        schema: Schema::new(fields),
    }))
    .unwrap()
}

/// PromQL leaf `m{predicates}` with columns `[ts, value, job]`.
fn series(predicates: Vec<Predicate>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "m".into() },
        predicates,
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
                Field::plain("job", DataType::Utf8, true),
            ],
            0,
            vec![],
        ),
    }))
    .unwrap()
}

fn compare(column: usize, op: CompareOpKind, value: ScalarValue) -> ScalarExpr {
    ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(column)),
        op,
        right: Box::new(ScalarExpr::Literal(value)),
        semantics: ExprSemantics::Sql,
    }
}

fn eq(column: usize, value: &str) -> ScalarExpr {
    compare(column, CompareOpKind::Eq, ScalarValue::Utf8(value.into()))
}

fn filter(child: Rc<OperatorNode>, pred: ScalarExpr) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
        pred: Predicate(pred),
        child,
    }))
    .unwrap()
}

fn kll(child: Rc<OperatorNode>, column: &str) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        family: FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
            Default::default(),
        ),
        input: SummaryUpdate::column(ColumnRef::Named(column.into())),
        reduction: Reduction::by(vec![JOB]),
        grouping: GroupingStrategy::default(),
        filter: None,
    }))
    .unwrap()
}

/// KLL of latency by job over `t` restricted by `pred`.
fn latency_where(pred: ScalarExpr) -> Rc<OperatorNode> {
    kll(filter(table(), pred), "latency")
}

/// Tumbling pane: `TimeRange(1m)` over `TimeShift(shift)` over `m`.
fn pane(shift_ms: i64, predicates: Vec<Predicate>) -> Rc<OperatorNode> {
    let shifted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
        shift: TimeShift {
            offset_ms: shift_ms,
            at: None,
        },
        child: series(predicates),
    }))
    .unwrap();
    let range = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(60_000),
        kind: TimeRangeKind::Range,
        child: shifted,
    }))
    .unwrap();
    kll(range, "value")
}

fn merge(children: Vec<Rc<OperatorNode>>) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge { children }))
}

fn rejected(children: Vec<Rc<OperatorNode>>) -> CoverageError {
    match merge(children) {
        Err(SchemaDerivationError::Coverage(error)) => error,
        other => panic!("expected a coverage error, got {other:?}"),
    }
}

fn column(name: &str) -> ColumnIdentity {
    ColumnIdentity {
        table: None,
        name: name.into(),
    }
}

fn utf8(values: &[&str]) -> Constraint {
    Constraint::In(
        values
            .iter()
            .map(|v| ScalarValue::Utf8((*v).into()))
            .collect(),
    )
}

fn relative(from_ms: i64, to_ms: i64) -> Option<(Bound<i64>, Bound<i64>)> {
    Some((Bound::Excluded(from_ms), Bound::Included(to_ms)))
}

/// A lifted filter leaves the definition over the bare scan.
#[test]
fn filters_lift_into_the_selection() {
    let scan = table();
    let us = kll(filter(Rc::clone(&scan), eq(REGION, "us")), "latency");
    let coverage = us.coverage().unwrap();
    assert!(Rc::ptr_eq(coverage.definition.children()[0], &scan));
    assert_eq!(
        coverage.selection,
        vec![SelectionBox {
            columns: [(column("region"), utf8(&["us"]))].into(),
            relative_time: None,
        }]
    );
}

/// Different populations merge into one box; the same population twice
/// would count every row twice.
#[test]
fn populations_merge_when_disjoint() {
    let merged = merge(vec![
        latency_where(eq(REGION, "us")),
        latency_where(eq(REGION, "eu")),
    ])
    .unwrap();
    assert_eq!(
        merged.coverage().unwrap().selection[0].columns[&column("region")],
        utf8(&["us", "eu"])
    );
    assert_eq!(
        rejected(vec![
            latency_where(eq(REGION, "us")),
            latency_where(eq(REGION, "us")),
        ]),
        CoverageError::PossibleOverlap
    );
}

/// Equal schemas do not make a KLL over latency and one over size mergeable.
#[test]
fn different_inputs_do_not_merge() {
    assert_eq!(
        rejected(vec![
            kll(filter(table(), eq(REGION, "us")), "latency"),
            kll(filter(table(), eq(REGION, "eu")), "size"),
        ]),
        CoverageError::DefinitionMismatch
    );
}

/// The same update over scans of different columns is a different computation.
#[test]
fn the_same_update_over_different_scan_schemas_does_not_merge() {
    let scan = |measure: &str| {
        table_with(vec![
            Field::plain("job", DataType::Utf8, false),
            Field::plain("region", DataType::Utf8, false),
            Field::plain(measure, DataType::Float64, false),
            Field::plain("value", DataType::Float64, false),
        ])
    };
    assert_eq!(
        rejected(vec![
            kll(filter(scan("latency"), eq(REGION, "us")), "value"),
            kll(filter(scan("size"), eq(REGION, "eu")), "value"),
        ]),
        CoverageError::DefinitionMismatch
    );
}

/// `shipping.region` and `billing.region` are different columns, so
/// restricting each says nothing about overlap.
#[test]
fn qualified_columns_stay_distinct() {
    let scan = || {
        let qualified = |table: &str| Field {
            table: Some(table.into()),
            ..Field::plain("region", DataType::Utf8, false)
        };
        table_with(vec![
            Field::plain("job", DataType::Utf8, false),
            qualified("shipping"),
            qualified("billing"),
            Field::plain("latency", DataType::Float64, false),
        ])
    };
    assert_eq!(
        rejected(vec![
            kll(filter(scan(), eq(1, "us")), "latency"),
            kll(filter(scan(), eq(2, "eu")), "latency"),
        ]),
        CoverageError::PossibleOverlap
    );
}

/// Panes take their time from `TimeRange` over `TimeShift`: adjacent panes
/// join, a gap stays two boxes, and the same pane twice overlaps.
#[test]
fn time_panes_merge_and_keep_gaps() {
    let first = pane(0, vec![]);
    assert_eq!(
        first.coverage().unwrap().selection[0].relative_time,
        relative(-60_000, 0)
    );
    assert!(Rc::ptr_eq(
        first.coverage().unwrap().definition.children()[0],
        first.children()[0].children()[0].children()[0]
    ));
    let joined = merge(vec![first.clone(), pane(60_000, vec![])]).unwrap();
    assert_eq!(
        joined.coverage().unwrap().selection,
        vec![SelectionBox {
            columns: Default::default(),
            relative_time: relative(-120_000, 0),
        }]
    );
    let gapped = merge(vec![first.clone(), pane(120_000, vec![])]).unwrap();
    assert_eq!(gapped.coverage().unwrap().selection.len(), 2);
    assert_eq!(
        rejected(vec![first.clone(), first]),
        CoverageError::PossibleOverlap
    );
}

/// Label matchers on the scan lift like any filter.
#[test]
fn scan_predicates_lift() {
    let api = pane(0, vec![Predicate(eq(2, "api"))]);
    let coverage = api.coverage().unwrap();
    assert_eq!(
        coverage.selection[0].columns[&column("job")],
        utf8(&["api"])
    );
    let scan = coverage.definition.children()[0];
    assert!(matches!(
        scan.non_asap(),
        Some(NonASAPOp::Scan { predicates, .. }) if predicates.is_empty()
    ));
    merge(vec![api, pane(0, vec![Predicate(eq(2, "web"))])]).unwrap();
}

/// Value ranges are selections: `latency < 100` and `latency >= 100` are
/// disjoint, `latency <= 100` overlaps `latency >= 100`.
#[test]
fn value_ranges_merge_when_disjoint() {
    let latency = |op| latency_where(compare(LATENCY, op, ScalarValue::Float64(100.0)));
    merge(vec![latency(CompareOpKind::Lt), latency(CompareOpKind::Ge)]).unwrap();
    assert_eq!(
        rejected(vec![latency(CompareOpKind::Le), latency(CompareOpKind::Ge)]),
        CoverageError::PossibleOverlap
    );
}

/// A predicate that is not a value set or interval on one column stays in
/// the definition, so states with different residuals do not merge.
#[test]
fn residual_predicates_stay_in_the_definition() {
    let doubled_above = |threshold: f64, region: &str| {
        let doubled = ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Mul,
            left: Box::new(ScalarExpr::Column(LATENCY)),
            right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(2.0))),
            semantics: ExprSemantics::Sql,
        };
        latency_where(ScalarExpr::BoolAnd(vec![
            ScalarExpr::Compare {
                left: Box::new(doubled),
                op: CompareOpKind::Gt,
                right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(threshold))),
                semantics: ExprSemantics::Sql,
            },
            eq(REGION, region),
        ]))
    };
    let state = doubled_above(10.0, "us");
    let definition = &state.coverage().unwrap().definition;
    assert!(matches!(
        definition.children()[0].non_asap(),
        Some(NonASAPOp::Filter { .. })
    ));
    merge(vec![state.clone(), doubled_above(10.0, "eu")]).unwrap();
    assert_eq!(
        rejected(vec![state, doubled_above(20.0, "eu")]),
        CoverageError::DefinitionMismatch
    );
}

/// An instant selector picks the latest sample per series, which is not a
/// selection of rows, so it stays in the definition.
#[test]
fn instant_selectors_stay_in_the_definition() {
    let instant = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(300_000),
        kind: TimeRangeKind::Instant,
        child: series(vec![]),
    }))
    .unwrap();
    let state = kll(Rc::clone(&instant), "value");
    let coverage = state.coverage().unwrap();
    assert_eq!(coverage.selection[0].relative_time, None);
    assert!(Rc::ptr_eq(coverage.definition.children()[0], &instant));
}

/// A merge is a state with coverage, so merges nest.
#[test]
fn merges_nest() {
    let us_eu = merge(vec![
        latency_where(eq(REGION, "us")),
        latency_where(eq(REGION, "eu")),
    ])
    .unwrap();
    merge(vec![us_eu.clone(), latency_where(eq(REGION, "apac"))]).unwrap();
    assert_eq!(
        rejected(vec![us_eu, latency_where(eq(REGION, "eu"))]),
        CoverageError::PossibleOverlap
    );
}

/// When a state is computed is not what it computes.
#[test]
fn timing_does_not_block_merges() {
    let ingested = Rc::new(
        (*pane(60_000, vec![]))
            .clone()
            .with_timing(Some(ExecutionTiming::IngestionTime)),
    );
    let queried = Rc::new(
        (*pane(0, vec![]))
            .clone()
            .with_timing(Some(ExecutionTiming::QueryTime)),
    );
    merge(vec![ingested, queried]).unwrap();
}

/// A merge built around `new` is still checked by `validate_structure`.
#[test]
fn forged_merges_fail_validation() {
    let us = latency_where(eq(REGION, "us"));
    let schema = us.schema.clone();
    let forged = Rc::new(OperatorNode::with_schema(
        Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![us.clone(), us],
        }),
        schema,
    ));
    assert!(forged.coverage().is_none());
    assert!(matches!(
        forged.validate_structure(),
        Err(SchemaDerivationError::Coverage(
            CoverageError::PossibleOverlap
        ))
    ));
}

/// Only summary states have coverage.
#[test]
fn non_summary_nodes_have_no_coverage() {
    assert!(table().coverage().is_none());
    assert!(filter(table(), eq(REGION, "us")).coverage().is_none());
}

/// Two output columns with the same `(table, name)` cannot be told apart in
/// a selection, so restrictions on them stay in the definition.
#[test]
fn ambiguous_column_names_do_not_lift() {
    let renamed = |restricted: usize, value: &str| {
        let item = |column: usize, alias: Option<&str>| ProjectItem {
            alias: alias.map(Into::into),
            expr: ScalarExpr::Column(column),
        };
        let project = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
            cols: vec![
                item(JOB, None),
                item(REGION, Some("k")),
                item(LATENCY, Some("k")),
                item(LATENCY, None),
            ],
            qualifier: None,
            child: table(),
        }))
        .unwrap();
        kll(filter(project, eq(restricted, value)), "latency")
    };
    assert_eq!(
        rejected(vec![renamed(1, "us"), renamed(2, "eu")]),
        CoverageError::DefinitionMismatch
    );
}

/// `latency = 1` and `latency = 1.0` select the same rows of a Float64 column.
#[test]
fn literals_of_different_types_are_not_proven_different() {
    let one = |value| latency_where(compare(LATENCY, CompareOpKind::Eq, value));
    assert!(merge(vec![
        one(ScalarValue::Int64(1)),
        one(ScalarValue::Float64(1.0))
    ])
    .is_err());
}

/// A scan predicate whose conjuncts only partly lift keeps just the rest.
#[test]
fn partly_lifted_scan_predicates_keep_only_the_rest() {
    let doubled_positive = ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Mul,
            left: Box::new(ScalarExpr::Column(1)),
            right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(2.0))),
            semantics: ExprSemantics::Promql,
        }),
        op: CompareOpKind::Gt,
        right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(0.0))),
        semantics: ExprSemantics::Promql,
    };
    let job = |name: &str| {
        pane(
            0,
            vec![Predicate(ScalarExpr::BoolAnd(vec![
                eq(2, name),
                doubled_positive.clone(),
            ]))],
        )
    };
    merge(vec![job("api"), job("web")]).unwrap();
}

/// The worked example of the design doc (§4.2.2): conditions on the
/// `SummaryAgg` filter, through a renaming `Project`, and the time window
/// move into the selection; the condition on an expression stays.
#[test]
fn design_doc_worked_example() {
    let (value, job, region) = (1, 2, 3);
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries { metric: "m".into() },
        predicates: vec![],
        schema: Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
                Field::plain("job", DataType::Utf8, true),
                Field::plain("region", DataType::Utf8, true),
            ],
            0,
            vec![],
        ),
    }))
    .unwrap();
    let shifted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
        shift: TimeShift {
            offset_ms: 120_000,
            at: None,
        },
        child: Rc::clone(&scan),
    }))
    .unwrap();
    let range = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(60_000),
        kind: TimeRangeKind::Range,
        child: shifted,
    }))
    .unwrap();
    let doubled_above_10 = ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Mul,
            left: Box::new(ScalarExpr::Column(value)),
            right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(2.0))),
            semantics: ExprSemantics::Sql,
        }),
        op: CompareOpKind::Gt,
        right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(10.0))),
        semantics: ExprSemantics::Sql,
    };
    let filtered = filter(
        range,
        ScalarExpr::BoolAnd(vec![eq(region, "us"), doubled_above_10.clone()]),
    );
    let item = |column: usize, alias: Option<&str>| ProjectItem {
        alias: alias.map(Into::into),
        expr: ScalarExpr::Column(column),
    };
    let project = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
        cols: vec![item(job, None), item(region, Some("r")), item(value, None)],
        qualifier: None,
        child: filtered,
    }))
    .unwrap();
    let below_100 = compare(2, CompareOpKind::Lt, ScalarValue::Float64(100.0));
    let state = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: project,
        family: FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
            Default::default(),
        ),
        input: SummaryUpdate::column(ColumnRef::Named("value".into())),
        reduction: Reduction::by(vec![0]),
        grouping: GroupingStrategy::default(),
        filter: Some(Predicate(below_100)),
    }))
    .unwrap();

    let coverage = state.coverage().unwrap();
    assert_eq!(
        coverage.selection,
        vec![SelectionBox {
            columns: [
                (column("r"), utf8(&["us"])),
                (
                    column("value"),
                    Constraint::Interval {
                        lower: Bound::Unbounded,
                        upper: Bound::Excluded(ScalarValue::Float64(100.0)),
                    },
                ),
            ]
            .into(),
            relative_time: relative(-180_000, -120_000),
        }]
    );
    // definition: SummaryAgg (no filter) over Project over
    // Filter(value * 2 > 10) over the bare Scan.
    let Some(ASAPOp::SummaryAgg { filter, child, .. }) = coverage.definition.asap() else {
        panic!("definition is a SummaryAgg");
    };
    assert!(filter.is_none());
    assert!(matches!(child.non_asap(), Some(NonASAPOp::Project { .. })));
    let kept = child.children()[0];
    assert!(matches!(
        kept.non_asap(),
        Some(NonASAPOp::Filter { pred, .. }) if pred.0 == doubled_above_10
    ));
    assert!(Rc::ptr_eq(kept.children()[0], &scan));
}
