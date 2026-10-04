//! Pass 1: local logical alternatives for each target sub-DAG. Strategies
//! propose rewrites and summary realizations; a candidate is pruned only when
//! it is provably invalid.

pub mod exact_composition;
pub mod explanation;
pub(crate) mod function_rules;
pub mod grouping;
pub mod logical_candidates;
pub mod maintained_population;
pub mod replacement;
pub mod rewrite;
pub mod rollup;
