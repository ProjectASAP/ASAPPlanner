//! `asap-sketch` — the L4 **sketch algebra** (sketch-bound IR) over the L3
//! intent algebra.
//!
//! Where L3 ([`asap_ir::intent_algebra`]) carries *intent* only
//! ("compute a quantile to ε accuracy"), this layer is the sketch-bound IR:
//! the sketch kind + parameters are committed ([`SummaryKind`] /
//! [`SummaryParams`]), and [`L4Node`] / [`SummaryExpr`] describe the summary
//! computation. It depends only on the IR crate.

pub mod expr;
pub mod schema;
pub mod sketch;

pub use expr::{L4Node, SummaryExpr};
pub use schema::{L4DataType, L4Field, L4Schema};
pub use sketch::{SketchQuery, SummaryKind, SummaryParams};
