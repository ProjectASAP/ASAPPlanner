//! `asap-frontend-common` — the front-end-facing, name-based operator tree
//! and its resolver into the unified IR.
//!
//! A front end builds an [`UnresolvedOp`] tree (column references are
//! name-based [`ColumnRef`](asap_types::ir::scalar::ColumnRef)s) during its
//! own `interpret` step and calls [`resolve_root`], which binds every
//! reference to a positional `ColumnId` and returns the
//! [`OperatorNode`](asap_types::ir::OperatorNode) DAG.
//!
//! - [`unresolved`] — [`UnresolvedOp`] / [`UnresolvedScalar`]: the tree.
//! - [`schema_resolver`] — [`SchemaResolver`]: builds the binding schema of a
//!   schemaless (PromQL) leaf from the names the query references.
//! - [`resolve`] — [`resolve_root`]: the bottom-up binding walk.

pub mod resolve;
pub mod schema_resolver;
pub mod unresolved;

pub use resolve::{resolve_expr, resolve_root, resolve_scalar_root, ResolveDAGError};
pub use schema_resolver::{SchemaCatalog, SchemaResolver, UsageDerivedCatalog};
pub use unresolved::{
    UnresolvedOp, UnresolvedPredicate, UnresolvedProjectItem, UnresolvedScalar, UnresolvedSortKey,
};
