//! Analytical resource cost for incrementally maintained summary deployments.
//!
//! The canonical workload and lifecycle types own deployment semantics. This
//! module only adds physical evidence absent from those schemas: state size,
//! window counts, and per-operation CPU measurements or complexity estimates.

use std::collections::HashSet;

use asap_types::post_asap::{
    SummaryExpr, SummaryMaintenanceLifecycle, SummaryMaintenanceLifecycleGuarantee,
    SummaryMaintenanceMode, SummaryNode,
};
use asap_types::workload::{DataArrival, DataWorkload, QueryWorkloadEntry};
use serde::{Deserialize, Serialize};

use crate::analytical_cost::{AnalyticalCostError, ResourceEstimate};
use crate::analytical_statistics::evaluations_in_horizon;
use crate::summary_maintenance_lifecycle::{evaluation_schedule, maintenance_mode};

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
    /// Independent summary-state instances per window: one for shared
    /// multi-subpopulation state, otherwise the resolved group count.
    pub physical_summary_count: u64,
    /// Resident bytes of one concrete state instance.
    pub state_bytes_per_summary: u64,
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
    pub physical_summary_count: u64,
    pub state_bytes_per_summary: u64,
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
            physical_summary_count: physical.physical_summary_count,
            state_bytes_per_summary: physical.state_bytes_per_summary,
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
            ("physical_summary_count", self.physical_summary_count),
            ("state_bytes_per_summary", self.state_bytes_per_summary),
            ("evaluation_count", self.evaluation_count),
        ] {
            if value == 0 {
                return Err(AnalyticalCostError::MissingOrZero(name));
            }
        }
        let bootstrap_is_consistent = if self.initial_input_rows == 0 {
            self.initial_input_bytes == 0 && self.initial_source_scan_bytes == 0
        } else {
            self.initial_input_bytes > 0 && self.initial_source_scan_bytes > 0
        };
        if !bootstrap_is_consistent {
            return Err(AnalyticalCostError::InconsistentOperatorStatistics(
                "streaming bootstrap rows, logical bytes, and source bytes disagree",
            ));
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SummaryOperationCounts {
    state_builds: u64,
    merges_per_read: u64,
    subtracts_per_read: u64,
    deletes_per_update: u64,
    readouts_per_read: u64,
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
    let inputs = inputs.validate()?;
    validate_guarantee(guarantee, inputs.data_arrival)?;
    let arriving_input_rows = arriving_rows_for_lifecycle(inputs, guarantee)?;
    let counts = count_operations(root)?;
    if counts.state_builds != 1 {
        // One flat evidence record cannot safely describe several summary
        // nodes with different inputs, state sizes, or algorithms. Complete
        // multi-node streaming DAGs use per-node evidence instead.
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

    let build_inserts = inputs
        .initial_input_rows
        .checked_mul(inputs.active_window_count)
        .and_then(|n| n.checked_mul(counts.state_builds))
        .ok_or(AnalyticalCostError::Overflow)?;
    let update_inserts = arriving_input_rows
        .checked_mul(inputs.active_window_count)
        .and_then(|n| n.checked_mul(counts.state_builds))
        .ok_or(AnalyticalCostError::Overflow)?;
    let instances = inputs.physical_summary_count as f64;
    let evaluations = inputs.evaluation_count as f64;
    let cpu_ops = (build_inserts as f64 + update_inserts as f64) * insert
        + evaluations * counts.merges_per_read as f64 * instances * merge
        + evaluations * counts.subtracts_per_read as f64 * instances * subtract
        + update_inserts as f64 * counts.deletes_per_update as f64 * delete
        + evaluations * counts.readouts_per_read as f64 * instances * readout;
    if !cpu_ops.is_finite() {
        return Err(AnalyticalCostError::Overflow);
    }

    let state_instances = inputs
        .active_window_count
        .checked_add(inputs.retained_window_count)
        .and_then(|n| n.checked_mul(inputs.physical_summary_count))
        .and_then(|n| n.checked_mul(counts.state_builds))
        .ok_or(AnalyticalCostError::Overflow)?;
    let retained_bytes = state_instances
        .checked_mul(inputs.state_bytes_per_summary)
        .ok_or(AnalyticalCostError::Overflow)?;
    // Merge/subtract may stream over persistent inputs but still needs one
    // result state per physical instance. Persistent retained windows are
    // already included above and are not loaded a second time.
    let transient_bytes = if counts.merges_per_read > 0 || counts.subtracts_per_read > 0 {
        inputs
            .physical_summary_count
            .checked_mul(inputs.state_bytes_per_summary)
            .ok_or(AnalyticalCostError::Overflow)?
    } else {
        0
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
            let active_ms = end.saturating_sub(start);
            if active_ms == 0 {
                return Err(AnalyticalCostError::IncompatibleLifecycleGuarantee);
            }
            active_ms
        }
        SummaryMaintenanceLifecycle::Shared { .. } => inputs.horizon_ms,
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
    if !value.is_finite() || value <= 0.0 {
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
            SummaryExpr::SummaryJoin { .. } => {
                return Err(AnalyticalCostError::UnsupportedSummaryOperation("join"));
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
    use asap_types::pre_asap::{ColumnRef, QueryExpr, Reduction};
    use asap_types::workload::{
        DataWorkload, Evidence, EvidenceSource, Predictability, Query, QueryRecurrence,
        QueryRequirements, QueryTimeScope, Rate, RepeatedDemand, RepetitionInterval, TimeSelection,
    };

    use super::*;

    fn physical() -> StreamingPhysicalInputEvidence {
        StreamingPhysicalInputEvidence {
            initial_input_bytes: 640,
            initial_source_scan_bytes: 640,
            active_window_count: 2,
            retained_window_count: 3,
            physical_summary_count: 2,
            state_bytes_per_summary: 100,
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
                physical_summary_count: 2,
                state_bytes_per_summary: 100,
                evaluation_count: 5,
            },
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(2.0),
                readout_cpu_ops: Some(3.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .unwrap();
        // 10 bootstrap + 10 arrivals, each routed to two active windows;
        // two physical summary instances are read 5 times.
        assert_eq!(estimate.cpu_ops, 110.0);
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
                physical_summary_count: 2,
                state_bytes_per_summary: 10,
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
        assert_eq!(estimate.cpu_ops, 5.0 + 12.0 + 18.0 + 20.0 + 42.0);
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
                    physical_summary_count: 1,
                    state_bytes_per_summary: 8,
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
                    physical_summary_count: 1,
                    state_bytes_per_summary: 8,
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
                    physical_summary_count: 1,
                    state_bytes_per_summary: 8,
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
                physical_summary_count: 1,
                state_bytes_per_summary: 8,
                evaluation_count: 1,
            },
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(1.0),
                readout_cpu_ops: Some(1.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .unwrap();
        assert_eq!(estimate.cpu_ops, 13.0); // 10 bootstrap + 2 updates + one read.
    }

    #[test]
    fn shared_retention_is_not_confused_with_the_planning_horizon() {
        let guarantee = SummaryMaintenanceLifecycleGuarantee {
            summary_maintenance_lifecycle: SummaryMaintenanceLifecycle::Shared {
                retention: asap_types::workload::DurationMs(999),
            },
            summary_maintenance_mode: SummaryMaintenanceMode::Incremental,
            evaluation_schedule: EvaluationSchedule::PerUpdate,
            output_representation: OutputRepresentation::SummaryState,
        };
        assert!(estimate_incremental_summary_maintenance(
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
                physical_summary_count: 1,
                state_bytes_per_summary: 8,
                evaluation_count: 1,
            },
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(1.0),
                readout_cpu_ops: Some(1.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .is_ok());
    }

    #[test]
    fn an_empty_bootstrap_is_valid_for_a_new_stream() {
        let estimate = estimate_incremental_summary_maintenance(
            &summary_with_operations(false, false, false),
            &continuous_guarantee(),
            StreamingSummaryInputs {
                data_arrival: DataArrival::ContinuouslyIngesting,
                initial_input_rows: 0,
                initial_input_bytes: 0,
                initial_source_scan_bytes: 0,
                ingestion_rate_per_second: 1.0,
                planning_time_ms: 0,
                horizon_ms: 1_000,
                active_window_count: 1,
                retained_window_count: 0,
                physical_summary_count: 1,
                state_bytes_per_summary: 8,
                evaluation_count: 1,
            },
            SummaryOperationCpuEvidence {
                insert_cpu_ops: Some(1.0),
                readout_cpu_ops: Some(1.0),
                ..SummaryOperationCpuEvidence::default()
            },
        )
        .unwrap();

        assert_eq!(estimate.cpu_ops, 2.0);
        assert_eq!(estimate.peak_memory_bytes, 8);
        assert_eq!(estimate.scan_bytes, 0);
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
}
