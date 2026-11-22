//! The whole physical plan DAG: how often each node is computed
//! ([`execution_demand`]) and the plan's cost (`cost`).

pub(crate) mod cost;
pub(crate) mod resident_windows;
pub mod execution_demand;
