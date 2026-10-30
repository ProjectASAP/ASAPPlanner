//! #509 Stage 2 decision data: the materialization of each sub-DAG.
//!
//! - [`materialization`] — whether a sub-DAG's output is stored and when it
//!   is computed ([`Materialization`], [`Medium`]).
//! - [`workload_plan`] — one evaluated assignment rebuilt as a physical DAG
//!   ([`PhysicalWorkloadPlan`]).
pub mod materialization;
pub mod workload_plan;

pub use materialization::{Materialization, Medium};
pub use workload_plan::PhysicalWorkloadPlan;
