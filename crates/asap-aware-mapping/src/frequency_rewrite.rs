//! Narrow frequency recognition over the resolved scalar/operator graph.
use asap_types::ir::non_asap::any_measure_filtered;
use asap_types::{
    ir::{ExprSemantics, NonASAPOp, Operator, OperatorNode, ProjectItem, ScalarExpr},
    pre_asap::{
        AggIntent, ArithmeticOpKind, CompareOpKind, DataType, GroupKeys, Reduction, ScalarValue,
    },
};
use std::rc::Rc;

// Only follow projections. Crossing a filter/limit would change the population.
fn expand(
    mut expr: ScalarExpr,
    mut node: Rc<OperatorNode>,
) -> Option<(ScalarExpr, Rc<OperatorNode>)> {
    while let Some(NonASAPOp::Project { cols, child, .. }) = node.non_asap() {
        expr = substitute(&expr, cols)?;
        node = Rc::clone(child);
    }
    Some((expr, node))
}
fn substitute(expr: &ScalarExpr, cols: &[ProjectItem]) -> Option<ScalarExpr> {
    Some(match expr {
        ScalarExpr::Column(id) => cols.get(*id)?.expr.clone(),
        ScalarExpr::Cast {
            expr,
            to,
            try_cast: false,
        } if *to == DataType::Float64 => ScalarExpr::Cast {
            expr: Box::new(substitute(expr, cols)?),
            to: to.clone(),
            try_cast: false,
        },
        ScalarExpr::Arithmetic {
            op,
            left,
            right,
            semantics,
        } => ScalarExpr::Arithmetic {
            op: op.clone(),
            left: Box::new(substitute(left, cols)?),
            right: Box::new(substitute(right, cols)?),
            semantics: *semantics,
        },
        ScalarExpr::FunctionCall { name, args } => ScalarExpr::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|arg| substitute(arg, cols))
                .collect::<Option<_>>()?,
        },
        _ => return None,
    })
}
fn uncast(expr: &ScalarExpr) -> &ScalarExpr {
    match expr {
        ScalarExpr::Cast {
            expr,
            to: DataType::Float64,
            try_cast: false,
        } => uncast(expr),
        _ => expr,
    }
}

pub(super) fn frequency_l2_rewrite(root: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
    let NonASAPOp::Project {
        cols,
        child,
        qualifier,
    } = root.non_asap()?
    else {
        return None;
    };
    let [item] = cols.as_slice() else {
        return None;
    };
    let (expr, outer) = expand(item.expr.clone(), Rc::clone(child))?;
    let ScalarExpr::FunctionCall { name, args } = &expr else {
        return None;
    };
    if !name.eq_ignore_ascii_case("sqrt") {
        return None;
    }
    let [arg] = args.as_slice() else {
        return None;
    };
    if !matches!(uncast(arg), ScalarExpr::Column(0)) {
        return None;
    }
    let NonASAPOp::Aggregate {
        reduction: Reduction::Reduce(keys),
        measures,
        filters,
        having: None,
        child,
        ..
    } = outer.non_asap()?
    else {
        return None;
    };
    if keys.is_without() || !keys.keys().is_empty() || any_measure_filtered(filters) {
        return None;
    }
    let [AggIntent::Sum { col: Some(col) }] = measures.as_slice() else {
        return None;
    };
    let (product, inner) = expand(ScalarExpr::Column(*col), Rc::clone(child))?;
    let ScalarExpr::Arithmetic {
        op: ArithmeticOpKind::Mul,
        left,
        right,
        semantics: ExprSemantics::Sql,
    } = &product
    else {
        return None;
    };
    // SQL Int64 multiplication can overflow. Admit only products already typed Float64.
    if product.scalar_type(&inner.schema).ok()?.0 != DataType::Float64 {
        return None;
    }
    let NonASAPOp::Aggregate {
        reduction: Reduction::Reduce(keys),
        measures,
        filters,
        having: None,
        child,
        ..
    } = inner.non_asap()?
    else {
        return None;
    };
    let [key] = keys.keys() else {
        return None;
    };
    if keys.is_without() || any_measure_filtered(filters) {
        return None;
    }
    let [AggIntent::Count { accuracy }] = measures.as_slice() else {
        return None;
    };
    if !matches!(
        (uncast(left), uncast(right)),
        (ScalarExpr::Column(1), ScalarExpr::Column(1))
    ) {
        return None;
    }
    let field = child.schema.fields.get(*key)?;
    // COUNT(*) GROUP BY NULL creates a real group; the frequency intent skips it.
    if field.nullable
        || !matches!(
            field.plain_dtype()?,
            DataType::Bool | DataType::Int64 | DataType::Utf8
        )
    {
        return None;
    }
    let aggregate = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
        reduction: Reduction::Reduce(GroupKeys::none()),
        measures: vec![AggIntent::FrequencyL2 {
            col: Some(*key),
            accuracy: accuracy.clone(),
        }],
        output_names: vec!["frequency_l2".into()],
        filters: vec![],
        having: None,
        child: Rc::clone(child),
    }))
    .ok()?;
    // L2 is positive for any nonempty unit-update population. Restore SQL SUM's
    // NULL on an empty relation without introducing another count computation.
    let rewritten = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
        cols: vec![ProjectItem {
            alias: Some(root.schema.fields.first()?.name.clone()),
            expr: ScalarExpr::Case {
                operand: None,
                branches: vec![(
                    ScalarExpr::Compare {
                        left: Box::new(ScalarExpr::Column(0)),
                        op: CompareOpKind::Eq,
                        right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(0.0))),
                        semantics: ExprSemantics::Sql,
                    },
                    ScalarExpr::Cast {
                        expr: Box::new(ScalarExpr::Literal(ScalarValue::Null)),
                        to: DataType::Float64,
                        try_cast: false,
                    },
                )],
                else_expr: Some(Box::new(ScalarExpr::Column(0))),
            },
        }],
        qualifier: qualifier.clone(),
        child: aggregate,
    }))
    .ok()?;
    (root.schema == rewritten.schema).then_some(rewritten)
}
