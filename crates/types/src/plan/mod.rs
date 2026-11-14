//! Stage 3's output: the plan, one physical ASAP DAG
//! ([`physical_asap_dag`]), and the materialized views it installs
//! ([`materialized_view`]); the time windows its nodes read
//! ([`time_window`]).
pub mod materialized_view;
pub mod physical_asap_dag;
pub mod time_window;
mod view_coverage;

pub use time_window::TimeWindow;
pub use view_coverage::view_coverage;
