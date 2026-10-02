use asap_types::ir::{
    ExprSemantics, NonASAPOp, OperatorNode, OperatorResultKind, Predicate, ScalarExpr,
};
use asap_types::pre_asap::{DataType, Field, ScalarValue, Schema, Source};
use std::rc::Rc;
fn scan() -> Rc<OperatorNode> {
    OperatorNode::non_asap_node(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "t".into(),
        },
        predicates: vec![],
        schema: Schema::new(vec![Field::plain("x", DataType::Float64, false)]),
    })
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
        assert!(op.validate_inputs().is_err());
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
        assert!(op.validate_inputs().is_err());
    }
}
/// Scalar typing validates every branch and never assigns placeholder types.
#[test]
fn scalar_signatures_fail_closed() {
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
            OperatorNode::non_asap_node(NonASAPOp::Values {
                rows: vec![],
                schema: Schema::default(),
            })
            .unwrap(),
        ),
        ScalarExpr::PromqlScalarFromVector(scan()),
    ] {
        assert!(expr.scalar_type(&scan().schema).is_err(), "{expr:?}");
    }
}
