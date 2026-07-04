//! `asap-l2` — the Layer-2 relational algebra + the L2→L3 converter.
//!
//! The parser front ends emit the per-language [`relational::QueryExpr`] tree;
//! [`convert_root`] lowers it to the canonical L3 [`QueryExpr`] in
//! [`asap_ir`], running the [`Binder`] for positional name resolution and
//! folding single-statistic sketchable aggregates into canonical shapes.
//!
//! This crate owns L2 *and* the converter because the converter needs both L2
//! and L3; it depends only on the L3 IR crate. Downstream consumers that reason
//! about L3 only (optimizer, sketch) depend on `asap-ir` directly and never
//! pull this lowering machinery.

pub mod binder;
pub mod canonicalize;
pub mod column_resolution;
pub mod lower;
pub mod relational;

pub use binder::{Binder, SchemaCatalog, UsageDerivedCatalog};
pub use canonicalize::canonicalize;
pub use column_resolution::{
    infer_schema_for_root, infer_source_schema, output_schema_for_aggregate, resolve_column_ref,
    resolve_column_refs, resolve_expr, ResolveError,
};
pub use lower::{convert, convert_root, ConvertError};
