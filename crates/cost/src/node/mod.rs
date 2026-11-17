//! One node of a physical plan DAG: its resource usage and cost
//! (`usage`) and its formula CPU split into named per-unit constants
//! (`cost_terms`), from the analytical per-operator resource model
//! ([`analytical`], `operator_statistics`), the measured CPU operations per
//! unit of work (`operations_per_unit`) and the bytes a state holds
//! ([`state_bytes`]).

pub mod analytical;
pub(crate) mod cost_terms;
pub(crate) mod operations_per_unit;
pub(crate) mod operator_statistics;
pub mod state_bytes;
pub(crate) mod usage;
