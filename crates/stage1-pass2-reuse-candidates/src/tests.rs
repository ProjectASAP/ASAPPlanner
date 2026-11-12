//! Pass 2's tests, one module per step. Each reads what the rules, the
//! readout requirements, dead-candidate elimination and the edges give for
//! a memo built from `crate::test_fixtures`.
mod chains;
mod dead_candidates;
mod edges;
mod evaluation_grids;
mod exact_only_groups;
mod readout_requirements;
mod rollup;
mod topk;
