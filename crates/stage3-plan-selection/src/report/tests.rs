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

/// Stage 2's alternatives pruned as `reason`, one per entry, all of the
/// same (query-time, None) candidate: only their reasons are read.
fn pruned_as(reasons: &[PruneReason]) -> Vec<PrunedCandidate<PhysicalCandidate>> {
    use asap_types::ir::properties::ExecutionTiming;
    use asap_types::stages::{CandidateForMemoGroupId, Lifetime, MemoGroupId, PhysicalProperties};
    reasons
        .iter()
        .map(|&reason| PrunedCandidate {
            candidate: PhysicalCandidate {
                logical_candidate_id: CandidateForMemoGroupId {
                    group: MemoGroupId(0),
                    index_in_group: 0,
                },
                physical_properties: PhysicalProperties::new(
                    ExecutionTiming::QueryTime,
                    Lifetime::None,
                )
                .unwrap(),
            },
            reason,
        })
        .collect()
}

/// A rejection is "excluded by the deployment" only when a source the
/// default deployment lacks pruned something: the default's own exclusion
/// (no query-time result kept across evaluations) leaves today's
/// `Infeasible`. Otherwise the reason counts each such source, the
/// caller's own by index, and names the query.
#[test]
fn only_exclusions_beyond_the_default_deployments_make_the_rejection() {
    let default_only = pruned_as(&[
        PruneReason::ExcludedByDeployment(ExclusionSource::QueryTimeResultsNotRetained),
        PruneReason::NoIngestionPipeline,
    ]);
    assert!(excluded_by_deployment(&default_only, Some(0)).is_none());
    assert!(excluded_by_deployment(&[], None).is_none());

    let restricted = pruned_as(&[
        PruneReason::ExcludedByDeployment(ExclusionSource::QueryTimeResultsNotRetained),
        PruneReason::ExcludedByDeployment(ExclusionSource::CounterNotExtrapolated),
        PruneReason::ExcludedByDeployment(ExclusionSource::ExcludedCandidate { index: 1 }),
        PruneReason::ExcludedByDeployment(ExclusionSource::CounterNotExtrapolated),
    ]);
    let Some(ErrorSelectingPlan::NoValidCandidate(rejections)) =
        excluded_by_deployment(&restricted, Some(0))
    else {
        panic!("expected a rejection");
    };
    assert_eq!(rejections.len(), 1);
    assert_eq!(rejections[0].cause, RejectionCause::ExcludedByDeployment);
    assert_eq!(
        rejections[0].reason,
        "query 0 has no candidate left to answer it; excluded by the deployment: \
         counter_not_extrapolated (2 candidates), excluded_candidates[1] (1 candidate); \
         the workload may be infeasible without these exclusions too"
    );
}
