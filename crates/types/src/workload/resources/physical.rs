//! Resource aggregation with independent byte dimensions and explicit CPU units.

use serde::{Deserialize, Serialize};

use super::{cpu::MeasuredCpu, measurement::Measurement};

pub type MeasuredResources = PhysicalResources<MeasuredCpu, Measurement>;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalResources<Cpu, Bytes> {
    pub cpu: Cpu,
    /// Maximum simultaneously live memory within the reported scope.
    pub peak_memory_bytes: Option<Bytes>,
    /// Memory still retained at the end of the reported scope.
    pub retained_memory_bytes: Option<Bytes>,
    /// Bytes read by scan operations, not storage occupancy.
    pub scan_bytes: Option<Bytes>,
    /// Logical encoded snapshot size, not in-memory state size.
    pub serialized_bytes: Option<Bytes>,
    /// Allocated filesystem space, not bytes read or written over time.
    pub disk_bytes: Option<Bytes>,
}

impl<Cpu: Default, Bytes> Default for PhysicalResources<Cpu, Bytes> {
    fn default() -> Self {
        Self {
            cpu: Cpu::default(),
            peak_memory_bytes: None,
            retained_memory_bytes: None,
            scan_bytes: None,
            serialized_bytes: None,
            disk_bytes: None,
        }
    }
}
