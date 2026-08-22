//! The post-ASAP IR: summary-bound types, distinct from
//! [`crate::pre_asap`]'s pre-ASAP IR.
//!
//! Where [`crate::pre_asap`] carries *intent* only ("compute a
//! quantile to ε accuracy"), this module is the summary-bound IR: the
//! summary family, kind, and parameters are committed (one `(Kind, Params)`
//! pair per family — [`sketch::ExactKind`]/[`sketch::ExactParams`],
//! [`sketch::SketchKind`]/[`sketch::SketchParams`],
//! [`sketch::SamplingKind`]/[`sketch::SamplingParams`],
//! [`sketch::WaveletKind`]/[`sketch::WaveletParams`],
//! [`sketch::StatModelKind`]/[`sketch::StatModelParams`]), and
//! [`expr::SummaryNode`] / [`expr::SummaryExpr`] describe the summary
//! computation.

pub mod expr;
pub mod query_time;
pub mod schema;
pub mod sketch;

pub use expr::{SummaryExpr, SummaryNode};
pub use query_time::{
    classic_cms_sizing, cms_posterior_error_bound, count_sketch_posterior_error_bound,
    cu_sketch_posterior_error_bound, traditional_a_priori_bound,
};
pub use schema::{SummaryFamilyType, SummaryField, SummarySchema};
pub use sketch::{
    ExactKind, ExactParams, SamplingKind, SamplingParams, SketchKind, SketchParams, SketchQuery,
    StatModelKind, StatModelParams, WaveletKind, WaveletParams,
};
