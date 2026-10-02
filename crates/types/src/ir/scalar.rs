//! Scalar expressions: value computation evaluated within the schema chosen by
//! the operator that owns them.
//!
//! A [`ScalarExpr`] never produces a table. It is owned by value by an operator
//! field (`Filter.pred`, `ProjectItem.expr`, `SortKey.expr`, `HAVING`, window
//! arguments, relabel values) or by a [`super::NonASAPOp::ScalarBridge`] leaf.
//! The only operator references inside a scalar tree are the explicit
//! plan-reading variants (`PromqlScalarFromVector`, `ScalarSubquery`, `Exists`,
//! `InSubquery`); every traversal of the operator DAG follows them.

use std::rc::Rc;

use serde::{Deserialize, Serialize};

use super::node::OperatorNode;
use crate::pre_asap::expr_ir::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use crate::pre_asap::query_expr::QueryExprError;
use crate::pre_asap::scalar_signature::MapScalarFunction;
use crate::pre_asap::schema::{ColumnId, DataType, Schema};

/// Which language's numeric and comparison rules an expression follows.
/// Both languages use `Float64`, so a result type alone does not preserve
/// NaN, ordering or error rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExprSemantics {
    Sql,
    Promql,
}

/// A scalar expression over the owning operator's input schema. Column
/// references are positional [`ColumnId`]s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ScalarExpr {
    Column(ColumnId),
    Literal(ScalarValue),
    /// Unary minus.
    Negative {
        expr: Box<ScalarExpr>,
        semantics: ExprSemantics,
    },
    Compare {
        left: Box<ScalarExpr>,
        op: CompareOpKind,
        right: Box<ScalarExpr>,
        semantics: ExprSemantics,
    },
    /// Flat conjunction (logical AND). An empty list is vacuously true.
    BoolAnd(Vec<ScalarExpr>),
    /// Flat disjunction (logical OR). An empty list is vacuously false.
    BoolOr(Vec<ScalarExpr>),
    Not(Box<ScalarExpr>),
    IsNull(Box<ScalarExpr>),
    IsNotNull(Box<ScalarExpr>),
    /// `CAST(expr AS to)`; `try_cast` for SQL `TRY_CAST` (NULL on failure).
    Cast {
        expr: Box<ScalarExpr>,
        to: DataType,
        try_cast: bool,
    },
    /// `expr [NOT] IN (v1, v2, …)`.
    InList {
        expr: Box<ScalarExpr>,
        list: Vec<ScalarExpr>,
        negated: bool,
    },
    /// Scalar function call, e.g. `LOWER(col)`, `ABS(x)`.
    FunctionCall { name: String, args: Vec<ScalarExpr> },
    Arithmetic {
        op: ArithmeticOpKind,
        left: Box<ScalarExpr>,
        right: Box<ScalarExpr>,
        semantics: ExprSemantics,
    },
    /// SQL `CASE` (both searched and simple forms). `operand` present for the
    /// simple form (`CASE expr WHEN …`), absent for searched.
    Case {
        operand: Option<Box<ScalarExpr>>,
        branches: Vec<(ScalarExpr, ScalarExpr)>,
        else_expr: Option<Box<ScalarExpr>>,
    },
    /// SQL `NOW()` / `CURRENT_TIMESTAMP`: the statement evaluation time.
    CurrentTimestamp,
    /// PromQL `time()`: the evaluation instant as Unix seconds (`Float64`).
    EvalTimestamp,
    /// PromQL `scalar(v)`: the single sample of an instant vector, NaN
    /// otherwise. The referenced operator is a real plan dependency.
    PromqlScalarFromVector(Rc<OperatorNode>),
    /// An uncorrelated SQL scalar subquery: one column; zero rows is NULL,
    /// more than one row is an error.
    ScalarSubquery(Rc<OperatorNode>),
    /// SQL `[NOT] EXISTS (subquery)`.
    Exists {
        subquery: Rc<OperatorNode>,
        negated: bool,
    },
    /// SQL `expr [NOT] IN (subquery)` over a one-column relation.
    InSubquery {
        expr: Box<ScalarExpr>,
        subquery: Rc<OperatorNode>,
        negated: bool,
    },
}

/// A row-level filter predicate (WHERE clause / PromQL label matcher).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Predicate(pub ScalarExpr);

/// One item in a SELECT projection list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectItem {
    pub alias: Option<String>,
    pub expr: ScalarExpr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SortKey {
    pub expr: ScalarExpr,
    pub ascending: bool,
    pub nulls_first: bool,
}

impl ScalarExpr {
    pub fn literal_f64(v: f64) -> Self {
        ScalarExpr::Literal(ScalarValue::Float64(v))
    }

    pub fn column(id: ColumnId) -> Self {
        ScalarExpr::Column(id)
    }

    /// If this expression is a `BoolAnd`, its elements; otherwise `self` alone.
    pub fn conjuncts(&self) -> &[ScalarExpr] {
        match self {
            ScalarExpr::BoolAnd(v) => v.as_slice(),
            _ => std::slice::from_ref(self),
        }
    }

    /// If this expression is a `BoolOr`, its elements; otherwise `self` alone.
    pub fn disjuncts(&self) -> &[ScalarExpr] {
        match self {
            ScalarExpr::BoolOr(v) => v.as_slice(),
            _ => std::slice::from_ref(self),
        }
    }

    /// The direct scalar sub-expressions.
    pub fn children(&self) -> Vec<&ScalarExpr> {
        match self {
            ScalarExpr::Column(_)
            | ScalarExpr::Literal(_)
            | ScalarExpr::CurrentTimestamp
            | ScalarExpr::EvalTimestamp
            | ScalarExpr::PromqlScalarFromVector(_)
            | ScalarExpr::ScalarSubquery(_)
            | ScalarExpr::Exists { .. } => vec![],
            ScalarExpr::Negative { expr, .. }
            | ScalarExpr::Not(expr)
            | ScalarExpr::IsNull(expr)
            | ScalarExpr::IsNotNull(expr)
            | ScalarExpr::Cast { expr, .. }
            | ScalarExpr::InSubquery { expr, .. } => vec![expr],
            ScalarExpr::Compare { left, right, .. } | ScalarExpr::Arithmetic { left, right, .. } => {
                vec![left, right]
            }
            ScalarExpr::BoolAnd(parts) | ScalarExpr::BoolOr(parts) => parts.iter().collect(),
            ScalarExpr::InList { expr, list, .. } => {
                let mut v = vec![expr.as_ref()];
                v.extend(list.iter());
                v
            }
            ScalarExpr::FunctionCall { args, .. } => args.iter().collect(),
            ScalarExpr::Case {
                operand,
                branches,
                else_expr,
            } => {
                let mut v = Vec::new();
                if let Some(op) = operand {
                    v.push(op.as_ref());
                }
                for (when, then) in branches {
                    v.push(when);
                    v.push(then);
                }
                if let Some(e) = else_expr {
                    v.push(e.as_ref());
                }
                v
            }
        }
    }

    /// The operator nodes this expression (transitively) reads: the explicit
    /// plan-reading variants. Every DAG traversal must follow these.
    pub fn operator_refs(&self) -> Vec<&Rc<OperatorNode>> {
        let mut out = Vec::new();
        self.collect_operator_refs(&mut out);
        out
    }

    fn collect_operator_refs<'a>(&'a self, out: &mut Vec<&'a Rc<OperatorNode>>) {
        match self {
            ScalarExpr::PromqlScalarFromVector(node) | ScalarExpr::ScalarSubquery(node) => {
                out.push(node)
            }
            ScalarExpr::Exists { subquery, .. } => out.push(subquery),
            ScalarExpr::InSubquery { subquery, .. } => out.push(subquery),
            _ => {}
        }
        for child in self.children() {
            child.collect_operator_refs(out);
        }
    }

    /// Rebuild this expression with `f` applied to every operator node it
    /// reads (recursively through scalar children).
    pub fn map_operator_refs(
        &self,
        f: &mut impl FnMut(&Rc<OperatorNode>) -> Rc<OperatorNode>,
    ) -> ScalarExpr {
        fn map_box<F: FnMut(&Rc<OperatorNode>) -> Rc<OperatorNode>>(
            e: &ScalarExpr,
            f: &mut F,
        ) -> Box<ScalarExpr> {
            Box::new(e.map_operator_refs(f))
        }
        match self {
            ScalarExpr::Column(_)
            | ScalarExpr::Literal(_)
            | ScalarExpr::CurrentTimestamp
            | ScalarExpr::EvalTimestamp => self.clone(),
            ScalarExpr::PromqlScalarFromVector(node) => ScalarExpr::PromqlScalarFromVector(f(node)),
            ScalarExpr::ScalarSubquery(node) => ScalarExpr::ScalarSubquery(f(node)),
            ScalarExpr::Exists { subquery, negated } => ScalarExpr::Exists {
                subquery: f(subquery),
                negated: *negated,
            },
            ScalarExpr::InSubquery {
                expr,
                subquery,
                negated,
            } => ScalarExpr::InSubquery {
                expr: map_box(expr, f),
                subquery: f(subquery),
                negated: *negated,
            },
            ScalarExpr::Negative { expr, semantics } => ScalarExpr::Negative {
                expr: map_box(expr, f),
                semantics: *semantics,
            },
            ScalarExpr::Compare {
                left,
                op,
                right,
                semantics,
            } => ScalarExpr::Compare {
                left: map_box(left, f),
                op: op.clone(),
                right: map_box(right, f),
                semantics: *semantics,
            },
            ScalarExpr::BoolAnd(parts) => {
                ScalarExpr::BoolAnd(parts.iter().map(|p| p.map_operator_refs(f)).collect())
            }
            ScalarExpr::BoolOr(parts) => {
                ScalarExpr::BoolOr(parts.iter().map(|p| p.map_operator_refs(f)).collect())
            }
            ScalarExpr::Not(e) => ScalarExpr::Not(map_box(e, f)),
            ScalarExpr::IsNull(e) => ScalarExpr::IsNull(map_box(e, f)),
            ScalarExpr::IsNotNull(e) => ScalarExpr::IsNotNull(map_box(e, f)),
            ScalarExpr::Cast { expr, to, try_cast } => ScalarExpr::Cast {
                expr: map_box(expr, f),
                to: to.clone(),
                try_cast: *try_cast,
            },
            ScalarExpr::InList {
                expr,
                list,
                negated,
            } => ScalarExpr::InList {
                expr: map_box(expr, f),
                list: list.iter().map(|p| p.map_operator_refs(f)).collect(),
                negated: *negated,
            },
            ScalarExpr::FunctionCall { name, args } => ScalarExpr::FunctionCall {
                name: name.clone(),
                args: args.iter().map(|p| p.map_operator_refs(f)).collect(),
            },
            ScalarExpr::Arithmetic {
                op,
                left,
                right,
                semantics,
            } => ScalarExpr::Arithmetic {
                op: op.clone(),
                left: map_box(left, f),
                right: map_box(right, f),
                semantics: *semantics,
            },
            ScalarExpr::Case {
                operand,
                branches,
                else_expr,
            } => ScalarExpr::Case {
                operand: operand.as_ref().map(|e| map_box(e, f)),
                branches: branches
                    .iter()
                    .map(|(w, t)| (w.map_operator_refs(f), t.map_operator_refs(f)))
                    .collect(),
                else_expr: else_expr.as_ref().map(|e| map_box(e, f)),
            },
        }
    }

    /// Every column referenced anywhere in this expression (not inside
    /// referenced operator subgraphs, which have their own scope).
    pub fn columns_referenced(&self) -> Vec<ColumnId> {
        let mut out = Vec::new();
        self.collect_columns(&mut out);
        out
    }

    fn collect_columns(&self, out: &mut Vec<ColumnId>) {
        if let ScalarExpr::Column(id) = self {
            out.push(*id);
        }
        for child in self.children() {
            child.collect_columns(out);
        }
    }

    /// Infer the `(DataType, nullable)` this expression produces against the
    /// input schema its owner evaluates it in. Approximate for unknown
    /// functions (post-ASAP binding refines with a real function/type
    /// registry). A reference to a field carrying summary state is an error:
    /// state must be read out before an expression can use it.
    pub fn scalar_type(&self, schema: &Schema) -> Result<(DataType, bool), QueryExprError> {
        Ok(match self {
            ScalarExpr::CurrentTimestamp => (DataType::Timestamp, false),
            ScalarExpr::EvalTimestamp => (DataType::Float64, false),
            ScalarExpr::Column(id) => match schema.fields.get(*id) {
                Some(c) => match c.plain_dtype() {
                    Some(dtype) => (dtype.clone(), c.nullable),
                    None => {
                        return Err(QueryExprError::InvalidScalarSignature(format!(
                            "column `{}` carries summary state and cannot be read as a value",
                            c.name
                        )))
                    }
                },
                None => (DataType::Float64, true),
            },
            ScalarExpr::Literal(s) => match s {
                ScalarValue::Int64(_) => (DataType::Int64, false),
                ScalarValue::Float64(_) => (DataType::Float64, false),
                ScalarValue::Utf8(_) => (DataType::Utf8, false),
                ScalarValue::Boolean(_) => (DataType::Bool, false),
                ScalarValue::Null => (DataType::Null, true),
                ScalarValue::Interval { .. } => (DataType::Interval, false),
            },
            // Boolean-valued expressions (SQL three-valued logic → nullable).
            ScalarExpr::Compare { .. }
            | ScalarExpr::BoolAnd(_)
            | ScalarExpr::BoolOr(_)
            | ScalarExpr::Not(_)
            | ScalarExpr::IsNull(_)
            | ScalarExpr::IsNotNull(_)
            | ScalarExpr::InList { .. }
            | ScalarExpr::Exists { .. }
            | ScalarExpr::InSubquery { .. } => (DataType::Bool, true),
            ScalarExpr::Negative { expr, .. } => {
                let (dtype, nullable) = expr.scalar_type(schema)?;
                (dtype, nullable)
            }
            ScalarExpr::Arithmetic {
                op, left, right, ..
            } => {
                let (lt, ln) = left.scalar_type(schema)?;
                let (rt, rn) = right.scalar_type(schema)?;
                // Temporal subtraction yields a fixed duration with a unit, not a
                // calendar interval or a floating-point number. Until the IR can
                // preserve that unit, fail instead of publishing a numeric schema.
                if matches!(op, ArithmeticOpKind::Sub)
                    && matches!(lt, DataType::Date | DataType::Timestamp)
                    && matches!(rt, DataType::Date | DataType::Timestamp)
                {
                    return Err(QueryExprError::InvalidScalarSignature(
                        "temporal subtraction produces an unsupported duration type".into(),
                    ));
                }
                let dtype = match (&lt, &rt) {
                    (DataType::Int64, DataType::Interval) | (DataType::Interval, DataType::Int64)
                        if matches!(op, ArithmeticOpKind::Mul) =>
                    {
                        DataType::Interval
                    }
                    (DataType::Timestamp, DataType::Interval)
                    | (DataType::Interval, DataType::Timestamp) => DataType::Timestamp,
                    (DataType::Date, DataType::Interval) | (DataType::Interval, DataType::Date) => {
                        DataType::Date
                    }
                    (DataType::Interval, DataType::Interval) => DataType::Interval,
                    (DataType::Int64, DataType::Int64) => DataType::Int64,
                    _ => DataType::Float64,
                };
                (dtype, ln || rn)
            }
            ScalarExpr::Cast { to, try_cast, expr } => {
                let (_, nullable) = expr.scalar_type(schema)?;
                (to.clone(), *try_cast || nullable)
            }
            ScalarExpr::FunctionCall { name, args } => {
                if name == "asap_element_access" {
                    element_access_type(args, schema)
                        .map_err(QueryExprError::InvalidScalarSignature)?
                } else if name == "asap_struct_field" {
                    struct_field_type(args, schema)
                        .map_err(QueryExprError::InvalidScalarSignature)?
                } else if let Some(function) = MapScalarFunction::from_name(name) {
                    let arguments = args
                        .iter()
                        .map(|arg| arg.scalar_type(schema))
                        .collect::<Result<Vec<_>, _>>()?;
                    function
                        .output_type(&arguments)
                        .map_err(QueryExprError::InvalidScalarSignature)?
                } else {
                    // Unknown functions retain the permissive legacy policy.
                    (DataType::Float64, true)
                }
            }
            ScalarExpr::Case {
                branches,
                else_expr,
                ..
            } => {
                if let Some((_, then)) = branches.first() {
                    (then.scalar_type(schema)?.0, true)
                } else if let Some(other) = else_expr {
                    other.scalar_type(schema)?
                } else {
                    (DataType::Null, true)
                }
            }
            // `scalar(v)` is one float sample (NaN when the vector is not
            // exactly one series); a scalar subquery is its single column.
            ScalarExpr::PromqlScalarFromVector(_) => (DataType::Float64, false),
            ScalarExpr::ScalarSubquery(node) => match node.schema.fields.first() {
                Some(field) => match field.plain_dtype() {
                    Some(dtype) => (dtype.clone(), true),
                    None => {
                        return Err(QueryExprError::InvalidScalarSignature(
                            "scalar subquery column carries summary state".into(),
                        ))
                    }
                },
                None => {
                    return Err(QueryExprError::InvalidScalarSignature(
                        "scalar subquery produces no column".into(),
                    ))
                }
            },
        })
    }
}

/// Resolve the bounded canonical `asap_struct_field(struct, selector)` operation.
/// Selectors are positive 1-based literal ordinals or exact literal field names.
fn struct_field_type(args: &[ScalarExpr], schema: &Schema) -> Result<(DataType, bool), String> {
    let [input, selector] = args else {
        return Err("struct field access requires a struct and constant selector".into());
    };
    let (dtype, nullable) = input.scalar_type(schema).map_err(|error| error.to_string())?;
    if nullable {
        return Err("nullable struct container access is unsupported".into());
    }
    let DataType::Struct { fields } = dtype else {
        return Err("struct field access requires a Struct input".into());
    };
    let field = match selector {
        ScalarExpr::Literal(ScalarValue::Int64(index)) if *index > 0 => {
            usize::try_from(*index - 1)
                .ok()
                .and_then(|index| fields.get(index))
                .ok_or("struct field ordinal is out of bounds")?
        }
        ScalarExpr::Literal(ScalarValue::Utf8(name)) => {
            let mut matches = fields.iter().filter(|field| field.name == *name);
            let field = matches.next().ok_or("struct field name does not exist")?;
            if matches.next().is_some() {
                return Err("struct field name is ambiguous".into());
            }
            field
        }
        _ => {
            return Err(
                "struct field selector must be a positive ordinal or field-name literal".into(),
            )
        }
    };
    Ok((field.dtype.clone(), field.nullable))
}

/// Resolve `asap_element_access(collection, index)`: Map access through the
/// map function contract, List access with integer indices.
fn element_access_type(args: &[ScalarExpr], schema: &Schema) -> Result<(DataType, bool), String> {
    let [input, index] = args else {
        return Err("element access requires a collection and index".into());
    };
    let source = input.scalar_type(schema).map_err(|e| e.to_string())?;
    let key = index.scalar_type(schema).map_err(|e| e.to_string())?;
    match &source.0 {
        DataType::Map { .. } => MapScalarFunction::Access.output_type(&[source, key]),
        DataType::List { element } => {
            if source.1 {
                return Err("nullable List container access is unsupported".into());
            }
            if !matches!(key.0, DataType::Int64 | DataType::Null) {
                return Err("List index must have integer type".into());
            }
            if matches!(index, ScalarExpr::Literal(ScalarValue::Int64(0))) {
                return Err(
                    "literal zero List index is unsupported without constant-array proof".into(),
                );
            }
            Ok((
                element.dtype.clone(),
                element.nullable || key.1 || key.0 == DataType::Null,
            ))
        }
        _ => Err("element access requires a Map or List".into()),
    }
}
