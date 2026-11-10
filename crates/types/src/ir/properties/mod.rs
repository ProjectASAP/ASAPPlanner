//! #511 §2.2–2.3: the planning properties a node carries besides its
//! operator and schema.
//!
//! - [`execution_data_state`] — when a node's value is produced
//!   ([`ExecutionTiming`]) and what its edge carries ([`ExecutionDataState`]).
//! - [`summary_coverage`] — what a summary state covers.
pub mod execution_data_state;
pub mod summary_coverage;

pub use execution_data_state::{DataPrimitive, ExecutionDataState, ExecutionTiming};
