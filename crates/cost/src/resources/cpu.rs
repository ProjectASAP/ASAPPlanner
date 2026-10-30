//! CPU quantities in modeled-work units.

use serde::{Deserialize, Serialize};

/// Modeled CPU work, never measured elapsed or process CPU time.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModeledCpu {
    pub(crate) cpu_ops: f64,
}
