//! Stage 3's output: the plan, one physical ASAP DAG
//! ([`physical_asap_dag`]), and the materialized views it installs
//! ([`materialized_view`]); the time windows its nodes read
//! ([`time_window`]).
mod content_id;
pub mod materialized_view;
pub mod physical_asap_dag;
pub mod time_window;
mod view_coverage;

pub use time_window::TimeWindow;
pub use view_coverage::view_coverage;

/// The version of the plan-file format: the DAG, its nodes' fields and the
/// query texts. Bumped to 16 when a sketch family started being written by
/// `asap_sketchlib`'s full `SketchAlgorithm` names (`{"Sketch": "KLL"}`,
/// `"CountMinSketch"`, ...). A plan file's `wire_version` must equal it. Lives
/// here so a runtime that depends only on the types crate can check it.
pub const INSTALLABLE_PLAN_WIRE_VERSION: u32 = 16;
