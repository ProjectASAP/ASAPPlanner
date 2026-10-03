//! #511 §2.2–2.3: node properties beyond the schema — the accuracy
//! guarantee, execution timing and data state, and summary coverage.

pub mod execution;
pub mod guarantee;
pub mod summary_coverage;
pub mod timing;

pub use execution::{DataPrimitive, ExecutionDataState, ExecutionDataStateError, ExecutionTiming};
pub use guarantee::{
    AccuracyError, BoundExpr, CompositionOperator, ErrorMetric, GuaranteeSource, ProbabilityExpr,
    ResultGuarantee,
};
