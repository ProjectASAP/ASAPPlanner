//! The directory's tests as one binary (the snapshot tests have their own):
//! one file is `cargo test -p asap-planner-cli --test planner_cli <file>::`.

mod asap_planner;
mod asap_planner_acceptance;
mod lowest_mean_latency_exactness;
