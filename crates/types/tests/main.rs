//! The directory's tests as one binary: linked one per file, each carried
//! its own copy of every dependency, which outgrew CI's disk.
//! One file: `cargo test -p asap-types --test types <file>::`.

mod column_lineage;
mod memo_aggregate_coverage;
mod memo_group_properties;
mod physical_asap_dag;
mod planner_vocabulary;
mod schema_rebuilding;
mod structure_contract;
mod summary_merge_structure;
mod tumbling_window_chain;
mod workload_compatibility;
