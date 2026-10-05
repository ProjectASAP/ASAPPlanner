use std::rc::Rc;

use datafusion::logical_expr::{BinaryExpr, Expr, Operator};

use asap_frontend_common::UnresolvedScalar as Unresolved;
use asap_types::ir::ExprSemantics;
use asap_types::pre_asap::{ArithmeticOpKind, ColumnRef, CompareOpKind, ScalarValue};

use crate::unified::error::SqlError as LoweringError;

use super::types::{arrow_to_dtype, scalar_value_to_asap};
use super::SqlLowerer;

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

impl SqlLowerer<'_> {
    /// Translate a DataFusion `Expr` to the name-based scalar tree. Every
    /// `Compare` / `Arithmetic` / `Negative` carries `ExprSemantics::Sql`.
    /// Subquery-valued expressions lower their plan as a root of its own
    /// (which is why this is a method: the plan walk needs the catalog).
    /// Returns `UnsupportedFeature` for anything not needed in v1.
    pub(super) fn lower_expr(&self, expr: &Expr) -> Result<Unresolved, LoweringError> {
        let bx = |e: &Expr| self.lower_expr(e).map(Box::new);
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

            // Keep Arrow date literals equivalent to SQL CAST('YYYY-MM-DD' AS DATE),
            // including typed nulls, without adding another canonical scalar variant.
            Expr::Literal(
                sv @ (datafusion::common::ScalarValue::Date32(_)
                | datafusion::common::ScalarValue::Date64(_)),
                _,
            ) => {
                let text = sv.cast_to(&datafusion::arrow::datatypes::DataType::Utf8)?;
                // Arrow formats Date64 with a time suffix; the canonical Date has
                // no time-of-day, just like Date64 catalog registration as Date32.
                let text = match text {
                    datafusion::common::ScalarValue::Utf8(Some(value)) => {
                        ScalarValue::Utf8(value.split('T').next().unwrap().to_owned())
                    }
                    other => scalar_value_to_asap(&other)?,
                };
                Ok(Unresolved::Cast {
                    expr: Box::new(Unresolved::Literal(text)),
                    to: asap_types::pre_asap::schema::DataType::Date,
                    try_cast: false,
                })
            }
            Expr::Literal(sv, _) => scalar_value_to_asap(sv).map(Unresolved::Literal),

            Expr::Alias(a) => self.lower_expr(&a.expr),

            Expr::BinaryExpr(BinaryExpr { left, op, right }) => match op {
                Operator::And => {
                    let parts = split_conjuncts(expr);
                    let lowered: Result<Vec<_>, _> =
                        parts.iter().map(|e| self.lower_expr(e)).collect();
                    Ok(Unresolved::BoolAnd(lowered?))
                }
                Operator::Or => {
                    let parts = split_disjuncts(expr);
                    let lowered: Result<Vec<_>, _> =
                        parts.iter().map(|e| self.lower_expr(e)).collect();
                    Ok(Unresolved::BoolOr(lowered?))
                }
                Operator::Eq => self.compare(left, CompareOpKind::Eq, right),
                Operator::NotEq => self.compare(left, CompareOpKind::Ne, right),
                Operator::Lt => self.compare(left, CompareOpKind::Lt, right),
                Operator::LtEq => self.compare(left, CompareOpKind::Le, right),
                Operator::Gt => self.compare(left, CompareOpKind::Gt, right),
                Operator::GtEq => self.compare(left, CompareOpKind::Ge, right),
                // BinaryExpr LIKE/ILIKE operators (from optimizer rewrites)
                Operator::LikeMatch => self.compare(left, CompareOpKind::Like, right),
                Operator::ILikeMatch => self.compare(left, CompareOpKind::ILike, right),
                Operator::NotLikeMatch => self.compare(left, CompareOpKind::NotLike, right),
                Operator::NotILikeMatch => self.compare(left, CompareOpKind::NotILike, right),
                // Arithmetic
                Operator::Plus => self.arith(left, ArithmeticOpKind::Add, right),
                Operator::Minus => self.arith(left, ArithmeticOpKind::Sub, right),
                Operator::Multiply => self.arith(left, ArithmeticOpKind::Mul, right),
                Operator::Divide => self.arith(left, ArithmeticOpKind::Div, right),
                Operator::Modulo => self.arith(left, ArithmeticOpKind::Mod, right),
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
                self.compare(&like.expr, op, &like.pattern)
            }

            // Unary minus. (DataFusion's planner already folds `-<number>`
            // into a negative literal, so this is a non-literal operand.)
            Expr::Negative(inner) => Ok(Unresolved::Negative {
                expr: bx(inner)?,
                semantics: ExprSemantics::Sql,
            }),

            // SQL CASE expression
            Expr::Case(c) => {
                let operand = c.expr.as_deref().map(bx).transpose()?;
                let branches = c
                    .when_then_expr
                    .iter()
                    .map(|(when, then)| Ok((self.lower_expr(when)?, self.lower_expr(then)?)))
                    .collect::<Result<Vec<_>, LoweringError>>()?;
                let else_expr = c.else_expr.as_deref().map(bx).transpose()?;
                Ok(Unresolved::Case {
                    operand,
                    branches,
                    else_expr,
                })
            }

            Expr::Not(inner) => Ok(Unresolved::Not(bx(inner)?)),

            Expr::IsNull(inner) => Ok(Unresolved::IsNull(bx(inner)?)),

            Expr::IsNotNull(inner) => Ok(Unresolved::IsNotNull(bx(inner)?)),

            Expr::Cast(c) => Ok(Unresolved::Cast {
                expr: bx(&c.expr)?,
                to: arrow_to_dtype(c.field.data_type())?,
                try_cast: false,
            }),

            // TRY_CAST returns NULL on conversion failure; preserve that semantic.
            Expr::TryCast(c) => Ok(Unresolved::Cast {
                expr: bx(&c.expr)?,
                to: arrow_to_dtype(c.field.data_type())?,
                try_cast: true,
            }),

            Expr::InList(il) => {
                let list: Result<Vec<_>, _> = il.list.iter().map(|e| self.lower_expr(e)).collect();
                Ok(Unresolved::InList {
                    expr: bx(&il.expr)?,
                    list: list?,
                    negated: il.negated,
                })
            }

            Expr::Between(b) => {
                // Normalize: `x BETWEEN low AND high` → `x >= low AND x <= high`.
                // `x NOT BETWEEN low AND high` → `x < low OR x > high`.
                if b.negated {
                    let lt = self.compare(&b.expr, CompareOpKind::Lt, &b.low)?;
                    let gt = self.compare(&b.expr, CompareOpKind::Gt, &b.high)?;
                    Ok(Unresolved::BoolOr(vec![lt, gt]))
                } else {
                    let x_low = self.compare(&b.expr, CompareOpKind::Ge, &b.low)?;
                    let x_high = self.compare(&b.expr, CompareOpKind::Le, &b.high)?;
                    Ok(Unresolved::BoolAnd(vec![x_low, x_high]))
                }
            }

            // `NOW()` / `CURRENT_TIMESTAMP` read the SQL statement evaluation
            // time. Keep this timestamp-typed leaf distinct from PromQL's
            // Float64 Unix-seconds `EvalTimestamp`. Issue #184.
            Expr::ScalarFunction(sf)
                if sf.args.is_empty()
                    && matches!(
                        sf.func.name().to_ascii_lowercase().as_str(),
                        "now" | "current_timestamp"
                    ) =>
            {
                Ok(Unresolved::CurrentTimestamp)
            }

            Expr::ScalarFunction(sf) => {
                let args: Result<Vec<_>, _> = sf.args.iter().map(|e| self.lower_expr(e)).collect();
                Ok(Unresolved::FunctionCall {
                    name: if sf.func.name().eq_ignore_ascii_case("arrayelement") {
                        "asap_element_access".into()
                    } else if sf.func.name().eq_ignore_ascii_case("tupleelement") {
                        "asap_struct_field".into()
                    } else if sf.func.name() == super::collection_planning::MAP_PLANNING_NAME {
                        "map".into()
                    } else {
                        sf.func.name().to_string()
                    },
                    args: args?,
                })
            }

            // Subquery-valued expressions. Each subquery plan is lowered as a
            // root of its own; `resolve_root` binds it in its own scope, so an
            // outer reference inside it has nothing to resolve against — a
            // correlated subquery is rejected rather than mislowered.
            Expr::ScalarSubquery(sq) => Ok(Unresolved::ScalarSubquery(Rc::new(
                self.lower_uncorrelated_subquery(sq, "scalar subquery")?,
            ))),
            Expr::Exists(ex) => Ok(Unresolved::Exists {
                subquery: Rc::new(self.lower_uncorrelated_subquery(&ex.subquery, "EXISTS")?),
                negated: ex.negated,
            }),
            Expr::InSubquery(is) => {
                let fields = is.subquery.subquery.schema().fields().len();
                if fields != 1 {
                    return Err(LoweringError::InvalidExpression(format!(
                        "IN (subquery) must select exactly one column, got {fields}"
                    )));
                }
                Ok(Unresolved::InSubquery {
                    expr: bx(&is.expr)?,
                    subquery: Rc::new(
                        self.lower_uncorrelated_subquery(&is.subquery, "IN (subquery)")?,
                    ),
                    negated: is.negated,
                })
            }

            other => Err(LoweringError::UnsupportedFeature(format!(
                "expression: {}",
                other
            ))),
        }
    }

    fn lower_uncorrelated_subquery(
        &self,
        sq: &datafusion::logical_expr::Subquery,
        what: &str,
    ) -> Result<asap_frontend_common::UnresolvedOp, LoweringError> {
        if !sq.outer_ref_columns.is_empty() {
            return Err(LoweringError::UnsupportedFeature(format!(
                "correlated {what}"
            )));
        }
        self.lower_plan(&sq.subquery)
    }

    pub(super) fn compare(
        &self,
        left: &Expr,
        op: CompareOpKind,
        right: &Expr,
    ) -> Result<Unresolved, LoweringError> {
        Ok(Unresolved::Compare {
            left: Box::new(self.lower_expr(left)?),
            op,
            right: Box::new(self.lower_expr(right)?),
            semantics: ExprSemantics::Sql,
        })
    }

    fn arith(
        &self,
        left: &Expr,
        op: ArithmeticOpKind,
        right: &Expr,
    ) -> Result<Unresolved, LoweringError> {
        Ok(Unresolved::Arithmetic {
            op,
            left: Box::new(self.lower_expr(left)?),
            right: Box::new(self.lower_expr(right)?),
            semantics: ExprSemantics::Sql,
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unified::sql::SqlCatalog;
    use asap_types::pre_asap::schema::DataType;
    use datafusion::common::ScalarValue as DfScalarValue;

    // Typed Arrow dates normalize to the same typed form as SQL date casts.
    #[test]
    fn arrow_date_literals_preserve_value_and_type() {
        let catalog = SqlCatalog::new();
        let lowerer = SqlLowerer::new(&catalog);
        for (value, expected) in [
            (
                DfScalarValue::Date32(Some(0)),
                ScalarValue::Utf8("1970-01-01".into()),
            ),
            (
                DfScalarValue::Date64(Some(-86_400_000)),
                ScalarValue::Utf8("1969-12-31".into()),
            ),
            (DfScalarValue::Date32(None), ScalarValue::Null),
            (DfScalarValue::Date64(None), ScalarValue::Null),
        ] {
            let actual = lowerer.lower_expr(&Expr::Literal(value, None)).unwrap();
            assert_eq!(
                actual,
                Unresolved::Cast {
                    expr: Box::new(Unresolved::Literal(expected)),
                    to: DataType::Date,
                    try_cast: false,
                }
            );
        }
    }

    // Unary minus over a non-literal is the `Negative` scalar, SQL-flavoured.
    #[test]
    fn unary_minus_lowers_to_negative_with_sql_semantics() {
        let catalog = SqlCatalog::new();
        let lowerer = SqlLowerer::new(&catalog);
        let expr = Expr::Negative(Box::new(Expr::Column(
            datafusion::common::Column::new_unqualified("x"),
        )));
        assert_eq!(
            lowerer.lower_expr(&expr).unwrap(),
            Unresolved::Negative {
                expr: Box::new(Unresolved::Column(ColumnRef::Named("x".into()))),
                semantics: ExprSemantics::Sql,
            }
        );
    }
}
