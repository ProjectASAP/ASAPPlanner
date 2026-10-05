//! Pass 1: local logical alternatives for each target sub-DAG
//! ([`logical_candidates`]), from the realizations of each aggregate intent
//! ([`realization`]). A candidate is pruned only when it is provably invalid.

pub mod logical_candidates;
pub mod maintained_population;
pub mod realization;
