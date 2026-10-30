//! #1095 Steps 3–5: the owner-scoped selection MILP (§2.0) over
//! [`MilpInputs`](crate::owners::MilpInputs), solved once with HiGHS.
//!
//! - [`model`]: the variables and the structural rows (outputs,
//!   owners, instances, reach, reader constraints, latency, accuracy), the
//!   solve and the raw solution;
//! - [`cost_terms`]: the cost model 1 objective, every coefficient a
//!   number `asap-cost` computed (§3);
//! - [`warm_start`]: a feasible plan with no sharing, HiGHS's first
//!   incumbent.
//!
//! Decoding the solution into a `DagOfSelectedCandidates`, the realizer call and the
//! `plan_cost` equality check are #1095 C5; nothing calls this module yet
//! outside its tests.
pub(crate) mod cost_terms;
pub(crate) mod model;
pub(crate) mod warm_start;
