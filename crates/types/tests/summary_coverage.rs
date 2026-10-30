//! Summary coverage is `definition + selection`, derived from the sub-DAG
//! (`docs/design_docs/proposals/asap-primitive-schema.md` §4.2). Merges need
//! the same definition and disjoint selections.
use std::ops::Bound;
use std::rc::Rc;
use std::time::Duration;

use asap_types::ir::operator::operator_properties::{Reduction, Source};
use asap_types::ir::operator::TimeShift;
use asap_types::ir::properties::summary_coverage::{
    ColumnIdentity, Constraint, CoverageError, SelectionBox,
};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::scalar::expr_ir::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::schema::{
    ExactKind, ExactParams, GroupingStrategy, MergeInputRule, SketchAlgorithm, SketchKind,
    SketchParams, SummaryUpdate,
};
use asap_types::ir::{
    ASAPOp, ExprSemantics, NonASAPOp, Operator, OperatorNode, OperatorResultKind, Predicate,
    ProjectItem, ScalarExpr, SchemaDerivationError, TimeRangeKind,
};
use asap_types::test_support::definition_id;

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
    summary(
        child,
        column,
        FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll {
                    k: 200,
                    failure_probability: None,
                },
            ),
            Default::default(),
        ),
    )
}

fn summary(child: Rc<OperatorNode>, column: &str, family: FieldDataType) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        family,
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
    kll(pane_rows(shift_ms, predicates), "value")
}

/// Exact counter-increase pane over the same rows as [`pane`].
fn increase_pane(shift_ms: i64, predicates: Vec<Predicate>) -> Rc<OperatorNode> {
    summary(
        pane_rows(shift_ms, predicates),
        "value",
        FieldDataType::ExactAggregate(ExactKind::Increase, ExactParams::Increase),
    )
}

/// KLL over `TimeRange(width_ms)` over `TimeShift(shift_ms)` over `m`: the
/// window `(-(shift + width), -shift]`.
fn window(shift_ms: i64, width_ms: u64) -> Rc<OperatorNode> {
    let shifted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
        shift: TimeShift {
            offset_ms: shift_ms,
            at: None,
        },
        child: series(vec![]),
    }))
    .unwrap();
    let range = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(width_ms),
        kind: TimeRangeKind::Range,
        child: shifted,
    }))
    .unwrap();
    kll(range, "value")
}

fn pane_rows(shift_ms: i64, predicates: Vec<Predicate>) -> Rc<OperatorNode> {
    let shifted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
        shift: TimeShift {
            offset_ms: shift_ms,
            at: None,
        },
        child: series(predicates),
    }))
    .unwrap();
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(60_000),
        kind: TimeRangeKind::Range,
        child: shifted,
    }))
    .unwrap()
}

fn merge(children: Vec<Rc<OperatorNode>>) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children,
        newest_panes: None,
        group_by: None,
    }))
}

fn rejected(children: Vec<Rc<OperatorNode>>) -> CoverageError {
    match merge(children) {
        Err(SchemaDerivationError::Coverage(error)) => error,
        other => panic!("expected a coverage error, got {other:?}"),
    }
}

fn column(name: &str) -> ColumnIdentity {
    ColumnIdentity::new(None, name)
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
    assert!(Rc::ptr_eq(coverage.definition().children()[0], &scan));
    assert_eq!(
        coverage.selection(),
        vec![SelectionBox::new(
            [(column("region"), utf8(&["us"]))].into(),
            None
        )]
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
        merged.coverage().unwrap().selection()[0].columns()[&column("region")],
        utf8(&["us", "eu"])
    );
    assert_eq!(
        rejected(vec![
            latency_where(eq(REGION, "us")),
            latency_where(eq(REGION, "us")),
        ]),
        CoverageError::InputsOverlap {
            required: MergeInputRule::Disjoint,
        }
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
        CoverageError::InputsOverlap {
            required: MergeInputRule::Disjoint,
        }
    );
}

/// Panes take their time from `TimeRange` over `TimeShift`: adjacent panes
/// join, a gap stays two boxes, and the same pane twice overlaps.
#[test]
fn time_panes_merge_and_keep_gaps() {
    let first = pane(0, vec![]);
    assert_eq!(
        first.coverage().unwrap().selection()[0].relative_time(),
        relative(-60_000, 0)
    );
    assert!(Rc::ptr_eq(
        first.coverage().unwrap().definition().children()[0],
        first.children()[0].children()[0].children()[0]
    ));
    let joined = merge(vec![first.clone(), pane(60_000, vec![])]).unwrap();
    assert_eq!(
        joined.coverage().unwrap().selection(),
        vec![SelectionBox::new(Default::default(), relative(-120_000, 0))]
    );
    let gapped = merge(vec![first.clone(), pane(120_000, vec![])]).unwrap();
    assert_eq!(gapped.coverage().unwrap().selection().len(), 2);
    assert_eq!(
        rejected(vec![first.clone(), first]),
        CoverageError::InputsOverlap {
            required: MergeInputRule::Disjoint,
        }
    );
}

/// Counter panes merge only when their time windows are disjoint: states
/// disjoint by a label but over the same window would interleave in time.
#[test]
fn counter_panes_merge_only_disjoint_in_time() {
    let joined = merge(vec![
        increase_pane(0, vec![]),
        increase_pane(60_000, vec![]),
    ])
    .unwrap();
    assert_eq!(
        joined.coverage().unwrap().selection()[0].relative_time(),
        relative(-120_000, 0)
    );
    let by_job = |job| increase_pane(0, vec![Predicate(eq(2, job))]);
    assert_eq!(
        rejected(vec![by_job("api"), by_job("web")]),
        CoverageError::InputsOverlap {
            required: MergeInputRule::DisjointInTime,
        }
    );
}

/// A day of one-minute panes (1,440 inputs) merges into one window, in
/// either order: the inputs' windows are sorted and only neighbours are
/// compared, so the check is O(n log n), not all pairs. A pane listed
/// twice still overlaps.
#[test]
fn a_day_of_minute_panes_merges_into_one_window() {
    const PANES: i64 = 1_440;
    let day = relative(-PANES * 60_000, 0);
    let newest_first: Vec<_> = (0..PANES).map(|i| pane(i * 60_000, vec![])).collect();
    let oldest_first: Vec<_> = newest_first.iter().rev().cloned().collect();
    for panes in [newest_first.clone(), oldest_first] {
        let merged = merge(panes).unwrap();
        merged.validate_structure().unwrap();
        let coverage = merged.coverage().unwrap();
        let selection = coverage.selection();
        assert_eq!(selection.len(), 1);
        assert_eq!(selection[0].relative_time(), day);
    }
    let counters: Vec<_> = (0..PANES)
        .map(|i| increase_pane(i * 60_000, vec![]))
        .collect();
    assert_eq!(
        merge(counters).unwrap().coverage().unwrap().selection()[0].relative_time(),
        day
    );
    let mut repeated = newest_first;
    repeated.push(pane(30 * 60_000, vec![]));
    assert_eq!(
        rejected(repeated),
        CoverageError::InputsOverlap {
            required: MergeInputRule::Disjoint,
        }
    );
}

/// A long window that spans two short ones overlaps both, though the
/// short ones are apart: sorted by start, the long window's neighbour
/// overlaps it, so the all-pairs check runs and rejects the merge. The
/// short windows alone merge as two boxes.
#[test]
fn a_long_window_spanning_others_is_rejected() {
    let long = window(0, 100_000);
    let (near, far) = (window(10_000, 10_000), window(50_000, 10_000));
    assert_eq!(
        rejected(vec![long.clone(), near.clone(), far.clone()]),
        CoverageError::InputsOverlap {
            required: MergeInputRule::Disjoint,
        }
    );
    assert_eq!(
        rejected(vec![near.clone(), far.clone(), long]),
        CoverageError::InputsOverlap {
            required: MergeInputRule::Disjoint,
        }
    );
    let apart = merge(vec![far, near]).unwrap();
    assert_eq!(
        apart
            .coverage()
            .unwrap()
            .selection()
            .iter()
            .map(|b| b.relative_time())
            .collect::<Vec<_>>(),
        [relative(-60_000, -50_000), relative(-20_000, -10_000)]
    );
}

/// Label matchers on the scan lift like any filter.
#[test]
fn scan_predicates_lift() {
    let api = pane(0, vec![Predicate(eq(2, "api"))]);
    let coverage = api.coverage().unwrap();
    assert_eq!(
        coverage.selection()[0].columns()[&column("job")],
        utf8(&["api"])
    );
    let scan = coverage.definition().children()[0];
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
        CoverageError::InputsOverlap {
            required: MergeInputRule::Disjoint,
        }
    );
}

/// Touching value ranges join like touching time windows; the joined range
/// stays explicit, since neither input took NULL rows. A gap stays apart.
#[test]
fn touching_value_ranges_join() {
    let latency = |op, v| latency_where(compare(LATENCY, op, ScalarValue::Float64(v)));
    let joined = merge(vec![
        latency(CompareOpKind::Lt, 100.0),
        latency(CompareOpKind::Ge, 100.0),
    ])
    .unwrap();
    assert_eq!(
        joined.coverage().unwrap().selection(),
        vec![SelectionBox::new(
            [(
                column("latency"),
                Constraint::Interval {
                    lower: Bound::Unbounded,
                    upper: Bound::Unbounded,
                },
            )]
            .into(),
            None
        )]
    );
    let gap = merge(vec![
        latency(CompareOpKind::Lt, 100.0),
        latency(CompareOpKind::Gt, 100.0),
    ])
    .unwrap();
    assert_eq!(gap.coverage().unwrap().selection().len(), 2);
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
    let coverage = state.coverage().unwrap();
    let definition = coverage.definition();
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
    assert_eq!(coverage.selection()[0].relative_time(), None);
    assert!(Rc::ptr_eq(coverage.definition().children()[0], &instant));
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
        CoverageError::InputsOverlap {
            required: MergeInputRule::Disjoint,
        }
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
            newest_panes: None,
            group_by: None,
        }),
        schema,
    ));
    assert!(forged.coverage().is_none());
    assert!(matches!(
        forged.validate_structure(),
        Err(SchemaDerivationError::Coverage(
            CoverageError::InputsOverlap {
                required: MergeInputRule::Disjoint,
            }
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

/// A rename outside a `Project` stays in the definition: here B's `Filter`
/// swaps the names of `region` and `tier`, so B's `tier = 'eu'` must not
/// read as `region = 'eu'` and look disjoint from A's `region = 'us'`.
#[test]
fn renaming_filters_stay_in_the_definition() {
    let scan = || {
        table_with(vec![
            Field::plain("job", DataType::Utf8, false),
            Field::plain("region", DataType::Utf8, false),
            Field::plain("tier", DataType::Utf8, false),
            Field::plain("latency", DataType::Float64, false),
        ])
    };
    let us = kll(filter(scan(), eq(1, "us")), "latency");
    let swapped = filter(scan(), eq(2, "eu"));
    let mut schema = swapped.schema.clone();
    schema.fields[1].name = "tier".into();
    schema.fields[2].name = "region".into();
    let swapped = OperatorNode::with_schema(swapped.operator.clone(), schema);
    let eu_tier = kll(Rc::new(swapped), "latency");
    assert_eq!(
        rejected(vec![us, eu_tier]),
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

/// The worked example of the design doc (§4.2.2), as the SQL frontend
/// lowers it: the `FILTER` condition and `region = 'us'` (in
/// `Scan.predicates`, through a renaming `Project`) move into the
/// selection; the condition on an expression stays.
#[test]
fn design_doc_worked_example() {
    let doubled_above_10 = ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Mul,
            left: Box::new(ScalarExpr::Column(LATENCY)),
            right: Box::new(ScalarExpr::Literal(ScalarValue::Int64(2))),
            semantics: ExprSemantics::Sql,
        }),
        op: CompareOpKind::Gt,
        right: Box::new(ScalarExpr::Literal(ScalarValue::Int64(10))),
        semantics: ExprSemantics::Sql,
    };
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "t".into(),
        },
        predicates: vec![Predicate(ScalarExpr::BoolAnd(vec![
            eq(REGION, "us"),
            doubled_above_10.clone(),
        ]))],
        schema: Schema::new(vec![
            Field::plain("job", DataType::Utf8, false),
            Field::plain("region", DataType::Utf8, false),
            Field::plain("latency", DataType::Float64, false),
        ]),
    }))
    .unwrap();
    let item = |column: usize, alias: Option<&str>| ProjectItem {
        alias: alias.map(Into::into),
        expr: ScalarExpr::Column(column),
    };
    let project = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
        cols: vec![
            item(JOB, None),
            item(REGION, Some("r")),
            item(LATENCY, None),
        ],
        qualifier: None,
        child: scan,
    }))
    .unwrap();
    let below_100 = compare(2, CompareOpKind::Lt, ScalarValue::Int64(100));
    let state = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: project,
        family: FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll {
                    k: 200,
                    failure_probability: None,
                },
            ),
            Default::default(),
        ),
        input: SummaryUpdate::column(ColumnRef::Named("latency".into())),
        reduction: Reduction::by(vec![0]),
        grouping: GroupingStrategy::default(),
        filter: Some(Predicate(below_100)),
    }))
    .unwrap();

    let coverage = state.coverage().unwrap();
    assert_eq!(
        coverage.selection(),
        vec![SelectionBox::new(
            [
                (column("r"), utf8(&["us"])),
                (
                    column("latency"),
                    Constraint::Interval {
                        lower: Bound::Unbounded,
                        upper: Bound::Excluded(ScalarValue::Int64(100)),
                    },
                ),
            ]
            .into(),
            None
        )]
    );
    // definition: SummaryAgg (no filter) over Project over
    // Scan t with only `latency * 2 > 10` left in its predicates.
    let Some(ASAPOp::SummaryAgg { filter, child, .. }) = coverage.definition().asap() else {
        panic!("definition is a SummaryAgg");
    };
    assert!(filter.is_none());
    assert!(matches!(child.non_asap(), Some(NonASAPOp::Project { .. })));
    assert!(matches!(
        child.children()[0].non_asap(),
        Some(NonASAPOp::Scan { predicates, .. }) if *predicates == vec![Predicate(doubled_above_10)]
    ));
}

/// The coverage of `node`, which must have one.
fn coverage_of(
    node: &OperatorNode,
) -> asap_types::ir::properties::summary_coverage::SummaryCoverage {
    node.coverage().expect("a summary state")
}

/// Equal definitions get equal ids whatever the node, its selection, timing
/// or guarantee; a different computation (another metric, another column,
/// another sketch) gets another id.
#[test]
fn equal_definitions_get_equal_ids() {
    let id = |node: &OperatorNode| definition_id(&coverage_of(node));
    let us = latency_where(eq(REGION, "us"));
    let eu = latency_where(eq(REGION, "eu"));
    assert_eq!(id(&us), id(&eu));
    let ingested = (*pane(0, vec![]))
        .clone()
        .with_timing(Some(ExecutionTiming::IngestionTime));
    assert_eq!(id(&ingested), id(&pane(60_000, vec![])));
    assert_eq!(id(&ingested).len(), 32);
    let size = kll(filter(table(), eq(REGION, "eu")), "size");
    assert_ne!(id(&size), id(&eu));
    assert_ne!(id(&pane(0, vec![])), id(&increase_pane(0, vec![])));
    assert_ne!(id(&pane(0, vec![])), id(&us));
}

/// Six 10-minute panes at offsets 0 to 50 minutes share one definition id,
/// each with its own time selection `(-(i + 1) · 10m, -i · 10m]`; their
/// merge has the same id and the hour as its selection.
#[test]
fn panes_of_an_hour_share_one_definition_with_distinct_times() {
    let panes: Vec<Rc<OperatorNode>> = (0..6).map(|i| window(i * 600_000, 600_000)).collect();
    let coverages: Vec<_> = panes.iter().map(|pane| coverage_of(pane)).collect();
    for (i, coverage) in coverages.iter().enumerate() {
        let i = i as i64;
        assert_eq!(definition_id(coverage), definition_id(&coverages[0]));
        assert_eq!(
            coverage.selection(),
            [SelectionBox::new(
                Default::default(),
                relative(-(i + 1) * 600_000, -i * 600_000)
            )]
        );
    }
    let hour = coverage_of(&merge(panes).unwrap());
    assert_eq!(definition_id(&hour), definition_id(&coverages[0]));
    assert_eq!(
        hour.selection(),
        [SelectionBox::new(
            Default::default(),
            relative(-3_600_000, 0)
        )]
    );
}

/// A selection round-trips through JSON; columns are a list and bounds
/// name their kind.
#[test]
fn selections_round_trip_through_json() {
    let selection = SelectionBox::new(
        [
            (column("region"), utf8(&["eu", "us"])),
            (
                column("latency"),
                Constraint::Interval {
                    lower: Bound::Unbounded,
                    upper: Bound::Excluded(ScalarValue::Float64(100.0)),
                },
            ),
        ]
        .into(),
        relative(-120_000, -60_000),
    );
    let json = serde_json::to_value(&selection).unwrap();
    assert_eq!(
        json["relative_time"],
        serde_json::json!([{"excluded": -120000}, {"included": -60000}])
    );
    assert_eq!(json["columns"].as_array().unwrap().len(), 2);
    assert_eq!(
        serde_json::from_value::<SelectionBox>(json).unwrap(),
        selection
    );
}

/// A `family` build over `child`'s `latency` (a table) or `value` (a
/// series), grouped by `reduction`.
fn grouped(
    child: Rc<OperatorNode>,
    family: FieldDataType,
    reduction: Reduction,
) -> Rc<OperatorNode> {
    let column = match child.schema.column_id("latency") {
        Some(_) => "latency",
        None => "value",
    };
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        family,
        input: SummaryUpdate::column(ColumnRef::Named(column.into())),
        reduction,
        grouping: GroupingStrategy::default(),
        filter: None,
    }))
    .unwrap()
}

fn kll_family() -> FieldDataType {
    FieldDataType::Sketch(
        SketchKind::new(
            SketchAlgorithm::Kll,
            SketchParams::Kll {
                k: 200,
                failure_probability: None,
            },
        ),
        GroupingStrategy::default(),
    )
}

fn rollup(state: Rc<OperatorNode>, keys: Vec<usize>) -> Result<Rc<OperatorNode>, String> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![state],
        group_by: Some(keys),
        newest_panes: None,
    }))
    .map_err(|error| error.to_string())
}

/// States by (job, region) over `region ∈ {us, eu}`, rolled up to region,
/// cover what a direct build by region over those rows does: the same
/// definition (and id) and the same selection.
#[test]
fn a_rollup_covers_the_coarser_build() {
    let rows = filter(
        table(),
        ScalarExpr::InList {
            expr: Box::new(ScalarExpr::Column(REGION)),
            list: vec![
                ScalarExpr::Literal(ScalarValue::Utf8("us".into())),
                ScalarExpr::Literal(ScalarValue::Utf8("eu".into())),
            ],
            negated: false,
        },
    );
    let finer = grouped(rows.clone(), kll_family(), Reduction::by(vec![JOB, REGION]));
    let coarse = grouped(rows, kll_family(), Reduction::by(vec![REGION]));
    let rolled = rollup(finer, vec![1]).unwrap();
    assert_eq!(rolled.schema, coarse.schema);
    let (rolled, coarse) = (rolled.coverage().unwrap(), coarse.coverage().unwrap());
    assert_eq!(definition_id(&rolled), definition_id(&coarse));
    assert_eq!(rolled.selection(), coarse.selection());
    assert_eq!(
        rolled.selection()[0].columns()[&column("region")],
        utf8(&["us", "eu"])
    );
}

/// Per-series states keep their input's columns in place, so a rollup by
/// the `job` label covers a build by `job` over the same series.
#[test]
fn a_rollup_of_per_series_states_covers_the_build_by_its_labels() {
    let rows = pane_rows(0, vec![]);
    let job = rows.schema.column_id("job").unwrap();
    let per_series = grouped(rows.clone(), kll_family(), Reduction::PerEntity);
    let state_job = per_series.schema.column_id("job").unwrap();
    let rolled = rollup(per_series, vec![state_job]).unwrap();
    let by_job = grouped(rows, kll_family(), Reduction::by(vec![job]));
    assert_eq!(
        definition_id(&rolled.coverage().unwrap()),
        definition_id(&by_job.coverage().unwrap())
    );
}

/// A rollup is rejected when a key is no group key of the input's build,
/// and when its family cannot merge different groups (counter states merge
/// consecutive windows of one series, IRate has no merge).
#[test]
fn invalid_rollups_fail_construction() {
    let without = grouped(
        table(),
        kll_family(),
        Reduction::Reduce(
            asap_types::ir::operator::operator_properties::GroupKeys::without(vec![REGION]),
        ),
    );
    assert!(rollup(without, vec![0])
        .unwrap_err()
        .contains(&CoverageError::NotAGroupKey { key: 0 }.to_string()));
    for (kind, params) in [
        (ExactKind::Increase, ExactParams::Increase),
        (ExactKind::Rate, ExactParams::Rate),
        (ExactKind::IRate, ExactParams::IRate),
    ] {
        let state = grouped(
            pane_rows(0, vec![]),
            FieldDataType::ExactAggregate(kind, params),
            Reduction::by(vec![2]),
        );
        assert!(
            rollup(state, vec![])
                .unwrap_err()
                .contains(&CoverageError::FamilyCannotRegroup.to_string()),
            "{kind:?}"
        );
    }
}

/// A rollup to no keys of a SQL relation's states covers the global build
/// over the same rows: a global SQL aggregate, one row even over no input
/// rows, as the rollup's one state.
#[test]
fn a_rollup_to_no_keys_of_a_relation_covers_the_global_build() {
    let by_job = grouped(table(), kll_family(), Reduction::by(vec![JOB]));
    let global = grouped(table(), kll_family(), Reduction::by(vec![]));
    let rolled = rollup(by_job, vec![]).unwrap();
    assert_eq!(rolled.schema, global.schema);
    assert_eq!(
        definition_id(&rolled.coverage().unwrap()),
        definition_id(&global.coverage().unwrap())
    );
}

/// `group_by` is left out of JSON when unset, so a merge that keeps its
/// groups serializes as before; a rollup's keys round-trip.
#[test]
fn group_by_is_serialized_only_for_a_rollup() {
    let state = grouped(table(), kll_family(), Reduction::by(vec![JOB, REGION]));
    let merge = ASAPOp::SummaryMerge {
        children: vec![0u32],
        group_by: None,
        newest_panes: None,
    };
    assert_eq!(
        serde_json::to_value(&merge).unwrap(),
        serde_json::json!({"SummaryMerge": {"children": [0]}})
    );
    let rollup_op: ASAPOp<u32> = ASAPOp::SummaryMerge {
        children: vec![0],
        group_by: Some(vec![1]),
        newest_panes: None,
    };
    let json = serde_json::to_string(&rollup_op).unwrap();
    assert_eq!(
        serde_json::from_str::<ASAPOp<u32>>(&json).unwrap(),
        rollup_op
    );
    assert!(rollup(state, vec![1]).is_ok());
}

/// An exact sum rolled up from (job, region) to region finalizes to the
/// direct build's column, type and nullability.
#[test]
fn a_finalized_rollup_has_the_direct_builds_schema() {
    let sum = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
    let finer = grouped(table(), sum.clone(), Reduction::by(vec![JOB, REGION]));
    let coarse = grouped(table(), sum, Reduction::by(vec![REGION]));
    let finalize = |state: Rc<OperatorNode>| {
        OperatorNode::new_shared(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
            child: state,
        }))
        .unwrap()
    };
    assert_eq!(
        finalize(rollup(finer, vec![1]).unwrap()).schema,
        finalize(coarse).schema
    );
}

/// A filter on the group keys of per-(job, region) states keeps whole
/// states, so it is still state: merging the kept states covers the global
/// build over the filtered rows. A filter reading the state column is not a
/// selection of states and is rejected.
#[test]
fn a_key_filter_over_states_selects_states() {
    let states = grouped(table(), kll_family(), Reduction::by(vec![JOB, REGION]));
    let kept = filter(Rc::clone(&states), eq(JOB, "a"));
    assert_eq!(kept.result_kind, OperatorResultKind::State);
    let rolled = rollup(kept, vec![]).unwrap();
    let direct = grouped(
        filter(table(), eq(JOB, "a")),
        kll_family(),
        Reduction::by(vec![]),
    );
    assert_eq!(rolled.schema, direct.schema);
    assert_eq!(
        definition_id(&rolled.coverage().unwrap()),
        definition_id(&direct.coverage().unwrap())
    );
    let on_state = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
        pred: Predicate(ScalarExpr::IsNull(Box::new(ScalarExpr::Column(2)))),
        child: states,
    }))
    .unwrap();
    assert_eq!(on_state.result_kind, OperatorResultKind::Relation);
    assert!(on_state.validate_structure().is_err());
}
