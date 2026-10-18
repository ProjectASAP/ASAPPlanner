//! The analytical resource and cost model plan selection prices with.
//!
//! - [`analytical_cost`] and [`physical_operator_statistics`] — the
//!   analytical per-operator resource model.
//! - [`physical_handoff_cost`] — transfer and materialization handoffs.
//! - [`query_physical_lowering`] — the physical operators a query lowers
//!   to, as the cost model prices them.
//! - [`storage_io`] — storage request and byte costs.
pub mod analytical_cost;
pub mod physical_handoff_cost;
pub mod physical_operator_statistics;
pub mod query_physical_lowering;
pub mod storage_io;
