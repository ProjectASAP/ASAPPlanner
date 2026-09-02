//! Analytical resource cost for incrementally maintained summary deployments.
//!
//! The canonical workload and lifecycle types own deployment semantics. This
//! module only adds physical evidence absent from those schemas: state size,
//! window counts, and per-operation CPU measurements or complexity estimates.

use std::collections::HashSet;

use asap_types::post_asap::{
    SketchAlgorithm, SummaryExpr, SummaryMaintenanceLifecycle,
    SummaryMaintenanceLifecycleGuarantee, SummaryMaintenanceMode, SummaryNode,
};
use asap_types::pre_asap::{agg_intent::AggIntent, QueryExpr};
use asap_types::workload::{DataArrival, DataWorkload, QueryWorkloadEntry};
use serde::{Deserialize, Serialize};

use crate::analytical_cost::{
    evaluations_in_horizon, AnalyticalCostError, ResourceCalibration, ResourceEstimate,
};
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
    pub data_arrival: DataArrival,
    pub initial_input_rows: u64,
    pub initial_input_bytes: u64,
    pub initial_source_scan_bytes: u64,
    pub ingestion_rate_per_second: f64,
    pub planning_time_ms: u64,
    pub horizon_ms: u64,
    pub active_window_count: u64,
    pub retained_window_count: u64,
    pub physical_sketch_count: u64,
    pub state_bytes_per_sketch: u64,
    pub evaluation_count: u64,
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
        query: &QueryWorkloadEntry,
        planning_time_ms: u64,
        horizon_ms: u64,
    ) -> Result<Self, AnalyticalCostError> {
        if data.arrival != DataArrival::ContinuouslyIngesting {
            return Err(AnalyticalCostError::UnsupportedDataArrival(data.arrival));
        }
        let initial_input_rows = data
            .input_cardinality
            .value_at(planning_time_ms)
            .copied()
            .ok_or(AnalyticalCostError::MissingOrStale("input_cardinality"))?;
        let ingestion_rate = data
            .ingestion_rate
            .value_at(planning_time_ms)
            .copied()
            .ok_or(AnalyticalCostError::MissingOrStale("ingestion_rate"))?;
        if !ingestion_rate.0.is_finite() || ingestion_rate.0 < 0.0 {
            return Err(AnalyticalCostError::InvalidIngestionRate(ingestion_rate.0));
        }
        if horizon_ms == 0 {
            return Err(AnalyticalCostError::MissingOrZero("horizon_ms"));
        }
        Self {
            data_arrival: data.arrival,
            initial_input_rows,
            initial_input_bytes: physical.initial_input_bytes,
            initial_source_scan_bytes: physical.initial_source_scan_bytes,
            ingestion_rate_per_second: ingestion_rate.0,
            planning_time_ms,
            horizon_ms,
            active_window_count: physical.active_window_count,
            retained_window_count: physical.retained_window_count,
            physical_sketch_count: physical.physical_sketch_count,
            state_bytes_per_sketch: physical.state_bytes_per_sketch,
            evaluation_count: evaluations_in_horizon(
                &query.recurrence,
                planning_time_ms,
                horizon_ms,
            )?,
        }
        .validate()
    }

    pub fn validate(self) -> Result<Self, AnalyticalCostError> {
        if self.data_arrival != DataArrival::ContinuouslyIngesting {
            return Err(AnalyticalCostError::UnsupportedDataArrival(
                self.data_arrival,
            ));
        }
        for (name, value) in [
            ("horizon_ms", self.horizon_ms),
            ("active_window_count", self.active_window_count),
            ("retained_window_count", self.retained_window_count),
            ("physical_sketch_count", self.physical_sketch_count),
            ("state_bytes_per_sketch", self.state_bytes_per_sketch),
            ("evaluation_count", self.evaluation_count),
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

/// Complete physical work for one raw evaluation. It is deliberately
/// per-evaluation so the same normalized query recurrence/horizon can multiply
/// both raw and summary alternatives.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StreamingRawInputEvidence {
    pub input_rows_per_evaluation: u64,
    pub input_bytes_per_evaluation: u64,
    pub source_scan_bytes_per_evaluation: u64,
    pub cpu_ops_per_row: f64,
    pub peak_memory_bytes: u64,
}

/// Adapter that supplies the existing lifecycle planner with analytical
/// streaming costs. It does not define lifecycle policy: the planner's
/// existing enums and legality checks remain authoritative.
#[derive(Debug, Clone)]
pub struct StreamingAnalyticalCostModel {
    pub summary_inputs: StreamingSummaryInputs,
    pub raw: StreamingRawInputEvidence,
    pub cpu: SummaryOperationCpuEvidence,
    pub join: Option<SummaryJoinEvidence>,
    pub calibration: ResourceCalibration,
    pub capabilities: SummaryMaintenanceCapabilities,
}

impl StreamingAnalyticalCostModel {
    fn calibrated(&self, estimate: ResourceEstimate) -> Option<Cost> {
        estimate.calibrated_cost(&self.calibration).ok().map(Cost)
    }

    fn lifecycle_inputs(&self) -> Option<SummaryMaintenanceLifecycleCostInputs> {
        let inputs = self.summary_inputs.validate().ok()?;
        let insert = required_cpu("insert_cpu_ops", self.cpu.insert_cpu_ops).ok()?;
        let readout = required_cpu("readout_cpu_ops", self.cpu.readout_cpu_ops).ok()?;
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
        let horizon_seconds = inputs.horizon_ms as f64 / 1_000.0;
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
        _summary: &SummaryNode,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        self.lifecycle_inputs().unwrap_or_default()
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
        guarantees: &[SummaryMaintenanceLifecycleGuarantee],
        _deployment_sum: Option<Cost>,
    ) -> Option<Cost> {
        let guarantee = guarantees.first()?;
        if !guarantees.iter().all(|candidate| candidate == guarantee) {
            return None;
        }
        self.calibrated(
            estimate_incremental_summary_maintenance_with_join(
                root,
                guarantee,
                self.summary_inputs,
                self.cpu,
                self.join,
            )
            .ok()?,
        )
    }

    fn raw_query_recompute_cost(&self, _target: &QueryExpr) -> Option<Cost> {
        self.calibrated(estimate_streaming_raw_recompute(self.raw, 1).ok()?)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SummaryOperationCounts {
    state_builds: u64,
    merges_per_read: u64,
    subtracts_per_read: u64,
    deletes_per_update: u64,
    readouts_per_read: u64,
    joins_per_read: u64,
}

pub fn estimate_streaming_raw_recompute(
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

/// Cost a selected incremental deployment without changing its lifecycle
/// decision. Shared `Rc` nodes are visited once, so shared state is built and
/// retained once. Summary merge/subtract/readout run per query evaluation;
/// summary delete runs per arriving update.
pub fn estimate_incremental_summary_maintenance(
    root: &SummaryNode,
    guarantee: &SummaryMaintenanceLifecycleGuarantee,
    inputs: StreamingSummaryInputs,
    cpu: SummaryOperationCpuEvidence,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    estimate_incremental_summary_maintenance_with_join(root, guarantee, inputs, cpu, None)
}

pub fn estimate_incremental_summary_maintenance_with_join(
    root: &SummaryNode,
    guarantee: &SummaryMaintenanceLifecycleGuarantee,
    inputs: StreamingSummaryInputs,
    cpu: SummaryOperationCpuEvidence,
    join: Option<SummaryJoinEvidence>,
) -> Result<ResourceEstimate, AnalyticalCostError> {
    let inputs = inputs.validate()?;
    validate_guarantee(guarantee, inputs.data_arrival)?;
    let arriving_input_rows = arriving_rows_for_lifecycle(inputs, guarantee)?;
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

    let build_inserts = inputs
        .initial_input_rows
        .checked_mul(counts.state_builds)
        .ok_or(AnalyticalCostError::Overflow)?;
    let update_inserts = arriving_input_rows
        .checked_mul(inputs.active_window_count)
        .and_then(|n| n.checked_mul(counts.state_builds))
        .ok_or(AnalyticalCostError::Overflow)?;
    let instances = inputs.physical_sketch_count as f64;
    let evaluations = inputs.evaluation_count as f64;
    let updates = arriving_input_rows as f64;
    let cpu_ops = (build_inserts as f64 + update_inserts as f64) * insert
        + evaluations * counts.merges_per_read as f64 * instances * merge
        + evaluations * counts.subtracts_per_read as f64 * instances * subtract
        + updates * counts.deletes_per_update as f64 * instances * delete
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

fn arriving_rows_for_lifecycle(
    inputs: StreamingSummaryInputs,
    guarantee: &SummaryMaintenanceLifecycleGuarantee,
) -> Result<u64, AnalyticalCostError> {
    let horizon_end = inputs
        .planning_time_ms
        .checked_add(inputs.horizon_ms)
        .ok_or(AnalyticalCostError::Overflow)?;
    let active_ms = match guarantee.summary_maintenance_lifecycle {
        SummaryMaintenanceLifecycle::Prepared {
            activate_at,
            retire_at,
        } => {
            if activate_at.0 >= retire_at.0 {
                return Err(AnalyticalCostError::IncompatibleLifecycleGuarantee);
            }
            let start = activate_at.0.max(inputs.planning_time_ms);
            let end = retire_at.0.min(horizon_end);
            end.saturating_sub(start)
        }
        SummaryMaintenanceLifecycle::Shared { retention } => {
            if retention.0 < inputs.horizon_ms {
                return Err(AnalyticalCostError::IncompatibleLifecycleGuarantee);
            }
            inputs.horizon_ms
        }
        SummaryMaintenanceLifecycle::ContinuouslyMaintained => inputs.horizon_ms,
        SummaryMaintenanceLifecycle::Ephemeral => {
            return Err(AnalyticalCostError::IncompatibleLifecycleGuarantee)
        }
    };
    let rows = inputs.ingestion_rate_per_second * active_ms as f64 / 1000.0;
    if !rows.is_finite() || rows > u64::MAX as f64 {
        return Err(AnalyticalCostError::Overflow);
    }
    Ok(rows.ceil() as u64)
}

fn validate_guarantee(
    guarantee: &SummaryMaintenanceLifecycleGuarantee,
    arrival: DataArrival,
) -> Result<(), AnalyticalCostError> {
    if guarantee.summary_maintenance_mode != SummaryMaintenanceMode::Incremental
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
        QueryRecurrence, QueryRequirements, QueryTimeScope, QueryWorkload, Rate, RepeatedDemand,
        RepeatingEntry, RepetitionInterval, TimeSelection,
    };

    use super::*;
    use crate::recurrence::Horizon;
    use crate::summary_maintenance_lifecycle::{
        global_selection_with_summary_maintenance_lifecycles,
        materialize_with_summary_maintenance_lifecycles, plan_summary_maintenance_lifecycles,
        SummaryMaintenanceLifecycleCapabilities, WorkloadDemand,
    };

    fn physical() -> StreamingPhysicalInputEvidence {
        StreamingPhysicalInputEvidence {
            initial_input_bytes: 640,
            initial_source_scan_bytes: 640,
            active_window_count: 2,
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

        let inputs =
            StreamingSummaryInputs::from_workload(physical(), &data, &query(), 100, 5_000).unwrap();
        assert_eq!(inputs.initial_input_rows, 10);
        assert_eq!(
            arriving_rows_for_lifecycle(inputs, &continuous_guarantee()).unwrap(),
            10
        );
        assert_eq!(inputs.evaluation_count, 5);
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
        let inputs =
            StreamingSummaryInputs::from_workload(empty, &data, &query(), 0, 5_000).unwrap();
        let estimate = estimate_incremental_summary_maintenance(
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
            data_arrival: DataArrival::ContinuouslyIngesting,
            initial_input_rows: 0,
            initial_input_bytes: 8,
            initial_source_scan_bytes: 0,
            ingestion_rate_per_second: 1.0,
            planning_time_ms: 0,
            horizon_ms: 1_000,
            active_window_count: 1,
            retained_window_count: 1,
            physical_sketch_count: 1,
            state_bytes_per_sketch: 8,
            evaluation_count: 1,
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
    fn existing_lifecycle_planner_selects_a_fully_costed_streaming_alternative() {
        let inputs = StreamingSummaryInputs {
            data_arrival: DataArrival::ContinuouslyIngesting,
            initial_input_rows: 10,
            initial_input_bytes: 640,
            initial_source_scan_bytes: 640,
            ingestion_rate_per_second: 2.0,
            planning_time_ms: 0,
            horizon_ms: 5_000,
            active_window_count: 2,
            retained_window_count: 3,
            physical_sketch_count: 2,
            state_bytes_per_sketch: 100,
            evaluation_count: 5,
        };
        let model = StreamingAnalyticalCostModel {
            summary_inputs: inputs,
            raw: StreamingRawInputEvidence {
                input_rows_per_evaluation: 20,
                input_bytes_per_evaluation: 1_280,
                source_scan_bytes_per_evaluation: 1_280,
                cpu_ops_per_row: 2.0,
                peak_memory_bytes: 320,
            },
            cpu: SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(2.0),
                readout_cpu_ops: Some(3.0),
                ..SummaryOperationCpuEvidence::default()
            },
            join: None,
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
        };
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
            Some(Cost(1_640.0))
        );
    }

    #[test]
    fn global_selection_compares_streaming_summary_and_raw_over_one_horizon() {
        let target = streaming_sum_query();
        let space = crate::replacement::search_workload(vec![("q", Rc::clone(&target))]);
        let workload = streaming_workload();
        let model = streaming_model();
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
        assert_eq!(plan.raw_recompute_total_cost, Some(Cost(8_200.0)));

        let mut raw_cheaper = model;
        raw_cheaper.raw = StreamingRawInputEvidence {
            input_rows_per_evaluation: 1,
            input_bytes_per_evaluation: 1,
            source_scan_bytes_per_evaluation: 0,
            cpu_ops_per_row: 0.0,
            peak_memory_bytes: 1,
        };
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
        assert_eq!(cheap_plan.raw_recompute_total_cost, Some(Cost(5.0)));
    }

    #[test]
    fn lifecycle_plan_does_not_fall_back_to_partial_agg_cost_for_a_join_root() {
        let workload = streaming_workload();
        let model = streaming_model();
        let plan = plan_summary_maintenance_lifecycles(
            summary_join(),
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
        costed.join = Some(SummaryJoinEvidence {
            matched_state_pairs_per_evaluation: 2,
            cpu_ops_per_matched_pair: 3.0,
            working_memory_bytes: 64,
        });
        let costed_plan = plan_summary_maintenance_lifecycles(
            summary_join(),
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
    fn mixed_arrival_fails_closed_until_backlog_and_stream_are_separate() {
        let data = DataWorkload {
            arrival: DataArrival::Mixed,
            ..DataWorkload::default()
        };
        assert_eq!(
            StreamingSummaryInputs::from_workload(physical(), &data, &query(), 0, 1_000),
            Err(AnalyticalCostError::UnsupportedDataArrival(
                DataArrival::Mixed
            ))
        );
    }

    #[test]
    fn direct_read_costs_build_updates_windows_and_recurrence() {
        let estimate = estimate_incremental_summary_maintenance(
            &summary_with_operations(false, false, false),
            &continuous_guarantee(),
            StreamingSummaryInputs {
                data_arrival: DataArrival::ContinuouslyIngesting,
                initial_input_rows: 10,
                initial_input_bytes: 640,
                initial_source_scan_bytes: 640,
                ingestion_rate_per_second: 2.0,
                planning_time_ms: 0,
                horizon_ms: 5_000,
                active_window_count: 2,
                retained_window_count: 3,
                physical_sketch_count: 2,
                state_bytes_per_sketch: 100,
                evaluation_count: 5,
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
        let estimate = estimate_incremental_summary_maintenance(
            &summary_with_operations(true, true, true),
            &continuous_guarantee(),
            StreamingSummaryInputs {
                data_arrival: DataArrival::ContinuouslyIngesting,
                initial_input_rows: 1,
                initial_input_bytes: 8,
                initial_source_scan_bytes: 8,
                ingestion_rate_per_second: 4.0,
                planning_time_ms: 0,
                horizon_ms: 1_000,
                active_window_count: 1,
                retained_window_count: 2,
                physical_sketch_count: 2,
                state_bytes_per_sketch: 10,
                evaluation_count: 3,
            },
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(1.0),
                merge_cpu_ops: Some(2.0),
                subtract_cpu_ops: Some(3.0),
                delete_cpu_ops: Some(5.0),
                readout_cpu_ops: Some(7.0),
            },
        )
        .unwrap();
        assert_eq!(estimate.cpu_ops, 5.0 + 40.0 + 12.0 + 18.0 + 42.0);
        // Three persistent windows plus one transient result, for two instances.
        assert_eq!(estimate.peak_memory_bytes, 80);
    }

    #[test]
    fn lifecycle_mode_and_schedule_must_match_existing_planner_semantics() {
        let mut guarantee = continuous_guarantee();
        guarantee.evaluation_schedule = EvaluationSchedule::OnRead;
        assert_eq!(
            estimate_incremental_summary_maintenance(
                &summary_with_operations(false, false, false),
                &guarantee,
                StreamingSummaryInputs {
                    data_arrival: DataArrival::ContinuouslyIngesting,
                    initial_input_rows: 1,
                    initial_input_bytes: 8,
                    initial_source_scan_bytes: 8,
                    ingestion_rate_per_second: 1.0,
                    planning_time_ms: 0,
                    horizon_ms: 1_000,
                    active_window_count: 1,
                    retained_window_count: 1,
                    physical_sketch_count: 1,
                    state_bytes_per_sketch: 8,
                    evaluation_count: 1,
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
            estimate_incremental_summary_maintenance(
                &summary_with_operations(true, false, false),
                &continuous_guarantee(),
                StreamingSummaryInputs {
                    data_arrival: DataArrival::ContinuouslyIngesting,
                    initial_input_rows: 1,
                    initial_input_bytes: 8,
                    initial_source_scan_bytes: 8,
                    ingestion_rate_per_second: 1.0,
                    planning_time_ms: 0,
                    horizon_ms: 1_000,
                    active_window_count: 1,
                    retained_window_count: 1,
                    physical_sketch_count: 1,
                    state_bytes_per_sketch: 8,
                    evaluation_count: 1,
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
            estimate_incremental_summary_maintenance(
                &summary_with_operations(false, false, false),
                &guarantee,
                StreamingSummaryInputs {
                    data_arrival: DataArrival::ContinuouslyIngesting,
                    initial_input_rows: 1,
                    initial_input_bytes: 8,
                    initial_source_scan_bytes: 8,
                    ingestion_rate_per_second: 1.0,
                    planning_time_ms: 0,
                    horizon_ms: 1_000,
                    active_window_count: 1,
                    retained_window_count: 1,
                    physical_sketch_count: 1,
                    state_bytes_per_sketch: 8,
                    evaluation_count: 1,
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
        let estimate = estimate_incremental_summary_maintenance(
            &summary_with_operations(false, false, false),
            &guarantee,
            StreamingSummaryInputs {
                data_arrival: DataArrival::ContinuouslyIngesting,
                initial_input_rows: 10,
                initial_input_bytes: 80,
                initial_source_scan_bytes: 80,
                ingestion_rate_per_second: 2.0,
                planning_time_ms: 0,
                horizon_ms: 5_000,
                active_window_count: 1,
                retained_window_count: 1,
                physical_sketch_count: 1,
                state_bytes_per_sketch: 8,
                evaluation_count: 1,
            },
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(1.0),
                readout_cpu_ops: Some(0.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .unwrap();
        assert_eq!(estimate.cpu_ops, 12.0); // 10 bootstrap + 2 updates in [1s, 2s].
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
            estimate_incremental_summary_maintenance(
                &summary_with_operations(false, false, false),
                &guarantee,
                StreamingSummaryInputs {
                    data_arrival: DataArrival::ContinuouslyIngesting,
                    initial_input_rows: 1,
                    initial_input_bytes: 8,
                    initial_source_scan_bytes: 8,
                    ingestion_rate_per_second: 1.0,
                    planning_time_ms: 0,
                    horizon_ms: 1_000,
                    active_window_count: 1,
                    retained_window_count: 1,
                    physical_sketch_count: 1,
                    state_bytes_per_sketch: 8,
                    evaluation_count: 1,
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
            data_arrival: DataArrival::ContinuouslyIngesting,
            initial_input_rows: 1,
            initial_input_bytes: 8,
            initial_source_scan_bytes: 8,
            ingestion_rate_per_second: 1.0,
            planning_time_ms: 0,
            horizon_ms: 1_000,
            active_window_count: 1,
            retained_window_count: 1,
            physical_sketch_count: 1,
            state_bytes_per_sketch: 8,
            evaluation_count: 2,
        };
        let cpu = SummaryOperationCpuEvidence {
            insert_cpu_ops: Some(1.0),
            readout_cpu_ops: Some(1.0),
            ..SummaryOperationCpuEvidence::default()
        };
        assert_eq!(
            estimate_incremental_summary_maintenance_with_join(
                &joined,
                &continuous_guarantee(),
                inputs,
                cpu,
                None,
            ),
            Err(AnalyticalCostError::MissingOrStale("summary_join"))
        );
        let estimate = estimate_incremental_summary_maintenance_with_join(
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
        assert_eq!(estimate.cpu_ops, 30.0); // 2 inserts + 4 readouts + 24 join ops.
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
            summary_inputs: StreamingSummaryInputs {
                data_arrival: DataArrival::ContinuouslyIngesting,
                initial_input_rows: 10,
                initial_input_bytes: 640,
                initial_source_scan_bytes: 640,
                ingestion_rate_per_second: 2.0,
                planning_time_ms: 0,
                horizon_ms: 5_000,
                active_window_count: 2,
                retained_window_count: 3,
                physical_sketch_count: 2,
                state_bytes_per_sketch: 100,
                evaluation_count: 5,
            },
            raw: StreamingRawInputEvidence {
                input_rows_per_evaluation: 20,
                input_bytes_per_evaluation: 1_280,
                source_scan_bytes_per_evaluation: 1_280,
                cpu_ops_per_row: 2.0,
                peak_memory_bytes: 320,
            },
            cpu: SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(2.0),
                readout_cpu_ops: Some(3.0),
                ..SummaryOperationCpuEvidence::default()
            },
            join: None,
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
}
