//! The post-ASAP L4 IR: sketch-bound types, distinct from
//! [`crate::pre_asap`]'s pre-ASAP L3 IR.
//!
//! Where L3 ([`crate::pre_asap`]) carries *intent* only ("compute a
//! quantile to ε accuracy"), this module is the sketch-bound IR: the sketch
//! kind + parameters are committed ([`sketch::SummaryKind`] /
//! [`sketch::SummaryParams`]), and [`expr::L4Node`] / [`expr::SummaryExpr`]
//! describe the summary computation.

pub mod expr;
pub mod schema;
pub mod sketch;

pub use expr::{L4Node, SummaryExpr};
pub use schema::{L4DataType, L4Field, L4Schema};
pub use sketch::{SketchQuery, SummaryKind, SummaryParams};
