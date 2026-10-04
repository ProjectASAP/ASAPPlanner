//! Narrow frequency recognition over the resolved scalar/operator graph.
//!
//! #509 Example 2 writes `L2(x)` and `Entropy(x)` as SQL idioms over grouped
//! unit counts. These rules name them as `FrequencyL2` / `FrequencyEntropy`
//! intents, so Pass 1 can offer exact, summary and UnivMon realizations.
//! Projection lineage, predicates, accuracy, SQL's empty-input NULL and
//! entropy units (nats) are preserved; integer products and nullable keys
//! are refused because overflow and NULL groups are observable in SQL.
use asap_types::ir::operator::non_asap::any_measure_filtered;
use asap_types::ir::operator::{AggIntent, GroupKeys, Reduction};
use asap_types::ir::scalar::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use asap_types::ir::schema::DataType;
use asap_types::ir::{ExprSemantics, NonASAPOp, Operator, OperatorNode, ProjectItem, ScalarExpr};
use std::rc::Rc;

/// The recognized form of a query whose root is the L2 or natural-log entropy
/// idiom over grouped unit counts, or `None` when neither rule applies.
pub fn recognize_frequency_idioms(root: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
    frequency_entropy_rewrite(root).or_else(|| frequency_l2_rewrite(root))
}

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
        ScalarExpr::Literal(_) => expr.clone(),
        ScalarExpr::Negative { expr, semantics } => ScalarExpr::Negative {
            expr: Box::new(substitute(expr, cols)?),
            semantics: *semantics,
        },
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

fn frequency_l2_rewrite(root: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
    let NonASAPOp::Project { cols, child, .. } = root.non_asap()? else {
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
    if !matches!(
        (uncast(left), uncast(right)),
        (ScalarExpr::Column(1), ScalarExpr::Column(1))
    ) {
        return None;
    }
    let (key, accuracy, input) = grouped_unit_count(&inner)?;
    sql_frequency_result(
        root,
        input,
        AggIntent::FrequencyL2 {
            col: Some(key),
            accuracy,
        },
        "frequency_l2",
        ScalarExpr::Column(0),
    )
}

// Both frequency rules require a complete, unfiltered count per non-NULL identity.
fn grouped_unit_count(
    node: &Rc<OperatorNode>,
) -> Option<(usize, asap_types::types::AccuracyTarget, Rc<OperatorNode>)> {
    let NonASAPOp::Aggregate {
        reduction: Reduction::Reduce(keys),
        measures,
        filters,
        having: None,
        child,
        ..
    } = node.non_asap()?
    else {
        return None;
    };
    let [key] = keys.keys() else {
        return None;
    };
    let [AggIntent::Count { accuracy }] = measures.as_slice() else {
        return None;
    };
    let field = child.schema.fields.get(*key)?;
    if keys.is_without()
        || any_measure_filtered(filters)
        || field.nullable
        || !matches!(
            field.plain_dtype()?,
            DataType::Bool | DataType::Int64 | DataType::Utf8
        )
    {
        return None;
    }
    Some((*key, accuracy.clone(), Rc::clone(child)))
}

fn probability_term(product: &ScalarExpr) -> Option<&ScalarExpr> {
    let ScalarExpr::Arithmetic {
        op: ArithmeticOpKind::Mul,
        left,
        right,
        semantics: ExprSemantics::Sql,
    } = product
    else {
        return None;
    };
    for (probability, logarithm) in [(left, right), (right, left)] {
        let ScalarExpr::FunctionCall { name, args } = logarithm.as_ref() else {
            continue;
        };
        if name.eq_ignore_ascii_case("ln") && args.as_slice() == [probability.as_ref().clone()] {
            return Some(probability);
        }
    }
    None
}

fn unit_count_term(expr: &ScalarExpr) -> bool {
    match uncast(expr) {
        ScalarExpr::Column(1) => true,
        ScalarExpr::Arithmetic { op: ArithmeticOpKind::Mul, left, right, semantics: ExprSemantics::Sql } => {
            [(left, right), (right, left)].into_iter().any(|(count, scale)| matches!(uncast(count), ScalarExpr::Column(1)) && matches!(scale.as_ref(), ScalarExpr::Literal(ScalarValue::Float64(value)) if *value == 1.0))
        },
        _ => false,
    }
}

fn frequency_entropy_rewrite(root: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
    use asap_types::ir::operator::{WindowFrameBound, WindowFrameOffset, WindowFuncKind};
    let NonASAPOp::Project { cols, child, .. } = root.non_asap()? else {
        return None;
    };
    let [item] = cols.as_slice() else {
        return None;
    };
    let (expr, outer) = expand(item.expr.clone(), Rc::clone(child))?;
    let ScalarExpr::Negative {
        expr,
        semantics: ExprSemantics::Sql,
    } = expr
    else {
        return None;
    };
    if !matches!(uncast(&expr), ScalarExpr::Column(0)) {
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
    let (product, window) = expand(ScalarExpr::Column(*col), Rc::clone(child))?;
    let probability = probability_term(&product)?;
    let ScalarExpr::Arithmetic {
        op: ArithmeticOpKind::Div,
        left,
        right,
        semantics: ExprSemantics::Sql,
    } = probability
    else {
        return None;
    };
    if !unit_count_term(left)
        || !matches!(uncast(right), ScalarExpr::Column(2))
        || probability.scalar_type(&window.schema).ok()?.0 != DataType::Float64
    {
        return None;
    }
    let NonASAPOp::SQLWindowFunc {
        func: WindowFuncKind::Sum,
        args,
        partition_by,
        order_by,
        frame: Some(frame),
        child,
        ..
    } = window.non_asap()?
    else {
        return None;
    };
    if partition_by.is_without()
        || !partition_by.keys().is_empty()
        || !order_by.is_empty()
        || args.as_slice() != [ScalarExpr::Column(1)]
    {
        return None;
    }
    if !matches!(
        frame.start_bound,
        WindowFrameBound::Preceding(WindowFrameOffset::Scalar(ScalarValue::Null))
    ) || !matches!(
        frame.end_bound,
        WindowFrameBound::Following(WindowFrameOffset::Scalar(ScalarValue::Null))
    ) {
        return None;
    }
    let (key, accuracy, input) = grouped_unit_count(child)?;
    let nats = ScalarExpr::Arithmetic {
        op: ArithmeticOpKind::Mul,
        left: Box::new(ScalarExpr::Column(0)),
        right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(
            std::f64::consts::LN_2,
        ))),
        semantics: ExprSemantics::Sql,
    };
    // -SUM(p*LN(p)) is negative zero for a single-identity population.
    let nats = ScalarExpr::Negative {
        expr: Box::new(ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Sub,
            left: Box::new(ScalarExpr::Literal(ScalarValue::Float64(0.0))),
            right: Box::new(nats),
            semantics: ExprSemantics::Sql,
        }),
        semantics: ExprSemantics::Sql,
    };
    sql_frequency_result(
        root,
        input,
        AggIntent::FrequencyEntropy {
            col: Some(key),
            accuracy,
        },
        "frequency_entropy",
        nats,
    )
}

// Both rules use an exact population guard: an approximate statistic may be
// zero even for nonempty input, and must not control SQL's NULL result.
fn sql_frequency_result(
    root: &Rc<OperatorNode>,
    input: Rc<OperatorNode>,
    measure: AggIntent,
    name: &str,
    value: ScalarExpr,
) -> Option<Rc<OperatorNode>> {
    use asap_types::{ir::operator::JoinKind, ir::Predicate, types::AccuracyTarget};
    let NonASAPOp::Project { qualifier, .. } = root.non_asap()? else {
        return None;
    };
    let aggregate = |measure, name: &str| {
        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::Reduce(GroupKeys::none()),
            measures: vec![measure],
            output_names: vec![name.into()],
            filters: vec![],
            having: None,
            child: Rc::clone(&input),
        }))
        .ok()
    };
    let statistic = aggregate(measure, name)?;
    let count = aggregate(
        AggIntent::Count {
            accuracy: AccuracyTarget::Exact,
        },
        "population_count",
    )?;
    let child = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Join {
        kind: JoinKind::Cross,
        pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
        left: statistic,
        right: count,
    }))
    .ok()?;
    let rewritten = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
        cols: vec![ProjectItem {
            alias: Some(root.schema.fields.first()?.name.clone()),
            expr: ScalarExpr::Case {
                operand: None,
                branches: vec![(
                    ScalarExpr::Compare {
                        left: Box::new(ScalarExpr::Column(1)),
                        op: CompareOpKind::Eq,
                        right: Box::new(ScalarExpr::Literal(ScalarValue::Int64(0))),
                        semantics: ExprSemantics::Sql,
                    },
                    ScalarExpr::Cast {
                        expr: Box::new(ScalarExpr::Literal(ScalarValue::Null)),
                        to: DataType::Float64,
                        try_cast: false,
                    },
                )],
                else_expr: Some(Box::new(value)),
            },
        }],
        qualifier: qualifier.clone(),
        child,
    }))
    .ok()?;
    (root.schema == rewritten.schema).then_some(rewritten)
}
