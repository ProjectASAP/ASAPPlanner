//! Column-reference and scalar-operator vocabulary shared by the whole
//! canonical [`QueryExpr`](super::query_expr::QueryExpr) tree.
//!
//! Issue #205: the scalar expression shapes (`Column`/`Literal`/`Compare`/…)
//! used to live in a separate, self-recursive `Expr<C>` tree here, reachable
//! from `QueryExpr` only through wrapper fields (`Predicate`, `ProjectItem`,
//! `SortKey`). They're variants of `QueryExpr<C>` itself now — one recursive
//! tree, not two type families joined by wrappers — generic over the same
//! column-reference state `C` the rest of `QueryExpr` already carries
//! (issue #179): [`ColumnRef`] (name-based, front-end-emitted) or
//! [`ColumnId`](super::schema::ColumnId) (positional, once bound).
//!
//! What's left here is the vocabulary those scalar variants are built from —
//! [`L3Scalar`], [`CompareOp`], [`ArithOp`] — the **union** of what the two
//! front ends need: PromQL contributes `Regex` / `NotRegex` (`=~` / `!~`); SQL
//! contributes arithmetic, `CASE`, `IN`, `CAST`, `IS [NOT] NULL`, scalar
//! function calls, and the `LIKE` / `ILIKE` comparison family.

use serde::{Deserialize, Serialize};

/// A name-based column reference — the front-end-emitted, unresolved state of
/// [`QueryExpr::Column`](super::query_expr::QueryExpr::Column) (`C =
/// ColumnRef`); the [`Binder`](super::binder::Binder) resolves it to a
/// positional [`ColumnId`](super::schema::ColumnId). Includes the two
/// PromQL-conventional synthetic columns.
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
