//! The directory's tests as one binary: linked one per file, each carried
//! its own copy of every dependency, which outgrew CI's disk.
//! One file: `cargo test -p asap-types --test types <file>::`.

mod materialized_view;
mod pane_chain;
mod physical_export;
mod planner_vocabulary;
mod schema_rebuilding;
mod structure_contract;
mod summary_coverage;
mod summary_merge_structure;
mod workload_compatibility;
