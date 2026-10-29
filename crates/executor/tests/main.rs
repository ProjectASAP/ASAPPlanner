//! The directory's tests as one binary: linked one per file, each carried
//! its own copy of every dependency, which outgrew CI's disk.
//! One file: `cargo test -p asap-executor --test executor <file>::`.

mod common;

mod blocking_resources;
mod current_series_heap;
mod current_timestamp;
mod deployment;
mod deployment_computation;
mod exact_state_columns;
mod hydra_cms_execution;
mod keyed_summary_row_weight;
mod node_ids;
mod physical_dag;
mod physical_semantics;
mod plan_properties;
mod planspace_series_identity_heap;
mod precompute_candidates;
mod precompute_population;
mod promql_binary;
mod promql_values;
mod raw_scan;
mod state_column_updater;
mod stored_pane_memory_limit;
mod summary_projection;
mod text_extremum_memory;
mod thread_safety;
mod time_series_window;
mod univmon_execution;
mod weighted_topk_binding;
