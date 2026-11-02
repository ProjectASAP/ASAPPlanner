//! The directory's tests as one binary: linked one per file, each carried
//! its own copy of every dependency, which outgrew CI's disk.
//! One file: `cargo test -p asap-frontend-metricsql --test metricsql <file>::`.

mod external_corpus;
mod lowering;
mod lowering_semantics;
mod victoriametrics_go_golden;

// The PromQL tests' binding helpers (`logical_plan`), for `external_corpus`.
#[path = "../../frontend-promql/tests/support.rs"]
#[allow(dead_code)]
mod support;
