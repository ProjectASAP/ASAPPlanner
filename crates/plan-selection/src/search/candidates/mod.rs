//! The Stage 3 part of the candidate graph (its DAG with costs, and
//! rejected-subgraph witnesses), and unit tests of Stage 2's
//! catalog (`asap_physical_asap_optimization::catalog`) that select with the
//! solver.
#[cfg(test)]
pub(crate) mod configurations;
pub(crate) mod dag;
#[cfg(test)]
mod factoring;
pub(crate) mod graph;
#[cfg(test)]
pub(crate) mod inventory;
#[cfg(test)]
pub(crate) mod physical;
#[cfg(test)]
pub(crate) mod reconstruction;
#[cfg(test)]
pub(crate) mod scopes;
