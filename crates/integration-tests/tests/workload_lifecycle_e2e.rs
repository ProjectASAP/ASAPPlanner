//! End-to-end coverage for workload-aware summary-maintenance planning:
//! source workload -> PromQL lowering -> candidate search -> lifecycle-aware
//! global selection -> materialized deployment guarantees.

use std::rc::Rc;

use asap_aware_mapping::cost_model::Cost;
use asap_aware_mapping::CostRate;
use asap_aware_mapping::{
    global_selection_with_summary_maintenance_lifecycles,
    materialize_with_summary_maintenance_lifecycles, search_workload_with, CostModel, Horizon,
    SummaryMaintenanceCapabilities, SummaryMaintenanceLifecycleCapabilities,
    SummaryMaintenanceLifecycleCostInputs, SummaryMaintenanceLifecycleRejection, WorkloadDemand,
};
use asap_frontend_promql::lower_promql_batch;
use asap_types::post_asap::{
    EvaluationSchedule, SummaryMaintenanceLifecycle, SummaryMaintenanceMode, SummaryNode,
};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataArrival, DataWorkload, Evidence, EvidenceSource,
    Predictability, Query, QueryLanguage, QueryRequirements, QueryTimeScope, QueryWorkload, Rate,
    RepeatedDemand, RepeatingEntry, RepetitionInterval, TimeSelection,
};

const NOW_MS: u64 = 1_000_000;

struct FullyCostedRuntime;

impl CostModel for FullyCostedRuntime {
    fn rank_candidates(
        &self,
        _intent: &AggIntent,
        candidates: &[asap_types::post_asap::SketchAlgorithm],
    ) -> Vec<asap_types::post_asap::SketchAlgorithm> {
        candidates.to_vec()
    }

    fn summary_maintenance_lifecycle_cost_inputs(
        &self,
        _summary: &SummaryNode,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        SummaryMaintenanceLifecycleCostInputs {
            build_cost: Some(Cost(10.0)),
            maintenance_cost_per_update: Some(Cost(1.0)),
            summary_read_cost: Some(Cost(1.0)),
            retention_cost_rate: Some(CostRate(0.1)),
            retirement_cost: Some(Cost(1.0)),
        }
    }

    fn summary_maintenance_capabilities(
        &self,
        _summary: &SummaryNode,
    ) -> SummaryMaintenanceCapabilities {
        SummaryMaintenanceCapabilities {
            incremental_update: true,
            merge: true,
            delete: true,
        }
    }
}

fn dashboard_workload() -> QueryWorkload {
    let query = Query("quantile_over_time(0.99, latency[5m])".into());
    let requirements = QueryRequirements {
        accuracy: AccuracyRequirement::Explicit(AccuracyTarget::Epsilon(0.01)),
        ..QueryRequirements::default()
    };
    QueryWorkload {
        language: QueryLanguage::PromQL,
        query_batch: Some(vec![BatchEntry {
            query: query.clone(),
            requirements: requirements.clone(),
            predictability: Predictability::AdHoc,
            invocations: 1,
            execute_at: None,
            time_selection: TimeSelection::default(),
        }]),
        repeating_queries: Some(vec![RepeatingEntry {
            query,
            demand: RepeatedDemand::FixedInterval(RepetitionInterval(1_000)),
            requirements,
            predictability: Predictability::Predictable { known_at: None },
            time_selection: TimeSelection {
                scope: QueryTimeScope::RealTime,
                ..TimeSelection::default()
            },
        }]),
        data_workload: Some(DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            ingestion_rate: Evidence {
                value: Some(Rate(1.0)),
                source: EvidenceSource::Observed,
                observed_at_ms: Some(NOW_MS),
                valid_for_ms: Some(60_000),
            },
            ..DataWorkload::default()
        }),
    }
}

#[test]
fn promql_dashboard_materializes_continuous_summary_with_explained_rejections() {
    let workload = dashboard_workload();
    workload.validate().unwrap();

    let lowered = lower_promql_batch(&workload)
        .into_iter()
        .next()
        .expect("one normalized workload entry")
        .expect("valid PromQL");
    let root = Rc::new(lowered);
    let strategies = asap_aware_mapping::default_strategies_with(&FullyCostedRuntime);
    let space = search_workload_with(vec![("dashboard", Rc::clone(&root))], &strategies);
    let target = Rc::clone(&space.roots[0].1);
    let capabilities = SummaryMaintenanceLifecycleCapabilities {
        ephemeral: true,
        prepared: false,
        shared: false,
        continuously_maintained: true,
    };

    let selection = global_selection_with_summary_maintenance_lifecycles(
        &space,
        &workload,
        &[1],
        NOW_MS,
        Some(Horizon(100.0)),
        capabilities,
        &FullyCostedRuntime,
    )
    .unwrap();
    let plan = materialize_with_summary_maintenance_lifecycles(
        &selection,
        &target,
        WorkloadDemand::new(&workload, &[1]),
        NOW_MS,
        Some(Horizon(100.0)),
        capabilities,
        &FullyCostedRuntime,
    )
    .unwrap()
    .expect("selected summary plan");

    assert!(!plan.selected_raw_recompute);
    assert_eq!(plan.expected_reads, Some(100.0));
    assert_eq!(plan.deployments.len(), 1);

    let deployment = &plan.deployments[0];
    let guarantee = deployment
        .summary_maintenance_lifecycle_guarantee
        .as_ref()
        .expect("selected lifecycle guarantee");
    assert_eq!(
        guarantee.summary_maintenance_lifecycle,
        SummaryMaintenanceLifecycle::ContinuouslyMaintained
    );
    assert_eq!(guarantee.evaluation_schedule, EvaluationSchedule::PerUpdate);
    assert_eq!(
        guarantee.summary_maintenance_mode,
        SummaryMaintenanceMode::Incremental
    );
    assert!(deployment.alternatives.iter().any(|alternative| {
        matches!(
            alternative.summary_maintenance_lifecycle,
            SummaryMaintenanceLifecycle::Prepared { .. }
        ) && alternative.rejection
            == Some(SummaryMaintenanceLifecycleRejection::RequiresPredictableOneTimeQuery)
    }));
    assert!(deployment.alternatives.iter().any(|alternative| {
        matches!(
            alternative.summary_maintenance_lifecycle,
            SummaryMaintenanceLifecycle::Shared { .. }
        ) && alternative.rejection
            == Some(SummaryMaintenanceLifecycleRejection::UnsupportedByRuntime)
    }));
}
