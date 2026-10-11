#![doc = include_str!("../README.md")]

pub mod key_by_label_values;
pub mod measurement;
pub mod summary_kernels;
pub use summary_kernels::traits;

mod statistic;
pub use key_by_label_values::KeyByLabelValues;
pub use measurement::Measurement;
pub use statistic::Statistic;
pub use traits::*;

pub use expressions::arithmetic;
pub mod capability;
pub use capability::capabilities;
pub use summary_kernels::factory;

/// The exact Planner contract used by these kernels.
pub use planner_types as planner;

pub mod dag;

pub mod evaluation;

mod error;
pub use error::Error;
pub mod expressions;
pub mod operators;
pub mod physical_planner;
pub mod plan;
pub mod runtime;
pub mod sources;
pub mod values;
