//! The directory's tests as one binary: linked one per file, each carried
//! its own copy of every dependency, which outgrew CI's disk.
//! One file: `cargo test -p asap-frontend-metricsql --test metricsql <file>::`.

mod lowering;
mod lowering_semantics;
mod victoriametrics_go_golden;
