//! Language-independent scalar expression IR.
//!
//! One generic [`Expr<C>`] spans the lowering boundary; the two layers are
//! aliases that differ only in the column-reference type `C`:
//!
//! - [`L2Expr`] = `Expr<ColumnRef>` — name-based. The per-language front ends
//!   emit it (PromQL label matchers, SQL `WHERE` / projection / sort-key
//!   expressions) on the Layer-2 `relational` tree.
//! - [`L3Expr`] = `Expr<ColumnId>` — **positional**. The canonical L3
//!   `query_expr` tree carries it; the converter resolves every `ColumnRef`
//!   against the in-scope schema to produce it, so L3 column identity is
//!   unambiguous (no name collisions across a join).
//!
//! `Expr<C>` shares the scalar/operator vocabulary
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

/// Scalar expression, generic over its column-reference type `C`. The two
/// lowering layers are aliases over the *same* shape — only the column
/// reference differs — so there is one definition (and one set of helpers) to
/// maintain, and the converter is a structural map that swaps `C`:
///
/// - [`L2Expr`] = `Expr<ColumnRef>` — name-based, front-end-emitted.
/// - [`L3Expr`] = `Expr<ColumnId>` — positional, resolved against the schema.
///
/// Flat conjunctions (`BoolAnd`) / disjunctions (`BoolOr`) make per-conjunct
/// selectivity estimation and label-matcher lowering straightforward without
/// recursive descent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Expr<C> {
    /// A column reference — `ColumnRef` (L2) or positional `ColumnId` (L3).
    Column(C),
    /// A constant literal value.
    Literal(L3Scalar),
    /// `left op right` — binary comparison.
    Compare {
        left: Box<Expr<C>>,
        op: CompareOp,
        right: Box<Expr<C>>,
    },
    /// Flat conjunction (logical AND). An empty list is vacuously true.
    BoolAnd(Vec<Expr<C>>),
    /// Flat disjunction (logical OR). An empty list is vacuously false.
    BoolOr(Vec<Expr<C>>),
    /// Logical NOT.
    Not(Box<Expr<C>>),
    /// `expr IS NULL`.
    IsNull(Box<Expr<C>>),
    /// `expr IS NOT NULL`.
    IsNotNull(Box<Expr<C>>),
    /// `CAST(expr AS to)`; `try_cast` for SQL `TRY_CAST` (NULL on failure).
    Cast {
        expr: Box<Expr<C>>,
        to: DataType,
        try_cast: bool,
    },
    /// `expr [NOT] IN (v1, v2, …)`.
    InList {
        expr: Box<Expr<C>>,
        list: Vec<Expr<C>>,
        negated: bool,
    },
    /// Scalar function call, e.g. `LOWER(col)`, `ABS(x)`.
    FunctionCall { name: String, args: Vec<Expr<C>> },
    /// Binary arithmetic: `left op right`.
    Arith {
        op: ArithOp,
        left: Box<Expr<C>>,
        right: Box<Expr<C>>,
    },
    /// SQL `CASE` (both searched and simple forms). `operand` present for the
    /// simple form (`CASE expr WHEN …`), absent for searched.
    Case {
        operand: Option<Box<Expr<C>>>,
        branches: Vec<(Expr<C>, Expr<C>)>,
        else_expr: Option<Box<Expr<C>>>,
    },
}

/// Layer-2 (name-based) scalar expression. Front ends emit this; the converter
/// resolves it into a positional [`L3Expr`].
pub type L2Expr = Expr<ColumnRef>;

/// Canonical L3 (positional) scalar expression — column references are
/// [`ColumnId`]s resolved against the in-scope schema, so identity is
/// unambiguous across joins / duplicate names.
pub type L3Expr = Expr<ColumnId>;

impl<C> Expr<C> {
    /// If this expression is a `BoolAnd`, return its elements; otherwise a
    /// single-element slice containing `self`.
    pub fn conjuncts(&self) -> &[Expr<C>] {
        match self {
            Expr::BoolAnd(v) => v.as_slice(),
            _ => std::slice::from_ref(self),
        }
    }

    /// If this expression is a `BoolOr`, return its elements; otherwise a
    /// single-element slice containing `self`.
    pub fn disjuncts(&self) -> &[Expr<C>] {
        match self {
            Expr::BoolOr(v) => v.as_slice(),
            _ => std::slice::from_ref(self),
        }
    }

    /// Recursively collect every column reference anywhere in this expression.
    /// Used by the Binder (L2) to seed usage-derived leaf schemas, and available
    /// to L4 (L3) for column-lineage / selectivity.
    pub fn columns_referenced(&self) -> Vec<&C> {
        match self {
            Expr::Column(c) => vec![c],
            Expr::Literal(_) => vec![],
            Expr::Compare { left, right, .. } | Expr::Arith { left, right, .. } => {
                let mut v = left.columns_referenced();
                v.extend(right.columns_referenced());
                v
            }
            Expr::BoolAnd(parts) | Expr::BoolOr(parts) => {
                parts.iter().flat_map(|e| e.columns_referenced()).collect()
            }
            Expr::Not(e) | Expr::IsNull(e) | Expr::IsNotNull(e) => e.columns_referenced(),
            Expr::Cast { expr, .. } => expr.columns_referenced(),
            Expr::InList { expr, list, .. } => {
                let mut v = expr.columns_referenced();
                v.extend(list.iter().flat_map(|e| e.columns_referenced()));
                v
            }
            Expr::FunctionCall { args, .. } => {
                args.iter().flat_map(|e| e.columns_referenced()).collect()
            }
            Expr::Case {
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
