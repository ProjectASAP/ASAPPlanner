//! #509 Stage 2, physical ASAP-aware optimization (concepts doc,
//! section 3, "Stage 2: physical alternatives").
//!
//! The planner facade calls [`generate_physical_asap_memo_candidates`] on
//! Pass 2's output (`asap_types::stages::LogicalASAPMemoCandidates`): each
//! logical candidate expanded into one physical candidate per offered
//! $(\tau, \ell)$, each logical edge into its physical edges with
//! $\alpha^{\log}$ and $\alpha^{\mathrm{phys}}$, and the pruned alternatives
//! (`asap_types::stages::PhysicalASAPMemoCandidates`). Stage 3 selects from
//! that output and builds the plan. The crate depends on no other stage
//! crate.

mod catalog;

pub use catalog::generate_physical_asap_memo_candidates;
