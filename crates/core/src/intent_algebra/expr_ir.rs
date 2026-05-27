//! Language-independent scalar expression IR.
//!
//! There are **two** scalar expression types, mirroring the lowering boundary:
//!
//! - [`L2Expr`] — name-based ([`Column(ColumnRef)`](ColumnRef)). The per-language
//!   front ends emit it (PromQL label matchers, SQL `WHERE` / projection /
//!   sort-key expressions) on the Layer-2 `relational` tree.
//! - [`L3Expr`] — **positional** ([`Column(ColumnId)`](ColumnId)). The canonical
//!   L3 `query_expr` tree carries it; the converter resolves every `L2Expr`
//!   column reference against the in-scope schema to produce it, so L3 column
//!   identity is unambiguous (no name collisions across a join).
//!
//! Both share the same shape and the scalar/operator vocabulary
//! ([`L3Scalar`], [`CompareOp`], [`ArithOp`]) — the **union** of what the two
//! front ends need: PromQL contributes `Regex` / `NotRegex` (`=~` / `!~`); SQL
//! contributes arithmetic, `CASE`, `IN`, `CAST`, `IS [NOT] NULL`, scalar
//! function calls, and the `LIKE` / `ILIKE` comparison family.

use serde::{Deserialize, Serialize};

use super::schema::{ColumnId, DataType};

/// A name-based column reference. This is an L2 / front-end concept — the
/// converter resolves every `ColumnRef` into a positional [`ColumnId`], so it
/// does not appear in the L3 [`QueryExpr`](super::query_expr::QueryExpr). It
/// includes the two PromQL-conventional synthetic columns.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColumnRef {
    Named(String),
    /// Table-qualified reference (`t.col` / `alias.col`). Resolved by
    /// `(table, name)` so a column name shared across a join (`a.k` vs `b.k`)
    /// binds to the correct side.
    Qualified {
        table: String,
        name: String,
    },
    /// The implicit metric sample value (PromQL — always the series value).
    SampleValue,
    /// All rows / COUNT(*).
    Wildcard,
}

/// A typed scalar constant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum L3Scalar {
    Int64(i64),
    Float64(f64),
    Utf8(String),
    Boolean(bool),
    Null,
}

/// Binary comparison operators.
///
/// `Regex` / `NotRegex` carry PromQL/RE2 regex-match semantics (`=~` / `!~`):
/// the right-hand side is a regular-expression pattern, not a literal value.
/// `Like` / `ILike` (+ negations) are the SQL pattern-match analogues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// SQL `LIKE` — RHS is a `%`/`_` glob pattern.
    Like,
    /// SQL `NOT LIKE`.
    NotLike,
    /// SQL `ILIKE` — case-insensitive `LIKE`.
    ILike,
    /// SQL `NOT ILIKE`.
    NotILike,
    /// RHS is a regular-expression pattern; matches PromQL `=~`.
    Regex,
    /// RHS is a regular-expression pattern; matches PromQL `!~`.
    NotRegex,
}

impl std::fmt::Display for CompareOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CompareOp::Eq => "==",
            CompareOp::Ne => "!=",
            CompareOp::Lt => "<",
            CompareOp::Le => "<=",
            CompareOp::Gt => ">",
            CompareOp::Ge => ">=",
            CompareOp::Like => "LIKE",
            CompareOp::NotLike => "NOT LIKE",
            CompareOp::ILike => "ILIKE",
            CompareOp::NotILike => "NOT ILIKE",
            CompareOp::Regex => "=~",
            CompareOp::NotRegex => "!~",
        })
    }
}

/// Binary arithmetic operators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

impl std::fmt::Display for ArithOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
            ArithOp::Mod => "%",
        })
    }
}

/// Layer-2 (name-based) scalar expression. Front ends emit this; the converter
/// resolves it into a positional [`L3Expr`]. Flat conjunctions (`BoolAnd`) /
/// disjunctions (`BoolOr`) make per-conjunct selectivity estimation and
/// label-matcher lowering straightforward without recursive descent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum L2Expr {
    /// Reference to a named column / label.
    Column(ColumnRef),
    /// A constant literal value.
    Literal(L3Scalar),
    /// `left op right` — binary comparison.
    Compare {
        left: Box<L2Expr>,
        op: CompareOp,
        right: Box<L2Expr>,
    },
    /// Flat conjunction (logical AND). An empty list is vacuously true.
    BoolAnd(Vec<L2Expr>),
    /// Flat disjunction (logical OR). An empty list is vacuously false.
    BoolOr(Vec<L2Expr>),
    /// Logical NOT.
    Not(Box<L2Expr>),
    /// `expr IS NULL`.
    IsNull(Box<L2Expr>),
    /// `expr IS NOT NULL`.
    IsNotNull(Box<L2Expr>),
    /// `CAST(expr AS to)`; `try_cast` for SQL `TRY_CAST` (NULL on failure).
    Cast {
        expr: Box<L2Expr>,
        to: DataType,
        try_cast: bool,
    },
    /// `expr [NOT] IN (v1, v2, …)`.
    InList {
        expr: Box<L2Expr>,
        list: Vec<L2Expr>,
        negated: bool,
    },
    /// Scalar function call, e.g. `LOWER(col)`, `ABS(x)`.
    FunctionCall { name: String, args: Vec<L2Expr> },
    /// Binary arithmetic: `left op right`.
    Arith {
        op: ArithOp,
        left: Box<L2Expr>,
        right: Box<L2Expr>,
    },
    /// SQL `CASE` (both searched and simple forms). `operand` present for the
    /// simple form (`CASE expr WHEN …`), absent for searched.
    Case {
        operand: Option<Box<L2Expr>>,
        branches: Vec<(L2Expr, L2Expr)>,
        else_expr: Option<Box<L2Expr>>,
    },
}

impl L2Expr {
    /// If this expression is a `BoolAnd`, return its elements; otherwise a
    /// single-element slice containing `self`.
    pub fn conjuncts(&self) -> &[L2Expr] {
        match self {
            L2Expr::BoolAnd(v) => v.as_slice(),
            _ => std::slice::from_ref(self),
        }
    }

    /// If this expression is a `BoolOr`, return its elements; otherwise a
    /// single-element slice containing `self`.
    pub fn disjuncts(&self) -> &[L2Expr] {
        match self {
            L2Expr::BoolOr(v) => v.as_slice(),
            _ => std::slice::from_ref(self),
        }
    }

    /// Recursively collect every `ColumnRef` referenced anywhere in this
    /// expression. Used by the Binder to seed usage-derived leaf schemas.
    pub fn columns_referenced(&self) -> Vec<&ColumnRef> {
        match self {
            L2Expr::Column(c) => vec![c],
            L2Expr::Literal(_) => vec![],
            L2Expr::Compare { left, right, .. } | L2Expr::Arith { left, right, .. } => {
                let mut v = left.columns_referenced();
                v.extend(right.columns_referenced());
                v
            }
            L2Expr::BoolAnd(parts) | L2Expr::BoolOr(parts) => {
                parts.iter().flat_map(|e| e.columns_referenced()).collect()
            }
            L2Expr::Not(e) | L2Expr::IsNull(e) | L2Expr::IsNotNull(e) => e.columns_referenced(),
            L2Expr::Cast { expr, .. } => expr.columns_referenced(),
            L2Expr::InList { expr, list, .. } => {
                let mut v = expr.columns_referenced();
                v.extend(list.iter().flat_map(|e| e.columns_referenced()));
                v
            }
            L2Expr::FunctionCall { args, .. } => {
                args.iter().flat_map(|e| e.columns_referenced()).collect()
            }
            L2Expr::Case {
                operand,
                branches,
                else_expr,
            } => {
                let mut v = vec![];
                if let Some(op) = operand {
                    v.extend(op.columns_referenced());
                }
                for (when, then) in branches {
                    v.extend(when.columns_referenced());
                    v.extend(then.columns_referenced());
                }
                if let Some(e) = else_expr {
                    v.extend(e.columns_referenced());
                }
                v
            }
        }
    }
}

/// Canonical L3 (positional) scalar expression. Same shape as [`L2Expr`] but
/// column references are positional [`ColumnId`]s resolved against the in-scope
/// schema, so identity is unambiguous across joins / duplicate names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum L3Expr {
    /// Positional reference to a column in the in-scope schema.
    Column(ColumnId),
    /// A constant literal value.
    Literal(L3Scalar),
    /// `left op right` — binary comparison.
    Compare {
        left: Box<L3Expr>,
        op: CompareOp,
        right: Box<L3Expr>,
    },
    /// Flat conjunction (logical AND). An empty list is vacuously true.
    BoolAnd(Vec<L3Expr>),
    /// Flat disjunction (logical OR). An empty list is vacuously false.
    BoolOr(Vec<L3Expr>),
    /// Logical NOT.
    Not(Box<L3Expr>),
    /// `expr IS NULL`.
    IsNull(Box<L3Expr>),
    /// `expr IS NOT NULL`.
    IsNotNull(Box<L3Expr>),
    /// `CAST(expr AS to)`; `try_cast` for SQL `TRY_CAST` (NULL on failure).
    Cast {
        expr: Box<L3Expr>,
        to: DataType,
        try_cast: bool,
    },
    /// `expr [NOT] IN (v1, v2, …)`.
    InList {
        expr: Box<L3Expr>,
        list: Vec<L3Expr>,
        negated: bool,
    },
    /// Scalar function call, e.g. `LOWER(col)`, `ABS(x)`.
    FunctionCall { name: String, args: Vec<L3Expr> },
    /// Binary arithmetic: `left op right`.
    Arith {
        op: ArithOp,
        left: Box<L3Expr>,
        right: Box<L3Expr>,
    },
    /// SQL `CASE` (both searched and simple forms).
    Case {
        operand: Option<Box<L3Expr>>,
        branches: Vec<(L3Expr, L3Expr)>,
        else_expr: Option<Box<L3Expr>>,
    },
}

impl L3Expr {
    /// If this expression is a `BoolAnd`, return its elements; otherwise a
    /// single-element slice containing `self`.
    pub fn conjuncts(&self) -> &[L3Expr] {
        match self {
            L3Expr::BoolAnd(v) => v.as_slice(),
            _ => std::slice::from_ref(self),
        }
    }

    /// If this expression is a `BoolOr`, return its elements; otherwise a
    /// single-element slice containing `self`.
    pub fn disjuncts(&self) -> &[L3Expr] {
        match self {
            L3Expr::BoolOr(v) => v.as_slice(),
            _ => std::slice::from_ref(self),
        }
    }

    /// Recursively collect every positional [`ColumnId`] referenced anywhere in
    /// this expression. Used by L4 for column-lineage and selectivity.
    pub fn columns_referenced(&self) -> Vec<ColumnId> {
        match self {
            L3Expr::Column(id) => vec![*id],
            L3Expr::Literal(_) => vec![],
            L3Expr::Compare { left, right, .. } | L3Expr::Arith { left, right, .. } => {
                let mut v = left.columns_referenced();
                v.extend(right.columns_referenced());
                v
            }
            L3Expr::BoolAnd(parts) | L3Expr::BoolOr(parts) => {
                parts.iter().flat_map(|e| e.columns_referenced()).collect()
            }
            L3Expr::Not(e) | L3Expr::IsNull(e) | L3Expr::IsNotNull(e) => e.columns_referenced(),
            L3Expr::Cast { expr, .. } => expr.columns_referenced(),
            L3Expr::InList { expr, list, .. } => {
                let mut v = expr.columns_referenced();
                v.extend(list.iter().flat_map(|e| e.columns_referenced()));
                v
            }
            L3Expr::FunctionCall { args, .. } => {
                args.iter().flat_map(|e| e.columns_referenced()).collect()
            }
            L3Expr::Case {
                operand,
                branches,
                else_expr,
            } => {
                let mut v = vec![];
                if let Some(op) = operand {
                    v.extend(op.columns_referenced());
                }
                for (when, then) in branches {
                    v.extend(when.columns_referenced());
                    v.extend(then.columns_referenced());
                }
                if let Some(e) = else_expr {
                    v.extend(e.columns_referenced());
                }
                v
            }
        }
    }
}
