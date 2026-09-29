//! Shared physical operators. This layer holds summary kernels and typed values.

pub mod key_by_label_values;
pub mod measurement;
pub mod summary_kernels;
pub use summary_kernels::traits;

mod statistic;
pub use key_by_label_values::KeyByLabelValues;
pub use measurement::Measurement;
pub use statistic::Statistic;
pub use traits::*;

pub mod capability;
pub use summary_kernels::factory;

/// The exact Planner contract used by these kernels.
pub use planner_types as planner;

mod error;
pub use error::Error;
pub mod values;
