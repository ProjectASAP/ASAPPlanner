//! Stage 3's selection MILP
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! sections 3–5): its variables over execution contexts and one function
//! per constraint family ([`model`]), the retention amount $h_v$ and
//! constraint (R) ([`retention`]), each query's exact-only reads,
//! constraint (X) ([`exact_only`]), the executions other executions
//! dominate ([`dominance`]), the copies of equivalent contexts that share
//! variables ([`equivalent_contexts`]), the graph algorithm it needs ([`graph`]),
//! the bound that leaves out executions dearer than a feasible plan before
//! scaling ([`upper_bound`]), and the accuracy extension ([`accuracy`]).
mod accuracy;
mod dominance;
mod equivalent_contexts;
pub(crate) mod exact_only;
mod graph;
mod kept_results;
mod mandatory_groups;
pub(crate) mod model;
mod retention;
mod upper_bound;

/// The relative tolerance to which two costs per second are taken as
/// equal (or one as no more than the other) when the MILP is reduced
/// before it is built: costs are rates times costs per run, so equal costs
/// can differ in their last bits.
///
/// Example: 0.1 × 3.0 and 0.3 differ by about 5.6e-17, well within
/// 1e-12 of 0.3.
const COST_RELATIVE_TOLERANCE: f64 = 1e-12;
