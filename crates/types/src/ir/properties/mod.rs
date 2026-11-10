//! #511 §2.2–2.3: the planning properties a node carries besides its
//! operator and schema (when its value is produced), and the selection
//! algebra an aggregate's coverage (`stages::AggregateCoverage`) is stated
//! in.
//!
//! - [`execution_data_state`] — when a node's value is produced
//!   ([`ExecutionTiming`]) and what its edge carries ([`ExecutionDataState`]).
//! - [`selection`] — which rows of a computation an aggregate's state took:
//!   boxes of per-column constraints, the selection algebra aggregate
//!   coverage (`stages::AggregateCoverage`) is built from.
pub mod execution_data_state;
pub mod selection;

pub use execution_data_state::{DataPrimitive, ExecutionDataState, ExecutionTiming};
