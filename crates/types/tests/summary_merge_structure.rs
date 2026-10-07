//! Window composition merges compatible summary states without consuming raw rows.
use asap_types::{
    ir::operator_properties::{Reduction, Source},
    ir::summary_coverage::CoverageError,
    ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, SchemaDerivationError},
    post_asap::{GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryUpdate},
    pre_asap::{ColumnRef, DataType, Field, FieldDataType, Schema},
};
use std::rc::Rc;
fn state(k: u32) -> Rc<OperatorNode> {
    state_with(FieldDataType::Sketch(
        SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k }),
        Default::default(),
    ))
}

fn state_with(family: FieldDataType) -> Rc<OperatorNode> {
    state_over(family, SummaryUpdate::column(ColumnRef::SampleValue))
}

fn state_over(family: FieldDataType, input: SummaryUpdate) -> Rc<OperatorNode> {
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "latencies".into(),
        },
        predicates: vec![],
        schema: Schema::new(vec![Field::plain("value", DataType::Float64, false)]),
    }))
    .unwrap();
    let summary = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryAgg {
        child: scan,
        family,
        input: input.clone(),
        reduction: Reduction::by(vec![]),
        grouping: GroupingStrategy::default(),
        filter: None,
    }))
    .unwrap();
    std::rc::Rc::new(
        summary
            .with_coverage(asap_types::ir::summary_coverage::SummaryCoverage {
                source: Source::Table {
                    table_ref: "latencies".into(),
                },
                regions: vec![asap_types::ir::summary_coverage::CoverageRegion {
                    time_ms: Some(0..1),
                    population: Default::default(),
                }],
                input,
                group_by: Reduction::by(vec![]),
            })
            .unwrap(),
    )
}
/// Two KLL panes compose into one typed logical state without timing assignment.
#[test]
fn compatible_panes_merge_structurally() {
    let root = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![state(200), shifted_state(200, 1, 2)],
    }))
    .unwrap();
    root.validate_structure().unwrap();
    assert_eq!(root.schema.fields.len(), 1);
}
/// An empty merge, raw rows and differently sized state cannot masquerade as compatible panes.
#[test]
fn incompatible_merge_inputs_fail() {
    for children in [
        vec![],
        vec![state(200), state(300)],
        vec![state(200).children()[0].clone()],
    ] {
        assert!(
            OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge { children })).is_err()
        );
    }
}

fn shifted_state(k: u32, start: i64, end: i64) -> Rc<OperatorNode> {
    shifted(&state(k), start, end)
}

fn shifted(state: &Rc<OperatorNode>, start: i64, end: i64) -> Rc<OperatorNode> {
    let mut node = (**state).clone();
    let region = &mut node.coverage.as_mut().unwrap().regions[0];
    region.time_ms = Some(start..end);
    Rc::new(node)
}

fn kll() -> FieldDataType {
    FieldDataType::Sketch(
        SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
        Default::default(),
    )
}

fn merge(children: Vec<Rc<OperatorNode>>) -> Result<Rc<OperatorNode>, SchemaDerivationError> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge { children }))
}

/// Equal schemas cannot tell a KLL over one column from a KLL over another;
/// the columns in their coverage can.
#[test]
fn different_coverage_columns_do_not_merge() {
    let named = state_over(
        kll(),
        SummaryUpdate::column(ColumnRef::Named("value".into())),
    );
    assert!(matches!(
        merge(vec![state(200), shifted(&named, 1, 2)]),
        Err(SchemaDerivationError::Coverage(
            CoverageError::ColumnMismatch
        ))
    ));
}

/// A `SummaryAgg` cannot declare columns other than its own input and grouping.
#[test]
fn declared_columns_must_match_the_summary() {
    let mut coverage = state(200).coverage.clone().unwrap();
    coverage.input = SummaryUpdate::column(ColumnRef::Named("value".into()));
    let fresh = (*state(200)).clone();
    assert!(matches!(
        fresh.clone().with_coverage(coverage.clone()),
        Err(SchemaDerivationError::Coverage(
            CoverageError::ColumnMismatch
        ))
    ));
    let mut forged = fresh;
    forged.coverage = Some(coverage);
    assert!(matches!(
        Rc::new(forged).validate_structure(),
        Err(SchemaDerivationError::Coverage(
            CoverageError::ColumnMismatch
        ))
    ));
}

/// Every merge input needs coverage. A nested merge has none until #646, so
/// it is rejected for now.
#[test]
fn merge_inputs_without_coverage_are_rejected() {
    let mut missing = (*state(200)).clone();
    missing.coverage = None;
    let nested = merge(vec![state(200)]).unwrap();
    for input in [Rc::new(missing), nested] {
        assert!(matches!(
            merge(vec![state(200), input]),
            Err(SchemaDerivationError::Coverage(CoverageError::UnknownInput))
        ));
    }
}
