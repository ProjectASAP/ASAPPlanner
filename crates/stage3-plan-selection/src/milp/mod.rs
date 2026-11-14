//! Stage 3's selection MILP
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! sections 3–5): the baseline's variables and one function per constraint
//! family ([`model`]), the graph algorithm it needs ([`graph`]), the bound
//! it scales by ([`upper_bound`]), and the execution-context and accuracy
//! extensions ([`execution_contexts`], [`accuracy`]).
mod accuracy;
mod execution_contexts;
mod graph;
pub(crate) mod model;
mod upper_bound;
