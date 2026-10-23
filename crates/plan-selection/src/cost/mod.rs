//! The analytical resource and cost model plan selection prices with.
//!
//! - [`analytical`] and [`operator_statistics`] — the analytical
//!   per-operator resource model.
//! - [`measured`] — measured operation costs and their calibration.
//! - [`per_node`] — the memo's per-node local prices.
pub mod analytical;
pub(crate) mod measured;
pub mod operator_statistics;
pub(crate) mod per_node;
