//! The directory's tests as one binary: linked one per file, each carried
//! its own copy of every dependency, which outgrew CI's disk.
//! One file: `cargo test -p asap-planner --test planner <file>::`.

mod time_series;

mod accuracy_at_query_roots;
mod columnar_batches;
mod e2e_plan;
mod execution_contexts;
mod runtime_install;
mod sql_table_scans;
mod summary_sharing;
