//! #511 §2.2: accuracy targets and the error metrics they are stated in
//! ([`AccuracyTarget`], [`AccuracyMetric`]), and machine-readable accuracy
//! guarantees ([`AccuracyGuarantee`]).
//!
//! - `target` — accuracy targets and the error metrics they are stated in.
//! - `guarantee` — machine-readable accuracy guarantees.
mod guarantee;
mod target;

pub use guarantee::{
    AccuracyError, AccuracyGuarantee, BoundExpr, CompositionOperator, GuaranteeSource,
    ProbabilityExpr, VALUES_PER_RESULT,
};
pub use target::{
    default_metric_for_root, AccuracyMetric, AccuracyScope, AccuracySpec, AccuracyTarget, NormScale,
};
