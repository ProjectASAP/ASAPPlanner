//! CPU quantities with explicit modeled-work or measured-time units.

use serde::{Deserialize, Serialize};

use super::measurement::Measurement;

/// Modeled CPU work, never measured elapsed or process CPU time.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeledCpu {
    pub cpu_ops: f64,
}

/// Process CPU nanoseconds per named operation, never modeled CPU operations.
/// An absent phase was not measured; it does not imply a free operation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredCpu {
    /// Empty construction; ingestion is charged separately.
    pub build_cpu_ns: Option<Measurement>,
    pub update_cpu_ns: Option<Measurement>,
    pub merge_cpu_ns: Option<Measurement>,
    /// One prepare pass after ingestion and before reads.
    pub prepare_cpu_ns: Option<Measurement>,
    pub read_cpu_ns: Option<Measurement>,
}
