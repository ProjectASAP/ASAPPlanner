//! Layer 3 — the canonical intent algebra IR.
//!
//! - [`query_expr`] — the canonical, language- and deployment-independent L3
//!   intent algebra ([`QueryExpr`] + [`AggIntent`]), with positional
//!   [`ColumnId`] schema flow.
//! - [`agg_intent`] — the L3 aggregation-intent vocabulary.
//! - [`expr_ir`] — the scalar expression IR ([`L2Expr`] / [`L3Expr`] /
//!   [`ColumnRef`]) shared by L2 and L3.
//! - [`schema`] — the per-edge [`Schema`] every L3 node carries.
//! - [`binder`] / [`column_resolution`] — name resolution: turn a `ColumnRef`
//!   into a positional `ColumnId` against an in-scope [`Schema`].
//! - [`canonicalize`] — post-lowering structural normalization of [`QueryExpr`]
//!   (issue #34).
//! - [`relational`] / [`lower`] — the Layer-2 per-language relational tree
//!   both front ends currently emit, and the converter (`convert_root`) that
//!   lowers it to canonical `QueryExpr`. **Legacy, pending deletion**: these
//!   two modules exist only until both front ends emit `QueryExpr` directly
//!   during their own `interpret` step (issue #179) — everything else here
//!   stays.
//!
//! Formerly the separate `asap-l2` crate; folded in here since after #179's
//! front-end migration, `binder`/`column_resolution`/`canonicalize` have no
//! front-end-specific logic left — they operate directly on this crate's own
//! `QueryExpr`.

pub mod agg_intent;
pub mod binder;
pub mod canonicalize;
pub mod column_resolution;
pub mod expr_ir;
pub mod lower;
pub mod query_expr;
pub mod relational;
pub mod resolve;
pub mod schema;

pub use agg_intent::{
    agg_accuracy, agg_is_exact, agg_is_mergeable, default_cardinality, default_quantile,
    is_frequency_heavy_hitter, ranking_measure, AggIntent, MathFunc, RankingMeasure, TimeFunc,
};
pub use binder::{Binder, SchemaCatalog, UsageDerivedCatalog};
pub use canonicalize::canonicalize;
pub use column_resolution::{
    infer_schema_for_root, infer_source_schema, output_schema_for_aggregate, resolve_column_ref,
    resolve_column_refs, resolve_expr, ResolveError,
};
pub use expr_ir::{ArithOp, ColumnRef, CompareOp, Expr, L2Expr, L3Expr, L3Scalar};
pub use lower::{convert, convert_root, ConvertError};
pub use resolve::{resolve_root, ResolveTreeError};
pub use query_expr::{
    aggregate_output_schema, AtModifier, BinaryOpKind, ColState, DataModel, GroupKeys, GroupSide,
    InfoMatcher, JoinKind, L2QueryExpr, L3QueryExpr, Predicate, ProjectItem, QueryExpr,
    QueryExprError, Reduction, SampleKind, SetOpKind, SortKey, Source, TimeShift, VectorGrouping,
    VectorMatch, VectorMatchKind, WindowFuncKind,
};
pub use schema::{Column, ColumnId, DataType, Schema};
