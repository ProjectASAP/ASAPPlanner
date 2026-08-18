//! The post-ASAP IR: sketch-bound types, distinct from
//! [`crate::pre_asap`]'s pre-ASAP IR.
//!
//! Where [`crate::pre_asap`] carries *intent* only ("compute a
//! quantile to ε accuracy"), this module is the sketch-bound IR: the sketch
//! kind + parameters are committed ([`sketch::SummaryKind`] /
//! [`sketch::SummaryParams`]), and [`expr::SummaryNode`] / [`expr::SummaryExpr`]
//! describe the summary computation.

pub mod expr;
pub mod schema;
pub mod sketch;

pub use expr::{SummaryExpr, SummaryNode};
pub use schema::{SummaryDataType, SummaryField, SummarySchema};
pub use sketch::{SketchQuery, SummaryKind, SummaryParams};
