//! The whole physical plan DAG: how often each node is computed
//! ([`execution_demand`]), the plan's cost (`cost`), how far back the
//! consumers of a time-indexed producer read (`read_back`) and how plans
//! rank (`ranking`).

pub(crate) mod cost;
pub mod execution_demand;
pub(crate) mod ranking;
pub(crate) mod read_back;
