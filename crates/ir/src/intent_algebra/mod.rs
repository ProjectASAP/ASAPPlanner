//! Layer 3 — the canonical intent algebra IR.
//!
//! - [`query_expr`] — the canonical, language- and deployment-independent L3
//!   intent algebra ([`QueryExpr`] + [`AggIntent`]), with positional
//!   [`ColumnId`] schema flow.
//! - [`agg_intent`] — the L3 aggregation-intent vocabulary.
//! - [`expr_ir`] — the scalar expression IR ([`L2Expr`] / [`L3Expr`] /
//!   [`ColumnRef`]) shared by L2 and L3.
//! - [`schema`] — the per-edge [`Schema`] every L3 node carries.
//! - [`names`] — binding / query identifiers.
//!
//! The Layer-2 relational tree and the L2→L3 converter (`convert_root`, the
//! `Binder`, column resolution) live in the `asap-l2` crate — front ends need
//! them, but L3-only consumers (optimizer, sketch) do not, so they stay out of
//! this crate. Workload-level CSE lives in `asap-plan`.

pub mod agg_intent;
pub mod expr_ir;
pub mod names;
pub mod query_expr;
pub mod schema;

pub use agg_intent::{
    agg_accuracy, agg_is_exact, agg_is_mergeable, default_cardinality, default_quantile, AggIntent, MathFunc,
};
pub use expr_ir::{ArithOp, ColumnRef, CompareOp, Expr, L2Expr, L3Expr, L3Scalar};
pub use names::{BindingName, QueryId};
pub use query_expr::{
    BinaryOpKind, BindingScope, DataModel, GroupKeys, GroupSide, JoinKind, Predicate, ProjectItem,
    QueryExpr, QueryExprError, SetOpKind, SortKey, Source, VectorGrouping, VectorMatch,
    VectorMatchKind, WindowFuncKind, WindowKind,
};
pub use schema::{cse_reuse_is_legal, Column, ColumnId, CseError, DataType, Schema};
