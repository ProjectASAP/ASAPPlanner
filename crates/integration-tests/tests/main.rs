//! The directory's tests as one binary: linked one per file, each carried
//! its own copy of every dependency, which outgrew CI's disk.
//! One file: `cargo test -p asap-integration-tests --test integration <file>::`.

mod physical_common;
mod planning_common;

mod accuracy_at_query_roots;
mod aggregate;
mod binary_op;
mod calibration_terms;
mod compressed_sample_windows;
mod crate_graph;
mod default_cost_plan_ranking;
mod deployment_exclusions;
mod exact_reduce_execution;
mod filtered_aggregates;
mod finer_aggregate_reuse;
mod frontend_timestamps;
mod independent_plan_seed;
mod joint_search_equivalence;
mod kll_tumbling_window_execution;
mod lowest_mean_latency;
mod lowest_mean_latency_common;
mod materialized_views;
mod memo_value_bounds;
mod nested;
mod operator_design_examples;
mod operator_sharing;
mod pass1_candidates;
mod pass2_promql;
mod pass2_sql;
mod per_query_accuracy_targets;
mod per_series_exact_execution;
mod plan_sharing;
mod production_rollup_workloads;
mod promql_series_execution;
mod promql_to_post_asap;
mod rollup_controlled;
mod scan;
mod schema;
mod sql_cardinality;
mod sql_frequency;
mod sql_relational_execution;
mod sql_to_post_asap;
mod stage3_objective;
mod time_range;
