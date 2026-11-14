//! The directory's tests as one binary: linked one per file, each carried
//! its own copy of every dependency, which outgrew CI's disk.
//! One file: `cargo test -p asap-integration-tests --test integration <file>::`.

mod physical_common;
mod planner_layering_common;
mod planning_common;

mod aggregate;
mod binary_op;
mod crate_graph;
mod filtered_aggregates;
mod finer_aggregate_reuse;
mod frontend_timestamps;
mod independent_plan_seed;
mod joint_search_equivalence;
mod kll_tumbling_window_execution;
mod materialized_views;
mod nested;
mod operator_design_examples;
mod operator_sharing;
mod pass1_candidates;
mod pass2_promql;
mod pass2_sql;
mod per_series_exact_execution;
mod plan_sharing;
mod production_rollup_workloads;
mod promql_to_post_asap;
mod rollup_controlled;
mod scan;
mod schema;
mod sql_cardinality;
mod sql_frequency;
mod sql_to_post_asap;
mod stage3_objective;
mod time_range;
