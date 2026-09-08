//! Evidence-backed physical-plan costing at the planner selection boundary.

use std::{cell::RefCell, rc::Rc};

use asap_types::post_asap::{SketchAlgorithm, SummaryExpr, SummaryNode};
use asap_types::pre_asap::{AggIntent, QueryExpr};
use asap_types::resources::CacheProfile;

use crate::analytical_cost::{
    estimate_physical_dag_comparison, AnalyticalCostError,
    EvidenceBackedPhysicalDag as PhysicalDag, PhysicalDagComparisonEstimate,
    PhysicalDagEstimateRequest, PhysicalNodeEvidence, ResourceCalibration,
};
use crate::cost_model::{Cost, CostModel, DefaultCostModel};
use crate::physical_operator_statistics::ComparisonScope;
use crate::query_physical_lowering::{
    lower_query_physical_dag, PhysicalNodeEvidenceProvider, PhysicalNodeRequest,
};
use crate::replacement::{Replacement, ReplacementSubDAG, TargetSubDAG};

/// One immutable generation of deployment evidence for a planner target.
///
/// The opaque version lets a provider bind every subsequent query and summary
/// lookup to the same catalog/runtime snapshot. A cost-model instance retains
/// this value for the target, so a caller that needs fresher evidence creates a
/// new model instead of mixing generations in one ranking decision.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalEvidenceSnapshot {
    pub version: String,
    pub scope: ComparisonScope,
    pub cache_profile: CacheProfile,
    pub storage_io: Option<crate::storage_io::StorageIoProfile>,
}

/// Deployment evidence needed to price one planner alternative.
///
/// The planner lowers raw queries and logical rewrites itself. A deployment
/// supplies the comparison scope and atomic evidence for each selected query
/// operator. Post-ASAP summary operators need a physical binder because their
/// implementation, placement, and retained-state layout are deployment
/// choices; that binder must return the complete summary DAG, including any
/// embedded `KeepPreAsap` work.
pub trait PlannerPhysicalPlanProvider {
    /// Atomically captures the comparison scope and evidence generation.
    fn capture_evidence_snapshot(
        &self,
        target: &TargetSubDAG<'_>,
    ) -> Result<PhysicalEvidenceSnapshot, AnalyticalCostError>;

    fn query_node_evidence(
        &self,
        snapshot: &PhysicalEvidenceSnapshot,
        request: PhysicalNodeRequest<'_>,
    ) -> Result<PhysicalNodeEvidence, AnalyticalCostError>;

    fn summary_physical_dag(
        &self,
        snapshot: &PhysicalEvidenceSnapshot,
        summary: &Rc<SummaryNode>,
        target: &TargetSubDAG<'_>,
    ) -> Result<PhysicalDag, AnalyticalCostError>;
}

/// Dimensional comparison retained for explanations and verification.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalPlanComparison {
    pub resources: PhysicalDagComparisonEstimate,
    pub raw_cost: Cost,
    pub candidate_cost: Cost,
    pub storage_io: Option<(
        crate::storage_io::StorageEstimate,
        crate::storage_io::StorageEstimate,
    )>,
}

/// Planner cost model that admits only complete, cheaper physical plans.
///
/// There is deliberately no structural or compact-formula fallback. Failure
/// to lower either alternative, missing statistics, an unknown physical
/// algorithm, or invalid source/horizon evidence makes the candidate
/// unavailable and leaves the target on its raw path.
pub struct PhysicalPlanCostModel<'a> {
    provider: &'a dyn PlannerPhysicalPlanProvider,
    calibration: ResourceCalibration,
    target_evidence: RefCell<Vec<CachedTargetEvidence>>,
}

struct CachedTargetEvidence {
    root: Rc<QueryExpr>,
    consumer_count: usize,
    snapshot: PhysicalEvidenceSnapshot,
    raw: PhysicalDag,
}

impl<'a> PhysicalPlanCostModel<'a> {
    pub fn new(
        provider: &'a dyn PlannerPhysicalPlanProvider,
        calibration: ResourceCalibration,
    ) -> Result<Self, AnalyticalCostError> {
        // Exported scores retain base calibration provenance even when the
        // objective is priced entirely by supplementary storage operations.
        if calibration.version.trim().is_empty() {
            return Err(AnalyticalCostError::MissingOrStale(
                "resource_calibration.version",
            ));
        }
        // A zero base objective may be supplemented by storage coefficients;
        // that check needs the target's immutable evidence snapshot.
        match calibration.validate() {
            Ok(()) | Err(AnalyticalCostError::ZeroCalibration) => {}
            Err(error) => return Err(error),
        }
        Ok(Self {
            provider,
            calibration,
            target_evidence: RefCell::new(Vec::new()),
        })
    }

    fn target_evidence(
        &self,
        target: &TargetSubDAG<'_>,
    ) -> Result<(PhysicalEvidenceSnapshot, PhysicalDag), AnalyticalCostError> {
        if let Some(cached) = self.target_evidence.borrow().iter().find(|cached| {
            Rc::ptr_eq(&cached.root, target.root) && cached.consumer_count == target.consumer_count
        }) {
            return Ok((cached.snapshot.clone(), cached.raw.clone()));
        }

        let snapshot = self.provider.capture_evidence_snapshot(target)?;
        if snapshot.version.trim().is_empty() {
            return Err(AnalyticalCostError::MissingOrStale(
                "planner_evidence_snapshot.version",
            ));
        }
        snapshot.scope.validate()?;
        let evidence = QueryEvidence {
            provider: self.provider,
            snapshot: &snapshot,
        };
        let raw = lower_query_physical_dag(target.root, &snapshot.scope, &evidence)?;
        self.target_evidence
            .borrow_mut()
            .push(CachedTargetEvidence {
                root: Rc::clone(target.root),
                consumer_count: target.consumer_count,
                snapshot: snapshot.clone(),
                raw: raw.clone(),
            });
        Ok((snapshot, raw))
    }

    pub fn estimate_candidate(
        &self,
        candidate: &ReplacementSubDAG,
        target: &TargetSubDAG<'_>,
    ) -> Result<PhysicalPlanComparison, AnalyticalCostError> {
        let (snapshot, raw) = self.target_evidence(target)?;
        // Aggregate cache hit ratios do not identify which independently rounded
        // extents issue requests. Do not mix cache-adjusted bytes/CPU with
        // uncached request counts until cache-aware extent evidence is available.
        if snapshot.storage_io.is_some()
            && matches!(&snapshot.cache_profile, CacheProfile::Evidence(_))
        {
            return Err(AnalyticalCostError::InvalidCacheEvidence(
                "storage operation costs require an explicit no-cache profile; cache-aware extent evidence is unavailable",
            ));
        }
        let scope = &snapshot.scope;
        let evidence = QueryEvidence {
            provider: self.provider,
            snapshot: &snapshot,
        };
        let replacement = match &candidate.replacement {
            Replacement::Rewrite(query) => lower_query_physical_dag(query, scope, &evidence)?,
            Replacement::Summary(summary) => match &summary.expr {
                SummaryExpr::KeepPreAsap(query) => {
                    lower_query_physical_dag(query, scope, &evidence)?
                }
                _ => self
                    .provider
                    .summary_physical_dag(&snapshot, summary, target)?,
            },
        };
        let resources = estimate_physical_dag_comparison(
            PhysicalDagEstimateRequest {
                nodes: &raw.nodes,
                root: &raw.root,
                scope,
                statistics: &raw,
                cache_profile: &snapshot.cache_profile,
            },
            PhysicalDagEstimateRequest {
                nodes: &replacement.nodes,
                root: &replacement.root,
                scope,
                statistics: &replacement,
                cache_profile: &snapshot.cache_profile,
            },
        )?;
        let storage_io = snapshot
            .storage_io
            .as_ref()
            .map(|profile| {
                Ok((
                    crate::storage_io::estimate_storage_io(
                        &raw,
                        scope,
                        profile,
                        &snapshot.version,
                    )?,
                    crate::storage_io::estimate_storage_io(
                        &replacement,
                        scope,
                        profile,
                        &snapshot.version,
                    )?,
                ))
            })
            .transpose()?;
        let (mut raw_cost, mut candidate_cost) = match self.calibration.validate() {
            Ok(()) => (
                Cost(resources.raw.calibrated_cost(&self.calibration)?),
                Cost(resources.candidate.calibrated_cost(&self.calibration)?),
            ),
            Err(AnalyticalCostError::ZeroCalibration) => {
                // Storage estimation above validates the coefficients and
                // evidence. At least one priced dimension must remain.
                let has_storage_objective = snapshot.storage_io.as_ref().is_some_and(|profile| {
                    let calibration = &profile.calibration;
                    [
                        calibration.cost_per_disk_read,
                        calibration.cost_per_disk_write,
                        calibration.cost_per_object_get,
                        calibration.cost_per_object_put,
                    ]
                    .into_iter()
                    .any(|coefficient| coefficient > 0.0)
                });
                if !has_storage_objective {
                    return Err(AnalyticalCostError::ZeroCalibration);
                }
                (Cost(0.0), Cost(0.0))
            }
            Err(error) => return Err(error),
        };
        if let Some((raw, candidate)) = &storage_io {
            raw_cost.0 += raw.cost;
            candidate_cost.0 += candidate.cost;
        }
        if !raw_cost.0.is_finite() || !candidate_cost.0.is_finite() {
            return Err(AnalyticalCostError::Overflow);
        }
        Ok(PhysicalPlanComparison {
            resources,
            raw_cost,
            candidate_cost,
            storage_io,
        })
    }
}

struct QueryEvidence<'a> {
    provider: &'a dyn PlannerPhysicalPlanProvider,
    snapshot: &'a PhysicalEvidenceSnapshot,
}

impl PhysicalNodeEvidenceProvider for QueryEvidence<'_> {
    fn evidence(
        &self,
        request: PhysicalNodeRequest<'_>,
    ) -> Result<PhysicalNodeEvidence, AnalyticalCostError> {
        self.provider.query_node_evidence(self.snapshot, request)
    }
}

impl CostModel for PhysicalPlanCostModel<'_> {
    fn candidate_cost_covers_complete_plan(&self) -> bool {
        true
    }

    fn candidate_cost(
        &self,
        candidate: &ReplacementSubDAG,
        target: &TargetSubDAG<'_>,
    ) -> Option<Cost> {
        self.estimate_candidate(candidate, target)
            .ok()
            .filter(|estimate| estimate.candidate_cost < estimate.raw_cost)
            .map(|estimate| estimate.candidate_cost)
    }

    fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm> {
        // Physical plans do not exist at algorithm enumeration time. Final
        // ranking happens after binding, through `candidate_cost` above.
        DefaultCostModel.rank_candidates(intent, candidates)
    }

    fn estimate_cost(&self, candidate: &ReplacementSubDAG, target: &TargetSubDAG<'_>) -> f64 {
        self.candidate_cost(candidate, target)
            .map_or(f64::NAN, |cost| cost.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::HashMap;

    use asap_types::pre_asap::{Column, DataType, QueryExpr, Reduction, Schema, Source};
    use asap_types::types::AccuracyTarget;
    use asap_types::workload::{
        DataArrival, DurationMs, QueryRecurrence, QueryTimeScope, TimeSelection, TimestampMs,
    };

    use crate::analytical_cost::{ExecutionMultiplicity, PhysicalDagNode, PhysicalOperator};
    use crate::physical_operator_statistics::{
        EdgeStatistics, OperatorStatistics, SourceCoverage, UnaryEdgeStatistics,
    };
    use crate::replacement::ReplacementStrategy;

    fn edge(rows: u64, bytes: u64) -> EdgeStatistics {
        EdgeStatistics { rows, bytes }
    }

    fn unary_edges(input: EdgeStatistics, output: EdgeStatistics) -> UnaryEdgeStatistics {
        UnaryEdgeStatistics {
            input,
            output,
            promql: None,
        }
    }

    fn scan_statistics(source_read_bytes: u64, edge: EdgeStatistics) -> OperatorStatistics {
        OperatorStatistics::Scan {
            edges: unary_edges(edge, edge),
            source_read_bytes,
        }
    }

    fn aggregate_statistics(input: EdgeStatistics, output: EdgeStatistics) -> OperatorStatistics {
        OperatorStatistics::HashAggregate {
            edges: unary_edges(input, output),
            group_count: 1,
            key_bytes: 0,
            accumulator_bytes_per_group: 8,
        }
    }

    fn pass_through_statistics(edge: EdgeStatistics) -> OperatorStatistics {
        OperatorStatistics::PassThrough {
            edges: unary_edges(edge, edge),
        }
    }

    fn query() -> Rc<QueryExpr> {
        Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::by(vec![]),
            measures: vec![AggIntent::Count {
                accuracy: AccuracyTarget::Epsilon(0.01),
            }],
            output_names: vec![],
            having: None,
            child: Rc::new(QueryExpr::Scan {
                source: Source::Table {
                    table_ref: "events".into(),
                },
                predicates: vec![],
                schema: Schema::new(vec![Column::new("value", DataType::Float64, false)]),
            }),
        })
    }

    fn scope() -> ComparisonScope {
        ComparisonScope {
            data_arrival: DataArrival::AtRest,
            planning_time: TimestampMs(1_000),
            horizon: DurationMs(10_000),
            recurrence: QueryRecurrence::OneTime {
                invocations: 10,
                execute_at: None,
            },
            time_selection: TimeSelection {
                scope: QueryTimeScope::Longitudinal,
                lookback: Some(DurationMs(10_000)),
                as_of: Some(TimestampMs(1_000)),
            },
            sources: vec![SourceCoverage {
                source: Source::Table {
                    table_ref: "events".into(),
                },
                source_snapshot_id: "snapshot-1".into(),
                predicates: vec![],
                info_matchers: vec![],
            }],
        }
    }

    struct TestProvider {
        storage_io: Option<crate::storage_io::StorageIoProfile>,
        summary_available: bool,
        candidate_scan_bytes: u64,
        snapshot_calls: Cell<u64>,
        raw_evidence_calls: Cell<u64>,
    }

    impl TestProvider {
        fn new(summary_available: bool, candidate_scan_bytes: u64) -> Self {
            Self {
                storage_io: None,
                summary_available,
                candidate_scan_bytes,
                snapshot_calls: Cell::new(0),
                raw_evidence_calls: Cell::new(0),
            }
        }

        fn summary_dag(&self, scope: &ComparisonScope) -> PhysicalDag {
            let scan_statistics = scan_statistics(self.candidate_scan_bytes, edge(100, 800));
            let aggregate_statistics = aggregate_statistics(edge(100, 800), edge(1, 8));
            let read_statistics = pass_through_statistics(edge(1, 8));
            let evidence = HashMap::from([
                (
                    "candidate-scan".into(),
                    PhysicalNodeEvidence {
                        physical_id: "candidate-scan".into(),
                        statistics: scan_statistics,
                        output_buffer_bytes: 8,
                    },
                ),
                (
                    "candidate-state".into(),
                    PhysicalNodeEvidence {
                        physical_id: "candidate-state".into(),
                        statistics: aggregate_statistics,
                        output_buffer_bytes: 8,
                    },
                ),
                (
                    "candidate-read".into(),
                    PhysicalNodeEvidence {
                        physical_id: "candidate-read".into(),
                        statistics: read_statistics,
                        output_buffer_bytes: 8,
                    },
                ),
            ]);
            PhysicalDag {
                nodes: vec![
                    PhysicalDagNode {
                        id: "candidate-scan".into(),
                        operator: PhysicalOperator::Scan,
                        children: vec![],
                        source_coverage: Some(scope.sources[0].clone()),
                        output_buffer_bytes: 8,
                        retained_bytes: 0,
                        execution: ExecutionMultiplicity::Once,
                    },
                    PhysicalDagNode {
                        id: "candidate-state".into(),
                        operator: PhysicalOperator::HashAggregate {
                            grouping_key_count: 0,
                            accumulator_count: 1,
                        },
                        children: vec!["candidate-scan".into()],
                        source_coverage: None,
                        output_buffer_bytes: 8,
                        retained_bytes: 8,
                        execution: ExecutionMultiplicity::Once,
                    },
                    PhysicalDagNode {
                        id: "candidate-read".into(),
                        operator: PhysicalOperator::PassThrough,
                        children: vec!["candidate-state".into()],
                        source_coverage: None,
                        output_buffer_bytes: 8,
                        retained_bytes: 0,
                        execution: ExecutionMultiplicity::PerEvaluation,
                    },
                ],
                root: "candidate-read".into(),
                evidence,
            }
        }
    }

    impl PlannerPhysicalPlanProvider for TestProvider {
        fn capture_evidence_snapshot(
            &self,
            _target: &TargetSubDAG<'_>,
        ) -> Result<PhysicalEvidenceSnapshot, AnalyticalCostError> {
            self.snapshot_calls.set(self.snapshot_calls.get() + 1);
            Ok(PhysicalEvidenceSnapshot {
                version: "test-snapshot-1".into(),
                scope: scope(),
                cache_profile: CacheProfile::no_cache(),
                storage_io: self.storage_io.clone(),
            })
        }

        fn query_node_evidence(
            &self,
            snapshot: &PhysicalEvidenceSnapshot,
            request: PhysicalNodeRequest<'_>,
        ) -> Result<PhysicalNodeEvidence, AnalyticalCostError> {
            assert_eq!(snapshot.version, "test-snapshot-1");
            self.raw_evidence_calls
                .set(self.raw_evidence_calls.get() + 1);
            let (physical_id, statistics) = match request.operator {
                PhysicalOperator::Scan => ("raw-scan", scan_statistics(800, edge(100, 800))),
                PhysicalOperator::HashAggregate {
                    grouping_key_count: 0,
                    accumulator_count: 1,
                } => (
                    "raw-aggregate",
                    aggregate_statistics(edge(100, 800), edge(1, 8)),
                ),
                _ => return Err(AnalyticalCostError::UnsupportedQueryOperator),
            };
            Ok(PhysicalNodeEvidence {
                physical_id: physical_id.into(),
                output_buffer_bytes: 8,
                statistics,
            })
        }

        fn summary_physical_dag(
            &self,
            snapshot: &PhysicalEvidenceSnapshot,
            _summary: &Rc<SummaryNode>,
            _target: &TargetSubDAG<'_>,
        ) -> Result<PhysicalDag, AnalyticalCostError> {
            assert_eq!(snapshot.version, "test-snapshot-1");
            self.summary_available
                .then(|| self.summary_dag(&snapshot.scope))
                .ok_or(AnalyticalCostError::MissingOrStale("summary_physical_plan"))
        }
    }

    fn calibration() -> ResourceCalibration {
        ResourceCalibration {
            cost_per_cpu_op: 1.0,
            cost_per_scan_byte: 1.0,
            cost_per_retained_byte: 1.0,
            version: "test-v1".into(),
        }
    }

    /// Both base-priced and storage-only objectives need identifiable calibration.
    #[test]
    fn blank_base_calibration_version_is_rejected_before_snapshot_lookup() {
        let provider = TestProvider::new(true, 800);
        for version in ["", " \t\n"] {
            for zero_base in [false, true] {
                let mut base = calibration();
                base.version = version.into();
                if zero_base {
                    base.cost_per_cpu_op = 0.0;
                    base.cost_per_scan_byte = 0.0;
                    base.cost_per_retained_byte = 0.0;
                }
                assert!(matches!(
                    PhysicalPlanCostModel::new(&provider, base),
                    Err(AnalyticalCostError::MissingOrStale(
                        "resource_calibration.version"
                    ))
                ));
            }
        }
        assert_eq!(provider.snapshot_calls.get(), 0);
    }

    // A request-only objective ranks complete evidence and rejects absent,
    // zero, or invalid supplemental calibration.
    #[test]
    fn storage_only_objective_requires_positive_valid_storage_calibration() {
        use crate::storage_io::*;
        let root = query();
        let target = TargetSubDAG::new(&root);
        let mut provider = TestProvider::new(true, 800);
        let snapshot = provider.capture_evidence_snapshot(&target).unwrap();
        let raw = lower_query_physical_dag(
            &root,
            &snapshot.scope,
            &QueryEvidence {
                provider: &provider,
                snapshot: &snapshot,
            },
        )
        .unwrap();
        let mut profile = StorageIoProfile {
            evidence_version: snapshot.version,
            observed_at_ms: 900,
            valid_until_ms: 2000,
            calibration: StorageCalibration {
                version: "requests-v1".into(),
                cost_per_disk_read: 0.0,
                cost_per_disk_write: 0.0,
                cost_per_object_get: 1.0,
                cost_per_object_put: 0.0,
            },
            nodes: HashMap::new(),
        };
        for dag in [raw, provider.summary_dag(&snapshot.scope)] {
            for node in dag.nodes {
                let statistics = dag.evidence[&node.id].statistics.clone();
                let accesses = match statistics {
                    OperatorStatistics::Scan {
                        source_read_bytes, ..
                    } => vec![StorageAccess {
                        operation: StorageOperation::ObjectGet,
                        extent_bytes: vec![source_read_bytes],
                        bytes_per_request: 400,
                    }],
                    _ => vec![],
                };
                profile.nodes.insert(
                    node.id.clone(),
                    StorageNodeEvidence {
                        node,
                        statistics,
                        accesses,
                    },
                );
            }
        }
        let base = ResourceCalibration {
            cost_per_cpu_op: 0.0,
            cost_per_scan_byte: 0.0,
            cost_per_retained_byte: 0.0,
            version: "unused-base-v1".into(),
        };
        let candidates =
            crate::replacement::SketchAlgorithmStrategy::default_cost_model().replacements(&target);
        provider.storage_io = Some(profile.clone());
        let model = PhysicalPlanCostModel::new(&provider, base.clone()).unwrap();
        let estimate = model.estimate_candidate(&candidates[0], &target).unwrap();
        assert_eq!(estimate.raw_cost.0, 20.0);
        assert_eq!(estimate.candidate_cost.0, 2.0);
        assert_eq!(
            model.candidate_cost(&candidates[0], &target),
            Some(Cost(2.0))
        );
        drop(model);
        for coefficient in [0.0, -1.0, f64::NAN] {
            profile.calibration.cost_per_object_get = coefficient;
            provider.storage_io = Some(profile.clone());
            let model = PhysicalPlanCostModel::new(&provider, base.clone()).unwrap();
            assert!(model.estimate_candidate(&candidates[0], &target).is_err());
        }
        provider.storage_io = None;
        let model = PhysicalPlanCostModel::new(&provider, base).unwrap();
        assert_eq!(
            model.estimate_candidate(&candidates[0], &target),
            Err(AnalyticalCostError::ZeroCalibration)
        );
    }

    /// Shared count defaults must not turn an omitted profile into measured zero I/O.
    #[test]
    fn missing_storage_profile_remains_unestimated() {
        let root = query();
        let target = TargetSubDAG::new(&root);
        let candidates =
            crate::replacement::SketchAlgorithmStrategy::default_cost_model().replacements(&target);
        let provider = TestProvider::new(true, 800);
        let model = PhysicalPlanCostModel::new(&provider, calibration()).unwrap();
        let estimate = model.estimate_candidate(&candidates[0], &target).unwrap();
        assert!(estimate.storage_io.is_none());
    }

    #[test]
    fn global_selection_uses_complete_physical_comparison() {
        let root = query();
        let space = crate::replacement::search_workload_with(
            vec![("q", Rc::clone(&root))],
            &crate::replacement::default_strategies(),
        );
        let planned_root = Rc::clone(&space.roots[0].1);
        let provider = TestProvider::new(true, 800);
        let model = PhysicalPlanCostModel::new(&provider, calibration()).unwrap();

        let selected = space.global_selection(&model);
        assert!(
            selected.for_target(&planned_root).unwrap().chosen.is_some(),
            "a fully bound build-once summary cheaper than ten raw scans must be selected"
        );
    }

    // Ranking must retain the uncovered byte of a nearly resident buffer cache.
    #[test]
    fn tiny_buffer_misses_still_affect_global_selection() {
        use crate::analytical_cost::{CacheCapacityEvidence, CacheEvidence};
        const WORKING_SET: u64 = 1_u64 << 63;
        struct AlmostResident(TestProvider);
        impl PlannerPhysicalPlanProvider for AlmostResident {
            fn capture_evidence_snapshot(
                &self,
                target: &TargetSubDAG<'_>,
            ) -> Result<PhysicalEvidenceSnapshot, AnalyticalCostError> {
                let mut snapshot = self.0.capture_evidence_snapshot(target)?;
                snapshot.cache_profile = CacheProfile::Evidence(CacheEvidence {
                    version: "almost-resident-v1".into(),
                    distinct_evaluations: 10,
                    repeated_identical_evaluations: 0,
                    result_invalidation_ratio: None,
                    result_cache: CacheCapacityEvidence {
                        working_set_bytes: 1,
                        capacity_bytes: 0,
                    },
                    buffer_cache: CacheCapacityEvidence {
                        working_set_bytes: WORKING_SET,
                        capacity_bytes: WORKING_SET - 1,
                    },
                });
                Ok(snapshot)
            }
            fn query_node_evidence(
                &self,
                snapshot: &PhysicalEvidenceSnapshot,
                request: PhysicalNodeRequest<'_>,
            ) -> Result<PhysicalNodeEvidence, AnalyticalCostError> {
                let mut evidence = self.0.query_node_evidence(snapshot, request)?;
                if let OperatorStatistics::Scan {
                    source_read_bytes, ..
                } = &mut evidence.statistics
                {
                    *source_read_bytes = WORKING_SET;
                }
                Ok(evidence)
            }
            fn summary_physical_dag(
                &self,
                snapshot: &PhysicalEvidenceSnapshot,
                summary: &Rc<SummaryNode>,
                target: &TargetSubDAG<'_>,
            ) -> Result<PhysicalDag, AnalyticalCostError> {
                self.0.summary_physical_dag(snapshot, summary, target)
            }
        }
        let space = crate::replacement::search_workload_with(
            vec![("q", query())],
            &crate::replacement::default_strategies(),
        );
        let provider = AlmostResident(TestProvider::new(true, WORKING_SET));
        let model = PhysicalPlanCostModel::new(
            &provider,
            ResourceCalibration {
                cost_per_cpu_op: 0.0,
                cost_per_scan_byte: 1.0,
                cost_per_retained_byte: 0.0,
                version: "disk-only-v1".into(),
            },
        )
        .unwrap();
        let selected = space.global_selection(&model);
        assert!(
            selected
                .for_target(&space.roots[0].1)
                .unwrap()
                .chosen
                .is_some(),
            "one remaining build read must rank ahead of ten remaining raw reads"
        );
    }

    // Versioned physical ranking cannot accept an anonymous calibration generation.
    #[test]
    fn blank_calibration_version_is_rejected_before_ranking() {
        let provider = TestProvider::new(true, 800);
        for version in ["", " \t\n"] {
            let mut coefficients = calibration();
            coefficients.version = version.into();
            assert!(matches!(
                PhysicalPlanCostModel::new(&provider, coefficients),
                Err(AnalyticalCostError::MissingOrStale(
                    "resource_calibration.version"
                ))
            ));
        }
    }

    #[test]
    fn missing_summary_evidence_keeps_the_raw_target() {
        let root = query();
        let space = crate::replacement::search_workload_with(
            vec![("q", Rc::clone(&root))],
            &crate::replacement::default_strategies(),
        );
        let planned_root = Rc::clone(&space.roots[0].1);
        let provider = TestProvider::new(false, 800);
        let model = PhysicalPlanCostModel::new(&provider, calibration()).unwrap();

        let selected = space.global_selection(&model);
        assert!(
            selected.for_target(&planned_root).unwrap().chosen.is_none(),
            "missing physical summary evidence must not fall back to a structural estimate"
        );
    }

    #[test]
    fn candidate_with_incomplete_scope_is_unavailable() {
        struct WrongScope(TestProvider);
        impl PlannerPhysicalPlanProvider for WrongScope {
            fn capture_evidence_snapshot(
                &self,
                target: &TargetSubDAG<'_>,
            ) -> Result<PhysicalEvidenceSnapshot, AnalyticalCostError> {
                self.0.capture_evidence_snapshot(target)
            }

            fn query_node_evidence(
                &self,
                snapshot: &PhysicalEvidenceSnapshot,
                request: PhysicalNodeRequest<'_>,
            ) -> Result<PhysicalNodeEvidence, AnalyticalCostError> {
                self.0.query_node_evidence(snapshot, request)
            }

            fn summary_physical_dag(
                &self,
                snapshot: &PhysicalEvidenceSnapshot,
                summary: &Rc<SummaryNode>,
                target: &TargetSubDAG<'_>,
            ) -> Result<PhysicalDag, AnalyticalCostError> {
                let mut dag = self.0.summary_physical_dag(snapshot, summary, target)?;
                dag.nodes[0]
                    .source_coverage
                    .as_mut()
                    .unwrap()
                    .source_snapshot_id = "other".into();
                Ok(dag)
            }
        }

        let root = query();
        let candidates = crate::replacement::SketchAlgorithmStrategy::default_cost_model()
            .replacements(&TargetSubDAG::new(&root));
        let provider = WrongScope(TestProvider::new(true, 800));
        let model = PhysicalPlanCostModel::new(&provider, calibration()).unwrap();
        assert_eq!(
            model.candidate_cost(&candidates[0], &TargetSubDAG::new(&root)),
            None
        );
    }

    #[test]
    fn blank_snapshot_version_is_unavailable_before_evidence_lookup() {
        struct BlankVersionProvider;
        impl PlannerPhysicalPlanProvider for BlankVersionProvider {
            fn capture_evidence_snapshot(
                &self,
                _target: &TargetSubDAG<'_>,
            ) -> Result<PhysicalEvidenceSnapshot, AnalyticalCostError> {
                Ok(PhysicalEvidenceSnapshot {
                    version: "  \t".into(),
                    scope: scope(),
                    cache_profile: CacheProfile::no_cache(),
                    storage_io: None,
                })
            }

            fn query_node_evidence(
                &self,
                _snapshot: &PhysicalEvidenceSnapshot,
                _request: PhysicalNodeRequest<'_>,
            ) -> Result<PhysicalNodeEvidence, AnalyticalCostError> {
                panic!("blank snapshot versions must fail before evidence lookup")
            }

            fn summary_physical_dag(
                &self,
                _snapshot: &PhysicalEvidenceSnapshot,
                _summary: &Rc<SummaryNode>,
                _target: &TargetSubDAG<'_>,
            ) -> Result<PhysicalDag, AnalyticalCostError> {
                panic!("blank snapshot versions must fail before summary binding")
            }
        }

        let root = query();
        let candidates = crate::replacement::SketchAlgorithmStrategy::default_cost_model()
            .replacements(&TargetSubDAG::new(&root));
        let model = PhysicalPlanCostModel::new(&BlankVersionProvider, calibration()).unwrap();
        assert_eq!(
            model.candidate_cost(&candidates[0], &TargetSubDAG::new(&root)),
            None
        );
    }

    #[test]
    fn complete_candidate_that_costs_more_than_raw_is_not_selected() {
        let root = query();
        let space = crate::replacement::search_workload_with(
            vec![("q", Rc::clone(&root))],
            &crate::replacement::default_strategies(),
        );
        let planned_root = Rc::clone(&space.roots[0].1);
        let provider = TestProvider::new(true, 100_000);
        let model = PhysicalPlanCostModel::new(&provider, calibration()).unwrap();

        let selected = space.global_selection(&model);
        assert!(selected.for_target(&planned_root).unwrap().chosen.is_none());
    }

    #[test]
    fn sibling_candidates_share_one_scope_and_raw_baseline() {
        let root = query();
        let candidates = crate::replacement::SketchAlgorithmStrategy::default_cost_model()
            .replacements(&TargetSubDAG::new(&root));
        assert!(candidates.len() >= 2);
        let provider = TestProvider::new(true, 800);
        let model = PhysicalPlanCostModel::new(&provider, calibration()).unwrap();
        let target = TargetSubDAG::new(&root);

        model.estimate_candidate(&candidates[0], &target).unwrap();
        model.estimate_candidate(&candidates[1], &target).unwrap();

        assert_eq!(provider.snapshot_calls.get(), 1);
        assert_eq!(
            provider.raw_evidence_calls.get(),
            2,
            "the scan and aggregate evidence for the raw baseline are captured once"
        );
    }
}
