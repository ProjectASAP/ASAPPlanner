//! The `asap_planner` command line: plans a workload up to a chosen stage
//! and prints every stage's output, as text or as a JSON document.
//!
//! [`planning_stages`] runs the stages from query text; the integration
//! tests and `planner_fuzz` plan through it too. [`planning_stages_text`]
//! prints them as text, [`candidate_labels`] names the candidates in both
//! outputs, and [`stable_output`] keeps the JSON document comparable across
//! changes that do not change the plan.

pub mod candidate_labels;
pub mod planning_stages;
pub mod planning_stages_text;
pub mod stable_output;
