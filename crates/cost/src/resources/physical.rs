//! Resource aggregation with independent byte dimensions and explicit CPU units.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PhysicalResources<Cpu, Bytes> {
    pub(crate) cpu: Cpu,
    /// Maximum simultaneously live memory within the reported scope.
    pub(crate) peak_memory_bytes: Option<Bytes>,
    /// Bytes read by scan operations, not storage occupancy.
    pub(crate) scan_bytes: Option<Bytes>,
}

impl<Cpu: Default, Bytes> Default for PhysicalResources<Cpu, Bytes> {
    fn default() -> Self {
        Self {
            cpu: Cpu::default(),
            peak_memory_bytes: None,
            scan_bytes: None,
        }
    }
}
