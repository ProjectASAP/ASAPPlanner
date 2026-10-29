//! CPU quantities in modeled-work units.

use serde::{Deserialize, Serialize};

/// Modeled CPU work, never measured elapsed or process CPU time.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeledCpu {
    pub cpu_ops: f64,
}
