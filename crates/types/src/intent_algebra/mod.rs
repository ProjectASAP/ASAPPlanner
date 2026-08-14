//! Layer 3 — the canonical intent algebra IR.
//!
//! - [`query_expr`] — the canonical, language- and deployment-independent L3
//!   intent algebra ([`QueryExpr`] + [`AggIntent`]), with positional
//!   [`ColumnId`] schema flow.
//! - [`agg_intent`] — the L3 aggregation-intent vocabulary.
//! - [`expr_ir`] — the scalar expression IR ([`L2Expr`] / [`L3Expr`] /
//!   [`ColumnRef`]) shared by L2 and L3.
//! - [`schema`] — the per-edge [`Schema`] every L3 node carries.
//!
//! The Layer-2 relational tree and the L2→L3 converter (`convert_root`, the
//! `Binder`, column resolution) live in the `asap-l2` crate — front ends need
//! them, but L3-only consumers (optimizer, sketch) do not, so they stay out of
//! this crate.

pub mod agg_intent;
pub mod expr_ir;
pub mod query_expr;
pub mod schema;

pub use agg_intent::{
    agg_accuracy, agg_is_exact, agg_is_mergeable, default_cardinality, default_quantile,
    is_frequency_heavy_hitter, ranking_measure, AggIntent, MathFunc, RankingMeasure, TimeFunc,
};
pub use expr_ir::{ArithOp, ColumnRef, CompareOp, Expr, L2Expr, L3Expr, L3Scalar};
pub use query_expr::{
    aggregate_output_schema, AtModifier, BinaryOpKind, DataModel, GroupKeys, GroupSide,
    InfoMatcher, JoinKind, Predicate, ProjectItem, QueryExpr, QueryExprError, Reduction,
    SampleKind, SetOpKind, SortKey, Source, TimeShift, VectorGrouping, VectorMatch,
    VectorMatchKind, WindowFuncKind,
};
pub use schema::{Column, ColumnId, DataType, Schema};
