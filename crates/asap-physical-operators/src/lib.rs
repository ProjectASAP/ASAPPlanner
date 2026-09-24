#![doc = include_str!("../README.md")]

pub mod summary_operators;
/// Compatibility alias for existing deployments.
pub use summary_operators as accumulators;
pub mod key_by_label_values;
pub mod measurement;
pub use summary_operators::traits;

mod aggregation_type;
mod statistic;
pub use aggregation_type::AggregationType;
pub use key_by_label_values::KeyByLabelValues;
pub use measurement::Measurement;
pub use statistic::Statistic;
pub use traits::*;

pub use expressions::arithmetic;
pub mod capability;
pub use summary_operators::factory;

/// The exact Planner contract used by these kernels.
pub use planner_types as planner;

pub mod dag;

pub mod stored_state;

mod error;
pub use error::Error;
pub mod binding;
pub mod expressions;
pub mod operators;
pub mod plan;
pub mod runtime;
pub mod sources;
pub mod values;
