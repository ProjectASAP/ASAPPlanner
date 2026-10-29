//! The directory's tests as one binary: linked one per file, each carried
//! its own copy of every dependency, which outgrew CI's disk.
//! One file: `cargo test -p asap-devtools --test devtools <file>::`.

mod cross_language;
mod memo_pricing;
mod planner_fuzz;
mod stage_pipeline;
mod viewer_contract;
