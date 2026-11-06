//! The directory's tests as one binary: linked one per file, each carried
//! its own copy of every dependency, which outgrew CI's disk.
//! One file: `cargo test -p asap-devtools --test devtools <file>::`.

mod asap_planner;
mod asap_planner_acceptance;
mod cross_language;
mod planner_fuzz;
mod viewer_contract;
