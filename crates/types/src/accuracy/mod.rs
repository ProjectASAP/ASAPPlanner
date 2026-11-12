//! Accuracy targets and the error metrics they are stated in
//! ([`AccuracyTarget`], [`AccuracyMetric`]), and machine-readable accuracy
//! guarantees ([`AccuracyGuarantee`]).
//!
//! - `target` — accuracy targets and the error metrics they are stated in.
//! - `guarantee` — machine-readable accuracy guarantees.
mod default_metric;
mod guarantee;
mod target;

pub use default_metric::default_metric_for_root;
pub use guarantee::{AccuracyGuarantee, BoundExpr, ProbabilityExpr};
pub use target::{AccuracyMetric, AccuracyScope, AccuracySpec, AccuracyTarget, NormScale};
