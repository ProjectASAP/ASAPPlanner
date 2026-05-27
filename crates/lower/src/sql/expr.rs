use datafusion::logical_expr::{BinaryExpr, Expr, Operator};

use asap_control_core::intent_algebra::{ArithOp, ColumnRef, CompareOp, L2Expr, L3Scalar};

use crate::error::LoweringError;

use super::types::{arrow_to_l3, scalar_value_to_l3};

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

/// Translate a DataFusion `Expr` to an `L2Expr`.
/// Returns `UnsupportedFeature` for anything not needed in v1.
pub(super) fn df_expr_to_l2(expr: &Expr) -> Result<L2Expr, LoweringError> {
    match expr {
        // Preserve DataFusion's relation qualifier so a column name shared
        // across a join (`a.k` vs `b.k`) resolves to the correct side.
        Expr::Column(col) => Ok(L2Expr::Column(match &col.relation {
            Some(rel) => ColumnRef::Qualified {
                table: rel.to_string(),
                name: col.name.clone(),
            },
            None => ColumnRef::Named(col.name.clone()),
        })),

        Expr::Literal(sv) => scalar_value_to_l3(sv).map(L2Expr::Literal),

        Expr::Alias(a) => df_expr_to_l2(&a.expr),

        Expr::BinaryExpr(BinaryExpr { left, op, right }) => match op {
            Operator::And => {
                let parts = split_conjuncts(expr);
                let l3_parts: Result<Vec<_>, _> = parts.iter().map(|e| df_expr_to_l2(e)).collect();
                Ok(L2Expr::BoolAnd(l3_parts?))
            }
            Operator::Or => {
                let parts = split_disjuncts(expr);
                let l3_parts: Result<Vec<_>, _> = parts.iter().map(|e| df_expr_to_l2(e)).collect();
                Ok(L2Expr::BoolOr(l3_parts?))
            }
            Operator::Eq => compare(left, CompareOp::Eq, right),
            Operator::NotEq => compare(left, CompareOp::Ne, right),
            Operator::Lt => compare(left, CompareOp::Lt, right),
            Operator::LtEq => compare(left, CompareOp::Le, right),
            Operator::Gt => compare(left, CompareOp::Gt, right),
            Operator::GtEq => compare(left, CompareOp::Ge, right),
            // BinaryExpr LIKE/ILIKE operators (from optimizer rewrites)
            Operator::LikeMatch => compare(left, CompareOp::Like, right),
            Operator::ILikeMatch => compare(left, CompareOp::ILike, right),
            Operator::NotLikeMatch => compare(left, CompareOp::NotLike, right),
            Operator::NotILikeMatch => compare(left, CompareOp::NotILike, right),
            // Arithmetic
            Operator::Plus => arith(left, ArithOp::Add, right),
            Operator::Minus => arith(left, ArithOp::Sub, right),
            Operator::Multiply => arith(left, ArithOp::Mul, right),
            Operator::Divide => arith(left, ArithOp::Div, right),
            Operator::Modulo => arith(left, ArithOp::Mod, right),
            other => Err(LoweringError::UnsupportedFeature(format!(
                "operator: {other:?}"
            ))),
        },

        // SQL LIKE / ILIKE (dedicated expr node from the SQL parser)
        Expr::Like(like) => {
            let op = match (like.negated, like.case_insensitive) {
                (false, false) => CompareOp::Like,
                (true, false) => CompareOp::NotLike,
                (false, true) => CompareOp::ILike,
                (true, true) => CompareOp::NotILike,
            };
            compare(&like.expr, op, &like.pattern)
        }

        // Unary minus: negate literals directly; wrap others in -1 * x.
        Expr::Negative(inner) => {
            let inner_l3 = df_expr_to_l2(inner)?;
            match inner_l3 {
                L2Expr::Literal(L3Scalar::Int64(v)) => Ok(L2Expr::Literal(L3Scalar::Int64(-v))),
                L2Expr::Literal(L3Scalar::Float64(v)) => Ok(L2Expr::Literal(L3Scalar::Float64(-v))),
                other => Ok(L2Expr::Arith {
                    op: ArithOp::Mul,
                    left: Box::new(L2Expr::Literal(L3Scalar::Int64(-1))),
                    right: Box::new(other),
                }),
            }
        }

        // SQL CASE expression
        Expr::Case(c) => {
            let operand = c
                .expr
                .as_ref()
                .map(|e| df_expr_to_l2(e).map(Box::new))
                .transpose()?;
            let branches = c
                .when_then_expr
                .iter()
                .map(|(when, then)| Ok((df_expr_to_l2(when)?, df_expr_to_l2(then)?)))
                .collect::<Result<Vec<_>, LoweringError>>()?;
            let else_expr = c
                .else_expr
                .as_ref()
                .map(|e| df_expr_to_l2(e).map(Box::new))
                .transpose()?;
            Ok(L2Expr::Case {
                operand,
                branches,
                else_expr,
            })
        }

        Expr::Not(inner) => Ok(L2Expr::Not(Box::new(df_expr_to_l2(inner)?))),

        Expr::IsNull(inner) => Ok(L2Expr::IsNull(Box::new(df_expr_to_l2(inner)?))),

        Expr::IsNotNull(inner) => Ok(L2Expr::IsNotNull(Box::new(df_expr_to_l2(inner)?))),

        Expr::Cast(c) => {
            let inner = df_expr_to_l2(&c.expr)?;
            let to = arrow_to_l3(&c.data_type)?;
            Ok(L2Expr::Cast {
                expr: Box::new(inner),
                to,
                try_cast: false,
            })
        }

        // TRY_CAST returns NULL on conversion failure; preserve that semantic.
        Expr::TryCast(c) => {
            let inner = df_expr_to_l2(&c.expr)?;
            let to = arrow_to_l3(&c.data_type)?;
            Ok(L2Expr::Cast {
                expr: Box::new(inner),
                to,
                try_cast: true,
            })
        }

        Expr::InList(il) => {
            let expr = df_expr_to_l2(&il.expr)?;
            let list: Result<Vec<_>, _> = il.list.iter().map(df_expr_to_l2).collect();
            Ok(L2Expr::InList {
                expr: Box::new(expr),
                list: list?,
                negated: il.negated,
            })
        }

        Expr::Between(b) => {
            // Normalize: `x BETWEEN low AND high` → `x >= low AND x <= high`.
            // `x NOT BETWEEN low AND high` → `x < low OR x > high`.
            let x_low = compare(&b.expr, CompareOp::Ge, &b.low)?;
            let x_high = compare(&b.expr, CompareOp::Le, &b.high)?;
            if b.negated {
                // NOT BETWEEN: invert each side
                let lt = compare(&b.expr, CompareOp::Lt, &b.low)?;
                let gt = compare(&b.expr, CompareOp::Gt, &b.high)?;
                Ok(L2Expr::BoolOr(vec![lt, gt]))
            } else {
                Ok(L2Expr::BoolAnd(vec![x_low, x_high]))
            }
        }

        Expr::ScalarFunction(sf) => {
            let args: Result<Vec<_>, _> = sf.args.iter().map(df_expr_to_l2).collect();
            Ok(L2Expr::FunctionCall {
                name: sf.func.name().to_string(),
                args: args?,
            })
        }

        other => Err(LoweringError::UnsupportedFeature(format!(
            "expression: {}",
            other
        ))),
    }
}

pub(super) fn compare(left: &Expr, op: CompareOp, right: &Expr) -> Result<L2Expr, LoweringError> {
    Ok(L2Expr::Compare {
        left: Box::new(df_expr_to_l2(left)?),
        op,
        right: Box::new(df_expr_to_l2(right)?),
    })
}

pub(super) fn arith(left: &Expr, op: ArithOp, right: &Expr) -> Result<L2Expr, LoweringError> {
    Ok(L2Expr::Arith {
        op,
        left: Box::new(df_expr_to_l2(left)?),
        right: Box::new(df_expr_to_l2(right)?),
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
