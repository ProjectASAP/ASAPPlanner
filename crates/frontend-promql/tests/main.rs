//! The directory's tests as one binary: linked one per file, each carried
//! its own copy of every dependency, which outgrew CI's disk.
//! One file: `cargo test -p asap-frontend-promql --test promql <file>::`.

mod support;

mod count_planning;
mod histogram_metadata;
mod promql_binding_regressions;
mod promql_conformance;
mod promql_equivalence;
mod promql_lowering;
mod scalar_design;
mod univmon_candidates;
