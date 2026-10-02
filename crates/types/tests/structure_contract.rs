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

/// A state family is not interchangeable with another sketch or a scalar field.
#[test]
fn state_evaluations_and_passthrough_keep_their_contracts() {
    use asap_types::ir::{ASAPOp, Operator, ProjectItem};
    use asap_types::post_asap::{
        FieldDataType, GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams,
        SketchStatistic, SummaryUpdate,
    };
    use asap_types::pre_asap::{ColumnRef, Reduction};
    let family = FieldDataType::Sketch(
        SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 100 }),
        GroupingStrategy::default(),
    );
    let state = Rc::new(
        OperatorNode::new(Operator::ASAP(ASAPOp::SummaryAgg {
            child: scan(),
            family,
            input: SummaryUpdate::column(ColumnRef::Named("x".into())),
            reduction: Reduction::by(vec![]),
            grouping: GroupingStrategy::default(),
            filter: None,
        }))
        .unwrap(),
    );
    state.validate_structure().unwrap();
    let pass = OperatorNode::non_asap_node(NonASAPOp::Project {
        child: state.clone(),
        cols: vec![ProjectItem {
            expr: ScalarExpr::Column(0),
            alias: None,
        }],
        qualifier: None,
    })
    .unwrap();
    pass.validate_structure().unwrap();
    assert_eq!(pass.result_kind, OperatorResultKind::State);
    assert!(ScalarExpr::Column(0).scalar_type(&pass.schema).is_err());
    assert!(ASAPOp::SummaryEstimate {
        summary_input: state.clone(),
        query: SketchStatistic::Cardinality
    }
    .validate_inputs()
    .is_err());
    assert!(ASAPOp::SummaryEstimate {
        summary_input: state.clone(),
        query: SketchStatistic::Quantile { q: 0.99 }
    }
    .validate_inputs()
    .is_ok());
    assert!(ASAPOp::FinalizeExactAccumulator { child: state }
        .validate_inputs()
        .is_err());
}

/// Phase validation checks dependencies, without declaring a computation query-only.
#[test]
fn execution_timing_checks_edges_not_function_names() {
    use asap_types::ir::ProjectItem;
    use asap_types::post_asap::ExecutionTiming::{IngestionTime, QueryTime};
    let input = Rc::new((*scan()).clone().with_timing(Some(IngestionTime)));
    let mut project = OperatorNode::new(asap_types::ir::Operator::NonASAP(NonASAPOp::Project {
        child: input,
        cols: vec![ProjectItem {
            expr: ScalarExpr::FunctionCall {
                name: "promql_abs".into(),
                args: vec![ScalarExpr::Column(0)],
            },
            alias: None,
        }],
        qualifier: None,
    }))
    .unwrap()
    .with_timing(Some(IngestionTime));
    Rc::new(project.clone())
        .validate_execution_timing()
        .unwrap();
    if let asap_types::ir::Operator::NonASAP(NonASAPOp::Project { child, .. }) =
        &mut project.operator
    {
        *child = Rc::new(child.as_ref().clone().with_timing(Some(QueryTime)));
    }
    assert!(Rc::new(project).validate_execution_timing().is_err());
}
