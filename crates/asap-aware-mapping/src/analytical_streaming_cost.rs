//! Analytical resource cost for incrementally maintained summary deployments.
//!
//! The canonical workload and lifecycle types own deployment semantics. This
//! module only adds physical evidence absent from those schemas: state size,
//! window counts, and per-operation CPU measurements or complexity estimates.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::post_asap::{
    SketchAlgorithm, SummaryExpr, SummaryMaintenanceLifecycle,
    SummaryMaintenanceLifecycleGuarantee, SummaryMaintenanceMode, SummaryNode,
};
use asap_types::pre_asap::{agg_intent::AggIntent, QueryExpr};
use asap_types::workload::{DataArrival, DataWorkload, QueryRecurrence, RepeatedDemand};
use serde::{Deserialize, Serialize};

use crate::analytical_cost::{AnalyticalCostError, ResourceCalibration, ResourceEstimate};
use crate::analytical_statistics::ComparisonScope;
use crate::cost_model::CostedSummaryDeployment;
use crate::cost_model::{Cost, CostModel, DefaultCostModel};
use crate::recurrence::CostRate;
use crate::replacement::{Replacement, ReplacementSubDAG, TargetSubDAG};
use crate::summary_maintenance_lifecycle::{
    evaluation_schedule, maintenance_mode, SummaryMaintenanceCapabilities,
    SummaryMaintenanceLifecycleCostInputs,
};

pub const ANALYTICAL_STREAMING_MODEL_VERSION: &str = "analytical-summary-incremental-v1";

/// Physical evidence that is not represented by [`DataWorkload`] for one
/// incrementally maintained summary deployment. Window counts describe the
/// already-selected physical deployment; this layer does not define another
/// tumbling/sliding policy enum.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StreamingPhysicalInputEvidence {
    /// Logical bytes in the snapshot used to bootstrap the state.
    pub initial_input_bytes: u64,
    /// Source bytes read while bootstrapping. Arriving stream bytes are not a
    /// disk scan and are therefore excluded.
    pub initial_source_scan_bytes: u64,
    /// Simultaneously open windows receiving each arriving item.
    pub active_window_count: u64,
    /// Window/state partitions receiving each bootstrap row.
    pub bootstrap_window_count: u64,
    /// Completed windows retained for query coverage.
    pub retained_window_count: u64,
    /// Independent state instances per window: one for shared
    /// multi-subpopulation state, otherwise the resolved group count.
    pub physical_sketch_count: u64,
    /// Resident bytes of one concrete state instance.
    pub state_bytes_per_sketch: u64,
}

/// Workload-normalized inputs for incremental maintenance over one finite
/// comparison horizon.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StreamingSummaryInputs {
    pub initial_input_rows: u64,
    pub initial_input_bytes: u64,
    pub initial_source_scan_bytes: u64,
    pub ingestion_rate_per_second: f64,
    pub active_window_count: u64,
    pub bootstrap_window_count: u64,
    pub retained_window_count: u64,
    pub physical_sketch_count: u64,
    pub state_bytes_per_sketch: u64,
}

impl StreamingSummaryInputs {
    /// Resolve snapshot size, arriving rows, and reads from the canonical
    /// workload. Positive fractional expected work rounds up conservatively.
    ///
    /// `Mixed` fails closed because today's workload schema cannot distinguish
    /// its at-rest backlog from its continuing-arrival cardinality.
    pub fn from_workload(
        physical: StreamingPhysicalInputEvidence,
        data: &DataWorkload,
        scope: &ComparisonScope,
    ) -> Result<Self, AnalyticalCostError> {
        let _ = scope.validate()?;
        if data.arrival != DataArrival::ContinuouslyIngesting || scope.data_arrival != data.arrival
        {
            return Err(AnalyticalCostError::UnsupportedDataArrival(data.arrival));
        }
        let initial_input_rows = data
            .input_cardinality
            .value_at(scope.planning_time.0)
            .copied()
            .ok_or(AnalyticalCostError::MissingOrStale("input_cardinality"))?;
        let ingestion_rate = data
            .ingestion_rate
            .value_at(scope.planning_time.0)
            .copied()
            .ok_or(AnalyticalCostError::MissingOrStale("ingestion_rate"))?;
        if !ingestion_rate.0.is_finite() || ingestion_rate.0 < 0.0 {
            return Err(AnalyticalCostError::InvalidIngestionRate(ingestion_rate.0));
        }
        Self {
            initial_input_rows,
            initial_input_bytes: physical.initial_input_bytes,
            initial_source_scan_bytes: physical.initial_source_scan_bytes,
            ingestion_rate_per_second: ingestion_rate.0,
            active_window_count: physical.active_window_count,
            bootstrap_window_count: physical.bootstrap_window_count,
            retained_window_count: physical.retained_window_count,
            physical_sketch_count: physical.physical_sketch_count,
            state_bytes_per_sketch: physical.state_bytes_per_sketch,
        }
        .validate()
    }

    pub fn validate(self) -> Result<Self, AnalyticalCostError> {
        for (name, value) in [
            ("active_window_count", self.active_window_count),
            ("bootstrap_window_count", self.bootstrap_window_count),
            ("physical_sketch_count", self.physical_sketch_count),
            ("state_bytes_per_sketch", self.state_bytes_per_sketch),
        ] {
            if value == 0 {
                return Err(AnalyticalCostError::MissingOrZero(name));
            }
        }
        if (self.initial_input_rows == 0) != (self.initial_input_bytes == 0)
            || (self.initial_input_rows == 0 && self.initial_source_scan_bytes != 0)
        {
            return Err(AnalyticalCostError::InconsistentBootstrapEvidence);
        }
        if !self.ingestion_rate_per_second.is_finite() || self.ingestion_rate_per_second < 0.0 {
            return Err(AnalyticalCostError::InvalidIngestionRate(
                self.ingestion_rate_per_second,
            ));
        }
        Ok(self)
    }
}

/// CPU operations for one concrete state operation on one state instance.
/// Missing evidence is legal only when the selected summary DAG does not use
/// that operation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SummaryOperationCpuEvidence {
    pub insert_cpu_ops: Option<f64>,
    pub merge_cpu_ops: Option<f64>,
    pub subtract_cpu_ops: Option<f64>,
    pub delete_cpu_ops: Option<f64>,
    /// Expirations/retractions routed to this DAG per second. Required only
    /// when an explicit `SummaryDelete` is present.
    pub delete_events_per_second: Option<f64>,
    /// Concrete state instances touched by one delete event.
    pub delete_routing_fanout: Option<u64>,
    pub readout_cpu_ops: Option<f64>,
}

/// Physical evidence for one `SummaryJoin` implementation. Cardinality and
/// working memory cannot be inferred from the logical join key alone.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SummaryJoinEvidence {
    pub matched_state_pairs_per_evaluation: u64,
    pub cpu_ops_per_matched_pair: f64,
    pub working_memory_bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamingAggregateEvidence {
    /// Index into `ComparisonScope.sources` for this state's bootstrap input.
    pub source_coverage_index: usize,
    /// Provider-owned identity of the physical bootstrap read. Equal source
    /// coverage alone does not prove two independent builds share I/O.
    pub bootstrap_read_identity: String,
    pub inputs: StreamingSummaryInputs,
    pub cpu: SummaryOperationCpuEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamingSummaryOperatorEvidence {
    pub cpu_ops: f64,
    pub working_memory_bytes: u64,
    /// Executions of this physical operator for one query evaluation.
    /// This is provider evidence, not inferred from a descendant state.
    pub executions_per_evaluation: u64,
    pub events_per_second: Option<f64>,
    pub routing_fanout: Option<u64>,
}

/// Physical evidence bound to the selected DAG's `Rc` identity. A copied,
/// structurally equal node is not silently treated as the same deployment.
#[derive(Debug, Clone, Default)]
pub struct StreamingNodeEvidence {
    aggregations: HashMap<*const SummaryNode, StreamingAggregateEvidence>,
    joins: HashMap<*const SummaryNode, SummaryJoinEvidence>,
    operations: HashMap<*const SummaryNode, StreamingSummaryOperatorEvidence>,
    operation_state_owners: HashMap<*const SummaryNode, *const SummaryNode>,
}

impl StreamingNodeEvidence {
    pub fn insert_aggregation(
        &mut self,
        node: &Rc<SummaryNode>,
        evidence: StreamingAggregateEvidence,
    ) {
        self.aggregations.insert(Rc::as_ptr(node), evidence);
    }

    pub fn insert_join(&mut self, node: &Rc<SummaryNode>, evidence: SummaryJoinEvidence) {
        self.joins.insert(Rc::as_ptr(node), evidence);
    }

    pub fn insert_operation(
        &mut self,
        node: &Rc<SummaryNode>,
        evidence: StreamingSummaryOperatorEvidence,
    ) {
        self.operations.insert(Rc::as_ptr(node), evidence);
    }

    /// Bind a stateful operation (currently `SummaryDelete`) to the exact
    /// aggregation deployment whose active interval it follows.
    pub fn insert_state_operation(
        &mut self,
        node: &Rc<SummaryNode>,
        state: &Rc<SummaryNode>,
        evidence: StreamingSummaryOperatorEvidence,
    ) {
        self.operations.insert(Rc::as_ptr(node), evidence);
        self.operation_state_owners
            .insert(Rc::as_ptr(node), Rc::as_ptr(state));
    }

    fn aggregation(&self, node: &SummaryNode) -> Option<StreamingAggregateEvidence> {
        self.aggregations.get(&(node as *const _)).cloned()
    }
}

/// Complete physical work for one raw evaluation. It is deliberately
/// per-evaluation so the same normalized query recurrence/horizon can multiply
/// both raw and summary alternatives.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StreamingRawInputEvidence {
    pub input_rows_per_evaluation: u64,
    pub input_bytes_per_evaluation: u64,
    pub source_scan_bytes_per_evaluation: u64,
    /// Encoded bytes added to the raw source by one arriving row.
    pub arriving_row_bytes: u64,
    pub cpu_ops_per_row: f64,
    pub peak_memory_bytes: u64,
}

/// Adapter that supplies the existing lifecycle planner with analytical
/// streaming costs. It does not define lifecycle policy: the planner's
/// existing enums and legality checks remain authoritative.
#[derive(Debug, Clone)]
pub struct StreamingAnalyticalCostModel {
    pub comparison_scope: ComparisonScope,
    pub raw: StreamingRawInputEvidence,
    pub node_evidence: StreamingNodeEvidence,
    pub calibration: ResourceCalibration,
    pub capabilities: SummaryMaintenanceCapabilities,
}

impl StreamingAnalyticalCostModel {
    fn canonical_inputs(&self, summary: &SummaryNode) -> Option<StreamingAggregateEvidence> {
        if self.comparison_scope.data_arrival != DataArrival::ContinuouslyIngesting {
            return None;
        }
        self.comparison_scope.validate().ok()?;
        let evidence = self.node_evidence.aggregation(summary)?;
        evidence.inputs.validate().ok()?;
        Some(evidence)
    }

    fn calibrated(&self, estimate: ResourceEstimate) -> Option<Cost> {
        estimate.calibrated_cost(&self.calibration).ok().map(Cost)
    }

    fn lifecycle_inputs(
        &self,
        summary: &SummaryNode,
    ) -> Option<SummaryMaintenanceLifecycleCostInputs> {
        let evidence = self.canonical_inputs(summary)?;
        let inputs = evidence.inputs;
        let insert = required_cpu("insert_cpu_ops", evidence.cpu.insert_cpu_ops).ok()?;
        let readout = required_cpu("readout_cpu_ops", evidence.cpu.readout_cpu_ops).ok()?;
        let build = self.calibrated(ResourceEstimate {
            cpu_ops: inputs.initial_input_rows as f64 * insert,
            peak_memory_bytes: 0,
            scan_bytes: inputs.initial_source_scan_bytes,
        })?;
        let maintenance = self.calibrated(ResourceEstimate {
            cpu_ops: inputs.active_window_count as f64 * insert,
            peak_memory_bytes: 0,
            scan_bytes: 0,
        })?;
        let read = self.calibrated(ResourceEstimate {
            cpu_ops: inputs.physical_sketch_count as f64 * readout,
            peak_memory_bytes: 0,
            scan_bytes: 0,
        })?;
        let retained = inputs
            .active_window_count
            .checked_add(inputs.retained_window_count)?
            .checked_mul(inputs.physical_sketch_count)?
            .checked_mul(inputs.state_bytes_per_sketch)?;
        let horizon_seconds = self.comparison_scope.horizon.0 as f64 / 1_000.0;
        let retention_total = self.calibrated(ResourceEstimate {
            cpu_ops: 0.0,
            peak_memory_bytes: retained,
            scan_bytes: 0,
        })?;
        Some(SummaryMaintenanceLifecycleCostInputs {
            build_cost: Some(build),
            maintenance_cost_per_update: Some(maintenance),
            summary_read_cost: Some(read),
            retention_cost_rate: Some(CostRate(retention_total.0 / horizon_seconds)),
            // Releasing memory has no modeled CPU or I/O. This is not an
            // implicit expiration/rebuild policy; those require an explicit
            // SummaryDelete or future authoritative lifecycle evidence.
            retirement_cost: Some(Cost::ZERO),
        })
    }
}

impl CostModel for StreamingAnalyticalCostModel {
    fn candidate_cost(
        &self,
        candidate: &ReplacementSubDAG,
        target: &TargetSubDAG<'_>,
    ) -> Option<Cost> {
        match &candidate.replacement {
            // Lifecycle selection supplies a complete override. If it cannot,
            // the candidate remains unavailable rather than receiving this
            // trait's structural fallback.
            Replacement::Summary(_) => None,
            Replacement::Rewrite(_) => DefaultCostModel.candidate_cost(candidate, target),
        }
    }

    fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm> {
        DefaultCostModel.rank_candidates(intent, candidates)
    }

    fn estimate_cost(&self, candidate: &ReplacementSubDAG, target: &TargetSubDAG<'_>) -> f64 {
        DefaultCostModel.estimate_cost(candidate, target)
    }

    fn summary_maintenance_lifecycle_cost_inputs(
        &self,
        summary: &SummaryNode,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        self.lifecycle_inputs(summary).unwrap_or_default()
    }

    fn summary_maintenance_capabilities(
        &self,
        _summary: &SummaryNode,
    ) -> SummaryMaintenanceCapabilities {
        self.capabilities
    }

    fn complete_summary_candidate_cost(
        &self,
        root: &SummaryNode,
        deployments: &[CostedSummaryDeployment<'_>],
        horizon: Option<crate::recurrence::Horizon>,
        expected_reads: Option<f64>,
    ) -> Option<Cost> {
        if horizon.map(|value| value.0 * 1_000.0) != Some(self.comparison_scope.horizon.0 as f64)
            || expected_reads != Some(self.comparison_scope.validate().ok()? as f64)
        {
            return None;
        }
        self.calibrated(
            estimate_heterogeneous_summary(
                root,
                deployments,
                &self.node_evidence,
                &self.comparison_scope,
            )
            .ok()?,
        )
    }

    fn raw_query_recompute_cost(&self, _target: &QueryExpr) -> Option<Cost> {
        self.calibrated(estimate_streaming_raw_recompute(self.raw, 1).ok()?)
    }

    fn raw_query_recompute_total_cost(
        &self,
        _target: &QueryExpr,
        expected_reads: f64,
    ) -> Option<Cost> {
        let evaluations = self.comparison_scope.validate().ok()?;
        if expected_reads != evaluations as f64 {
            return None;
        }
        let aggregate = self.node_evidence.aggregations.values().next().cloned()?;
        let sources: HashSet<_> = self
            .node_evidence
            .aggregations
            .values()
            .map(|evidence| evidence.source_coverage_index)
            .collect();
        if sources.len() != 1
            || sources
                .iter()
                .any(|index| *index >= self.comparison_scope.sources.len())
            || self.node_evidence.aggregations.values().any(|evidence| {
                evidence.inputs.initial_input_rows != self.raw.input_rows_per_evaluation
                    || evidence.inputs.initial_input_bytes != self.raw.input_bytes_per_evaluation
                    || evidence.inputs.initial_source_scan_bytes
                        != self.raw.source_scan_bytes_per_evaluation
            })
        {
            return None;
        }
        self.calibrated(
            estimate_evolving_streaming_raw(
                self.raw,
                aggregate.inputs.ingestion_rate_per_second,
                &self.comparison_scope,
            )
            .ok()?,
        )
    }
}

fn estimate_heterogeneous_summary(
    root: &SummaryNode,
    deployments: &[CostedSummaryDeployment<'_>],
    evidence: &StreamingNodeEvidence,
    scope: &ComparisonScope,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    let evaluations = scope.validate()? as f64;
    let by_node: HashMap<_, _> = deployments
        .iter()
        .map(|deployment| (deployment.summary as *const _, deployment))
        .collect();
    let mut cpu_ops = 0.0;
    let mut persistent_bytes = 0_u64;
    let mut scans = HashMap::<String, (usize, u64)>::new();
    for deployment in deployments {
        let node_evidence = evidence
            .aggregation(deployment.summary)
            .ok_or(AnalyticalCostError::MissingOrStale("summary_agg"))?;
        if node_evidence.source_coverage_index >= scope.sources.len() {
            return Err(AnalyticalCostError::MissingComparisonScope(
                "summary source coverage",
            ));
        }
        let inputs = node_evidence.inputs.validate()?;
        validate_guarantee(deployment.guarantee, scope.data_arrival)?;
        let (bootstrap, updates, _) = lifecycle_row_counts(inputs, deployment.guarantee, scope)?;
        let insert = required_cpu("insert_cpu_ops", node_evidence.cpu.insert_cpu_ops)?;
        let insert_calls = bootstrap
            .checked_mul(inputs.bootstrap_window_count)
            .and_then(|calls| {
                updates
                    .checked_mul(inputs.active_window_count)
                    .and_then(|updates| calls.checked_add(updates))
            })
            .ok_or(AnalyticalCostError::Overflow)?;
        cpu_ops += insert_calls as f64 * insert;
        let retained = inputs
            .active_window_count
            .checked_add(inputs.retained_window_count)
            .and_then(|windows| windows.checked_mul(inputs.physical_sketch_count))
            .and_then(|states| states.checked_mul(inputs.state_bytes_per_sketch))
            .ok_or(AnalyticalCostError::Overflow)?;
        persistent_bytes = persistent_bytes
            .checked_add(retained)
            .ok_or(AnalyticalCostError::Overflow)?;
        if node_evidence.bootstrap_read_identity.is_empty() {
            return Err(AnalyticalCostError::MissingOrStale(
                "bootstrap_read_identity",
            ));
        }
        match scans.entry(node_evidence.bootstrap_read_identity.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((
                    node_evidence.source_coverage_index,
                    inputs.initial_source_scan_bytes,
                ));
            }
            std::collections::hash_map::Entry::Occupied(entry)
                if *entry.get()
                    != (
                        node_evidence.source_coverage_index,
                        inputs.initial_source_scan_bytes,
                    ) =>
            {
                return Err(AnalyticalCostError::ComparisonScopeMismatch(
                    "bootstrap source bytes",
                ));
            }
            _ => {}
        }
    }
    let covered_sources: HashSet<_> = scans.values().map(|(index, _)| *index).collect();
    if covered_sources.len() != scope.sources.len()
        || !(0..scope.sources.len()).all(|index| covered_sources.contains(&index))
    {
        return Err(AnalyticalCostError::ComparisonScopeMismatch("sources"));
    }

    #[expect(clippy::too_many_arguments, reason = "recursive DAG traversal state")]
    fn visit_ops(
        node: &SummaryNode,
        seen: &mut HashSet<*const SummaryNode>,
        by_node: &HashMap<*const SummaryNode, &CostedSummaryDeployment<'_>>,
        evidence: &StreamingNodeEvidence,
        scope: &ComparisonScope,
        evaluations: f64,
        cpu_ops: &mut f64,
        transient_bytes: &mut u64,
    ) -> Result<(), AnalyticalCostError> {
        if !seen.insert(node as *const _) {
            return Ok(());
        }
        match &node.expr {
            SummaryExpr::KeepPreAsap(_) | SummaryExpr::SummaryAgg { .. } => {}
            SummaryExpr::SummaryMerge { children } => {
                let operation = evidence
                    .operations
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_merge"))?;
                let merge = validated_operator_cpu("summary_merge", operation.cpu_ops)?;
                *cpu_ops += evaluations
                    * validated_operator_executions("summary_merge", operation)? as f64
                    * merge;
                *transient_bytes = (*transient_bytes).max(operation.working_memory_bytes);
                for child in children {
                    visit_ops(
                        child,
                        seen,
                        by_node,
                        evidence,
                        scope,
                        evaluations,
                        cpu_ops,
                        transient_bytes,
                    )?;
                }
            }
            SummaryExpr::SummarySubtract { left, right } => {
                let operation = evidence
                    .operations
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_subtract"))?;
                *cpu_ops += evaluations
                    * validated_operator_executions("summary_subtract", operation)? as f64
                    * validated_operator_cpu("summary_subtract", operation.cpu_ops)?;
                *transient_bytes = (*transient_bytes).max(operation.working_memory_bytes);
                visit_ops(
                    left,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluations,
                    cpu_ops,
                    transient_bytes,
                )?;
                visit_ops(
                    right,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluations,
                    cpu_ops,
                    transient_bytes,
                )?;
            }
            SummaryExpr::SummaryDelete { summary_input, .. } => {
                let operation = evidence
                    .operations
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_delete"))?;
                let state_ptr = evidence
                    .operation_state_owners
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_delete_owner"))?;
                let deployment = by_node
                    .get(state_ptr)
                    .copied()
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_delete_owner"))?;
                let state = evidence
                    .aggregation(deployment.summary)
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_delete_owner"))?;
                let (_, _, active_ms) =
                    lifecycle_row_counts(state.inputs, deployment.guarantee, scope)?;
                let rate = operation
                    .events_per_second
                    .filter(|rate| rate.is_finite() && *rate >= 0.0)
                    .ok_or(AnalyticalCostError::MissingOrStale(
                        "delete_events_per_second",
                    ))?;
                let fanout = operation
                    .routing_fanout
                    .filter(|fanout| *fanout > 0)
                    .ok_or(AnalyticalCostError::MissingOrStale("delete_routing_fanout"))?;
                *cpu_ops += (rate * active_ms as f64 / 1_000.0).ceil()
                    * fanout as f64
                    * validated_operator_cpu("summary_delete", operation.cpu_ops)?;
                *transient_bytes = (*transient_bytes).max(operation.working_memory_bytes);
                visit_ops(
                    summary_input,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluations,
                    cpu_ops,
                    transient_bytes,
                )?;
            }
            SummaryExpr::SummaryEstimate { summary_input, .. } => {
                let operation = evidence
                    .operations
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_readout"))?;
                *cpu_ops += evaluations
                    * validated_operator_executions("summary_readout", operation)? as f64
                    * validated_operator_cpu("summary_readout", operation.cpu_ops)?;
                *transient_bytes = (*transient_bytes).max(operation.working_memory_bytes);
                visit_ops(
                    summary_input,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluations,
                    cpu_ops,
                    transient_bytes,
                )?;
            }
            SummaryExpr::SummaryJoin { outer, inner, .. } => {
                let join = evidence
                    .joins
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_join"))?;
                if join.matched_state_pairs_per_evaluation == 0
                    || !join.cpu_ops_per_matched_pair.is_finite()
                    || join.cpu_ops_per_matched_pair < 0.0
                    || join.working_memory_bytes == 0
                {
                    return Err(AnalyticalCostError::MissingOrStale("summary_join"));
                }
                *cpu_ops += evaluations
                    * join.matched_state_pairs_per_evaluation as f64
                    * join.cpu_ops_per_matched_pair;
                *transient_bytes = (*transient_bytes).max(join.working_memory_bytes);
                visit_ops(
                    outer,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluations,
                    cpu_ops,
                    transient_bytes,
                )?;
                visit_ops(
                    inner,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluations,
                    cpu_ops,
                    transient_bytes,
                )?;
            }
        }
        Ok(())
    }

    let mut transient_bytes = 0;
    visit_ops(
        root,
        &mut HashSet::new(),
        &by_node,
        evidence,
        scope,
        evaluations,
        &mut cpu_ops,
        &mut transient_bytes,
    )?;
    if !cpu_ops.is_finite() {
        return Err(AnalyticalCostError::Overflow);
    }
    Ok(ResourceEstimate {
        cpu_ops,
        peak_memory_bytes: persistent_bytes
            .checked_add(transient_bytes)
            .ok_or(AnalyticalCostError::Overflow)?,
        scan_bytes: scans.values().try_fold(0_u64, |sum, (_, bytes)| {
            sum.checked_add(*bytes).ok_or(AnalyticalCostError::Overflow)
        })?,
    })
}

fn estimate_evolving_streaming_raw(
    evidence: StreamingRawInputEvidence,
    ingestion_rate_per_second: f64,
    scope: &ComparisonScope,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    if ingestion_rate_per_second > 0.0 && evidence.arriving_row_bytes == 0 {
        return Err(AnalyticalCostError::MissingOrZero("arriving_row_bytes"));
    }
    let offsets = evaluation_offsets_ms(scope)?;
    let mut cpu_ops = 0.0;
    let mut scan_bytes = 0_u64;
    for offset in offsets {
        let arriving = (ingestion_rate_per_second * offset as f64 / 1_000.0).ceil();
        if !arriving.is_finite() || arriving > u64::MAX as f64 {
            return Err(AnalyticalCostError::Overflow);
        }
        let arriving = arriving as u64;
        let rows = evidence
            .input_rows_per_evaluation
            .checked_add(arriving)
            .ok_or(AnalyticalCostError::Overflow)?;
        let arriving_bytes = arriving
            .checked_mul(evidence.arriving_row_bytes)
            .ok_or(AnalyticalCostError::Overflow)?;
        scan_bytes = scan_bytes
            .checked_add(evidence.source_scan_bytes_per_evaluation)
            .and_then(|bytes| bytes.checked_add(arriving_bytes))
            .ok_or(AnalyticalCostError::Overflow)?;
        cpu_ops += rows as f64 * evidence.cpu_ops_per_row;
    }
    if !cpu_ops.is_finite() {
        return Err(AnalyticalCostError::Overflow);
    }
    Ok(ResourceEstimate {
        cpu_ops,
        peak_memory_bytes: evidence.peak_memory_bytes,
        scan_bytes,
    })
}

fn evaluation_offsets_ms(scope: &ComparisonScope) -> Result<Vec<u64>, AnalyticalCostError> {
    let count = scope.validate()?;
    let offsets = match &scope.recurrence {
        QueryRecurrence::OneTime {
            invocations,
            execute_at,
        } => vec![
            execute_at.map_or(0, |at| at.0.saturating_sub(scope.planning_time.0));
            *invocations as usize
        ],
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(interval)) => {
            (1..=count).map(|n| n * u64::from(interval.0)).collect()
        }
        QueryRecurrence::Repeated(RepeatedDemand::Scheduled(schedule)) => schedule
            .iter()
            .filter(|at| {
                at.0 >= scope.planning_time.0
                    && at.0 <= scope.planning_time.0.saturating_add(scope.horizon.0)
            })
            .map(|at| at.0 - scope.planning_time.0)
            .collect(),
        QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(_)) => (1..=count)
            .map(|n| scope.horizon.0.saturating_mul(n) / count)
            .collect(),
        QueryRecurrence::Unknown => return Err(AnalyticalCostError::InvalidRecurrence),
    };
    Ok(offsets)
}

#[cfg(test)]
fn evidence_nodes(root: &SummaryNode) -> (Vec<&SummaryNode>, Vec<&SummaryNode>) {
    fn visit<'a>(
        node: &'a SummaryNode,
        seen: &mut HashSet<*const SummaryNode>,
        aggregations: &mut Vec<&'a SummaryNode>,
        joins: &mut Vec<&'a SummaryNode>,
    ) {
        if !seen.insert(node as *const _) {
            return;
        }
        match &node.expr {
            SummaryExpr::SummaryAgg { child, .. } => {
                aggregations.push(node);
                visit(child, seen, aggregations, joins);
            }
            SummaryExpr::SummaryMerge { children } => {
                for child in children {
                    visit(child, seen, aggregations, joins);
                }
            }
            SummaryExpr::SummarySubtract { left, right }
            | SummaryExpr::SummaryJoin {
                outer: left,
                inner: right,
                ..
            } => {
                if matches!(&node.expr, SummaryExpr::SummaryJoin { .. }) {
                    joins.push(node);
                }
                visit(left, seen, aggregations, joins);
                visit(right, seen, aggregations, joins);
            }
            SummaryExpr::SummaryDelete { summary_input, .. }
            | SummaryExpr::SummaryEstimate { summary_input, .. } => {
                visit(summary_input, seen, aggregations, joins);
            }
            SummaryExpr::KeepPreAsap(_) => {}
        }
    }
    let mut aggregations = Vec::new();
    let mut joins = Vec::new();
    visit(root, &mut HashSet::new(), &mut aggregations, &mut joins);
    (aggregations, joins)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg(test)]
struct SummaryOperationCounts {
    state_builds: u64,
    merges_per_read: u64,
    subtracts_per_read: u64,
    deletes_per_update: u64,
    readouts_per_read: u64,
    joins_per_read: u64,
}

/// Low-level diagnostic for a fixed-cardinality raw evaluation. Final planner
/// comparison uses the scope-bound evolving estimator instead.
pub(crate) fn estimate_streaming_raw_recompute(
    evidence: StreamingRawInputEvidence,
    evaluation_count: u64,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    for (name, value) in [
        (
            "raw_input_rows_per_evaluation",
            evidence.input_rows_per_evaluation,
        ),
        (
            "raw_input_bytes_per_evaluation",
            evidence.input_bytes_per_evaluation,
        ),
        ("raw_peak_memory_bytes", evidence.peak_memory_bytes),
        ("evaluation_count", evaluation_count),
    ] {
        if value == 0 {
            return Err(AnalyticalCostError::MissingOrZero(name));
        }
    }
    if !evidence.cpu_ops_per_row.is_finite() || evidence.cpu_ops_per_row < 0.0 {
        return Err(AnalyticalCostError::InvalidOperationCost(
            "raw_cpu_ops_per_row",
            evidence.cpu_ops_per_row,
        ));
    }
    let cpu_ops = evidence.input_rows_per_evaluation as f64
        * evidence.cpu_ops_per_row
        * evaluation_count as f64;
    if !cpu_ops.is_finite() {
        return Err(AnalyticalCostError::Overflow);
    }
    Ok(ResourceEstimate {
        cpu_ops,
        peak_memory_bytes: evidence.peak_memory_bytes,
        scan_bytes: evidence
            .source_scan_bytes_per_evaluation
            .checked_mul(evaluation_count)
            .ok_or(AnalyticalCostError::Overflow)?,
    })
}

/// Low-level diagnostic for a homogeneous deployment. Final planner ranking
/// uses the per-node whole-DAG estimator above. Shared `Rc` nodes are visited
/// once; explicit delete frequency comes from deletion evidence.
#[cfg(test)]
fn estimate_incremental_summary_maintenance(
    root: &SummaryNode,
    guarantee: &SummaryMaintenanceLifecycleGuarantee,
    inputs: StreamingSummaryInputs,
    cpu: SummaryOperationCpuEvidence,
    scope: &ComparisonScope,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    estimate_incremental_summary_maintenance_with_join(root, guarantee, inputs, cpu, None, scope)
}

#[cfg(test)]
fn estimate_incremental_summary_maintenance_with_join(
    root: &SummaryNode,
    guarantee: &SummaryMaintenanceLifecycleGuarantee,
    inputs: StreamingSummaryInputs,
    cpu: SummaryOperationCpuEvidence,
    join: Option<SummaryJoinEvidence>,
    scope: &ComparisonScope,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    let inputs = inputs.validate()?;
    let evaluation_count = scope.validate()?;
    validate_guarantee(guarantee, scope.data_arrival)?;
    let (bootstrap_input_rows, arriving_input_rows, active_ms) =
        lifecycle_row_counts(inputs, guarantee, scope)?;
    let counts = count_operations(root)?;
    if counts.state_builds == 0 {
        return Err(AnalyticalCostError::UnsupportedCandidate);
    }

    let insert = required_cpu("insert_cpu_ops", cpu.insert_cpu_ops)?;
    let merge = required_cpu_when(counts.merges_per_read, "merge_cpu_ops", cpu.merge_cpu_ops)?;
    let subtract = required_cpu_when(
        counts.subtracts_per_read,
        "subtract_cpu_ops",
        cpu.subtract_cpu_ops,
    )?;
    let delete = required_cpu_when(
        counts.deletes_per_update,
        "delete_cpu_ops",
        cpu.delete_cpu_ops,
    )?;
    let delete_events = if counts.deletes_per_update == 0 {
        0_u64
    } else {
        let rate = cpu
            .delete_events_per_second
            .filter(|rate| rate.is_finite() && *rate >= 0.0)
            .ok_or(AnalyticalCostError::MissingOrStale(
                "delete_events_per_second",
            ))?;
        let fanout = cpu
            .delete_routing_fanout
            .filter(|fanout| *fanout > 0)
            .ok_or(AnalyticalCostError::MissingOrStale("delete_routing_fanout"))?;
        let events = (rate * active_ms as f64 / 1_000.0).ceil();
        if !events.is_finite() || events > u64::MAX as f64 {
            return Err(AnalyticalCostError::Overflow);
        }
        (events as u64)
            .checked_mul(fanout)
            .ok_or(AnalyticalCostError::Overflow)?
    };
    let readout = required_cpu_when(
        counts.readouts_per_read,
        "readout_cpu_ops",
        cpu.readout_cpu_ops,
    )?;
    let join_cpu = match (counts.joins_per_read, join) {
        (0, _) => 0.0,
        (_, Some(evidence))
            if evidence.matched_state_pairs_per_evaluation > 0
                && evidence.cpu_ops_per_matched_pair.is_finite()
                && evidence.cpu_ops_per_matched_pair >= 0.0
                && evidence.working_memory_bytes > 0 =>
        {
            evidence.matched_state_pairs_per_evaluation as f64 * evidence.cpu_ops_per_matched_pair
        }
        (_, Some(evidence))
            if !evidence.cpu_ops_per_matched_pair.is_finite()
                || evidence.cpu_ops_per_matched_pair < 0.0 =>
        {
            return Err(AnalyticalCostError::InvalidOperationCost(
                "summary_join_cpu_ops_per_matched_pair",
                evidence.cpu_ops_per_matched_pair,
            ));
        }
        _ => return Err(AnalyticalCostError::MissingOrStale("summary_join")),
    };

    let build_inserts = bootstrap_input_rows
        .checked_mul(inputs.bootstrap_window_count)
        .ok_or(AnalyticalCostError::Overflow)?
        .checked_mul(counts.state_builds)
        .ok_or(AnalyticalCostError::Overflow)?;
    let update_inserts = arriving_input_rows
        .checked_mul(inputs.active_window_count)
        .and_then(|n| n.checked_mul(counts.state_builds))
        .ok_or(AnalyticalCostError::Overflow)?;
    let instances = inputs.physical_sketch_count as f64;
    let evaluations = evaluation_count as f64;
    let cpu_ops = (build_inserts as f64 + update_inserts as f64) * insert
        + evaluations * counts.merges_per_read as f64 * instances * merge
        + evaluations * counts.subtracts_per_read as f64 * instances * subtract
        + delete_events as f64 * counts.deletes_per_update as f64 * delete
        + evaluations * counts.readouts_per_read as f64 * instances * readout
        + evaluations * counts.joins_per_read as f64 * join_cpu;
    if !cpu_ops.is_finite() {
        return Err(AnalyticalCostError::Overflow);
    }

    let state_instances = inputs
        .active_window_count
        .checked_add(inputs.retained_window_count)
        .and_then(|n| n.checked_mul(inputs.physical_sketch_count))
        .and_then(|n| n.checked_mul(counts.state_builds))
        .ok_or(AnalyticalCostError::Overflow)?;
    let retained_bytes = state_instances
        .checked_mul(inputs.state_bytes_per_sketch)
        .ok_or(AnalyticalCostError::Overflow)?;
    // Merge/subtract may stream over persistent inputs but still needs one
    // result state per physical instance. Persistent retained windows are
    // already included above and are not loaded a second time.
    let transient_bytes = if counts.merges_per_read > 0 || counts.subtracts_per_read > 0 {
        inputs
            .physical_sketch_count
            .checked_mul(inputs.state_bytes_per_sketch)
            .ok_or(AnalyticalCostError::Overflow)?
    } else {
        0
    };
    let join_bytes = match (counts.joins_per_read, join) {
        (0, _) => 0,
        (_, Some(evidence)) => evidence.working_memory_bytes,
        _ => return Err(AnalyticalCostError::MissingOrStale("summary_join")),
    };
    let bootstrap_row_buffer = if inputs.initial_input_rows == 0 {
        0
    } else {
        inputs
            .initial_input_bytes
            .div_ceil(inputs.initial_input_rows)
    };
    Ok(ResourceEstimate {
        cpu_ops,
        peak_memory_bytes: retained_bytes
            .checked_add(transient_bytes)
            .and_then(|bytes| bytes.checked_add(join_bytes))
            .ok_or(AnalyticalCostError::Overflow)?
            .max(bootstrap_row_buffer),
        scan_bytes: inputs.initial_source_scan_bytes,
    })
}

fn lifecycle_row_counts(
    inputs: StreamingSummaryInputs,
    guarantee: &SummaryMaintenanceLifecycleGuarantee,
    scope: &ComparisonScope,
) -> Result<(u64, u64, u64), AnalyticalCostError> {
    let horizon_end = scope
        .planning_time
        .0
        .checked_add(scope.horizon.0)
        .ok_or(AnalyticalCostError::Overflow)?;
    let (bootstrap_extra_ms, active_ms) = match guarantee.summary_maintenance_lifecycle {
        SummaryMaintenanceLifecycle::Prepared {
            activate_at,
            retire_at,
        } => {
            if activate_at.0 >= retire_at.0 {
                return Err(AnalyticalCostError::IncompatibleLifecycleGuarantee);
            }
            let activation = activate_at.0.max(scope.planning_time.0).min(horizon_end);
            let bootstrap_extra_ms = activation.saturating_sub(scope.planning_time.0);
            let start = activation;
            let end = retire_at.0.min(horizon_end);
            (bootstrap_extra_ms, end.saturating_sub(start))
        }
        SummaryMaintenanceLifecycle::Shared { retention } => {
            if retention.0 < scope.horizon.0 {
                return Err(AnalyticalCostError::IncompatibleLifecycleGuarantee);
            }
            (0, scope.horizon.0)
        }
        SummaryMaintenanceLifecycle::ContinuouslyMaintained => (0, scope.horizon.0),
        SummaryMaintenanceLifecycle::Ephemeral => {
            return Err(AnalyticalCostError::IncompatibleLifecycleGuarantee)
        }
    };
    let bootstrap_extra = inputs.ingestion_rate_per_second * bootstrap_extra_ms as f64 / 1000.0;
    let updates = inputs.ingestion_rate_per_second * active_ms as f64 / 1000.0;
    if !bootstrap_extra.is_finite()
        || !updates.is_finite()
        || bootstrap_extra > u64::MAX as f64
        || updates > u64::MAX as f64
    {
        return Err(AnalyticalCostError::Overflow);
    }
    Ok((
        inputs
            .initial_input_rows
            .checked_add(bootstrap_extra.ceil() as u64)
            .ok_or(AnalyticalCostError::Overflow)?,
        updates.ceil() as u64,
        active_ms,
    ))
}

fn validate_guarantee(
    guarantee: &SummaryMaintenanceLifecycleGuarantee,
    arrival: DataArrival,
) -> Result<(), AnalyticalCostError> {
    if guarantee.output_representation != asap_types::post_asap::OutputRepresentation::SummaryState
        || guarantee.summary_maintenance_mode != SummaryMaintenanceMode::Incremental
        || guarantee.summary_maintenance_mode
            != maintenance_mode(&guarantee.summary_maintenance_lifecycle, arrival)
        || guarantee.evaluation_schedule
            != evaluation_schedule(&guarantee.summary_maintenance_lifecycle, arrival)
    {
        return Err(AnalyticalCostError::IncompatibleLifecycleGuarantee);
    }
    Ok(())
}

fn required_cpu(name: &'static str, value: Option<f64>) -> Result<f64, AnalyticalCostError> {
    let value = value.ok_or(AnalyticalCostError::MissingOrStale(name))?;
    if !value.is_finite() || value < 0.0 {
        return Err(AnalyticalCostError::InvalidOperationCost(name, value));
    }
    Ok(value)
}

fn validated_operator_cpu(name: &'static str, value: f64) -> Result<f64, AnalyticalCostError> {
    if !value.is_finite() || value < 0.0 {
        Err(AnalyticalCostError::InvalidOperationCost(name, value))
    } else {
        Ok(value)
    }
}

fn validated_operator_executions(
    name: &'static str,
    evidence: &StreamingSummaryOperatorEvidence,
) -> Result<u64, AnalyticalCostError> {
    if evidence.executions_per_evaluation == 0 {
        return Err(AnalyticalCostError::MissingOrZero(name));
    }
    Ok(evidence.executions_per_evaluation)
}

#[cfg(test)]
fn required_cpu_when(
    count: u64,
    name: &'static str,
    value: Option<f64>,
) -> Result<f64, AnalyticalCostError> {
    if count == 0 {
        return Ok(0.0);
    }
    required_cpu(name, value)
}

#[cfg(test)]
fn count_operations(root: &SummaryNode) -> Result<SummaryOperationCounts, AnalyticalCostError> {
    fn visit(
        node: &SummaryNode,
        seen: &mut HashSet<*const SummaryNode>,
        counts: &mut SummaryOperationCounts,
    ) -> Result<(), AnalyticalCostError> {
        if !seen.insert(node as *const SummaryNode) {
            return Ok(());
        }
        match &node.expr {
            SummaryExpr::KeepPreAsap(_) => {}
            SummaryExpr::SummaryAgg { child, .. } => {
                counts.state_builds = counts
                    .state_builds
                    .checked_add(1)
                    .ok_or(AnalyticalCostError::Overflow)?;
                visit(child, seen, counts)?;
            }
            SummaryExpr::SummaryMerge { children } => {
                if children.is_empty() {
                    return Err(AnalyticalCostError::InvalidPhysicalDag(
                        "summary merge has no children",
                    ));
                }
                counts.merges_per_read = counts
                    .merges_per_read
                    .checked_add(children.len().saturating_sub(1) as u64)
                    .ok_or(AnalyticalCostError::Overflow)?;
                for child in children {
                    visit(child, seen, counts)?;
                }
            }
            SummaryExpr::SummarySubtract { left, right } => {
                counts.subtracts_per_read = counts
                    .subtracts_per_read
                    .checked_add(1)
                    .ok_or(AnalyticalCostError::Overflow)?;
                visit(left, seen, counts)?;
                visit(right, seen, counts)?;
            }
            SummaryExpr::SummaryDelete { summary_input, .. } => {
                counts.deletes_per_update = counts
                    .deletes_per_update
                    .checked_add(1)
                    .ok_or(AnalyticalCostError::Overflow)?;
                visit(summary_input, seen, counts)?;
            }
            SummaryExpr::SummaryEstimate { summary_input, .. } => {
                counts.readouts_per_read = counts
                    .readouts_per_read
                    .checked_add(1)
                    .ok_or(AnalyticalCostError::Overflow)?;
                visit(summary_input, seen, counts)?;
            }
            SummaryExpr::SummaryJoin { outer, inner, .. } => {
                counts.joins_per_read = counts
                    .joins_per_read
                    .checked_add(1)
                    .ok_or(AnalyticalCostError::Overflow)?;
                visit(outer, seen, counts)?;
                visit(inner, seen, counts)?;
            }
        }
        Ok(())
    }

    let mut counts = SummaryOperationCounts::default();
    visit(root, &mut HashSet::new(), &mut counts)?;
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use asap_types::post_asap::{
        EvaluationSchedule, ExactKind, ExactParams, GroupingStrategy, OutputRepresentation,
        SummaryExpr, SummaryFamilyType, SummaryField, SummaryMaintenanceLifecycle,
        SummaryMaintenanceLifecycleGuarantee, SummaryMaintenanceMode, SummarySchema,
    };
    use asap_types::pre_asap::{
        agg_intent::AggIntent, Column, ColumnRef, DataType, QueryExpr, Reduction, Schema, Source,
    };
    use asap_types::workload::{
        DataWorkload, Evidence, EvidenceSource, Predictability, Query, QueryLanguage,
        QueryRecurrence, QueryRequirements, QueryTimeScope, QueryWorkload, QueryWorkloadEntry,
        Rate, RepeatedDemand, RepeatingEntry, RepetitionInterval, TimeSelection,
    };

    use super::*;
    use crate::recurrence::Horizon;
    use crate::summary_maintenance_lifecycle::{
        global_selection_with_summary_maintenance_lifecycles,
        materialize_with_summary_maintenance_lifecycles, plan_summary_maintenance_lifecycles,
        SummaryMaintenanceLifecycleCapabilities, WorkloadDemand,
    };

    fn estimate_test(
        root: &SummaryNode,
        guarantee: &SummaryMaintenanceLifecycleGuarantee,
        inputs: StreamingSummaryInputs,
        cpu: SummaryOperationCpuEvidence,
    ) -> Result<ResourceEstimate, AnalyticalCostError> {
        estimate_incremental_summary_maintenance(root, guarantee, inputs, cpu, &streaming_scope())
    }

    fn estimate_join_test(
        root: &SummaryNode,
        guarantee: &SummaryMaintenanceLifecycleGuarantee,
        inputs: StreamingSummaryInputs,
        cpu: SummaryOperationCpuEvidence,
        join: Option<SummaryJoinEvidence>,
    ) -> Result<ResourceEstimate, AnalyticalCostError> {
        estimate_incremental_summary_maintenance_with_join(
            root,
            guarantee,
            inputs,
            cpu,
            join,
            &streaming_scope(),
        )
    }

    fn scope_for(
        data: &DataWorkload,
        query: &QueryWorkloadEntry,
        planning_time_ms: u64,
        horizon_ms: u64,
    ) -> ComparisonScope {
        ComparisonScope::from_workload(
            data,
            query,
            asap_types::workload::TimestampMs(planning_time_ms),
            asap_types::workload::DurationMs(horizon_ms),
            vec![crate::analytical_statistics::SourceCoverage {
                source: Source::TimeSeries {
                    metric: "metrics".into(),
                },
                snapshot_id: "stream-start".into(),
                predicates: vec![],
            }],
        )
        .unwrap()
    }

    fn physical() -> StreamingPhysicalInputEvidence {
        StreamingPhysicalInputEvidence {
            initial_input_bytes: 640,
            initial_source_scan_bytes: 640,
            active_window_count: 2,
            bootstrap_window_count: 1,
            retained_window_count: 3,
            physical_sketch_count: 2,
            state_bytes_per_sketch: 100,
        }
    }

    fn query() -> QueryWorkloadEntry {
        QueryWorkloadEntry {
            query: Query("streaming count".into()),
            requirements: QueryRequirements::default(),
            predictability: Predictability::Unknown,
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                RepetitionInterval(1_000),
            )),
            time_selection: TimeSelection {
                scope: QueryTimeScope::Unknown,
                lookback: None,
                as_of: None,
            },
        }
    }

    fn continuous_guarantee() -> SummaryMaintenanceLifecycleGuarantee {
        SummaryMaintenanceLifecycleGuarantee {
            summary_maintenance_lifecycle: SummaryMaintenanceLifecycle::ContinuouslyMaintained,
            summary_maintenance_mode: SummaryMaintenanceMode::Incremental,
            evaluation_schedule: EvaluationSchedule::PerUpdate,
            output_representation: OutputRepresentation::SummaryState,
        }
    }

    #[test]
    fn workload_adapter_derives_updates_and_reads_over_one_horizon() {
        let data = DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            ingestion_rate: Evidence {
                value: Some(Rate(2.0)),
                source: EvidenceSource::Observed,
                observed_at_ms: Some(100),
                valid_for_ms: Some(10_000),
            },
            input_cardinality: Evidence {
                value: Some(10),
                source: EvidenceSource::Observed,
                observed_at_ms: Some(100),
                valid_for_ms: Some(10_000),
            },
            ..DataWorkload::default()
        };

        let scope = scope_for(&data, &query(), 100, 5_000);
        let inputs = StreamingSummaryInputs::from_workload(physical(), &data, &scope).unwrap();
        assert_eq!(inputs.initial_input_rows, 10);
        assert_eq!(
            lifecycle_row_counts(inputs, &continuous_guarantee(), &scope)
                .unwrap()
                .1,
            10
        );
        assert_eq!(scope.validate().unwrap(), 5);
    }

    #[test]
    fn pure_streaming_can_bootstrap_from_an_empty_state() {
        let data = DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            ingestion_rate: Evidence {
                value: Some(Rate(2.0)),
                source: EvidenceSource::Declared,
                observed_at_ms: None,
                valid_for_ms: None,
            },
            input_cardinality: Evidence {
                value: Some(0),
                source: EvidenceSource::Declared,
                observed_at_ms: None,
                valid_for_ms: None,
            },
            ..DataWorkload::default()
        };
        let mut empty = physical();
        empty.initial_input_bytes = 0;
        empty.initial_source_scan_bytes = 0;
        let scope = scope_for(&data, &query(), 0, 5_000);
        let inputs = StreamingSummaryInputs::from_workload(empty, &data, &scope).unwrap();
        let estimate = estimate_test(
            &summary_with_operations(false, false, false),
            &continuous_guarantee(),
            inputs,
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(2.0),
                readout_cpu_ops: Some(0.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .unwrap();
        assert_eq!(estimate.cpu_ops, 40.0); // 10 arrivals * 2 active windows * 2 ops.
        assert_eq!(estimate.scan_bytes, 0);
    }

    #[test]
    fn bootstrap_rows_and_bytes_must_be_present_together() {
        let mut inputs = StreamingSummaryInputs {
            initial_input_rows: 0,
            initial_input_bytes: 8,
            initial_source_scan_bytes: 0,
            ingestion_rate_per_second: 1.0,
            active_window_count: 1,
            bootstrap_window_count: 1,
            retained_window_count: 1,
            physical_sketch_count: 1,
            state_bytes_per_sketch: 8,
        };
        assert_eq!(
            inputs.validate(),
            Err(AnalyticalCostError::InconsistentBootstrapEvidence)
        );
        inputs.initial_input_rows = 1;
        inputs.initial_input_bytes = 0;
        assert_eq!(
            inputs.validate(),
            Err(AnalyticalCostError::InconsistentBootstrapEvidence)
        );
    }

    #[test]
    fn no_completed_windows_is_a_valid_streaming_deployment() {
        let mut inputs = streaming_inputs();
        inputs.retained_window_count = 0;
        assert!(inputs.validate().is_ok());
    }

    #[test]
    fn bootstrap_rows_are_routed_to_declared_window_assignments() {
        let mut inputs = streaming_inputs();
        inputs.ingestion_rate_per_second = 0.0;
        inputs.bootstrap_window_count = 3;
        let estimate = estimate_test(
            &summary_with_operations(false, false, false),
            &continuous_guarantee(),
            inputs,
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(2.0),
                readout_cpu_ops: Some(0.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .unwrap();
        assert_eq!(estimate.cpu_ops, 60.0);
    }

    #[test]
    fn lifecycle_output_must_remain_summary_state() {
        let mut guarantee = continuous_guarantee();
        guarantee.output_representation = OutputRepresentation::FinalizedValue;
        assert_eq!(
            estimate_test(
                &summary_with_operations(false, false, false),
                &guarantee,
                streaming_inputs(),
                streaming_cpu(),
            ),
            Err(AnalyticalCostError::IncompatibleLifecycleGuarantee)
        );
    }

    #[test]
    fn existing_lifecycle_planner_selects_a_fully_costed_streaming_alternative() {
        let inputs = StreamingSummaryInputs {
            initial_input_rows: 10,
            initial_input_bytes: 640,
            initial_source_scan_bytes: 640,
            ingestion_rate_per_second: 2.0,
            active_window_count: 2,
            bootstrap_window_count: 1,
            retained_window_count: 3,
            physical_sketch_count: 2,
            state_bytes_per_sketch: 100,
        };
        let mut model = streaming_model();
        let workload = QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![RepeatingEntry {
                query: Query("streaming count".into()),
                demand: RepeatedDemand::FixedInterval(RepetitionInterval(1_000)),
                requirements: QueryRequirements::default(),
                predictability: Predictability::Predictable { known_at: None },
                time_selection: TimeSelection::default(),
            }]),
            data_workload: Some(DataWorkload {
                arrival: DataArrival::ContinuouslyIngesting,
                ingestion_rate: Evidence {
                    value: Some(Rate(2.0)),
                    source: EvidenceSource::Declared,
                    observed_at_ms: None,
                    valid_for_ms: None,
                },
                ..DataWorkload::default()
            }),
        };
        let root = summary_with_operations(false, false, false);
        bind_aggregations(&mut model, &root, inputs, streaming_cpu());
        let plan = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        let selected = plan.deployments[0]
            .summary_maintenance_lifecycle_guarantee
            .as_ref()
            .unwrap();
        assert_eq!(
            selected.summary_maintenance_mode,
            SummaryMaintenanceMode::Incremental
        );
        assert!(matches!(
            selected.summary_maintenance_lifecycle,
            SummaryMaintenanceLifecycle::Shared { .. }
                | SummaryMaintenanceLifecycle::ContinuouslyMaintained
        ));
        assert!(plan.summary_total_cost.is_some());
        assert_eq!(
            model.raw_query_recompute_cost(&QueryExpr::promql_scalar(1.0)),
            Some(Cost(980.0))
        );
    }

    #[test]
    fn global_selection_compares_streaming_summary_and_raw_over_one_horizon() {
        let target = streaming_sum_query();
        let space = crate::replacement::search_workload(vec![("q", Rc::clone(&target))]);
        let workload = streaming_workload();
        let mut model = streaming_model();
        for group in space.groups() {
            for candidate in &group.candidates {
                if let Replacement::Summary(root) = &candidate.replacement {
                    bind_aggregations(&mut model, root, streaming_inputs(), streaming_cpu());
                }
            }
        }
        let selection = global_selection_with_summary_maintenance_lifecycles(
            &space,
            &workload,
            &[0],
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        let plan = materialize_with_summary_maintenance_lifecycles(
            &selection,
            &space.roots[0].1,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap()
        .unwrap();
        assert!(!plan.selected_raw_recompute);
        assert_eq!(plan.raw_recompute_total_cost, Some(Cost(5_600.0)));

        let mut missing_baseline = model.clone();
        missing_baseline.raw.input_rows_per_evaluation = 9;
        let unavailable = global_selection_with_summary_maintenance_lifecycles(
            &space,
            &workload,
            &[0],
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &missing_baseline,
        )
        .unwrap();
        assert!(unavailable
            .for_target(&space.roots[0].1)
            .unwrap()
            .chosen
            .is_none());

        let mut raw_cheaper = model;
        for evidence in raw_cheaper.node_evidence.aggregations.values_mut() {
            evidence.cpu.insert_cpu_ops = Some(10_000.0);
        }
        let cheap_selection = global_selection_with_summary_maintenance_lifecycles(
            &space,
            &workload,
            &[0],
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &raw_cheaper,
        )
        .unwrap();
        assert!(cheap_selection
            .for_target(&space.roots[0].1)
            .unwrap()
            .chosen
            .is_none());
        let cheap_plan = materialize_with_summary_maintenance_lifecycles(
            &cheap_selection,
            &space.roots[0].1,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &raw_cheaper,
        )
        .unwrap()
        .unwrap();
        assert!(cheap_plan.selected_raw_recompute);
        assert_eq!(cheap_plan.raw_recompute_total_cost, Some(Cost(5_600.0)));
    }

    #[test]
    fn lifecycle_plan_does_not_fall_back_to_partial_agg_cost_for_a_join_root() {
        let workload = streaming_workload();
        let root = summary_join();
        let mut model = streaming_model();
        bind_aggregations(&mut model, &root, streaming_inputs(), streaming_cpu());
        let plan = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(plan.deployments.len(), 2);
        assert_eq!(plan.summary_total_cost, None);

        let mut costed = model;
        let join_node = evidence_nodes(&root).1[0];
        costed.node_evidence.joins.insert(
            join_node as *const _,
            SummaryJoinEvidence {
                matched_state_pairs_per_evaluation: 2,
                cpu_ops_per_matched_pair: 3.0,
                working_memory_bytes: 64,
            },
        );
        let costed_plan = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &costed,
        )
        .unwrap();
        assert!(costed_plan.summary_total_cost.is_some());
    }

    #[test]
    fn whole_dag_cost_requires_and_uses_each_rc_bound_state_evidence() {
        let workload = streaming_workload();
        let root = summary_join();
        let (aggregations, joins) = evidence_nodes(&root);
        let mut model = streaming_model();
        model.node_evidence.aggregations.insert(
            aggregations[0] as *const _,
            StreamingAggregateEvidence {
                source_coverage_index: 0,
                bootstrap_read_identity: "left-bootstrap".into(),
                inputs: streaming_inputs(),
                cpu: streaming_cpu(),
            },
        );
        let incomplete = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(incomplete.summary_total_cost, None);

        let mut second_inputs = streaming_inputs();
        second_inputs.state_bytes_per_sketch = 250;
        let mut second_cpu = streaming_cpu();
        second_cpu.insert_cpu_ops = Some(5.0);
        model.node_evidence.aggregations.insert(
            aggregations[1] as *const _,
            StreamingAggregateEvidence {
                source_coverage_index: 0,
                bootstrap_read_identity: "right-bootstrap".into(),
                inputs: second_inputs,
                cpu: second_cpu,
            },
        );
        model.node_evidence.joins.insert(
            joins[0] as *const _,
            SummaryJoinEvidence {
                matched_state_pairs_per_evaluation: 2,
                cpu_ops_per_matched_pair: 3.0,
                working_memory_bytes: 64,
            },
        );
        model.node_evidence.operations.insert(
            Rc::as_ptr(&root),
            StreamingSummaryOperatorEvidence {
                cpu_ops: 3.0,
                working_memory_bytes: 0,
                executions_per_evaluation: 1,
                events_per_second: None,
                routing_fanout: None,
            },
        );
        let complete = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert!(complete.summary_total_cost.is_some());

        model
            .node_evidence
            .operations
            .get_mut(&Rc::as_ptr(&root))
            .unwrap()
            .working_memory_bytes = 128;
        let larger_workspace = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        // The join's 64-byte workspace and the readout's 128-byte workspace
        // are not live together, so peak rises by 64 rather than their sum.
        assert_eq!(
            larger_workspace.summary_total_cost.unwrap().0 - complete.summary_total_cost.unwrap().0,
            64.0
        );

        // Equal SourceCoverage does not imply that two independent state
        // builds share one physical read. Only a provider-owned read identity
        // permits scan de-duplication.
        let mut shared_read = model;
        for aggregate in shared_read.node_evidence.aggregations.values_mut() {
            aggregate.bootstrap_read_identity = "one-physical-read".into();
        }
        let shared = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &shared_read,
        )
        .unwrap();
        assert_eq!(
            larger_workspace.summary_total_cost.unwrap().0 - shared.summary_total_cost.unwrap().0,
            640.0
        );
    }

    #[test]
    fn mixed_arrival_fails_closed_until_backlog_and_stream_are_separate() {
        let data = DataWorkload {
            arrival: DataArrival::Mixed,
            ..DataWorkload::default()
        };
        let mut scope = streaming_scope();
        scope.data_arrival = DataArrival::Mixed;
        assert_eq!(
            StreamingSummaryInputs::from_workload(physical(), &data, &scope),
            Err(AnalyticalCostError::UnsupportedDataArrival(
                DataArrival::Mixed
            ))
        );
    }

    #[test]
    fn direct_read_costs_build_updates_windows_and_recurrence() {
        let estimate = estimate_test(
            &summary_with_operations(false, false, false),
            &continuous_guarantee(),
            StreamingSummaryInputs {
                initial_input_rows: 10,
                initial_input_bytes: 640,
                initial_source_scan_bytes: 640,
                ingestion_rate_per_second: 2.0,
                active_window_count: 2,
                bootstrap_window_count: 1,
                retained_window_count: 3,
                physical_sketch_count: 2,
                state_bytes_per_sketch: 100,
            },
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(2.0),
                readout_cpu_ops: Some(3.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .unwrap();
        // 10 bootstrap + 10 arrivals into two active windows; two states read 5 times.
        assert_eq!(estimate.cpu_ops, 90.0);
        assert_eq!(estimate.peak_memory_bytes, 1_000);
        assert_eq!(estimate.scan_bytes, 640);
    }

    #[test]
    fn operations_use_update_or_read_multiplicity_and_shared_state_once() {
        let estimate = estimate_test(
            &summary_with_operations(true, true, true),
            &continuous_guarantee(),
            StreamingSummaryInputs {
                initial_input_rows: 1,
                initial_input_bytes: 8,
                initial_source_scan_bytes: 8,
                ingestion_rate_per_second: 4.0,
                active_window_count: 1,
                bootstrap_window_count: 1,
                retained_window_count: 2,
                physical_sketch_count: 2,
                state_bytes_per_sketch: 10,
            },
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(1.0),
                merge_cpu_ops: Some(2.0),
                subtract_cpu_ops: Some(3.0),
                delete_cpu_ops: Some(5.0),
                delete_events_per_second: Some(4.0),
                delete_routing_fanout: Some(2),
                readout_cpu_ops: Some(7.0),
            },
        )
        .unwrap();
        assert_eq!(estimate.cpu_ops, 21.0 + 20.0 + 30.0 + 200.0 + 70.0);
        // Three persistent windows plus one transient result, for two instances.
        assert_eq!(estimate.peak_memory_bytes, 80);
    }

    #[test]
    fn lifecycle_mode_and_schedule_must_match_existing_planner_semantics() {
        let mut guarantee = continuous_guarantee();
        guarantee.evaluation_schedule = EvaluationSchedule::OnRead;
        assert_eq!(
            estimate_test(
                &summary_with_operations(false, false, false),
                &guarantee,
                StreamingSummaryInputs {
                    initial_input_rows: 1,
                    initial_input_bytes: 8,
                    initial_source_scan_bytes: 8,
                    ingestion_rate_per_second: 1.0,
                    active_window_count: 1,
                    bootstrap_window_count: 1,
                    retained_window_count: 1,
                    physical_sketch_count: 1,
                    state_bytes_per_sketch: 8,
                },
                SummaryOperationCpuEvidence {
                    insert_cpu_ops: Some(1.0),
                    readout_cpu_ops: Some(1.0),
                    ..SummaryOperationCpuEvidence::default()
                },
            ),
            Err(AnalyticalCostError::IncompatibleLifecycleGuarantee)
        );
    }

    #[test]
    fn missing_cost_for_an_operation_in_the_dag_fails_closed() {
        assert_eq!(
            estimate_test(
                &summary_with_operations(true, false, false),
                &continuous_guarantee(),
                StreamingSummaryInputs {
                    initial_input_rows: 1,
                    initial_input_bytes: 8,
                    initial_source_scan_bytes: 8,
                    ingestion_rate_per_second: 1.0,
                    active_window_count: 1,
                    bootstrap_window_count: 1,
                    retained_window_count: 1,
                    physical_sketch_count: 1,
                    state_bytes_per_sketch: 8,
                },
                SummaryOperationCpuEvidence {
                    insert_cpu_ops: Some(1.0),
                    readout_cpu_ops: Some(1.0),
                    ..SummaryOperationCpuEvidence::default()
                },
            ),
            Err(AnalyticalCostError::MissingOrStale("merge_cpu_ops"))
        );
    }

    #[test]
    fn direct_build_mode_is_not_mispriced_as_incremental_maintenance() {
        let mut guarantee = continuous_guarantee();
        guarantee.summary_maintenance_lifecycle = SummaryMaintenanceLifecycle::Ephemeral;
        guarantee.summary_maintenance_mode = SummaryMaintenanceMode::DirectBuild;
        guarantee.evaluation_schedule = EvaluationSchedule::OneShot;
        assert_eq!(
            estimate_test(
                &summary_with_operations(false, false, false),
                &guarantee,
                StreamingSummaryInputs {
                    initial_input_rows: 1,
                    initial_input_bytes: 8,
                    initial_source_scan_bytes: 8,
                    ingestion_rate_per_second: 1.0,
                    active_window_count: 1,
                    bootstrap_window_count: 1,
                    retained_window_count: 1,
                    physical_sketch_count: 1,
                    state_bytes_per_sketch: 8,
                },
                SummaryOperationCpuEvidence {
                    insert_cpu_ops: Some(1.0),
                    readout_cpu_ops: Some(1.0),
                    ..SummaryOperationCpuEvidence::default()
                },
            ),
            Err(AnalyticalCostError::IncompatibleLifecycleGuarantee)
        );
    }

    #[test]
    fn prepared_maintenance_charges_only_its_active_interval() {
        let guarantee = SummaryMaintenanceLifecycleGuarantee {
            summary_maintenance_lifecycle: SummaryMaintenanceLifecycle::Prepared {
                activate_at: asap_types::workload::TimestampMs(1_000),
                retire_at: asap_types::workload::TimestampMs(2_000),
            },
            summary_maintenance_mode: SummaryMaintenanceMode::Incremental,
            evaluation_schedule: EvaluationSchedule::PerUpdate,
            output_representation: OutputRepresentation::SummaryState,
        };
        let estimate = estimate_test(
            &summary_with_operations(false, false, false),
            &guarantee,
            StreamingSummaryInputs {
                initial_input_rows: 10,
                initial_input_bytes: 80,
                initial_source_scan_bytes: 80,
                ingestion_rate_per_second: 2.0,
                active_window_count: 1,
                bootstrap_window_count: 1,
                retained_window_count: 1,
                physical_sketch_count: 1,
                state_bytes_per_sketch: 8,
            },
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(1.0),
                readout_cpu_ops: Some(0.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .unwrap();
        // Two pre-activation arrivals join the bootstrap; two more are
        // maintained while Prepared is active.
        assert_eq!(estimate.cpu_ops, 14.0);
    }

    #[test]
    fn shared_retention_must_cover_the_comparison_horizon() {
        let guarantee = SummaryMaintenanceLifecycleGuarantee {
            summary_maintenance_lifecycle: SummaryMaintenanceLifecycle::Shared {
                retention: asap_types::workload::DurationMs(999),
            },
            summary_maintenance_mode: SummaryMaintenanceMode::Incremental,
            evaluation_schedule: EvaluationSchedule::PerUpdate,
            output_representation: OutputRepresentation::SummaryState,
        };
        assert_eq!(
            estimate_test(
                &summary_with_operations(false, false, false),
                &guarantee,
                StreamingSummaryInputs {
                    initial_input_rows: 1,
                    initial_input_bytes: 8,
                    initial_source_scan_bytes: 8,
                    ingestion_rate_per_second: 1.0,
                    active_window_count: 1,
                    bootstrap_window_count: 1,
                    retained_window_count: 1,
                    physical_sketch_count: 1,
                    state_bytes_per_sketch: 8,
                },
                SummaryOperationCpuEvidence {
                    insert_cpu_ops: Some(1.0),
                    readout_cpu_ops: Some(1.0),
                    ..SummaryOperationCpuEvidence::default()
                },
            ),
            Err(AnalyticalCostError::IncompatibleLifecycleGuarantee)
        );
    }

    #[test]
    fn raw_recompute_uses_the_same_evaluation_horizon() {
        let estimate = estimate_streaming_raw_recompute(
            StreamingRawInputEvidence {
                input_rows_per_evaluation: 100,
                input_bytes_per_evaluation: 6_400,
                source_scan_bytes_per_evaluation: 6_400,
                arriving_row_bytes: 64,
                cpu_ops_per_row: 2.0,
                peak_memory_bytes: 800,
            },
            5,
        )
        .unwrap();
        assert_eq!(estimate.cpu_ops, 1_000.0);
        assert_eq!(estimate.scan_bytes, 32_000);
        assert_eq!(estimate.peak_memory_bytes, 800);
    }

    #[test]
    fn summary_join_requires_cardinality_and_working_memory_evidence() {
        let joined = summary_join();
        let inputs = StreamingSummaryInputs {
            initial_input_rows: 1,
            initial_input_bytes: 8,
            initial_source_scan_bytes: 8,
            ingestion_rate_per_second: 1.0,
            active_window_count: 1,
            bootstrap_window_count: 1,
            retained_window_count: 1,
            physical_sketch_count: 1,
            state_bytes_per_sketch: 8,
        };
        let cpu = SummaryOperationCpuEvidence {
            insert_cpu_ops: Some(1.0),
            readout_cpu_ops: Some(1.0),
            ..SummaryOperationCpuEvidence::default()
        };
        assert_eq!(
            estimate_join_test(&joined, &continuous_guarantee(), inputs, cpu, None,),
            Err(AnalyticalCostError::MissingOrStale("summary_join"))
        );
        let estimate = estimate_join_test(
            &joined,
            &continuous_guarantee(),
            inputs,
            cpu,
            Some(SummaryJoinEvidence {
                matched_state_pairs_per_evaluation: 3,
                cpu_ops_per_matched_pair: 4.0,
                working_memory_bytes: 32,
            }),
        )
        .unwrap();
        assert_eq!(estimate.cpu_ops, 77.0);
        assert_eq!(estimate.peak_memory_bytes, 64); // 4 persistent states + join memory.
    }

    fn summary_with_operations(merge: bool, subtract: bool, delete: bool) -> Rc<SummaryNode> {
        let state_type = SummaryFamilyType::ExactAggregate(ExactKind::Count, ExactParams::Count);
        let schema = SummarySchema {
            fields: vec![SummaryField {
                name: "count".into(),
                dtype: state_type.clone(),
                nullable: false,
            }],
            time_index: None,
        };
        let leaf = Rc::new(SummaryNode {
            expr: SummaryExpr::KeepPreAsap(Rc::new(QueryExpr::promql_scalar(1.0))),
            schema: schema.clone(),
            guarantee: None,
        });
        let agg = Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryAgg {
                child: leaf,
                family: state_type,
                col: ColumnRef::Wildcard,
                reduction: Reduction::by(vec![]),
                grouping: GroupingStrategy::PerSubpopulationInstance,
            },
            schema: schema.clone(),
            guarantee: None,
        });
        let mut root = Rc::clone(&agg);
        if merge {
            root = Rc::new(SummaryNode {
                expr: SummaryExpr::SummaryMerge {
                    children: vec![Rc::clone(&agg), Rc::clone(&agg)],
                },
                schema: schema.clone(),
                guarantee: None,
            });
        }
        if subtract {
            root = Rc::new(SummaryNode {
                expr: SummaryExpr::SummarySubtract {
                    left: Rc::clone(&root),
                    right: Rc::clone(&agg),
                },
                schema: schema.clone(),
                guarantee: None,
            });
        }
        if delete {
            root = Rc::new(SummaryNode {
                expr: SummaryExpr::SummaryDelete {
                    summary_input: root,
                    key: ColumnRef::Wildcard,
                },
                schema: schema.clone(),
                guarantee: None,
            });
        }
        Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryEstimate {
                summary_input: root,
                query: asap_types::post_asap::SketchQuery::PointCount {
                    key: ColumnRef::Wildcard,
                    value: None,
                },
            },
            schema,
            guarantee: None,
        })
    }

    fn summary_join() -> Rc<SummaryNode> {
        let left = summary_with_operations(false, false, false);
        let right = summary_with_operations(false, false, false);
        let SummaryExpr::SummaryEstimate {
            summary_input: left,
            ..
        } = &left.expr
        else {
            unreachable!()
        };
        let SummaryExpr::SummaryEstimate {
            summary_input: right,
            ..
        } = &right.expr
        else {
            unreachable!()
        };
        let schema = left.schema.clone();
        let join = Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryJoin {
                outer: Rc::clone(left),
                inner: Rc::clone(right),
                key: ColumnRef::Wildcard,
                family: SummaryFamilyType::ExactAggregate(ExactKind::Count, ExactParams::Count),
            },
            schema: schema.clone(),
            guarantee: None,
        });
        Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryEstimate {
                summary_input: join,
                query: asap_types::post_asap::SketchQuery::PointCount {
                    key: ColumnRef::Wildcard,
                    value: None,
                },
            },
            schema,
            guarantee: None,
        })
    }

    fn streaming_sum_query() -> Rc<QueryExpr> {
        let scan = Rc::new(QueryExpr::Scan {
            source: Source::TimeSeries {
                metric: "metrics".into(),
            },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        });
        Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::by(vec![]),
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![],
            having: None,
            child: scan,
        })
    }

    fn streaming_workload() -> QueryWorkload {
        QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![RepeatingEntry {
                query: Query("sum(metrics)".into()),
                demand: RepeatedDemand::FixedInterval(RepetitionInterval(1_000)),
                requirements: QueryRequirements::default(),
                predictability: Predictability::Predictable { known_at: None },
                time_selection: TimeSelection::default(),
            }]),
            data_workload: Some(DataWorkload {
                arrival: DataArrival::ContinuouslyIngesting,
                ingestion_rate: Evidence {
                    value: Some(Rate(2.0)),
                    source: EvidenceSource::Declared,
                    observed_at_ms: None,
                    valid_for_ms: None,
                },
                ..DataWorkload::default()
            }),
        }
    }

    fn streaming_model() -> StreamingAnalyticalCostModel {
        StreamingAnalyticalCostModel {
            comparison_scope: streaming_scope(),
            raw: StreamingRawInputEvidence {
                input_rows_per_evaluation: 10,
                input_bytes_per_evaluation: 640,
                source_scan_bytes_per_evaluation: 640,
                arriving_row_bytes: 64,
                cpu_ops_per_row: 2.0,
                peak_memory_bytes: 320,
            },
            node_evidence: StreamingNodeEvidence::default(),
            calibration: ResourceCalibration {
                cost_per_cpu_op: 1.0,
                cost_per_scan_byte: 1.0,
                cost_per_retained_byte: 1.0,
                version: "test".into(),
            },
            capabilities: SummaryMaintenanceCapabilities {
                incremental_update: true,
                merge: false,
                delete: false,
            },
        }
    }

    fn streaming_inputs() -> StreamingSummaryInputs {
        StreamingSummaryInputs {
            initial_input_rows: 10,
            initial_input_bytes: 640,
            initial_source_scan_bytes: 640,
            ingestion_rate_per_second: 2.0,
            active_window_count: 2,
            bootstrap_window_count: 1,
            retained_window_count: 3,
            physical_sketch_count: 2,
            state_bytes_per_sketch: 100,
        }
    }

    fn streaming_cpu() -> SummaryOperationCpuEvidence {
        SummaryOperationCpuEvidence {
            insert_cpu_ops: Some(2.0),
            readout_cpu_ops: Some(3.0),
            ..SummaryOperationCpuEvidence::default()
        }
    }

    fn bind_aggregations(
        model: &mut StreamingAnalyticalCostModel,
        root: &Rc<SummaryNode>,
        inputs: StreamingSummaryInputs,
        cpu: SummaryOperationCpuEvidence,
    ) {
        for node in evidence_nodes(root).0 {
            model.node_evidence.aggregations.insert(
                node as *const _,
                StreamingAggregateEvidence {
                    source_coverage_index: 0,
                    bootstrap_read_identity: "shared-bootstrap".into(),
                    inputs,
                    cpu,
                },
            );
        }
        fn bind_ops(
            model: &mut StreamingAnalyticalCostModel,
            node: &SummaryNode,
            seen: &mut HashSet<*const SummaryNode>,
            inputs: StreamingSummaryInputs,
            cpu: SummaryOperationCpuEvidence,
        ) {
            if !seen.insert(node as *const _) {
                return;
            }
            let operation = match &node.expr {
                SummaryExpr::SummaryMerge { .. } => {
                    cpu.merge_cpu_ops
                        .map(|cpu_ops| StreamingSummaryOperatorEvidence {
                            cpu_ops,
                            working_memory_bytes: inputs.state_bytes_per_sketch,
                            executions_per_evaluation: 1,
                            events_per_second: None,
                            routing_fanout: None,
                        })
                }
                SummaryExpr::SummarySubtract { .. } => {
                    cpu.subtract_cpu_ops
                        .map(|cpu_ops| StreamingSummaryOperatorEvidence {
                            cpu_ops,
                            working_memory_bytes: inputs.state_bytes_per_sketch,
                            executions_per_evaluation: 1,
                            events_per_second: None,
                            routing_fanout: None,
                        })
                }
                SummaryExpr::SummaryDelete { .. } => {
                    cpu.delete_cpu_ops
                        .map(|cpu_ops| StreamingSummaryOperatorEvidence {
                            cpu_ops,
                            working_memory_bytes: 0,
                            executions_per_evaluation: 1,
                            events_per_second: cpu.delete_events_per_second,
                            routing_fanout: cpu.delete_routing_fanout,
                        })
                }
                SummaryExpr::SummaryEstimate { .. } => {
                    cpu.readout_cpu_ops
                        .map(|cpu_ops| StreamingSummaryOperatorEvidence {
                            cpu_ops,
                            working_memory_bytes: 0,
                            executions_per_evaluation: 1,
                            events_per_second: None,
                            routing_fanout: None,
                        })
                }
                _ => None,
            };
            if let Some(operation) = operation {
                model
                    .node_evidence
                    .operations
                    .insert(node as *const _, operation);
                if let SummaryExpr::SummaryDelete { summary_input, .. } = &node.expr {
                    fn owning_agg(node: &SummaryNode) -> Option<*const SummaryNode> {
                        match &node.expr {
                            SummaryExpr::SummaryAgg { .. } => Some(node as *const _),
                            SummaryExpr::SummaryMerge { children } => {
                                children.first().and_then(|child| owning_agg(child))
                            }
                            SummaryExpr::SummarySubtract { left, .. }
                            | SummaryExpr::SummaryJoin { outer: left, .. } => owning_agg(left),
                            SummaryExpr::SummaryDelete { summary_input, .. }
                            | SummaryExpr::SummaryEstimate { summary_input, .. } => {
                                owning_agg(summary_input)
                            }
                            SummaryExpr::KeepPreAsap(_) => None,
                        }
                    }
                    if let Some(owner) = owning_agg(summary_input) {
                        model
                            .node_evidence
                            .operation_state_owners
                            .insert(node as *const _, owner);
                    }
                }
            }
            match &node.expr {
                SummaryExpr::SummaryAgg { child, .. } => bind_ops(model, child, seen, inputs, cpu),
                SummaryExpr::SummaryMerge { children } => {
                    for child in children {
                        bind_ops(model, child, seen, inputs, cpu);
                    }
                }
                SummaryExpr::SummarySubtract { left, right }
                | SummaryExpr::SummaryJoin {
                    outer: left,
                    inner: right,
                    ..
                } => {
                    bind_ops(model, left, seen, inputs, cpu);
                    bind_ops(model, right, seen, inputs, cpu);
                }
                SummaryExpr::SummaryDelete { summary_input, .. }
                | SummaryExpr::SummaryEstimate { summary_input, .. } => {
                    bind_ops(model, summary_input, seen, inputs, cpu)
                }
                SummaryExpr::KeepPreAsap(_) => {}
            }
        }
        bind_ops(model, root, &mut HashSet::new(), inputs, cpu);
    }

    fn streaming_scope() -> ComparisonScope {
        let workload = streaming_workload();
        let entry = workload.entries().next().unwrap();
        ComparisonScope::from_workload(
            workload.data_workload.as_ref().unwrap(),
            &entry,
            asap_types::workload::TimestampMs(0),
            asap_types::workload::DurationMs(5_000),
            vec![crate::analytical_statistics::SourceCoverage {
                source: Source::TimeSeries {
                    metric: "metrics".into(),
                },
                snapshot_id: "stream-start".into(),
                predicates: vec![],
            }],
        )
        .unwrap()
    }
}
