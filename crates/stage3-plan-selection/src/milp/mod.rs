//! Stage 3's selection MILP
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! sections 3–5): its variables over execution contexts and one function
//! per constraint family ([`model`]), the retention amount $h_v$ and
//! constraint (R) ([`execution_contexts`]), each query's exact-only reads,
//! constraint (X) ([`exact_only`]), the graph algorithm it needs
//! ([`graph`]), the bound it scales by ([`upper_bound`]), and the accuracy
//! extension ([`accuracy`]).
mod accuracy;
mod exact_only;
mod execution_contexts;
mod graph;
pub(crate) mod model;
mod upper_bound;
