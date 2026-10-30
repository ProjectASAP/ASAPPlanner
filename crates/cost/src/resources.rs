//! Resource dimensions of analytical estimates.
//!
//! The CPU payload establishes its unit and operation scope; CPU operations
//! must never be interpreted as nanoseconds. `None` means unavailable, not zero.

pub mod cpu;
pub mod physical;

pub use cpu::ModeledCpu;
pub use physical::PhysicalResources;
#[cfg(test)]
mod tests {
    use super::*;

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
