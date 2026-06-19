//! Layers 2–3 of the controller pipeline.
//!
//! - [`relational`] — the Layer-2 per-language algebra tree the parser front
//!   ends emit (PromQL / SQL).
//! - [`lower`] — the L2→L3 converter ([`convert_root`]), which runs the
//!   [`Binder`] for name resolution and folds single-statistic sketchable
//!   aggregates into canonical shapes.
//! - [`query_expr`] — the canonical, language- and deployment-independent L3
//!   intent algebra ([`QueryExpr`] + [`AggIntent`]), with positional
//!   [`ColumnId`] schema flow.
//! - [`cse`] — workload-level common-sub-expression elimination over L3.

pub mod agg_intent;
pub mod binder;
pub mod column_resolution;
pub mod cse;
pub mod expr_ir;
pub mod lower;
pub mod names;
pub mod query_expr;
pub mod relational;
pub mod schema;

pub use agg_intent::{
    agg_accuracy, agg_is_exact, agg_is_mergeable, default_cardinality, default_quantile, AggIntent,
};
pub use binder::{Binder, SchemaCatalog, UsageDerivedCatalog};
pub use column_resolution::{
    infer_schema_for_root, infer_source_schema, output_schema_for_aggregate, resolve_column_ref,
    resolve_column_refs, resolve_expr, ResolveError,
};
pub use cse::{dedupe_subtrees, CseWorkloadPlan};
pub use expr_ir::{ArithOp, ColumnRef, CompareOp, Expr, L2Expr, L3Expr, L3Scalar};
pub use lower::{convert, convert_root, ConvertError};
pub use names::{BindingName, QueryId};
pub use query_expr::{
    BinaryOpKind, BindingScope, DataModel, GroupKeys, GroupSide, JoinKind, Predicate,
    ProjectItem, QueryExpr, QueryExprError, SetOpKind, SortKey, Source, VectorGrouping,
    VectorMatch, VectorMatchKind, WindowFuncKind, WindowKind,
};
pub use schema::{cse_reuse_is_legal, Column, ColumnId, CseError, DataType, Schema};
