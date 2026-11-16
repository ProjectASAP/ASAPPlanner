use super::*;
use asap_cost::{ClusterLimits, ClusterObjective, ResourceUnitCost};

/// Without a declared memory capacity nothing rejects a candidate for
/// its memory, so a peak over `LARGE_PEAK_MEMORY_BYTES` is reported
/// as a warning (the eval's 24 h quantiles plan peaked at 1.43 TB).
/// Under cost model 3 the cluster's memory is declared and its capacity
/// check applies instead, so there is no warning.
#[test]
fn a_very_large_peak_memory_is_warned_about_only_without_a_declared_capacity() {
    let usage_over_time = CostModel::UsageOverTime(ResourceUnitCost::illustrative());
    assert_eq!(
        large_peak_memory_warning(1_430_000_000_000, &usage_over_time).as_deref(),
        Some(
            "peak memory 1430000000000 bytes is over 1000000000000 bytes and the deployment \
             declares no memory capacity to check it against"
        )
    );
    assert_eq!(
        large_peak_memory_warning(LARGE_PEAK_MEMORY_BYTES, &usage_over_time),
        None
    );
    let cluster = CostModel::ClusterCapacity(ClusterLimits {
        memory_bytes: u64::MAX,
        disk_bytes: u64::MAX,
        cpu_cores: None,
        objective: ClusterObjective::LowestLatency,
    });
    assert_eq!(large_peak_memory_warning(1_430_000_000_000, &cluster), None);
}
