//! Resource dimensions shared by analytical estimates and benchmark evidence.
//!
//! The CPU payload establishes its unit and operation scope; CPU operations
//! must never be interpreted as nanoseconds. Byte values can be exact integers
//! or measurements carrying uncertainty. `None` means unavailable, not zero.
//! Cache assumptions share this schema namespace but are not additive resource
//! consumption; their numerical interpretation belongs to an estimator.

pub mod cache;
pub mod cpu;
pub mod measurement;
pub mod physical;
pub mod storage;
pub use cache::{CacheCapacityEvidence, CacheEvidence, CacheProfile};
pub use storage::StorageResources;

pub use cpu::{MeasuredCpu, ModeledCpu};
pub use measurement::Measurement;
pub use physical::{MeasuredResources, PhysicalResources};
pub mod boundary;
pub use boundary::{BoundaryKind, BoundaryResources, MaterializationMedium};

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
