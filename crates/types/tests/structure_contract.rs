use asap_types::ir::operator::Source;
use asap_types::ir::scalar::ScalarValue;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{
    ExprSemantics, NonASAPOp, OperatorNode, OperatorResultKind, Predicate, ScalarExpr,
};
use asap_types::test_support::validate_asap_inputs;
use std::rc::Rc;
fn scan() -> Rc<OperatorNode> {
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "t".into(),
        },
        predicates: vec![],
        schema: Schema::new(vec![Field::plain("x", DataType::Float64, false)]),
    }))
    .unwrap()
}
/// Resolved filters cannot hide invalid scalar types or out-of-scope columns.
#[test]
fn invalid_predicates_are_rejected() {
    for expr in [
        ScalarExpr::Column(7),
        ScalarExpr::literal_f64(1.0),
        ScalarExpr::Not(Box::new(ScalarExpr::literal_f64(1.0))),
    ] {
        let op = NonASAPOp::Filter {
            child: scan(),
            pred: Predicate(expr),
        };
        assert!(asap_types::test_support::validate_non_asap_inputs(&op).is_err());
    }
}
/// Common metadata must agree with the actual operation, including result kind.
#[test]
fn retained_result_kind_is_checked() {
    let mut node = (*scan()).clone();
    node.result_kind = OperatorResultKind::RangeVector;
    assert!(Rc::new(node).validate_structure().is_err());
}
/// Values validates arity and declared nullability without inventing columns.
#[test]
fn values_contract_is_checked() {
    for row in [
        vec![],
        vec![ScalarExpr::Literal(ScalarValue::Null)],
        vec![ScalarExpr::Literal(ScalarValue::Utf8("x".into()))],
    ] {
        let op = NonASAPOp::Values {
            rows: vec![row],
            schema: scan().schema.clone(),
        };
        assert!(asap_types::test_support::validate_non_asap_inputs(&op).is_err());
    }
}
/// Scalar typing validates every branch and never assigns placeholder types.
#[test]
fn scalar_type_rules_fail_closed() {
    for expr in [
        ScalarExpr::Column(99),
        ScalarExpr::FunctionCall {
            name: "not_registered".into(),
            args: vec![],
        },
        ScalarExpr::Negative {
            expr: Box::new(ScalarExpr::Literal(ScalarValue::Utf8("x".into()))),
            semantics: ExprSemantics::Sql,
        },
        ScalarExpr::Case {
            operand: None,
            branches: vec![(ScalarExpr::literal_f64(1.0), ScalarExpr::literal_f64(2.0))],
            else_expr: None,
        },
        ScalarExpr::ScalarSubquery(
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Values {
                rows: vec![],
                schema: Schema::default(),
            }))
            .unwrap(),
        ),
        ScalarExpr::PromqlScalarFromVector(scan()),
    ] {
        assert!(expr.scalar_type(&scan().schema).is_err(), "{expr:?}");
    }
}

/// A state family is not interchangeable with another sketch or a scalar field.
#[test]
fn state_evaluations_and_passthrough_keep_their_contracts() {
    use asap_types::ir::operator::Reduction;
    use asap_types::ir::scalar::ColumnRef;
    use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SketchStatistic, SummaryUpdate};
    use asap_types::ir::{ASAPOp, Operator, ProjectItem};
    let family = FieldDataType::Sketch(SketchAlgorithm::Kll);
    let state = Rc::new(
        OperatorNode::new(Operator::ASAP(ASAPOp::SummaryAgg {
            child: scan(),
            summary_type: family,
            input: SummaryUpdate::column(ColumnRef::Named("x".into())),
            reduction: Reduction::by(vec![]),
            filter: None,
        }))
        .unwrap(),
    );
    state.validate_structure().unwrap();
    let pass = OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Project {
        child: state.clone(),
        cols: vec![ProjectItem {
            expr: ScalarExpr::Column(0),
            alias: None,
        }],
    }))
    .unwrap();
    pass.validate_structure().unwrap();
    assert_eq!(pass.result_kind, OperatorResultKind::State);
    assert!(ScalarExpr::<Rc<OperatorNode>>::Column(0)
        .scalar_type(&pass.schema)
        .is_err());
    assert!(validate_asap_inputs(&ASAPOp::SummaryEstimate {
        summary_input: state.clone(),
        query: SketchStatistic::Cardinality
    })
    .is_err());
    assert!(validate_asap_inputs(&ASAPOp::SummaryEstimate {
        summary_input: state.clone(),
        query: SketchStatistic::Quantile { q: 0.99 }
    })
    .is_ok());
    assert!(validate_asap_inputs(&ASAPOp::FinalizeExactAccumulator { child: state }).is_err());
}

/// Both operator categories use the same fallible schema-deriving constructor.
#[test]
fn shared_construction_derives_both_operator_categories() {
    use asap_types::ir::operator::Reduction;
    use asap_types::ir::scalar::ColumnRef;
    use asap_types::ir::schema::FieldDataType;
    use asap_types::ir::schema::{ExactKind, ExactParams, SummaryUpdate};
    use asap_types::ir::{ASAPOp, Operator, ProjectItem};

    let input = scan();
    let state = Rc::new(
        OperatorNode::new(Operator::ASAP(ASAPOp::SummaryAgg {
            child: input.clone(),
            summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            input: SummaryUpdate::column(ColumnRef::Named("x".into())),
            reduction: Reduction::by(vec![]),
            filter: None,
        }))
        .unwrap(),
    );
    assert_eq!(state.result_kind, OperatorResultKind::State);
    assert!(!state.schema.fields.last().unwrap().is_plain());
    assert!(state.guarantee.is_none());
    state.validate_structure().unwrap();

    assert!(
        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
            child: input.clone(),
            cols: vec![ProjectItem {
                expr: ScalarExpr::Column(99),
                alias: None
            }],
        }))
        .is_err()
    );
    assert!(OperatorNode::new_shared(Operator::ASAP(ASAPOp::Extension {
        child: input,
        name: "unregistered".into(),
    }))
    .is_err());
}
