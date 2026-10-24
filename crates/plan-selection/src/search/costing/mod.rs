//! Execution demand, proposal estimates, pricing certificates, and calibration.
pub(crate) mod calibration;
// Independent selected-graph demand assertions; runtime demand is owned by
// the shared execution-cost evaluator.
#[cfg(test)]
pub(crate) mod demand;
pub(crate) mod lower_bounds;
pub(crate) mod proposals;
