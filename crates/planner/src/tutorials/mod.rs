//! # Tutorials
//!
//! A guide-level introduction to ASAPPlanner, read in order. Each chapter is
//! a doc-only module whose page comes from a markdown file beside this one,
//! so it renders next to the API docs and its code blocks run as doctests.
//!
//! 1. [`_01_background`]: monitoring queries, sketches and materialization;
//!    memo groups, candidates and plans.
//! 2. [`_02_planning_a_workload`]: planning a PromQL workload and reading
//!    the selected plan.
//! 3. [`_03_accuracy_targets`]: stating how accurate each query must be.
//! 4. [`_04_deployment_and_cost`]: how deployment capabilities and cost
//!    inputs change the plan.
//! 5. [`_05_executing_a_plan`]: running the selected plan with
//!    `asap-executor`.
//! 6. [`_06_extending`]: adding a sketch family or a reuse rule.

#[doc = include_str!("_01_background.md")]
pub mod _01_background {}

#[doc = include_str!("_02_planning_a_workload.md")]
pub mod _02_planning_a_workload {}

#[doc = include_str!("_03_accuracy_targets.md")]
pub mod _03_accuracy_targets {}

#[doc = include_str!("_04_deployment_and_cost.md")]
pub mod _04_deployment_and_cost {}

#[doc = include_str!("_05_executing_a_plan.md")]
pub mod _05_executing_a_plan {}

#[doc = include_str!("_06_extending.md")]
pub mod _06_extending {}
