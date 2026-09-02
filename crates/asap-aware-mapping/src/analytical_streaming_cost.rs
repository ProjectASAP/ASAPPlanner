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
use asap_types::pre_asap::{agg_intent::AggIntent, Predicate, QueryExpr, Source};
use asap_types::workload::{DataArrival, DataWorkload, QueryRecurrence, RepeatedDemand};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use crate::analytical_cost::ExecutionMultiplicity;
use crate::analytical_cost::{
    estimate_physical_dag, AnalyticalCostError, PhysicalDagNode, PhysicalOperator,
    ResourceCalibration, ResourceEstimate,
};
use crate::analytical_statistics::{
    evaluations_in_horizon, ComparisonScope, EdgeStatistics, OperatorStatistics,
};
use crate::cost_model::CostedSummaryDeployment;
use crate::cost_model::{Cost, CostModel, DefaultCostModel};
use crate::recurrence::CostRate;
use crate::replacement::{Replacement, ReplacementSubDAG, TargetSubDAG};
use crate::summary_maintenance_lifecycle::{
    evaluation_schedule, maintenance_mode, SummaryMaintenanceCapabilities,
    SummaryMaintenanceLifecycleCostInputs,
};

pub const ANALYTICAL_STREAMING_MODEL_VERSION: &str = "analytical-summary-incremental-v1";

/// Owned, immutable evidence for one fully lowered physical DAG.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamingPhysicalDagEvidence {
    pub nodes: Vec<PhysicalDagNode>,
    pub root: String,
    pub statistics: HashMap<String, OperatorStatistics>,
}

impl StreamingPhysicalDagEvidence {
    fn estimate(&self, scope: &ComparisonScope) -> Result<ResourceEstimate, AnalyticalCostError> {
        estimate_physical_dag(&self.nodes, &self.root, scope, &self.statistics)
    }
}

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryJoinEvidence {
    pub physical_id: String,
    pub inputs: Vec<EdgeStatistics>,
    pub output: EdgeStatistics,
    pub matched_state_pairs_per_evaluation: u64,
    pub cpu_ops_per_matched_pair: f64,
    pub working_memory_bytes: u64,
    pub output_buffer_bytes: u64,
    pub executions_per_evaluation: u64,
    pub io_bytes_per_execution: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamingAggregateEvidence {
    pub physical_id: String,
    pub input: EdgeStatistics,
    pub output: EdgeStatistics,
    /// Index into `ComparisonScope.sources` for this state's bootstrap input.
    pub source_coverage_index: usize,
    /// Provider-owned identity of the physical bootstrap read. Equal source
    /// coverage alone does not prove two independent builds share I/O.
    pub bootstrap_read_identity: String,
    pub inputs: StreamingSummaryInputs,
    pub cpu: SummaryOperationCpuEvidence,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamingSummaryOperatorEvidence {
    pub physical_id: String,
    pub inputs: Vec<EdgeStatistics>,
    pub output: EdgeStatistics,
    pub cpu_ops: f64,
    pub working_memory_bytes: u64,
    pub output_buffer_bytes: u64,
    /// Executions of this physical operator for one query evaluation.
    /// This is provider evidence, not inferred from a descendant state.
    pub executions_per_evaluation: u64,
    pub events_per_second: Option<f64>,
    pub routing_fanout: Option<u64>,
    pub io_bytes_per_execution: Option<u64>,
}

/// Non-aggregation work for a retained pre-ASAP subtree over the comparison
/// horizon. Bootstrap/source I/O belongs exclusively to the owning aggregate,
/// and summary insertion belongs exclusively to its insert evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamingRetainedQueryEvidence {
    pub physical_id: String,
    pub preprocessing_cpu_ops_over_horizon: f64,
    /// Execution workspace, excluding the separately declared output buffer.
    pub working_memory_bytes: u64,
    pub output_buffer_bytes: u64,
    pub physical_dag: StreamingPhysicalDagEvidence,
}

/// Physical evidence bound to the selected DAG's `Rc` identity. A copied,
/// structurally equal node is not silently treated as the same deployment.
#[derive(Debug, Clone, Default)]
pub struct StreamingNodeEvidence {
    aggregations: HashMap<*const SummaryNode, StreamingAggregateEvidence>,
    joins: HashMap<*const SummaryNode, SummaryJoinEvidence>,
    operations: HashMap<*const SummaryNode, StreamingSummaryOperatorEvidence>,
    operation_state_owners: HashMap<*const SummaryNode, *const SummaryNode>,
    retained_queries: HashMap<*const SummaryNode, StreamingRetainedQueryEvidence>,
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

    pub fn insert_retained_query(
        &mut self,
        node: &Rc<SummaryNode>,
        evidence: StreamingRetainedQueryEvidence,
    ) {
        self.retained_queries.insert(Rc::as_ptr(node), evidence);
    }

    fn aggregation(&self, node: &SummaryNode) -> Option<StreamingAggregateEvidence> {
        self.aggregations.get(&(node as *const _)).cloned()
    }
}

/// Complete physical work for one raw evaluation. It is deliberately
/// per-evaluation so the same normalized query recurrence/horizon can multiply
/// both raw and summary alternatives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamingRawInputEvidence {
    pub input_rows_per_evaluation: u64,
    pub input_bytes_per_evaluation: u64,
    pub source_scan_bytes_per_evaluation: u64,
    /// Encoded bytes added to the raw source by one arriving row.
    pub arriving_row_bytes: u64,
    pub ingestion_rate_per_second: f64,
    pub physical_dag: StreamingPhysicalDagEvidence,
}

/// Adapter that supplies the existing lifecycle planner with analytical
/// streaming costs. It does not define lifecycle policy: the planner's
/// existing enums and legality checks remain authoritative.
#[derive(Debug, Clone)]
pub struct StreamingAnalyticalCostModel {
    pub node_evidence: StreamingNodeEvidence,
    pub calibration: ResourceCalibration,
    pub capabilities: SummaryMaintenanceCapabilities,
    target_comparisons: HashMap<*const QueryExpr, StreamingTargetComparison>,
    candidate_comparisons: HashSet<(*const QueryExpr, *const SummaryNode)>,
}

#[derive(Debug, Clone)]
struct StreamingTargetComparison {
    scope: ComparisonScope,
    raw: StreamingRawInputEvidence,
}

fn query_scan_selections(query: &QueryExpr, out: &mut Vec<(Source, Vec<Predicate>)>) {
    use QueryExpr::*;
    match query {
        Scan {
            source, predicates, ..
        } => out.push((source.clone(), predicates.clone())),
        PromqlVectorFromScalar(child) | PromqlScalarFromVector(child) => {
            query_scan_selections(child, out)
        }
        PromqlRelabel { child, .. }
        | Filter { child, .. }
        | Project { child, .. }
        | Aggregate { child, .. }
        | Dedup { child, .. }
        | PromqlSubquery { child, .. }
        | TimeRange { child, .. }
        | TimeShift { child, .. }
        | SQLWindowFunc { child, .. }
        | PromqlSeriesSample { child, .. }
        | PromqlInfoEnrich { child, .. }
        | Sort { child, .. }
        | Limit { child, .. } => query_scan_selections(child, out),
        Concat { children } => children
            .iter()
            .for_each(|child| query_scan_selections(child, out)),
        Join { left, right, .. } | SetOp { left, right, .. } => {
            query_scan_selections(left, out);
            query_scan_selections(right, out);
        }
        BinaryOp { lhs, rhs, .. } => {
            query_scan_selections(lhs, out);
            query_scan_selections(rhs, out);
        }
        PromqlScalarBridge(_)
        | EvalTimestamp
        | CurrentTimestamp
        | Column(_)
        | Literal(_)
        | Compare { .. }
        | BoolAnd(_)
        | BoolOr(_)
        | Not(_)
        | IsNull(_)
        | IsNotNull(_)
        | Cast { .. }
        | InList { .. }
        | FunctionCall { .. }
        | Arithmetic { .. }
        | Case { .. } => {}
    }
}

fn validate_query_scope(
    target: &QueryExpr,
    scope: &ComparisonScope,
) -> Result<(), AnalyticalCostError> {
    let mut actual = Vec::new();
    query_scan_selections(target, &mut actual);
    let mut declared: Vec<_> = scope
        .sources
        .iter()
        .map(|coverage| (coverage.source.clone(), coverage.predicates.clone()))
        .collect();
    for selection in actual {
        let Some(index) = declared.iter().position(|value| value == &selection) else {
            return Err(AnalyticalCostError::ComparisonScopeMismatch(
                "raw target source lineage",
            ));
        };
        declared.swap_remove(index);
    }
    Ok(())
}

fn validate_physical_scope_coverage(
    physical: &StreamingPhysicalDagEvidence,
    scope: &ComparisonScope,
) -> Result<(), AnalyticalCostError> {
    let mut declared = scope.sources.clone();
    for node in physical
        .nodes
        .iter()
        .filter(|node| node.operator == PhysicalOperator::Scan)
    {
        let coverage = node
            .source_coverage
            .as_ref()
            .ok_or_else(|| AnalyticalCostError::MissingScanSourceCoverage(node.id.clone()))?;
        let Some(index) = declared.iter().position(|value| value == coverage) else {
            return Err(AnalyticalCostError::ComparisonScopeMismatch(
                "physical source coverage",
            ));
        };
        declared.swap_remove(index);
    }
    if !declared.is_empty() {
        return Err(AnalyticalCostError::ComparisonScopeMismatch(
            "physical source coverage",
        ));
    }
    Ok(())
}

fn validate_raw_snapshot_dimensions(
    raw: &StreamingRawInputEvidence,
    scope: &ComparisonScope,
) -> Result<(), AnalyticalCostError> {
    if scope.sources.len() != 1 || !raw.ingestion_rate_per_second.is_finite() {
        return Err(AnalyticalCostError::MissingComparisonScope(
            "single-source raw evolution",
        ));
    }
    let mut rows = 0_u64;
    let mut bytes = 0_u64;
    let mut scan = 0_u64;
    for offset in evaluation_offsets_ms(scope)? {
        let arrivals = (raw.ingestion_rate_per_second * offset as f64 / 1_000.0).ceil();
        if !arrivals.is_finite() || arrivals < 0.0 || arrivals > u64::MAX as f64 {
            return Err(AnalyticalCostError::Overflow);
        }
        let arrivals = arrivals as u64;
        rows = rows
            .checked_add(raw.input_rows_per_evaluation)
            .and_then(|value| value.checked_add(arrivals))
            .ok_or(AnalyticalCostError::Overflow)?;
        bytes = bytes
            .checked_add(raw.input_bytes_per_evaluation)
            .and_then(|value| {
                arrivals
                    .checked_mul(raw.arriving_row_bytes)
                    .and_then(|arriving| value.checked_add(arriving))
            })
            .ok_or(AnalyticalCostError::Overflow)?;
        scan = scan
            .checked_add(raw.source_scan_bytes_per_evaluation)
            .and_then(|value| {
                arrivals
                    .checked_mul(raw.arriving_row_bytes)
                    .and_then(|arriving| value.checked_add(arriving))
            })
            .ok_or(AnalyticalCostError::Overflow)?;
    }
    let scan_node = raw
        .physical_dag
        .nodes
        .iter()
        .find(|node| node.operator == PhysicalOperator::Scan)
        .ok_or(AnalyticalCostError::MissingComparisonScope("raw scan"))?;
    let statistics = raw
        .physical_dag
        .statistics
        .get(&scan_node.id)
        .ok_or_else(|| AnalyticalCostError::MissingOperatorStatistics(scan_node.id.clone()))?;
    if statistics.inputs.as_slice() != [EdgeStatistics { rows, bytes }]
        || statistics.source_scan_bytes != scan
    {
        return Err(AnalyticalCostError::ComparisonScopeMismatch(
            "raw source evolution",
        ));
    }
    Ok(())
}

fn evaluation_offsets_ms(scope: &ComparisonScope) -> Result<Vec<u64>, AnalyticalCostError> {
    let count = scope.validate()?;
    match &scope.recurrence {
        QueryRecurrence::OneTime {
            invocations,
            execute_at,
        } => Ok(vec![
            execute_at.map_or(0, |at| at
                .0
                .saturating_sub(scope.planning_time.0));
            *invocations as usize
        ]),
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(interval)) => {
            Ok((1..=count).map(|n| n * u64::from(interval.0)).collect())
        }
        QueryRecurrence::Repeated(RepeatedDemand::Scheduled(schedule)) => Ok(schedule
            .iter()
            .filter(|at| {
                at.0 >= scope.planning_time.0
                    && at.0 <= scope.planning_time.0.saturating_add(scope.horizon.0)
            })
            .map(|at| at.0 - scope.planning_time.0)
            .collect()),
        QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(_)) => Ok((1..=count)
            .map(|n| scope.horizon.0.saturating_mul(n) / count)
            .collect()),
        QueryRecurrence::Unknown => Err(AnalyticalCostError::InvalidRecurrence),
    }
}

impl StreamingAnalyticalCostModel {
    pub fn new(
        calibration: ResourceCalibration,
        capabilities: SummaryMaintenanceCapabilities,
    ) -> Self {
        Self {
            node_evidence: StreamingNodeEvidence::default(),
            calibration,
            capabilities,
            target_comparisons: HashMap::new(),
            candidate_comparisons: HashSet::new(),
        }
    }

    /// Bind one candidate and its raw baseline to the same target-specific
    /// comparison context. Rebinding a target to different evidence is
    /// rejected rather than silently replacing the canonical context.
    pub fn bind_candidate_comparison(
        &mut self,
        target: &Rc<QueryExpr>,
        root: &Rc<SummaryNode>,
        scope: ComparisonScope,
        raw: StreamingRawInputEvidence,
    ) -> Result<(), AnalyticalCostError> {
        scope.validate()?;
        validate_query_scope(target, &scope)?;
        validate_physical_scope_coverage(&raw.physical_dag, &scope)?;
        validate_raw_snapshot_dimensions(&raw, &scope)?;
        raw.physical_dag.estimate(&scope)?;
        let target_ptr = Rc::as_ptr(target);
        if let Some(existing) = self.target_comparisons.get(&target_ptr) {
            if existing.scope != scope || existing.raw != raw {
                return Err(AnalyticalCostError::ComparisonScopeMismatch(
                    "target comparison",
                ));
            }
        }
        // Commit only after every validation above succeeds. Shared nodes do
        // not carry one owning target; context identity is `(target, root)`.
        self.target_comparisons
            .entry(target_ptr)
            .or_insert(StreamingTargetComparison { scope, raw });
        self.candidate_comparisons
            .insert((target_ptr, Rc::as_ptr(root)));
        Ok(())
    }

    fn canonical_inputs(&self, summary: &SummaryNode) -> Option<StreamingAggregateEvidence> {
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
        horizon: Option<crate::recurrence::Horizon>,
    ) -> Option<SummaryMaintenanceLifecycleCostInputs> {
        let evidence = self.canonical_inputs(summary)?;
        let inputs = evidence.inputs;
        let insert = required_cpu("insert_cpu_ops", evidence.cpu.insert_cpu_ops).ok()?;
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
        let retained = inputs
            .active_window_count
            .checked_add(inputs.retained_window_count)?
            .checked_mul(inputs.physical_sketch_count)?
            .checked_mul(inputs.state_bytes_per_sketch)?;
        let retention_total = self.calibrated(ResourceEstimate {
            cpu_ops: 0.0,
            peak_memory_bytes: retained,
            scan_bytes: 0,
        })?;
        let horizon_seconds = horizon.filter(|value| value.0 > 0.0)?.0;
        Some(SummaryMaintenanceLifecycleCostInputs {
            build_cost: Some(build),
            maintenance_cost_per_update: Some(maintenance),
            // Readout is a separate physical operator in the complete DAG.
            // A state-only candidate therefore does not fabricate readout
            // evidence merely to keep a lifecycle alternative selectable.
            summary_read_cost: Some(Cost::ZERO),
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
        _target: &TargetSubDAG<'_>,
    ) -> Option<Cost> {
        match &candidate.replacement {
            // Lifecycle selection supplies a complete override. If it cannot,
            // the candidate remains unavailable rather than receiving this
            // trait's structural fallback.
            Replacement::Summary(_) => None,
            Replacement::Rewrite(_) => None,
        }
    }

    fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm> {
        DefaultCostModel.rank_candidates(intent, candidates)
    }

    fn estimate_cost(&self, _candidate: &ReplacementSubDAG, _target: &TargetSubDAG<'_>) -> f64 {
        f64::INFINITY
    }

    fn summary_maintenance_lifecycle_cost_inputs(
        &self,
        _summary: &SummaryNode,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        SummaryMaintenanceLifecycleCostInputs::default()
    }

    fn summary_maintenance_lifecycle_cost_inputs_for_horizon(
        &self,
        summary: &SummaryNode,
        horizon: Option<crate::recurrence::Horizon>,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        self.lifecycle_inputs(summary, horizon).unwrap_or_default()
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
        target: Option<&QueryExpr>,
        deployments: &[CostedSummaryDeployment<'_>],
        horizon: Option<crate::recurrence::Horizon>,
        expected_reads: Option<f64>,
    ) -> Option<Cost> {
        let root_ptr = root as *const _;
        let target_ptr = match target {
            Some(target) => target as *const _,
            None => {
                let mut targets = self
                    .candidate_comparisons
                    .iter()
                    .filter_map(|(target, candidate)| (*candidate == root_ptr).then_some(*target));
                let only = targets.next()?;
                if targets.next().is_some() {
                    return None;
                }
                only
            }
        };
        if !self.candidate_comparisons.contains(&(target_ptr, root_ptr)) {
            return None;
        }
        let comparison = self.target_comparisons.get(&target_ptr)?;
        if horizon.map(|value| value.0 * 1_000.0) != Some(comparison.scope.horizon.0 as f64)
            || expected_reads != Some(comparison.scope.validate().ok()? as f64)
        {
            return None;
        }
        self.calibrated(
            estimate_heterogeneous_summary(
                root,
                deployments,
                &self.node_evidence,
                &comparison.scope,
                &comparison.raw,
            )
            .ok()?,
        )
    }

    fn raw_query_recompute_cost(&self, target: &QueryExpr) -> Option<Cost> {
        let _ = target;
        None
    }

    fn raw_query_recompute_total_cost(
        &self,
        target: &QueryExpr,
        expected_reads: f64,
    ) -> Option<Cost> {
        let target_ptr = target as *const _;
        let comparison = self.target_comparisons.get(&target_ptr)?;
        let evaluations = comparison.scope.validate().ok()?;
        if expected_reads != evaluations as f64 {
            return None;
        }
        self.calibrated(
            comparison
                .raw
                .physical_dag
                .estimate(&comparison.scope)
                .ok()?,
        )
    }
}

fn estimate_heterogeneous_summary(
    root: &SummaryNode,
    deployments: &[CostedSummaryDeployment<'_>],
    evidence: &StreamingNodeEvidence,
    scope: &ComparisonScope,
    raw: &StreamingRawInputEvidence,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    validate_summary_edges_and_physical_ids(root, evidence)?;
    fn query_scan_selections<'a>(
        query: &'a QueryExpr,
        out: &mut Vec<(&'a Source, &'a [Predicate])>,
    ) {
        use QueryExpr::*;
        match query {
            Scan {
                source, predicates, ..
            } => out.push((source, predicates)),
            PromqlVectorFromScalar(child) | PromqlScalarFromVector(child) => {
                query_scan_selections(child, out)
            }
            PromqlRelabel { child, .. }
            | Filter { child, .. }
            | Project { child, .. }
            | Aggregate { child, .. }
            | Dedup { child, .. }
            | PromqlSubquery { child, .. }
            | TimeRange { child, .. }
            | TimeShift { child, .. }
            | SQLWindowFunc { child, .. }
            | PromqlSeriesSample { child, .. }
            | PromqlInfoEnrich { child, .. }
            | Sort { child, .. }
            | Limit { child, .. } => query_scan_selections(child, out),
            Concat { children } => {
                for child in children {
                    query_scan_selections(child, out);
                }
            }
            Join { left, right, .. } | SetOp { left, right, .. } => {
                query_scan_selections(left, out);
                query_scan_selections(right, out);
            }
            BinaryOp { lhs, rhs, .. } => {
                query_scan_selections(lhs, out);
                query_scan_selections(rhs, out);
            }
            PromqlScalarBridge(_)
            | EvalTimestamp
            | CurrentTimestamp
            | Column(_)
            | Literal(_)
            | Compare { .. }
            | BoolAnd(_)
            | BoolOr(_)
            | Not(_)
            | IsNull(_)
            | IsNotNull(_)
            | Cast { .. }
            | InList { .. }
            | FunctionCall { .. }
            | Arithmetic { .. }
            | Case { .. } => {}
        }
    }
    fn summary_scan_selections<'a>(
        node: &'a SummaryNode,
        seen: &mut HashSet<*const SummaryNode>,
        out: &mut Vec<(&'a Source, &'a [Predicate])>,
    ) {
        if !seen.insert(node as *const _) {
            return;
        }
        match &node.expr {
            SummaryExpr::KeepPreAsap(query) => query_scan_selections(query, out),
            SummaryExpr::SummaryAgg { child, .. } => summary_scan_selections(child, seen, out),
            SummaryExpr::SummaryMerge { children } => {
                for child in children {
                    summary_scan_selections(child, seen, out);
                }
            }
            SummaryExpr::SummarySubtract { left, right }
            | SummaryExpr::SummaryJoin {
                outer: left,
                inner: right,
                ..
            } => {
                summary_scan_selections(left, seen, out);
                summary_scan_selections(right, seen, out);
            }
            SummaryExpr::SummaryDelete { summary_input, .. }
            | SummaryExpr::SummaryEstimate { summary_input, .. } => {
                summary_scan_selections(summary_input, seen, out)
            }
        }
    }
    let evaluations = scope.validate()? as f64;
    let by_node: HashMap<_, _> = deployments
        .iter()
        .map(|deployment| (deployment.summary as *const _, deployment))
        .collect();
    let mut cpu_ops = 0.0;
    let mut persistent_bytes = 0_u64;
    let mut scans = HashMap::<String, (usize, u64)>::new();
    let mut physical_states = HashMap::<
        String,
        (
            StreamingAggregateEvidence,
            SummaryMaintenanceLifecycleGuarantee,
        ),
    >::new();
    for deployment in deployments {
        let node_evidence = evidence
            .aggregation(deployment.summary)
            .ok_or(AnalyticalCostError::MissingOrStale("summary_agg"))?;
        if node_evidence.source_coverage_index >= scope.sources.len() {
            return Err(AnalyticalCostError::MissingComparisonScope(
                "summary source coverage",
            ));
        }
        let SummaryExpr::SummaryAgg { child, .. } = &deployment.summary.expr else {
            return Err(AnalyticalCostError::UnsupportedCandidate);
        };
        let inputs = node_evidence.inputs.validate()?;
        if matches!(&child.expr, SummaryExpr::KeepPreAsap(_))
            && (inputs.initial_input_rows != raw.input_rows_per_evaluation
                || inputs.initial_input_bytes != raw.input_bytes_per_evaluation
                || inputs.initial_source_scan_bytes != raw.source_scan_bytes_per_evaluation
                || inputs.ingestion_rate_per_second != raw.ingestion_rate_per_second)
        {
            return Err(AnalyticalCostError::ComparisonScopeMismatch(
                "source-root bootstrap evolution",
            ));
        }
        let mut actual_selections = Vec::new();
        summary_scan_selections(child, &mut HashSet::new(), &mut actual_selections);
        let declared = &scope.sources[node_evidence.source_coverage_index];
        if actual_selections.len() != 1
            || actual_selections[0].0 != &declared.source
            || actual_selections[0].1 != declared.predicates.as_slice()
        {
            return Err(AnalyticalCostError::ComparisonScopeMismatch(
                "summary source lineage",
            ));
        }
        validate_guarantee(deployment.guarantee, scope.data_arrival)?;
        match physical_states.entry(node_evidence.physical_id.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((node_evidence.clone(), deployment.guarantee.clone()));
            }
            std::collections::hash_map::Entry::Occupied(entry)
                if entry.get() != &(node_evidence.clone(), deployment.guarantee.clone()) =>
            {
                return Err(AnalyticalCostError::ComparisonScopeMismatch(
                    "summary physical identity",
                ));
            }
            std::collections::hash_map::Entry::Occupied(_) => continue,
        }
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

    #[expect(clippy::too_many_arguments, reason = "CPU and I/O traversal state")]
    fn visit_ops(
        node: &SummaryNode,
        seen: &mut HashSet<String>,
        by_node: &HashMap<*const SummaryNode, &CostedSummaryDeployment<'_>>,
        evidence: &StreamingNodeEvidence,
        scope: &ComparisonScope,
        evaluations: f64,
        cpu_ops: &mut f64,
        io_bytes: &mut u64,
    ) -> Result<(), AnalyticalCostError> {
        let physical_id = summary_physical_id(node, evidence)?;
        if !seen.insert(physical_id) {
            return Ok(());
        }
        match &node.expr {
            SummaryExpr::KeepPreAsap(_) => {
                let retained = evidence
                    .retained_queries
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("keep_pre_asap"))?;
                let physical = retained.physical_dag.estimate(scope)?;
                if physical.cpu_ops != retained.preprocessing_cpu_ops_over_horizon
                    || physical.scan_bytes != 0
                    || retained
                        .working_memory_bytes
                        .checked_add(retained.output_buffer_bytes)
                        != Some(physical.peak_memory_bytes)
                {
                    return Err(AnalyticalCostError::ComparisonScopeMismatch(
                        "keep_pre_asap physical DAG",
                    ));
                }
                if !retained.preprocessing_cpu_ops_over_horizon.is_finite()
                    || retained.preprocessing_cpu_ops_over_horizon < 0.0
                {
                    return Err(AnalyticalCostError::InvalidOperationCost(
                        "keep_pre_asap",
                        retained.preprocessing_cpu_ops_over_horizon,
                    ));
                }
                *cpu_ops += retained.preprocessing_cpu_ops_over_horizon;
            }
            SummaryExpr::SummaryAgg { child, .. } => {
                visit_ops(
                    child,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluations,
                    cpu_ops,
                    io_bytes,
                )?;
            }
            SummaryExpr::SummaryMerge { children } => {
                let operation = evidence
                    .operations
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_merge"))?;
                let merge = validated_operator_cpu("summary_merge", operation.cpu_ops)?;
                *cpu_ops += evaluations
                    * validated_operator_executions("summary_merge", operation)? as f64
                    * merge;
                add_operator_io(io_bytes, operation, evaluations)?;
                for child in children {
                    visit_ops(
                        child,
                        seen,
                        by_node,
                        evidence,
                        scope,
                        evaluations,
                        cpu_ops,
                        io_bytes,
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
                add_operator_io(io_bytes, operation, evaluations)?;
                visit_ops(
                    left,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluations,
                    cpu_ops,
                    io_bytes,
                )?;
                visit_ops(
                    right,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluations,
                    cpu_ops,
                    io_bytes,
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
                fn collect_aggs(
                    node: &SummaryNode,
                    seen: &mut HashSet<*const SummaryNode>,
                    out: &mut Vec<*const SummaryNode>,
                ) {
                    if !seen.insert(node as *const _) {
                        return;
                    }
                    match &node.expr {
                        SummaryExpr::SummaryAgg { child, .. } => {
                            out.push(node as *const _);
                            collect_aggs(child, seen, out);
                        }
                        SummaryExpr::SummaryMerge { children } => {
                            children
                                .iter()
                                .for_each(|child| collect_aggs(child, seen, out));
                        }
                        SummaryExpr::SummarySubtract { left, right }
                        | SummaryExpr::SummaryJoin {
                            outer: left,
                            inner: right,
                            ..
                        } => {
                            collect_aggs(left, seen, out);
                            collect_aggs(right, seen, out);
                        }
                        SummaryExpr::SummaryDelete { summary_input, .. }
                        | SummaryExpr::SummaryEstimate { summary_input, .. } => {
                            collect_aggs(summary_input, seen, out)
                        }
                        SummaryExpr::KeepPreAsap(_) => {}
                    }
                }
                let mut reachable = Vec::new();
                collect_aggs(summary_input, &mut HashSet::new(), &mut reachable);
                if reachable.as_slice() != [*state_ptr] {
                    return Err(AnalyticalCostError::ComparisonScopeMismatch(
                        "summary delete owner",
                    ));
                }
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
                let delete_events = (rate * active_ms as f64 / 1_000.0).ceil() * fanout as f64;
                *cpu_ops += delete_events
                    * validated_operator_executions("summary_delete", operation)? as f64
                    * validated_operator_cpu("summary_delete", operation.cpu_ops)?;
                add_operator_io(io_bytes, operation, delete_events)?;
                visit_ops(
                    summary_input,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluations,
                    cpu_ops,
                    io_bytes,
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
                add_operator_io(io_bytes, operation, evaluations)?;
                visit_ops(
                    summary_input,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluations,
                    cpu_ops,
                    io_bytes,
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
                    || join.executions_per_evaluation == 0
                {
                    return Err(AnalyticalCostError::MissingOrStale("summary_join"));
                }
                *cpu_ops += evaluations
                    * join.executions_per_evaluation as f64
                    * join.matched_state_pairs_per_evaluation as f64
                    * join.cpu_ops_per_matched_pair;
                let join_io = join
                    .io_bytes_per_execution
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_join_io"))?;
                *io_bytes = io_bytes
                    .checked_add(
                        join_io
                            .checked_mul(join.executions_per_evaluation)
                            .and_then(|bytes| bytes.checked_mul(evaluations as u64))
                            .ok_or(AnalyticalCostError::Overflow)?,
                    )
                    .ok_or(AnalyticalCostError::Overflow)?;
                visit_ops(
                    outer,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluations,
                    cpu_ops,
                    io_bytes,
                )?;
                visit_ops(
                    inner,
                    seen,
                    by_node,
                    evidence,
                    scope,
                    evaluations,
                    cpu_ops,
                    io_bytes,
                )?;
            }
        }
        Ok(())
    }

    let mut operator_io_bytes = 0;
    visit_ops(
        root,
        &mut HashSet::new(),
        &by_node,
        evidence,
        scope,
        evaluations,
        &mut cpu_ops,
        &mut operator_io_bytes,
    )?;
    let transient_bytes = estimate_transient_liveness(root, evidence)?;
    if !cpu_ops.is_finite() {
        return Err(AnalyticalCostError::Overflow);
    }
    Ok(ResourceEstimate {
        cpu_ops,
        peak_memory_bytes: persistent_bytes
            .checked_add(transient_bytes)
            .ok_or(AnalyticalCostError::Overflow)?,
        scan_bytes: scans
            .values()
            .try_fold(operator_io_bytes, |sum, (_, bytes)| {
                sum.checked_add(*bytes).ok_or(AnalyticalCostError::Overflow)
            })?,
    })
}

fn add_operator_io(
    total: &mut u64,
    operation: &StreamingSummaryOperatorEvidence,
    execution_units: f64,
) -> Result<(), AnalyticalCostError> {
    let bytes = operation
        .io_bytes_per_execution
        .ok_or(AnalyticalCostError::MissingOrStale("summary operator io"))?;
    if operation.executions_per_evaluation == 0
        || !execution_units.is_finite()
        || execution_units < 0.0
        || execution_units.fract() != 0.0
        || execution_units > u64::MAX as f64
    {
        return Err(AnalyticalCostError::MissingOrStale(
            "summary operator executions",
        ));
    }
    *total = total
        .checked_add(
            bytes
                .checked_mul(operation.executions_per_evaluation)
                .and_then(|value| value.checked_mul(execution_units as u64))
                .ok_or(AnalyticalCostError::Overflow)?,
        )
        .ok_or(AnalyticalCostError::Overflow)?;
    Ok(())
}

fn validate_summary_edges_and_physical_ids(
    root: &SummaryNode,
    evidence: &StreamingNodeEvidence,
) -> Result<(), AnalyticalCostError> {
    fn children(node: &SummaryNode) -> Vec<&SummaryNode> {
        match &node.expr {
            SummaryExpr::KeepPreAsap(_) => vec![],
            SummaryExpr::SummaryAgg { child, .. } => vec![child],
            SummaryExpr::SummaryMerge { children } => {
                children.iter().map(|child| child.as_ref()).collect()
            }
            SummaryExpr::SummarySubtract { left, right }
            | SummaryExpr::SummaryJoin {
                outer: left,
                inner: right,
                ..
            } => vec![left, right],
            SummaryExpr::SummaryDelete { summary_input, .. }
            | SummaryExpr::SummaryEstimate { summary_input, .. } => vec![summary_input],
        }
    }
    fn metadata(
        node: &SummaryNode,
        evidence: &StreamingNodeEvidence,
    ) -> Result<(String, Vec<EdgeStatistics>, EdgeStatistics), AnalyticalCostError> {
        match &node.expr {
            SummaryExpr::KeepPreAsap(_) => {
                let retained = evidence
                    .retained_queries
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("keep_pre_asap"))?;
                let root = retained
                    .physical_dag
                    .nodes
                    .iter()
                    .find(|candidate| candidate.id == retained.physical_dag.root)
                    .ok_or(AnalyticalCostError::InvalidPhysicalDag(
                        "missing retained-query root",
                    ))?;
                let stats = retained
                    .physical_dag
                    .statistics
                    .get(&root.id)
                    .ok_or_else(|| {
                        AnalyticalCostError::MissingOperatorStatistics(root.id.clone())
                    })?;
                Ok((retained.physical_id.clone(), vec![], stats.output))
            }
            SummaryExpr::SummaryAgg { .. } => {
                let value = evidence
                    .aggregations
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_agg"))?;
                Ok((value.physical_id.clone(), vec![value.input], value.output))
            }
            SummaryExpr::SummaryJoin { .. } => {
                let value = evidence
                    .joins
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("summary_join"))?;
                Ok((
                    value.physical_id.clone(),
                    value.inputs.clone(),
                    value.output,
                ))
            }
            _ => {
                let value = evidence
                    .operations
                    .get(&(node as *const _))
                    .ok_or(AnalyticalCostError::MissingOrStale("summary operation"))?;
                Ok((
                    value.physical_id.clone(),
                    value.inputs.clone(),
                    value.output,
                ))
            }
        }
    }
    fn visit(
        node: &SummaryNode,
        evidence: &StreamingNodeEvidence,
        seen: &mut HashSet<*const SummaryNode>,
        physical: &mut HashMap<String, (Vec<EdgeStatistics>, EdgeStatistics, String)>,
    ) -> Result<EdgeStatistics, AnalyticalCostError> {
        if !seen.insert(node as *const _) {
            return metadata(node, evidence).map(|(_, _, output)| output);
        }
        let child_nodes = children(node);
        let child_outputs = child_nodes
            .iter()
            .map(|child| visit(child, evidence, seen, physical))
            .collect::<Result<Vec<_>, _>>()?;
        let child_physical_ids = child_nodes
            .iter()
            .map(|child| summary_physical_id(child, evidence))
            .collect::<Result<Vec<_>, _>>()?;
        let (id, inputs, output) = metadata(node, evidence)?;
        let local_fingerprint = match &node.expr {
            SummaryExpr::KeepPreAsap(_) => {
                format!("{:?}", evidence.retained_queries.get(&(node as *const _)))
            }
            SummaryExpr::SummaryAgg { .. } => {
                format!("{:?}", evidence.aggregations.get(&(node as *const _)))
            }
            SummaryExpr::SummaryJoin { .. } => {
                format!("{:?}", evidence.joins.get(&(node as *const _)))
            }
            _ => format!("{:?}", evidence.operations.get(&(node as *const _))),
        };
        // A provider identity names the complete physical operator, including
        // its inputs. Equal local widths/costs do not make operators consuming
        // different physical children the same deployment.
        let fingerprint = format!("{local_fingerprint}|children={child_physical_ids:?}");
        if id.is_empty()
            || inputs != child_outputs
            || !output.is_consistent()
            || inputs.iter().any(|edge| !edge.is_consistent())
        {
            return Err(AnalyticalCostError::ComparisonScopeMismatch(
                "summary physical edge statistics",
            ));
        }
        match physical.entry(id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((inputs, output, fingerprint));
            }
            std::collections::hash_map::Entry::Occupied(entry)
                if entry.get() != &(inputs, output, fingerprint) =>
            {
                return Err(AnalyticalCostError::ComparisonScopeMismatch(
                    "summary physical identity",
                ));
            }
            _ => {}
        }
        Ok(output)
    }
    visit(root, evidence, &mut HashSet::new(), &mut HashMap::new()).map(|_| ())
}

fn summary_physical_id(
    node: &SummaryNode,
    evidence: &StreamingNodeEvidence,
) -> Result<String, AnalyticalCostError> {
    match &node.expr {
        SummaryExpr::KeepPreAsap(_) => evidence
            .retained_queries
            .get(&(node as *const _))
            .map(|value| value.physical_id.clone()),
        SummaryExpr::SummaryAgg { .. } => evidence
            .aggregations
            .get(&(node as *const _))
            .map(|value| value.physical_id.clone()),
        SummaryExpr::SummaryJoin { .. } => evidence
            .joins
            .get(&(node as *const _))
            .map(|value| value.physical_id.clone()),
        _ => evidence
            .operations
            .get(&(node as *const _))
            .map(|value| value.physical_id.clone()),
    }
    .ok_or(AnalyticalCostError::MissingOrStale(
        "summary physical identity",
    ))
}

/// Simulate a deterministic child-before-parent physical schedule. Completed
/// child output buffers remain live until their final consumer executes;
/// operator workspace and its output buffer coexist during that execution.
fn estimate_transient_liveness(
    root: &SummaryNode,
    evidence: &StreamingNodeEvidence,
) -> Result<u64, AnalyticalCostError> {
    fn children(node: &SummaryNode) -> Vec<&SummaryNode> {
        match &node.expr {
            SummaryExpr::KeepPreAsap(_) => vec![],
            SummaryExpr::SummaryAgg { child, .. } => vec![child],
            SummaryExpr::SummaryMerge { children } => {
                children.iter().map(|child| child.as_ref()).collect()
            }
            SummaryExpr::SummarySubtract { left, right }
            | SummaryExpr::SummaryJoin {
                outer: left,
                inner: right,
                ..
            } => vec![left, right],
            SummaryExpr::SummaryDelete { summary_input, .. }
            | SummaryExpr::SummaryEstimate { summary_input, .. } => vec![summary_input],
        }
    }
    fn visit<'a>(
        node: &'a SummaryNode,
        evidence: &StreamingNodeEvidence,
        seen: &mut HashSet<String>,
        uses: &mut HashMap<String, usize>,
        order: &mut Vec<&'a SummaryNode>,
    ) -> Result<(), AnalyticalCostError> {
        if !seen.insert(summary_physical_id(node, evidence)?) {
            return Ok(());
        }
        for child in children(node) {
            *uses
                .entry(summary_physical_id(child, evidence)?)
                .or_default() += 1;
            visit(child, evidence, seen, uses, order)?;
        }
        order.push(node);
        Ok(())
    }
    fn memory(
        node: &SummaryNode,
        evidence: &StreamingNodeEvidence,
    ) -> Result<(u64, u64), AnalyticalCostError> {
        match &node.expr {
            SummaryExpr::KeepPreAsap(_) => evidence
                .retained_queries
                .get(&(node as *const _))
                .map(|value| (value.working_memory_bytes, value.output_buffer_bytes))
                .ok_or(AnalyticalCostError::MissingOrStale("keep_pre_asap")),
            SummaryExpr::SummaryAgg { .. } => Ok((0, 0)),
            SummaryExpr::SummaryJoin { .. } => evidence
                .joins
                .get(&(node as *const _))
                .map(|value| (value.working_memory_bytes, value.output_buffer_bytes))
                .ok_or(AnalyticalCostError::MissingOrStale("summary_join")),
            SummaryExpr::SummaryMerge { .. }
            | SummaryExpr::SummarySubtract { .. }
            | SummaryExpr::SummaryDelete { .. }
            | SummaryExpr::SummaryEstimate { .. } => evidence
                .operations
                .get(&(node as *const _))
                .map(|value| (value.working_memory_bytes, value.output_buffer_bytes))
                .ok_or(AnalyticalCostError::MissingOrStale("summary operation")),
        }
    }

    let mut uses = HashMap::new();
    let mut order = Vec::new();
    visit(root, evidence, &mut HashSet::new(), &mut uses, &mut order)?;
    let outputs: HashMap<_, _> = order
        .iter()
        .map(|node| {
            memory(node, evidence)
                .and_then(|(_, output)| summary_physical_id(node, evidence).map(|id| (id, output)))
        })
        .collect::<Result<_, _>>()?;
    let mut live = 0_u64;
    let mut peak = 0_u64;
    for node in order {
        let (workspace, output) = memory(node, evidence)?;
        peak = peak.max(
            live.checked_add(workspace)
                .and_then(|bytes| bytes.checked_add(output))
                .ok_or(AnalyticalCostError::Overflow)?,
        );
        live = live
            .checked_add(output)
            .ok_or(AnalyticalCostError::Overflow)?;
        for child in children(node) {
            let child_id = summary_physical_id(child, evidence)?;
            let remaining =
                uses.get_mut(&child_id)
                    .ok_or(AnalyticalCostError::InvalidPhysicalDag(
                        "missing summary consumer count",
                    ))?;
            *remaining -= 1;
            if *remaining == 0 {
                live = live
                    .checked_sub(outputs[&child_id])
                    .ok_or(AnalyticalCostError::Overflow)?;
            }
        }
    }
    Ok(peak)
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
    let join_cpu = match (counts.joins_per_read, join.as_ref()) {
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
    let join_bytes = match (counts.joins_per_read, join.as_ref()) {
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
        let target = streaming_sum_query();
        bind_aggregations(&mut model, &target, &root, inputs, streaming_cpu());
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
        assert_eq!(model.raw_query_recompute_cost(&target), None);
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
                    bind_aggregations(
                        &mut model,
                        &group.target,
                        root,
                        streaming_inputs(),
                        streaming_cpu(),
                    );
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
        assert_eq!(plan.raw_recompute_total_cost, Some(Cost(5_264.0)));

        let mut missing_baseline = model.clone();
        missing_baseline
            .target_comparisons
            .get_mut(&Rc::as_ptr(&space.roots[0].1))
            .unwrap()
            .raw
            .physical_dag
            .statistics
            .clear();
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
        assert_eq!(cheap_plan.raw_recompute_total_cost, Some(Cost(5_264.0)));
    }

    #[test]
    fn raw_evolution_is_bound_to_the_requested_target() {
        let target_a = streaming_sum_query();
        let target_b = streaming_sum_query();
        let root_a = summary_with_operations(false, false, false);
        let root_b = summary_with_operations(false, false, false);
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target_a,
            &root_a,
            streaming_inputs(),
            streaming_cpu(),
        );
        let mut faster = streaming_inputs();
        // Candidate-local intermediate cardinality is not the raw target's
        // planning-time cardinality and must not constrain its baseline.
        faster.initial_input_rows = 7;
        faster.initial_input_bytes = 448;
        faster.initial_source_scan_bytes = 448;
        faster.ingestion_rate_per_second = 4.0;
        bind_aggregations(&mut model, &target_b, &root_b, faster, streaming_cpu());
        model
            .target_comparisons
            .get_mut(&Rc::as_ptr(&target_b))
            .unwrap()
            .raw = {
            let mut raw = streaming_raw();
            raw.ingestion_rate_per_second = 4.0;
            let statistics = raw.physical_dag.statistics.get_mut("raw-scan").unwrap();
            statistics.source_scan_bytes = 7_040;
            statistics.inputs[0] = EdgeStatistics {
                rows: 110,
                bytes: 7_040,
            };
            statistics.output = statistics.inputs[0];
            raw
        };

        let a = model.raw_query_recompute_total_cost(&target_a, 5.0);
        let b = model.raw_query_recompute_total_cost(&target_b, 5.0);
        assert_eq!(a, Some(Cost(5_264.0)));
        assert!(b.unwrap().0 > a.unwrap().0);
        assert_eq!(model.raw_query_recompute_total_cost(&target_a, 5.0), a);
    }

    #[test]
    fn comparison_binding_is_transactional_and_shared_nodes_allow_two_targets() {
        let target_a = streaming_sum_query();
        let target_b = streaming_sum_query();
        let root = summary_with_operations(false, false, false);
        let mut model = streaming_model();
        let mut wrong_scope = streaming_scope();
        wrong_scope.sources[0].source = Source::TimeSeries {
            metric: "wrong".into(),
        };
        assert_eq!(
            model.bind_candidate_comparison(&target_a, &root, wrong_scope, streaming_raw(),),
            Err(AnalyticalCostError::ComparisonScopeMismatch(
                "raw target source lineage"
            ))
        );
        assert!(model.target_comparisons.is_empty());
        assert!(model.candidate_comparisons.is_empty());

        model
            .bind_candidate_comparison(&target_a, &root, streaming_scope(), streaming_raw())
            .unwrap();
        model
            .bind_candidate_comparison(&target_b, &root, streaming_scope(), streaming_raw())
            .unwrap();
        assert_eq!(model.candidate_comparisons.len(), 2);
    }

    #[test]
    fn delete_owner_must_be_the_unique_state_reachable_from_its_input() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = summary_with_operations(false, false, true);
        let mut cpu = streaming_cpu();
        cpu.delete_cpu_ops = Some(1.0);
        cpu.delete_events_per_second = Some(1.0);
        cpu.delete_routing_fanout = Some(1);
        let mut model = streaming_model();
        model.capabilities.delete = true;
        bind_aggregations(&mut model, &target, &root, streaming_inputs(), cpu);
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &root.expr else {
            unreachable!();
        };
        let delete_ptr = Rc::as_ptr(summary_input);
        let unrelated = summary_with_operations(false, false, false);
        let unrelated_agg = evidence_nodes(&unrelated).0[0] as *const _;
        model
            .node_evidence
            .operation_state_owners
            .insert(delete_ptr, unrelated_agg);

        let plan = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(plan.summary_total_cost, None);
    }

    #[test]
    fn summary_edge_and_io_evidence_fail_closed() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = summary_join();
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        let join = evidence_nodes(&root).1[0];
        model.node_evidence.joins.insert(
            join as *const _,
            SummaryJoinEvidence {
                physical_id: "join-edge".into(),
                inputs: vec![test_edge(), EdgeStatistics { rows: 2, bytes: 16 }],
                output: test_edge(),
                matched_state_pairs_per_evaluation: 1,
                cpu_ops_per_matched_pair: 1.0,
                working_memory_bytes: 1,
                output_buffer_bytes: 0,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
            },
        );
        let bad_edge = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(bad_edge.summary_total_cost, None);

        model
            .node_evidence
            .joins
            .get_mut(&(join as *const _))
            .unwrap()
            .inputs = vec![test_edge(), test_edge()];
        model
            .node_evidence
            .operations
            .get_mut(&Rc::as_ptr(&root))
            .unwrap()
            .io_bytes_per_execution = None;
        let missing_io = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(missing_io.summary_total_cost, None);
    }

    #[test]
    fn summary_edges_io_and_physical_identity_fail_closed() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = summary_join();
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        let (_, joins) = evidence_nodes(&root);
        model.node_evidence.insert_join(
            &Rc::new(joins[0].clone()),
            SummaryJoinEvidence {
                physical_id: "unused".into(),
                inputs: vec![test_edge(), test_edge()],
                output: test_edge(),
                matched_state_pairs_per_evaluation: 1,
                cpu_ops_per_matched_pair: 1.0,
                working_memory_bytes: 1,
                output_buffer_bytes: 0,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
            },
        );
        // Bind the actual join, then make one parent input disagree with its
        // child's output.
        model.node_evidence.joins.insert(
            joins[0] as *const _,
            SummaryJoinEvidence {
                physical_id: "join-edge".into(),
                inputs: vec![test_edge(), EdgeStatistics { rows: 2, bytes: 16 }],
                output: test_edge(),
                matched_state_pairs_per_evaluation: 1,
                cpu_ops_per_matched_pair: 1.0,
                working_memory_bytes: 1,
                output_buffer_bytes: 0,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
            },
        );
        let bad_edge = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(bad_edge.summary_total_cost, None);

        model
            .node_evidence
            .joins
            .get_mut(&(joins[0] as *const _))
            .unwrap()
            .inputs = vec![test_edge(), test_edge()];
        model
            .node_evidence
            .operations
            .get_mut(&Rc::as_ptr(&root))
            .unwrap()
            .io_bytes_per_execution = None;
        let missing_io = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(missing_io.summary_total_cost, None);
    }

    #[test]
    fn liveness_does_not_add_disjoint_execution_workspaces() {
        let target = streaming_sum_query();
        let root = summary_join();
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        let join = evidence_nodes(&root).1[0];
        model.node_evidence.joins.insert(
            join as *const _,
            SummaryJoinEvidence {
                physical_id: "huge-join".into(),
                inputs: vec![test_edge(), test_edge()],
                output: test_edge(),
                matched_state_pairs_per_evaluation: 1,
                cpu_ops_per_matched_pair: 1.0,
                working_memory_bytes: u64::MAX,
                output_buffer_bytes: 0,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
            },
        );
        model
            .node_evidence
            .operations
            .get_mut(&Rc::as_ptr(&root))
            .unwrap()
            .working_memory_bytes = u64::MAX;
        assert_eq!(
            estimate_transient_liveness(&root, &model.node_evidence),
            Ok(u64::MAX)
        );
    }

    #[test]
    fn conflicting_evidence_cannot_alias_one_provider_physical_identity() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = summary_join();
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        let aggregations = evidence_nodes(&root).0;
        let first = aggregations[0] as *const _;
        let second = aggregations[1] as *const _;
        model
            .node_evidence
            .aggregations
            .get_mut(&first)
            .unwrap()
            .physical_id = "aliased-state".into();
        let second_evidence = model.node_evidence.aggregations.get_mut(&second).unwrap();
        second_evidence.physical_id = "aliased-state".into();
        second_evidence.cpu.insert_cpu_ops = Some(99.0);

        let plan = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(plan.summary_total_cost, None);
    }

    #[test]
    fn lifecycle_plan_does_not_fall_back_to_partial_agg_cost_for_a_join_root() {
        let workload = streaming_workload();
        let root = summary_join();
        let target = streaming_sum_query();
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
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
                physical_id: "costed-join".into(),
                inputs: vec![test_edge(), test_edge()],
                output: test_edge(),
                matched_state_pairs_per_evaluation: 2,
                cpu_ops_per_matched_pair: 3.0,
                working_memory_bytes: 64,
                output_buffer_bytes: 64,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
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
        let target = streaming_sum_query();
        let (aggregations, joins) = evidence_nodes(&root);
        let mut model = streaming_model();
        bind_comparison(&mut model, &target, &root);
        model.node_evidence.aggregations.insert(
            aggregations[0] as *const _,
            StreamingAggregateEvidence {
                physical_id: "left-state".into(),
                input: test_edge(),
                output: test_edge(),
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
                physical_id: "right-state".into(),
                input: test_edge(),
                output: test_edge(),
                source_coverage_index: 0,
                bootstrap_read_identity: "right-bootstrap".into(),
                inputs: second_inputs,
                cpu: second_cpu,
            },
        );
        model.node_evidence.joins.insert(
            joins[0] as *const _,
            SummaryJoinEvidence {
                physical_id: "join".into(),
                inputs: vec![test_edge(), test_edge()],
                output: test_edge(),
                matched_state_pairs_per_evaluation: 2,
                cpu_ops_per_matched_pair: 3.0,
                working_memory_bytes: 64,
                output_buffer_bytes: 64,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
            },
        );
        model.node_evidence.operations.insert(
            Rc::as_ptr(&root),
            StreamingSummaryOperatorEvidence {
                physical_id: "root-readout".into(),
                inputs: vec![test_edge()],
                output: test_edge(),
                cpu_ops: 3.0,
                working_memory_bytes: 0,
                output_buffer_bytes: 0,
                executions_per_evaluation: 1,
                events_per_second: None,
                routing_fanout: None,
                io_bytes_per_execution: Some(0),
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
        // The join's 64-byte output remains live while the readout's workspace
        // is active. The join's execution workspace is released first.
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
    fn whole_dag_fails_closed_for_missing_retained_work_or_false_source_lineage() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let root = summary_with_operations(false, false, false);
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        model.node_evidence.retained_queries.clear();
        let missing_retained = plan_summary_maintenance_lifecycles(
            Rc::clone(&root),
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(missing_retained.summary_total_cost, None);

        bind_comparison(&mut model, &target, &root);
        model
            .target_comparisons
            .get_mut(&Rc::as_ptr(&target))
            .unwrap()
            .scope
            .sources[0]
            .source = Source::TimeSeries {
            metric: "other_metric".into(),
        };
        let false_lineage = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &model,
        )
        .unwrap();
        assert_eq!(false_lineage.summary_total_cost, None);
    }

    #[test]
    fn aggregate_recurses_into_child_operations_and_state_only_needs_no_readout() {
        let workload = streaming_workload();
        let target = streaming_sum_query();
        let estimated = summary_with_operations(false, false, false);
        let SummaryExpr::SummaryEstimate { summary_input, .. } = &estimated.expr else {
            unreachable!();
        };
        let state_only = Rc::clone(summary_input);
        let mut no_readout_cpu = streaming_cpu();
        no_readout_cpu.readout_cpu_ops = None;
        let mut state_model = streaming_model();
        bind_aggregations(
            &mut state_model,
            &target,
            &state_only,
            streaming_inputs(),
            no_readout_cpu,
        );
        let state_plan = plan_summary_maintenance_lifecycles(
            state_only,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &state_model,
        )
        .unwrap();
        assert!(state_plan.summary_total_cost.is_some());

        let child = summary_with_operations(true, false, false);
        let nested = Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryAgg {
                child,
                family: SummaryFamilyType::ExactAggregate(ExactKind::Count, ExactParams::Count),
                col: ColumnRef::Wildcard,
                reduction: Reduction::by(vec![]),
                grouping: GroupingStrategy::PerSubpopulationInstance,
            },
            schema: estimated.schema.clone(),
            guarantee: None,
        });
        let mut nested_cpu = streaming_cpu();
        nested_cpu.merge_cpu_ops = Some(1.0);
        let mut nested_model = streaming_model();
        bind_aggregations(
            &mut nested_model,
            &target,
            &nested,
            streaming_inputs(),
            nested_cpu,
        );
        nested_model
            .node_evidence
            .operations
            .retain(|_, operation| operation.cpu_ops != 1.0 || operation.working_memory_bytes == 0);
        let nested_plan = plan_summary_maintenance_lifecycles(
            nested,
            WorkloadDemand::new(&workload, &[0]),
            0,
            Some(Horizon(5.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &nested_model,
        )
        .unwrap();
        assert_eq!(nested_plan.summary_total_cost, None);
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
    fn lifecycle_retention_rate_integrates_to_one_peak_capacity_charge() {
        let target = streaming_sum_query();
        let root = summary_with_operations(false, false, false);
        let mut model = streaming_model();
        bind_aggregations(
            &mut model,
            &target,
            &root,
            streaming_inputs(),
            streaming_cpu(),
        );
        let aggregation = evidence_nodes(&root).0[0];
        let inputs = model
            .lifecycle_inputs(aggregation, Some(Horizon(5.0)))
            .unwrap();
        let integrated = inputs.retention_cost_rate.unwrap().0 * 5.0;
        // (2 active + 3 retained) * 2 states * 100 bytes, calibrated once.
        assert_eq!(integrated, 1_000.0);
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
                physical_id: "diagnostic-join".into(),
                inputs: vec![test_edge(), test_edge()],
                output: test_edge(),
                matched_state_pairs_per_evaluation: 3,
                cpu_ops_per_matched_pair: 4.0,
                working_memory_bytes: 32,
                output_buffer_bytes: 0,
                executions_per_evaluation: 1,
                io_bytes_per_execution: Some(0),
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
            expr: SummaryExpr::KeepPreAsap(Rc::new(QueryExpr::Scan {
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
            })),
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
        StreamingAnalyticalCostModel::new(
            ResourceCalibration {
                cost_per_cpu_op: 1.0,
                cost_per_scan_byte: 1.0,
                cost_per_retained_byte: 1.0,
                version: "test".into(),
            },
            SummaryMaintenanceCapabilities {
                incremental_update: true,
                merge: false,
                delete: false,
            },
        )
    }

    fn streaming_raw() -> StreamingRawInputEvidence {
        let scope = streaming_scope();
        let node = PhysicalDagNode {
            id: "raw-scan".into(),
            operator: PhysicalOperator::Scan,
            children: vec![],
            source_coverage: Some(scope.sources[0].clone()),
            output_buffer_bytes: 0,
            retained_bytes: 0,
            execution: ExecutionMultiplicity::Once,
        };
        let statistics = OperatorStatistics {
            source_scan_bytes: 5_120,
            inputs: vec![EdgeStatistics {
                rows: 80,
                bytes: 5_120,
            }],
            output: EdgeStatistics {
                rows: 80,
                bytes: 5_120,
            },
            group_count: None,
            key_bytes: None,
            aggregate_value_bytes: None,
            k: None,
            hash_join_build_side: None,
        };
        StreamingRawInputEvidence {
            input_rows_per_evaluation: 10,
            input_bytes_per_evaluation: 640,
            source_scan_bytes_per_evaluation: 640,
            arriving_row_bytes: 64,
            ingestion_rate_per_second: 2.0,
            physical_dag: StreamingPhysicalDagEvidence {
                nodes: vec![node],
                root: "raw-scan".into(),
                statistics: HashMap::from([("raw-scan".into(), statistics)]),
            },
        }
    }

    fn zero_physical_dag() -> StreamingPhysicalDagEvidence {
        let node = PhysicalDagNode {
            id: "retained-pass".into(),
            operator: PhysicalOperator::Scan,
            children: vec![],
            source_coverage: Some(streaming_scope().sources[0].clone()),
            output_buffer_bytes: 0,
            retained_bytes: 0,
            execution: ExecutionMultiplicity::Once,
        };
        let statistics = OperatorStatistics {
            source_scan_bytes: 0,
            inputs: vec![test_edge()],
            output: test_edge(),
            group_count: None,
            key_bytes: None,
            aggregate_value_bytes: None,
            k: None,
            hash_join_build_side: None,
        };
        StreamingPhysicalDagEvidence {
            nodes: vec![node],
            root: "retained-pass".into(),
            statistics: HashMap::from([("retained-pass".into(), statistics)]),
        }
    }

    fn bind_comparison(
        model: &mut StreamingAnalyticalCostModel,
        target: &Rc<QueryExpr>,
        root: &Rc<SummaryNode>,
    ) {
        model
            .bind_candidate_comparison(target, root, streaming_scope(), streaming_raw())
            .unwrap();
        fn retained(
            model: &mut StreamingAnalyticalCostModel,
            node: &Rc<SummaryNode>,
            seen: &mut HashSet<*const SummaryNode>,
        ) {
            if !seen.insert(Rc::as_ptr(node)) {
                return;
            }
            match &node.expr {
                SummaryExpr::KeepPreAsap(_) => {
                    model.node_evidence.insert_retained_query(
                        node,
                        StreamingRetainedQueryEvidence {
                            physical_id: format!("retained-{node:p}"),
                            preprocessing_cpu_ops_over_horizon: 1.0,
                            working_memory_bytes: 8,
                            output_buffer_bytes: 0,
                            physical_dag: zero_physical_dag(),
                        },
                    );
                }
                SummaryExpr::SummaryAgg { child, .. } => retained(model, child, seen),
                SummaryExpr::SummaryMerge { children } => {
                    for child in children {
                        retained(model, child, seen);
                    }
                }
                SummaryExpr::SummarySubtract { left, right }
                | SummaryExpr::SummaryJoin {
                    outer: left,
                    inner: right,
                    ..
                } => {
                    retained(model, left, seen);
                    retained(model, right, seen);
                }
                SummaryExpr::SummaryDelete { summary_input, .. }
                | SummaryExpr::SummaryEstimate { summary_input, .. } => {
                    retained(model, summary_input, seen)
                }
            }
        }
        retained(model, root, &mut HashSet::new());
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

    fn test_edge() -> EdgeStatistics {
        EdgeStatistics { rows: 1, bytes: 8 }
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
        target: &Rc<QueryExpr>,
        root: &Rc<SummaryNode>,
        inputs: StreamingSummaryInputs,
        cpu: SummaryOperationCpuEvidence,
    ) {
        bind_comparison(model, target, root);
        for node in evidence_nodes(root).0 {
            model.node_evidence.aggregations.insert(
                node as *const _,
                StreamingAggregateEvidence {
                    physical_id: format!("agg-{node:p}"),
                    input: test_edge(),
                    output: test_edge(),
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
                            physical_id: format!("merge-{node:p}"),
                            inputs: match &node.expr {
                                SummaryExpr::SummaryMerge { children } => {
                                    vec![test_edge(); children.len()]
                                }
                                _ => unreachable!(),
                            },
                            output: test_edge(),
                            cpu_ops,
                            working_memory_bytes: inputs.state_bytes_per_sketch,
                            output_buffer_bytes: 0,
                            executions_per_evaluation: 1,
                            events_per_second: None,
                            routing_fanout: None,
                            io_bytes_per_execution: Some(0),
                        })
                }
                SummaryExpr::SummarySubtract { .. } => {
                    cpu.subtract_cpu_ops
                        .map(|cpu_ops| StreamingSummaryOperatorEvidence {
                            physical_id: format!("subtract-{node:p}"),
                            inputs: vec![test_edge(), test_edge()],
                            output: test_edge(),
                            cpu_ops,
                            working_memory_bytes: inputs.state_bytes_per_sketch,
                            output_buffer_bytes: 0,
                            executions_per_evaluation: 1,
                            events_per_second: None,
                            routing_fanout: None,
                            io_bytes_per_execution: Some(0),
                        })
                }
                SummaryExpr::SummaryDelete { .. } => {
                    cpu.delete_cpu_ops
                        .map(|cpu_ops| StreamingSummaryOperatorEvidence {
                            physical_id: format!("delete-{node:p}"),
                            inputs: vec![test_edge()],
                            output: test_edge(),
                            cpu_ops,
                            working_memory_bytes: 0,
                            output_buffer_bytes: 0,
                            executions_per_evaluation: 1,
                            events_per_second: cpu.delete_events_per_second,
                            routing_fanout: cpu.delete_routing_fanout,
                            io_bytes_per_execution: Some(0),
                        })
                }
                SummaryExpr::SummaryEstimate { .. } => {
                    cpu.readout_cpu_ops
                        .map(|cpu_ops| StreamingSummaryOperatorEvidence {
                            physical_id: format!("readout-{node:p}"),
                            inputs: vec![test_edge()],
                            output: test_edge(),
                            cpu_ops,
                            working_memory_bytes: 0,
                            output_buffer_bytes: 0,
                            executions_per_evaluation: 1,
                            events_per_second: None,
                            routing_fanout: None,
                            io_bytes_per_execution: Some(0),
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
                    fn owning_aggs(
                        node: &SummaryNode,
                        seen: &mut HashSet<*const SummaryNode>,
                        owners: &mut Vec<*const SummaryNode>,
                    ) {
                        if !seen.insert(node as *const _) {
                            return;
                        }
                        match &node.expr {
                            SummaryExpr::SummaryAgg { child, .. } => {
                                owners.push(node as *const _);
                                owning_aggs(child, seen, owners);
                            }
                            SummaryExpr::SummaryMerge { children } => {
                                for child in children {
                                    owning_aggs(child, seen, owners);
                                }
                            }
                            SummaryExpr::SummarySubtract { left, right }
                            | SummaryExpr::SummaryJoin {
                                outer: left,
                                inner: right,
                                ..
                            } => {
                                owning_aggs(left, seen, owners);
                                owning_aggs(right, seen, owners);
                            }
                            SummaryExpr::SummaryDelete { summary_input, .. }
                            | SummaryExpr::SummaryEstimate { summary_input, .. } => {
                                owning_aggs(summary_input, seen, owners);
                            }
                            SummaryExpr::KeepPreAsap(_) => {}
                        }
                    }
                    let mut owners = Vec::new();
                    owning_aggs(summary_input, &mut HashSet::new(), &mut owners);
                    owners.sort_unstable();
                    owners.dedup();
                    if let [owner] = owners.as_slice() {
                        model
                            .node_evidence
                            .operation_state_owners
                            .insert(node as *const _, *owner);
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
