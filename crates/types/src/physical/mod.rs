//! #509 Stage 2 decision data: the physical properties of each candidate.
//!
//! - [`properties`] — when a candidate runs and how long its output lives
//!   ([`PhysicalProperties`], [`Lifetime`], [`Medium`]), and what a consumer
//!   requires of them ([`PhysicalRequirement`], [`satisfies_physical`]).
//! - [`workload_plan`] — one evaluated assignment rebuilt as a physical DAG
//!   ([`PhysicalWorkloadPlan`]).
pub mod properties;
pub mod workload_plan;

pub use properties::{
    satisfies_physical, Evaluation, EvaluationSchedule, InvalidPhysicalProperties, Lifetime,
    Medium, PhysicalProperties, PhysicalRequirement,
};
pub use workload_plan::PhysicalWorkloadPlan;
