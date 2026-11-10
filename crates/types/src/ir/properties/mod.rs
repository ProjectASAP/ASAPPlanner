//! #511 §2.2–2.3: the planning properties a node carries besides its
//! operator and schema (when its value is produced), and what an aggregate
//! covers with the selection algebra it is stated in.
//!
//! - [`execution_data_state`] — when a node's value is produced
//!   ([`ExecutionTiming`]) and what its edge carries ([`ExecutionDataState`]).
//! - [`aggregate_coverage`] — what an aggregate computes and which rows it
//!   took ([`AggregateCoverage`]), derived over an operator's inputs.
//! - [`selection`] — which rows of a computation an aggregate's state took:
//!   boxes of per-column constraints, the selection algebra aggregate
//!   coverage is built from.
pub mod aggregate_coverage;
pub mod column_lineage;
pub mod execution_data_state;
pub mod selection;

pub use aggregate_coverage::{AggregateCoverage, AggregateDefinition, CoverageError};
pub use column_lineage::ColumnLineage;

pub use execution_data_state::{DataPrimitive, ExecutionDataState, ExecutionTiming};
