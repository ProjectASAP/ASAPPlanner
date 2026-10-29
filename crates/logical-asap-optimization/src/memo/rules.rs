//! Bounded logical memo rule expansion. Rules expose computations and typed
//! dependencies; physical configuration and cost selection live downstream.
pub(super) mod finer_aggregate;
pub(super) mod reuse;
pub(super) mod rollup;
pub(super) mod scan_labels;
pub(super) mod shared_producer;
pub(super) mod shared_scans;
pub(super) mod state_configurations;
pub(super) mod summary_inputs;
pub(super) mod summary_results;
pub(super) mod topk;
