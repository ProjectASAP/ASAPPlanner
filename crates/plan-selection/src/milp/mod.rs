//! #1095 Steps 3–5: the owner-scoped selection MILP (§2.0) over
//! [`MilpInputs`](crate::owners::MilpInputs), solved once with HiGHS.
//!
//! - [`model`]: the variables and the structural rows (outputs,
//!   owners, instances, reach, reader constraints, latency, accuracy), the
//!   solve and the raw solution;
//! - [`cost_terms`]: the cost model 1 objective, every coefficient a
//!   number `asap-cost` computed (§3);
//! - [`warm_start`]: a feasible plan with no sharing, HiGHS's first
//!   incumbent;
//! - [`decode`]: the solution as a `DagOfSelectedCandidates`, one node per
//!   candidate run.
//!
//! [`crate::select`] runs them, realizes the plan once and checks it.
pub(crate) mod cost_terms;
pub(crate) mod decode;
pub(crate) mod model;
pub(crate) mod warm_start;
