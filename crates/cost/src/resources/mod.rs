//! Resource dimensions of analytical estimates.
//!
//! The CPU payload establishes its unit and operation scope; CPU operations
//! must never be interpreted as nanoseconds. `None` means unavailable, not zero.

mod cpu;
mod physical;

pub(crate) use cpu::ModeledCpu;
pub(crate) use physical::PhysicalResources;

use crate::QueryLatency;

/// What one candidate uses, per second of wall time unless the name says
/// otherwise, without prices: its nodes' CPU, scans, retained memory and disk
/// added up, with the candidate's peak cores and per-query latency. Every [`CostModel`](crate::CostModel) reads
/// it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResourceUsage {
    /// CPU operations per second: ingestion-time and query-time work.
    pub cpu_ops_per_second: f64,
    /// Bytes scanned per second.
    pub scanned_bytes_per_second: f64,
    /// Memory held between computations, averaged over time: retained
    /// byte-seconds per second (ingestion-time state always, query-time
    /// state while its evaluation runs).
    pub average_memory_bytes: f64,
    /// The most memory live at once, assuming every evaluation overlaps:
    /// in-memory materialized state plus each computation's working memory, once
    /// per consumer that recomputes it, with the consumers materialized at ingestion time
    /// of one refresh window counted once.
    pub peak_memory_bytes: u64,
    /// Bytes retained on disk: state materialized on `StorageMedium::Disk`, plus the
    /// raw data query-time scans read (the longest window of each source).
    pub disk_bytes: u64,
    /// CPU cores to provision for the peak: the ingestion work's cores plus
    /// one per query evaluating at once.
    pub peak_cpu_cores: u32,
    /// One per query, in workload order: its latency per evaluation.
    pub latency_per_query: Vec<QueryLatency>,
}
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
