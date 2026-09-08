//! Resource dimensions shared by analytical estimates and benchmark evidence.
//!
//! The CPU payload establishes its unit and operation scope; CPU operations
//! must never be interpreted as nanoseconds. Byte values can be exact integers
//! or measurements carrying uncertainty. `None` means unavailable, not zero.

use serde::{Deserialize, Serialize};

/// Modeled CPU work, never measured elapsed or process CPU time.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeledCpu {
    pub cpu_ops: f64,
}

/// A measured quantity whose unit and operation scope are set by its field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measurement {
    pub value: f64,
    pub stddev: Option<f64>,
    pub samples: u32,
    /// Measurement procedure and scope, such as allocator heap versus payload.
    #[serde(default)]
    pub method: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Distinct CPU payloads reject each other's units at the wire boundary;
    /// measured phase metadata survives without filling unknown phases.
    #[test]
    fn cpu_units_and_measurement_metadata_remain_distinct() {
        let modeled = ModeledCpu { cpu_ops: 12.0 };
        assert!(
            serde_json::from_value::<MeasuredCpu>(serde_json::to_value(modeled).unwrap()).is_err()
        );
        let measured = MeasuredCpu {
            update_cpu_ns: Some(Measurement {
                value: 12.0,
                stddev: Some(0.5),
                samples: 5,
                method: Some("test process CPU clock".into()),
            }),
            ..Default::default()
        };
        let encoded = serde_json::to_value(&measured).unwrap();
        assert!(serde_json::from_value::<ModeledCpu>(encoded.clone()).is_err());
        assert_eq!(
            serde_json::from_value::<MeasuredCpu>(encoded).unwrap(),
            measured
        );
        assert!(measured.build_cpu_ns.is_none());
        assert!(measured.read_cpu_ns.is_none());
    }

    /// Default resources preserve unavailable dimensions instead of inventing zeros.
    #[test]
    fn missing_dimensions_remain_unavailable() {
        let resources = PhysicalResources::<Option<f64>, u64>::default();
        assert_eq!(resources.cpu, None);
        assert_eq!(resources.peak_memory_bytes, None);
        assert_eq!(resources.retained_memory_bytes, None);
        assert_eq!(resources.scan_bytes, None);
        assert_eq!(resources.serialized_bytes, None);
        assert_eq!(resources.disk_bytes, None);
    }

    /// Storage occupancy and scan traffic are independent even though both use bytes.
    #[test]
    fn byte_dimensions_round_trip_independently() {
        let resources = PhysicalResources {
            cpu: Some(12.0),
            peak_memory_bytes: Some(100),
            retained_memory_bytes: Some(70),
            scan_bytes: Some(200),
            serialized_bytes: Some(40),
            disk_bytes: Some(4096),
        };
        let json = serde_json::to_value(resources).unwrap();
        let decoded: PhysicalResources<Option<f64>, u64> = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, resources);
    }
}
