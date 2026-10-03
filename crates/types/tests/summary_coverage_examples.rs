//! The examples in docs/design_docs/proposals/asap-primitive-schema.md, built as real
//! SummaryAgg -> SummaryMerge plans. Every input has the same schema
//! `(job: Utf8, state: KLL{k=200})`; only coverage differs.
use asap_types::ir::summary_coverage::{CoverageError, CoverageRegion, SummaryCoverage};
use asap_types::ir::{
    ASAPOp, ExprSemantics, NonASAPOp, Operator, OperatorNode, Predicate, ScalarExpr,
    SchemaDerivationError,
};
use asap_types::post_asap::{
    GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryUpdate,
};
use asap_types::pre_asap::{
    ColumnRef, CompareOpKind, DataType, Field, FieldDataType, Reduction, ScalarValue, Schema,
    Source,
};
use std::rc::Rc;

const MIN: i64 = 60_000;

fn requests() -> Source {
    Source::Table {
        table_ref: "requests".into(),
    }
}

fn scan() -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: requests(),
        predicates: vec![],
        schema: Schema::new(vec![
            Field::plain("job", DataType::Utf8, false),
            Field::plain("region", DataType::Utf8, false),
            Field::plain("tier", DataType::Utf8, false),
            Field::plain("latency", DataType::Float64, false),
            Field::plain("size", DataType::Float64, false),
        ]),
    }))
    .unwrap()
}

/// `region = value`, evaluated on the scan schema.
fn region_is(value: &str) -> Predicate {
    Predicate(ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(1)),
        op: CompareOpKind::Eq,
        right: Box::new(ScalarExpr::Literal(ScalarValue::Utf8(value.into()))),
        semantics: ExprSemantics::Sql,
    })
}

fn region(time_ms: Option<std::ops::Range<i64>>, population: &[(&str, &str)]) -> CoverageRegion {
    CoverageRegion {
        time_ms,
        population: population
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

/// p99-ready KLL over `column`, grouped by job, with declared coverage.
fn kll_over(
    column: &str,
    filter: Option<Predicate>,
    coverage: SummaryCoverage,
) -> Rc<OperatorNode> {
    let node = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryAgg {
        child: scan(),
        family: FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
            GroupingStrategy::default(),
        ),
        input: SummaryUpdate::column(ColumnRef::Named(column.into())),
        reduction: Reduction::by(vec![0]),
        grouping: GroupingStrategy::default(),
        filter,
    }))
    .unwrap();
    Rc::new(node.with_coverage(coverage).unwrap())
}

fn kll(time_ms: Option<std::ops::Range<i64>>, population: &[(&str, &str)]) -> Rc<OperatorNode> {
    kll_over(
        "latency",
        None,
        SummaryCoverage {
            source: requests(),
            regions: vec![region(time_ms, population)],
        },
    )
}

fn merge(children: Vec<Rc<OperatorNode>>) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
    let schema = children[0].schema.clone();
    let merged = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge { children }))?;
    merged.validate_structure()?;
    // Schema never changes; only coverage does.
    assert_eq!(merged.schema, schema);
    Ok(merged)
}

fn regions(node: &OperatorNode) -> Vec<CoverageRegion> {
    node.coverage.as_ref().unwrap().regions.clone()
}

fn rejected(result: Result<Rc<OperatorNode>, SchemaDerivationError>, expected: CoverageError) {
    match result {
        Err(SchemaDerivationError::Coverage(actual)) => assert_eq!(actual, expected),
        other => panic!("expected {expected:?}, got {other:?}"),
    }
}

/// Example 1: adjacent panes coalesce, gaps stay, overlapping windows are rejected.
#[test]
fn example_1_time() {
    let adjacent = merge(vec![kll(Some(0..MIN), &[]), kll(Some(MIN..2 * MIN), &[])]).unwrap();
    assert_eq!(regions(&adjacent), vec![region(Some(0..2 * MIN), &[])]);

    let gapped = merge(vec![
        kll(Some(0..MIN), &[]),
        kll(Some(2 * MIN..3 * MIN), &[]),
    ])
    .unwrap();
    assert_eq!(
        regions(&gapped),
        vec![
            region(Some(0..MIN), &[]),
            region(Some(2 * MIN..3 * MIN), &[])
        ]
    );

    rejected(
        merge(vec![
            kll(Some(0..2 * MIN), &[]),
            kll(Some(MIN..3 * MIN), &[]),
        ]),
        CoverageError::PossibleOverlap,
    );
}

/// Example 2: disjoint label values merge; different labels or equal values are rejected.
#[test]
fn example_2_population() {
    let t = Some(0..MIN);
    let us_eu = merge(vec![
        kll(t.clone(), &[("region", "us")]),
        kll(t.clone(), &[("region", "eu")]),
    ])
    .unwrap();
    assert_eq!(
        regions(&us_eu),
        vec![
            region(t.clone(), &[("region", "eu")]),
            region(t.clone(), &[("region", "us")]),
        ]
    );
    for other in [("tier", "premium"), ("region", "us")] {
        rejected(
            merge(vec![
                kll(t.clone(), &[("region", "us")]),
                kll(t.clone(), &[other]),
            ]),
            CoverageError::PossibleOverlap,
        );
    }
}

/// Example 3: time and population stay paired; never widened to {us,eu} × [0,2).
#[test]
fn example_3_joint_regions() {
    let joint = merge(vec![
        kll(Some(0..MIN), &[("region", "us")]),
        kll(Some(MIN..2 * MIN), &[("region", "eu")]),
    ])
    .unwrap();
    assert_eq!(
        regions(&joint),
        vec![
            region(Some(MIN..2 * MIN), &[("region", "eu")]),
            region(Some(0..MIN), &[("region", "us")]),
        ]
    );
}

/// A table without a time column declares no time bounds.
#[test]
fn tabular_source_without_time_bounds() {
    let by_region = merge(vec![
        kll(None, &[("region", "us")]),
        kll(None, &[("region", "eu")]),
    ])
    .unwrap();
    assert_eq!(regions(&by_region).len(), 2);
    rejected(
        merge(vec![
            kll(None, &[("region", "us")]),
            kll(Some(0..MIN), &[("region", "us")]),
        ]),
        CoverageError::PossibleOverlap,
    );
}

/// Inputs must read the same source and share the producer's update and reduction.
#[test]
fn incompatible_inputs() {
    let other_source = kll_over(
        "latency",
        None,
        SummaryCoverage {
            source: Source::Table {
                table_ref: "other".into(),
            },
            regions: vec![region(Some(MIN..2 * MIN), &[])],
        },
    );
    rejected(
        merge(vec![kll(Some(0..MIN), &[]), other_source]),
        CoverageError::SourceMismatch,
    );

    // Same schema (both Float64 columns), different update expression.
    let size = kll_over(
        "size",
        None,
        SummaryCoverage {
            source: requests(),
            regions: vec![region(Some(MIN..2 * MIN), &[])],
        },
    );
    assert!(matches!(
        merge(vec![kll(Some(0..MIN), &[]), size]),
        Err(SchemaDerivationError::InvalidScalarSignature(message))
            if message.contains("update expression and reduction")
    ));
}

/// Summary nodes must carry coverage, and merges reject inputs without it.
#[test]
fn coverage_is_required() {
    let mut missing = (*kll(Some(MIN..2 * MIN), &[])).clone();
    missing.coverage = None;
    let missing = Rc::new(missing);
    assert!(matches!(
        missing.validate_structure(),
        Err(SchemaDerivationError::Coverage(CoverageError::Missing))
    ));
    rejected(
        merge(vec![kll(Some(0..MIN), &[]), missing]),
        CoverageError::UnknownInput,
    );
}

/// Trusted declarations: population is not checked against the filter (#570).
/// Both states hold US data, yet the wrong declaration lets them merge.
#[test]
fn wrong_population_declaration_is_accepted_until_570() {
    let declared = |value: &str| SummaryCoverage {
        source: requests(),
        regions: vec![region(Some(0..MIN), &[("region", value)])],
    };
    let a = kll_over("latency", Some(region_is("us")), declared("eu"));
    let b = kll_over("latency", Some(region_is("us")), declared("us"));
    assert!(merge(vec![a, b]).is_ok());
}
