//! #1095 Steps 3–5: the group selection MILP (§2.0) over
//! [`MilpInputs`](crate::groups::MilpInputs): one candidate per result slot
//! per execution context, solved once with HiGHS from a warm start.
//!
//! - [`model`]: the variables and the structural rows (coverage, slots,
//!   validity, used only if read, sharing, reach, reader constraints,
//!   latency, accuracy), the solve and the raw solution;
//! - [`cost_terms`]: the cost model 1 objective, every coefficient a
//!   number `asap-cost` computed (§3);
//! - [`warm_start`]: each query's cheapest plan, then greedy sharing,
//!   HiGHS's first incumbent;
//! - [`completion`]: the exact objective of a plan given as binaries, the
//!   cheapest values of the continuous variables found without a solver;
//! - [`decode`]: the solution as a `DagOfSelectedCandidates`, one node per
//!   candidate run.
//!
//! [`crate::select`] runs them, realizes the plan once and checks it.
pub(crate) mod completion;
pub(crate) mod cost_terms;
pub(crate) mod decode;
pub(crate) mod model;
pub(crate) mod warm_start;
