//! #511 §2.2–2.3: the planning properties a node carries besides its
//! operator and schema.
//!
//! - [`accuracy`] — accuracy requirements and the error metrics they are
//!   stated in ([`AccuracyTarget`], [`AccuracyMetric`]).
//! - [`guarantee`] — machine-readable accuracy guarantees
//!   ([`AccuracyGuarantee`]).
//! - [`execution_data_state`] — when a node's value is produced
//!   ([`ExecutionTiming`]) and what its edge carries ([`ExecutionDataState`]).
//! - [`summary_coverage`] — what a summary state covers.
//! - [`column_lineage`] — where each output column of a memo group comes from.
pub mod accuracy;
pub mod column_lineage;
pub mod execution_data_state;
pub mod guarantee;
pub mod summary_coverage;

pub use accuracy::{
    default_metric_for_root, AccuracyMetric, AccuracyScope, AccuracySpec, AccuracyTarget, NormScale,
};
pub use execution_data_state::{DataPrimitive, ExecutionDataState, ExecutionTiming};
pub use guarantee::{
    AccuracyError, AccuracyGuarantee, BoundExpr, CompositionOperator, GuaranteeSource,
    ProbabilityExpr, VALUES_PER_RESULT,
};
