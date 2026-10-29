//! The directory's tests as one binary: linked one per file, each carried
//! its own copy of every dependency, which outgrew CI's disk.
//! One file: `cargo test -p asap-planner --test planner <file>::`.

mod time_series;

mod columnar_batches;
mod deployment_accuracy_model;
mod e2e_plan;
mod per_sample_updater;
mod plan_stages;
mod runtime_install;
mod sql_per_group_topk;
mod sql_table_scans;
mod summary_sharing;
