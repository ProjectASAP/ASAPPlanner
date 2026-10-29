//! Bounded logical memo rule expansion. Rules expose computations and typed
//! dependencies; physical configuration and cost selection live downstream.
pub(crate) mod finer_aggregate;
pub(super) mod reuse;
pub(crate) mod rollup;
pub(crate) mod scan_labels;
pub(crate) mod shared_configuration_readers;
pub(super) mod shared_producer;
pub(crate) mod summary_inputs;
pub(crate) mod summary_results;
pub(crate) mod topk;
