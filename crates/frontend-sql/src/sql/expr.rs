use std::rc::Rc;

use datafusion::logical_expr::{BinaryExpr, Expr, Operator};

use asap_types::pre_asap::{ArithmeticOpKind, ColumnRef, CompareOpKind, ScalarValue};

use crate::error::SqlError as LoweringError;

use super::types::{arrow_to_dtype, scalar_value_to_asap};
use super::Unresolved;

pub(super) fn split_conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::BinaryExpr(BinaryExpr {
            left,
            op: Operator::And,
            right,
        }) => {
            let mut v = split_conjuncts(left);
            v.extend(split_conjuncts(right));
            v
        }
        _ => vec![expr],
    }
}

/// Translate a DataFusion `Expr` to the canonical, unresolved tree.
/// Returns `UnsupportedFeature` for anything not needed in v1.
pub(super) fn df_expr_to_unresolved(expr: &Expr) -> Result<Unresolved, LoweringError> {
    match expr {
        // Preserve DataFusion's relation qualifier so a column name shared
        // across a join (`a.k` vs `b.k`) resolves to the correct side.
        Expr::Column(col) => Ok(Unresolved::Column(match &col.relation {
            Some(rel) => ColumnRef::Qualified {
                table: rel.to_string(),
                name: col.name.clone(),
            },
            None => ColumnRef::Named(col.name.clone()),
        })),

        Expr::Literal(sv) => scalar_value_to_asap(sv).map(Unresolved::Literal),

        Expr::Alias(a) => df_expr_to_unresolved(&a.expr),

        Expr::BinaryExpr(BinaryExpr { left, op, right }) => match op {
            Operator::And => {
                let parts = split_conjuncts(expr);
                let lowered: Result<Vec<_>, _> =
                    parts.iter().map(|e| df_expr_to_unresolved(e)).collect();
                Ok(Unresolved::BoolAnd(lowered?))
            }
            Operator::Or => {
                let parts = split_disjuncts(expr);
                let lowered: Result<Vec<_>, _> =
                    parts.iter().map(|e| df_expr_to_unresolved(e)).collect();
                Ok(Unresolved::BoolOr(lowered?))
            }
            Operator::Eq => compare(left, CompareOpKind::Eq, right),
            Operator::NotEq => compare(left, CompareOpKind::Ne, right),
            Operator::Lt => compare(left, CompareOpKind::Lt, right),
            Operator::LtEq => compare(left, CompareOpKind::Le, right),
            Operator::Gt => compare(left, CompareOpKind::Gt, right),
            Operator::GtEq => compare(left, CompareOpKind::Ge, right),
            // BinaryExpr LIKE/ILIKE operators (from optimizer rewrites)
            Operator::LikeMatch => compare(left, CompareOpKind::Like, right),
            Operator::ILikeMatch => compare(left, CompareOpKind::ILike, right),
            Operator::NotLikeMatch => compare(left, CompareOpKind::NotLike, right),
            Operator::NotILikeMatch => compare(left, CompareOpKind::NotILike, right),
            // Arithmetic
            Operator::Plus => arith(left, ArithmeticOpKind::Add, right),
            Operator::Minus => arith(left, ArithmeticOpKind::Sub, right),
            Operator::Multiply => arith(left, ArithmeticOpKind::Mul, right),
            Operator::Divide => arith(left, ArithmeticOpKind::Div, right),
            Operator::Modulo => arith(left, ArithmeticOpKind::Mod, right),
            other => Err(LoweringError::UnsupportedFeature(format!(
                "operator: {other:?}"
            ))),
        },

        // SQL LIKE / ILIKE (dedicated expr node from the SQL parser)
        Expr::Like(like) => {
            let op = match (like.negated, like.case_insensitive) {
                (false, false) => CompareOpKind::Like,
                (true, false) => CompareOpKind::NotLike,
                (false, true) => CompareOpKind::ILike,
                (true, true) => CompareOpKind::NotILike,
            };
            compare(&like.expr, op, &like.pattern)
        }

        // Unary minus: negate literals directly; wrap others in -1 * x.
        Expr::Negative(inner) => {
            let inner = df_expr_to_unresolved(inner)?;
            match inner {
                Unresolved::Literal(ScalarValue::Int64(v)) => {
                    Ok(Unresolved::Literal(ScalarValue::Int64(-v)))
                }
                Unresolved::Literal(ScalarValue::Float64(v)) => {
                    Ok(Unresolved::Literal(ScalarValue::Float64(-v)))
                }
                other => Ok(Unresolved::Arithmetic {
                    op: ArithmeticOpKind::Mul,
                    left: Rc::new(Unresolved::Literal(ScalarValue::Int64(-1))),
                    right: Rc::new(other),
                }),
            }
        }

        // SQL CASE expression
        Expr::Case(c) => {
            let operand = c
                .expr
                .as_ref()
                .map(|e| df_expr_to_unresolved(e).map(Rc::new))
                .transpose()?;
            let branches = c
                .when_then_expr
                .iter()
                .map(|(when, then)| {
                    Ok((df_expr_to_unresolved(when)?, df_expr_to_unresolved(then)?))
                })
                .collect::<Result<Vec<_>, LoweringError>>()?;
            let else_expr = c
                .else_expr
                .as_ref()
                .map(|e| df_expr_to_unresolved(e).map(Rc::new))
                .transpose()?;
            Ok(Unresolved::Case {
                operand,
                branches,
                else_expr,
            })
        }

        Expr::Not(inner) => Ok(Unresolved::Not(Rc::new(df_expr_to_unresolved(inner)?))),

        Expr::IsNull(inner) => Ok(Unresolved::IsNull(Rc::new(df_expr_to_unresolved(inner)?))),

        Expr::IsNotNull(inner) => Ok(Unresolved::IsNotNull(Rc::new(df_expr_to_unresolved(
            inner,
        )?))),

        Expr::Cast(c) => {
            let inner = df_expr_to_unresolved(&c.expr)?;
            let to = arrow_to_dtype(&c.data_type)?;
            Ok(Unresolved::Cast {
                expr: Rc::new(inner),
                to,
                try_cast: false,
            })
        }

        // TRY_CAST returns NULL on conversion failure; preserve that semantic.
        Expr::TryCast(c) => {
            let inner = df_expr_to_unresolved(&c.expr)?;
            let to = arrow_to_dtype(&c.data_type)?;
            Ok(Unresolved::Cast {
                expr: Rc::new(inner),
                to,
                try_cast: true,
            })
        }

        Expr::InList(il) => {
            let expr = df_expr_to_unresolved(&il.expr)?;
            let list: Result<Vec<_>, _> = il.list.iter().map(df_expr_to_unresolved).collect();
            Ok(Unresolved::InList {
                expr: Rc::new(expr),
                list: list?,
                negated: il.negated,
            })
        }

        Expr::Between(b) => {
            // Normalize: `x BETWEEN low AND high` → `x >= low AND x <= high`.
            // `x NOT BETWEEN low AND high` → `x < low OR x > high`.
            let x_low = compare(&b.expr, CompareOpKind::Ge, &b.low)?;
            let x_high = compare(&b.expr, CompareOpKind::Le, &b.high)?;
            if b.negated {
                // NOT BETWEEN: invert each side
                let lt = compare(&b.expr, CompareOpKind::Lt, &b.low)?;
                let gt = compare(&b.expr, CompareOpKind::Gt, &b.high)?;
                Ok(Unresolved::BoolOr(vec![lt, gt]))
            } else {
                Ok(Unresolved::BoolAnd(vec![x_low, x_high]))
            }
        }

        Expr::ScalarFunction(sf) => {
            let args: Result<Vec<_>, _> = sf.args.iter().map(df_expr_to_unresolved).collect();
            Ok(Unresolved::FunctionCall {
                name: sf.func.name().to_string(),
                args: args?,
            })
        }

        // Subquery-valued expressions in a predicate/projection — `x > (SELECT
        // …)`, `x IN (SELECT …)`, `EXISTS (SELECT …)`. These need a subquery
        // node in the unresolved expression IR (and a correlated-vs-uncorrelated
        // decision); rejected cleanly until that lands rather than mislowered.
        // Derived tables in `FROM` (the common nesting shape) ARE supported —
        // see `lower_plan`'s `SubqueryAlias` arm.
        Expr::ScalarSubquery(_) | Expr::InSubquery(_) | Expr::Exists(_) => Err(
            LoweringError::UnsupportedFeature("subquery-valued expression in predicate".into()),
        ),

        other => Err(LoweringError::UnsupportedFeature(format!(
            "expression: {}",
            other
        ))),
    }
}

pub(super) fn compare(
    left: &Expr,
    op: CompareOpKind,
    right: &Expr,
) -> Result<Unresolved, LoweringError> {
    Ok(Unresolved::Compare {
        left: Rc::new(df_expr_to_unresolved(left)?),
        op,
        right: Rc::new(df_expr_to_unresolved(right)?),
    })
}

pub(super) fn arith(
    left: &Expr,
    op: ArithmeticOpKind,
    right: &Expr,
) -> Result<Unresolved, LoweringError> {
    Ok(Unresolved::Arithmetic {
        op,
        left: Rc::new(df_expr_to_unresolved(left)?),
        right: Rc::new(df_expr_to_unresolved(right)?),
    })
}

pub(super) fn split_disjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::BinaryExpr(BinaryExpr {
            left,
            op: Operator::Or,
            right,
        }) => {
            let mut v = split_disjuncts(left);
            v.extend(split_disjuncts(right));
            v
        }
        _ => vec![expr],
    }
}
