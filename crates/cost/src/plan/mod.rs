//! The whole physical plan DAG: how often each node is computed
//! ([`execution_demand`]), the plan's cost (`cost`), the tumbling windows it
//! retains (`retention`) and how plans rank (`ranking`).

pub(crate) mod cost;
pub mod execution_demand;
pub(crate) mod ranking;
pub(crate) mod retention;
