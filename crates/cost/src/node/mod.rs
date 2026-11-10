//! One node of a physical plan DAG: its resource usage and cost
//! (`usage`), from the analytical per-operator resource model
//! ([`analytical`], `operator_statistics`) and the bytes a state holds
//! ([`state_bytes`]).

pub mod analytical;
pub(crate) mod operator_statistics;
pub mod state_bytes;
pub(crate) mod usage;
