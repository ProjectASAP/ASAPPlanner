//! #511 §2.2–2.3: the planning properties a node carries besides its
//! operator and schema.
//!
//! - [`execution_data_state`] — when a node's value is produced
//!   ([`ExecutionTiming`]) and what its edge carries ([`ExecutionDataState`]).
//! - [`aggregate_coverage`] — what an aggregate (a summary state or an exact
//!   `Aggregate`) covers.
pub mod aggregate_coverage;
pub mod execution_data_state;

pub use execution_data_state::{DataPrimitive, ExecutionDataState, ExecutionTiming};
