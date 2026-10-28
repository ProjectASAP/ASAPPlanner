#![doc = include_str!("../README.md")]

pub mod summary_kernels;
pub use summary_kernels::traits;

pub use traits::*;

pub use expressions::arithmetic;
pub mod capability;
mod supported_summaries;
pub use summary_kernels::factory;
pub use summary_kernels::state_column_updater::StateColumnUpdater;
pub use supported_summaries::supported_summary_families;

/// The exact Planner contract used by these kernels.
pub use planner_types as planner;

/// The Arrow version batches are built with, for runtimes that build or read
/// columns ([`values::Batch::try_from_columns`], [`values::Batch::column`]).
pub use arrow;

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
