//! #509 Stage 2 decision data: the physical properties of each candidate.
//!
//! - [`properties`] — when a candidate runs, how long its output lives and
//!   on which schedule it is delivered ([`PhysicalProperties`], [`Lifetime`],
//!   [`Medium`], [`Schedule`]).
//! - [`workload_plan`] — one evaluated assignment rebuilt as a physical DAG
//!   ([`PhysicalWorkloadPlan`]).
pub mod properties;
pub mod workload_plan;

pub use properties::{
    EvaluationSchedule, ExecutionTiming, InvalidPhysicalProperties, Lifetime, Medium,
    PhysicalProperties, Schedule,
};
pub use workload_plan::PhysicalWorkloadPlan;
