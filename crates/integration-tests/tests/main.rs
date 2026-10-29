//! The directory's tests as one binary: linked one per file, each carried
//! its own copy of every dependency, which outgrew CI's disk.
//! One file: `cargo test -p asap-integration-tests --test integration <file>::`.

mod physical_common;
mod planner_layering_common;

mod aggregate;
mod binary_op;
mod declared_group_counts;
mod deployment_capabilities;
mod exact_composition;
mod filtered_aggregates;
mod finer_aggregate_reuse;
mod frequency_execution;
mod frontend_timestamps;
mod identical_exact_queries;
mod independent_plan_seed;
mod joint_search_equivalence;
mod kll_pane_execution;
mod materialization_e2e;
mod materialized_views;
mod memo_covers_whole_workload_pass2;
mod memo_enumeration;
mod nested;
mod operator_design_examples;
mod operator_sharing;
mod per_computation_candidates_sql;
mod per_series_exact_execution;
mod planner_layering_example1;
mod planner_layering_example2;
mod planner_layering_example3;
mod planner_layering_example4;
mod precompute_raw_samples;
mod production_rollup_workloads;
mod promql_numeric_regressions;
mod promql_to_post_asap;
mod range_evaluation;
mod reuse_rules;
mod rollup_controlled;
mod runtime_handoff;
mod scan;
mod schema;
mod sharing_issue_regressions;
mod sketch_rollup;
mod sketch_rollup_planning;
mod sql_cardinality;
mod sql_frequency;
mod sql_group_bound;
mod sql_to_physical;
mod sql_to_post_asap;
mod sql_trailing_window;
mod sql_two_percentiles;
mod stage1_logical_asap_optimization;
mod stage2_physical_asap_optimization_sql;
mod summary_coverage_e2e;
mod time_range;
mod tumbling_pane_merge;
