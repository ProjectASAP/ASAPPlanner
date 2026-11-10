//! The directory's tests as one binary: linked one per file, each carried
//! its own copy of every dependency, which outgrew CI's disk.
//! One file: `cargo test -p asap-integration-tests --test integration <file>::`.

mod physical_common;
mod planner_layering_common;

mod accuracy_at_query_roots;
mod aggregate;
mod binary_op;
mod contracts;
mod crate_graph;
mod filtered_aggregates;
mod finer_aggregate_reuse;
mod frontend_timestamps;
mod identical_exact_queries;
mod independent_plan_seed;
mod joint_search_equivalence;
mod kll_tumbling_window_execution;
mod materialized_views;
mod milp_objective;
mod nested;
mod operator_design_examples;
mod operator_sharing;
mod per_computation_candidates_sql;
mod per_series_exact_execution;
mod production_rollup_workloads;
mod promql_to_post_asap;
mod rollup_controlled;
mod scan;
mod schema;
mod sharing_issue_regressions;
mod sql_cardinality;
mod sql_frequency;
mod sql_to_post_asap;
mod stage1_logical_asap_optimization;
mod tier_markers;
mod time_range;
