//! The canonical pre-ASAP intent algebra IR.
//!
//! - [`query_expr`] — the canonical, language- and deployment-independent
//!   intent algebra: one recursive [`QueryExpr`] tree (relational operators
//!   *and* scalar expression shapes both, since issue #205) + [`AggIntent`],
//!   generic over the column-reference state (positional [`ColumnId`] once
//!   bound, name-based [`ColumnRef`] before).
//! - [`agg_intent`] — the aggregation-intent vocabulary.
//! - [`expr_ir`] — the [`ColumnRef`] column-reference type and the scalar
//!   operator/literal vocabulary ([`ScalarValue`], [`CompareOpKind`], [`ArithmeticOpKind`])
//!   [`QueryExpr`]'s scalar variants are built from.
//! - [`schema`] — the per-edge [`Schema`] every node carries.
//! - [`binder`] / [`column_resolution`] — name resolution: turn a `ColumnRef`
//!   into a positional `ColumnId` against an in-scope [`Schema`].
//! - [`resolve`] — binds a whole front-end-emitted [`UnresolvedQueryExpr`] tree to
//!   canonical [`ResolvedQueryExpr`] (issue #179): both front ends
//!   (`asap-frontend-promql`, `asap-frontend-sql`) construct `UnresolvedQueryExpr`
//!   directly during their own `interpret` step and call
//!   [`resolve_root`] on the result — there is no separate per-language
//!   relational tree or converter anymore.
//! - [`canonicalize`] — post-lowering structural normalization of [`QueryExpr`]
//!   (issue #34), run by [`resolve_root`].
//! - [`cse`] — workload-level structural common-subexpression elimination
//!   over an already-`resolve_root`'d tree (issue #212, #222, #223), run
//!   *after* `resolve_root` / `canonicalize` and *before* implementation
//!   (`asap_aware_mapping::replacement`).
//!
//! Formerly the separate `asap-l2` crate; folded in here since
//! `binder`/`column_resolution`/`canonicalize`/`resolve` have no
//! front-end-specific logic — they operate directly on this crate's own
//! `QueryExpr`.

pub mod agg_intent;
pub mod binder;
pub mod canonicalize;
pub mod column_resolution;
pub mod cse;
pub mod expr_ir;
pub mod query_expr;
pub mod resolve;
pub mod schema;

pub use agg_intent::{
    agg_accuracy, agg_is_exact, agg_is_mergeable, default_cardinality, default_quantile,
    is_heavy_hitter_ranking, ranking_measure, AggIntent, MathFunc, RankingMeasure, TimeFunc,
};
pub use binder::{Binder, SchemaCatalog, UsageDerivedCatalog};
pub use canonicalize::canonicalize;
pub use column_resolution::{
    output_schema_for_aggregate, resolve_column_ref, resolve_column_refs, resolve_expr,
    ResolveError,
};
pub use cse::share_common_subtrees;
pub use expr_ir::{ArithmeticOpKind, ColumnRef, CompareOpKind, ScalarValue};
pub use query_expr::{
    aggregate_output_schema, AtModifier, BinaryOpKind, ColState, DataModel, GroupKeys, GroupSide,
    InfoMatcher, JoinKind, Predicate, ProjectItem, QueryExpr, QueryExprError, Reduction,
    ResolvedQueryExpr, SampleKind, SetOpKind, SortKey, Source, TimeShift, UnresolvedQueryExpr,
    VectorGrouping, VectorMatch, VectorMatchKind, WindowFrame, WindowFrameBound, WindowFrameOffset,
    WindowFrameUnits, WindowFuncKind,
};
pub use resolve::{resolve_root, ResolveTreeError};
pub use schema::{Column, ColumnId, DataType, Schema};
