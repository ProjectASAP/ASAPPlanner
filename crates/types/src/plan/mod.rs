//! Stage 3's output: the plan, one physical ASAP DAG
//! ([`physical_asap_dag`]), and the materialized views it installs
//! ([`materialized_view`]).
pub mod materialized_view;
pub mod physical_asap_dag;
mod view_coverage;

pub use view_coverage::view_coverage;
