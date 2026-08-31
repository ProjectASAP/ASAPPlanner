//! Workload-aware physical summary-maintenance lifecycle planning.
//!
//! A **summary-maintenance lifecycle** is the physical policy for when one
//! materialized summary state is created, retained or shared, updated as data
//! arrives, and retired. It is deliberately narrower than the end-to-end data
//! lifecycle and independent of query recurrence. Recurrence says when and how
//! often queries will read the result. The planner converts that demand into
//! expected reads and an evaluation rate, then uses those quantities to compare
//! rebuilding per query with retaining or continuously maintaining state.
//! Recurrence does not itself prescribe a state-maintenance policy.
//!
//! This module enumerates and costs `Ephemeral`, `Prepared`, `Shared`, and
//! `ContinuouslyMaintained` alternatives for every unique `SummaryAgg` in a
//! materialized plan. [`SummaryMaintenanceMode`] is an orthogonal detail of
//! the selected deployment: state is either built directly or updated
//! incrementally. Unknown evidence stays unknown and therefore cannot make a
//! long-lived alternative win.

use std::collections::HashSet;
use std::rc::Rc;

use asap_types::post_asap::{EvaluationSchedule, OutputRepresentation, SummaryMaintenanceMode};
use asap_types::post_asap::{SummaryExpr, SummaryMaintenanceLifecycle, SummaryNode};
use asap_types::workload::{
    DataArrival, Predictability, QueryRecurrence, QueryWorkload, RepeatedDemand, TimestampMs,
    WorkloadError,
};

use crate::cost_model::{Cost, CostModel};
use crate::recurrence::{CostRate, EvaluationRate, Horizon, UpdateRate};

/// Summary-maintenance lifecycle shapes supported by the target runtime.
///
/// These flags describe runtime implementation capabilities, not which
/// alternative the planner prefers. A supported alternative may still be
/// rejected because workload evidence is missing or its cost is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryMaintenanceLifecycleCapabilities {
    /// The runtime can build a fresh state for each invocation and retire it
    /// after that invocation finishes.
    pub ephemeral: bool,
    /// The runtime can build state before a predictable execution and retain
    /// it until that scheduled execution window ends.
    pub prepared: bool,
    /// The runtime can retain one state and reuse it across multiple reads.
    pub shared: bool,
    /// The runtime can keep state current by applying arriving data updates.
    pub continuously_maintained: bool,
}

/// Capabilities of one concrete summary family/state representation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SummaryMaintenanceCapabilities {
    pub incremental_update: bool,
    pub merge: bool,
    pub delete: bool,
}

impl SummaryMaintenanceLifecycleCapabilities {
    pub const ALL: Self = Self {
        ephemeral: true,
        prepared: true,
        shared: true,
        continuously_maintained: true,
    };
}

impl Default for SummaryMaintenanceLifecycleCapabilities {
    fn default() -> Self {
        Self::ALL
    }
}

/// Primitive costs for one concrete summary state. Every field is optional:
/// missing statistics produce an uncosted alternative, never a zero.
///
/// The lifecycle planner combines these state-specific inputs with workload
/// rates, invocation counts, and the optimization horizon. All `Cost` fields
/// are one-time costs unless their name explicitly says otherwise.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SummaryMaintenanceLifecycleCostInputs {
    /// One-time cost to construct the state from its input.
    pub build_cost: Option<Cost>,
    /// Cost to incorporate one arriving input update into existing state.
    pub maintenance_cost_per_update: Option<Cost>,
    /// Cost of one read or finalization from already-built summary state.
    pub summary_read_cost: Option<Cost>,
    /// Cost per second for retaining the state over a lifecycle window.
    pub retention_cost_rate: Option<CostRate>,
    /// One-time cost to release or retire the state.
    pub retirement_cost: Option<Cost>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryMaintenanceLifecycleRejection {
    UnsupportedByRuntime,
    RequiresPredictableOneTimeQuery,
    RequiresMultipleReads,
    RequiresHorizon,
    RequiresContinuousData,
    MissingOrStaleIngestionRate,
    SummaryDoesNotSupportIncrementalUpdates,
    SummaryDoesNotSupportDeletion,
    MissingCostEvidence,
}

/// One candidate physical policy for a particular summary deployment.
///
/// `total_cost: None` never means zero: it means the planner lacks enough
/// evidence to cost the candidate. Such a candidate is not selectable and its
/// `rejection` explains why.
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryMaintenanceLifecycleAlternative {
    /// State creation, retention, sharing, update, and retirement policy.
    pub summary_maintenance_lifecycle: SummaryMaintenanceLifecycle,
    /// How this lifecycle obtains and refreshes its summary state.
    pub maintenance_mode: SummaryMaintenanceMode,
    /// Complete cost over the requested horizon, when every input is known.
    pub total_cost: Option<Cost>,
    /// Why this alternative cannot be selected; `None` means it is legal and
    /// fully costed.
    pub rejection: Option<SummaryMaintenanceLifecycleRejection>,
    /// Human-readable premises used when deriving and costing the alternative.
    pub assumptions: Vec<String>,
}

impl SummaryMaintenanceLifecycleAlternative {
    fn selectable(&self) -> bool {
        self.rejection.is_none() && self.total_cost.is_some()
    }
}

/// One unique summary-state deployment. Shared `Rc` nodes are emitted once.
#[derive(Debug, Clone)]
pub struct SummaryMaintenanceDeployment {
    /// Traversal-local ordinal used to associate this deployment with exports;
    /// it is not a persistent identity across independently planned DAGs.
    pub summary_index: usize,
    /// The unique materialized `SummaryAgg` represented by this deployment.
    pub summary: Rc<SummaryNode>,
    /// Cheapest legal, fully costed lifecycle, or `None` when none is
    /// selectable.
    pub selected_summary_maintenance_lifecycle: Option<SummaryMaintenanceLifecycle>,
    /// Maintenance mechanism paired with the selected lifecycle. It is `None`
    /// exactly when no lifecycle was selected.
    pub selected_maintenance_mode: Option<SummaryMaintenanceMode>,
    /// When the selected deployment is evaluated. It is `None` when no
    /// lifecycle was selected.
    pub evaluation_schedule: Option<EvaluationSchedule>,
    /// Physical value exposed by this deployment to its downstream consumer.
    pub output_representation: OutputRepresentation,
    /// Every lifecycle shape considered, including rejected and uncosted ones.
    pub alternatives: Vec<SummaryMaintenanceLifecycleAlternative>,
}

/// Workload-aware physical deployment decisions for every unique summary
/// state reachable from one materialized post-ASAP root.
#[derive(Debug, Clone)]
pub struct SummaryMaintenanceLifecyclePlan {
    /// Root of the materialized post-ASAP DAG being deployed.
    pub root: Rc<SummaryNode>,
    /// One entry per unique reachable `SummaryAgg`; shared `Rc` nodes appear
    /// only once.
    pub deployments: Vec<SummaryMaintenanceDeployment>,
    /// Caller-supplied optimization horizon used to turn rates into total
    /// costs. `None` keeps horizon-dependent alternatives unselectable.
    pub horizon: Option<Horizon>,
    /// Aggregate recurring query-evaluation rate derived from the workload.
    pub evaluation_rate: Option<EvaluationRate>,
    /// Fresh source-data ingestion rate, when supplied by the workload.
    pub update_rate: Option<UpdateRate>,
    pub expected_reads: Option<f64>,
    pub selected_raw_recompute: bool,
    pub summary_total_cost: Option<Cost>,
    pub raw_recompute_total_cost: Option<Cost>,
}

/// Explicit association between a materialized target and the normalized
/// workload entries whose demand consumes it.
#[derive(Debug, Clone, Copy)]
pub struct WorkloadDemand<'a> {
    pub workload: &'a QueryWorkload,
    pub entry_indices: &'a [usize],
}

impl<'a> WorkloadDemand<'a> {
    pub const fn new(workload: &'a QueryWorkload, entry_indices: &'a [usize]) -> Self {
        Self {
            workload,
            entry_indices,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SummaryMaintenanceLifecyclePlanError {
    #[error(transparent)]
    InvalidWorkload(#[from] WorkloadError),
    #[error("optimization horizon must be finite and strictly positive")]
    InvalidHorizon,
    #[error("workload entry index {index} is out of bounds for {entry_count} entries")]
    InvalidWorkloadEntry { index: usize, entry_count: usize },
    #[error("a workload-demand binding must contain at least one entry")]
    EmptyWorkloadDemand,
    #[error("workload entry index {index} appears more than once in one demand binding")]
    DuplicateWorkloadEntry { index: usize },
}

/// Workload-wide evidence derived specifically for summary-maintenance
/// lifecycle enumeration and costing.
///
/// This is not another workload input model. [`QueryWorkload`] and its
/// normalized entries remain the source of truth. Unlike one
/// [`asap_types::workload::QueryWorkloadEntry`], these values aggregate all
/// entries at a particular planning time and optional horizon. It also cannot
/// reuse [`crate::recurrence::RecurrenceProfile`], which describes recurrence
/// for one candidate target and counts consumers rather than invocations.
#[derive(Debug)]
struct SummaryMaintenanceWorkloadFacts {
    /// Total one-time and recurring reads inside the horizon. `None` means a
    /// recurrence or horizon was unknown, not zero reads.
    reads: Option<f64>,
    /// Sum of declared invocations across all one-time workload entries.
    one_time_invocations: u64,
    /// Sum of usable recurring query rates in evaluations per second.
    evaluation_rate: Option<EvaluationRate>,
    /// Fresh workload-level ingestion rate in updates per second.
    update_rate: Option<UpdateRate>,
    /// Whether the workload's source data is static, arriving, mixed, or
    /// unknown.
    arrival: DataArrival,
    /// Earliest known activation and latest scheduled execution across
    /// predictable one-time entries. `None` means no valid preparation window.
    prepared_window: Option<(TimestampMs, TimestampMs)>,
    /// Whether every bound consumer is a predictable one-time query suitable
    /// for prepared state.
    prepared_eligible: bool,
    /// Whether maintaining the selected moving time scope requires deleting
    /// expired input from summary state.
    requires_deletion: bool,
}

/// Validate a materialized plan, enumerate lifecycle alternatives for each
/// unique summary state, and select the cheapest legal alternative whose cost
/// is fully known.
pub fn plan_summary_maintenance_lifecycles(
    root: Rc<SummaryNode>,
    demand: WorkloadDemand<'_>,
    now_ms: u64,
    horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    cost_model: &dyn CostModel,
) -> Result<SummaryMaintenanceLifecyclePlan, SummaryMaintenanceLifecyclePlanError> {
    demand.workload.validate()?;
    if horizon.is_some_and(|h| !h.0.is_finite() || h.0 <= 0.0) {
        return Err(SummaryMaintenanceLifecyclePlanError::InvalidHorizon);
    }
    let facts = workload_facts(demand.workload, demand.entry_indices, now_ms, horizon)?;
    let mut summaries = Vec::new();
    collect_summary_aggs(&root, &mut HashSet::new(), &mut summaries);
    let deployments: Vec<SummaryMaintenanceDeployment> = summaries
        .into_iter()
        .enumerate()
        .map(|(summary_index, summary)| {
            let alternatives = alternatives_for(
                &facts,
                horizon,
                capabilities,
                cost_model.summary_maintenance_capabilities(&summary),
                cost_model.summary_maintenance_lifecycle_cost_inputs(&summary),
            );
            let selected = alternatives
                .iter()
                .filter(|candidate| candidate.selectable())
                .min_by(|a, b| a.total_cost.unwrap().0.total_cmp(&b.total_cost.unwrap().0))
                .map(|candidate| {
                    (
                        candidate.summary_maintenance_lifecycle.clone(),
                        candidate.maintenance_mode,
                    )
                });
            let evaluation_schedule = selected.as_ref().map(|(lifecycle, _)| match lifecycle {
                SummaryMaintenanceLifecycle::Ephemeral => EvaluationSchedule::OneShot,
                SummaryMaintenanceLifecycle::Prepared { .. }
                | SummaryMaintenanceLifecycle::Shared { .. }
                    if matches!(
                        facts.arrival,
                        DataArrival::ContinuouslyIngesting | DataArrival::Mixed
                    ) =>
                {
                    EvaluationSchedule::PerUpdate
                }
                SummaryMaintenanceLifecycle::Prepared { .. } => EvaluationSchedule::OneShot,
                SummaryMaintenanceLifecycle::Shared { .. } => EvaluationSchedule::OnRead,
                SummaryMaintenanceLifecycle::ContinuouslyMaintained => {
                    EvaluationSchedule::PerUpdate
                }
            });
            SummaryMaintenanceDeployment {
                summary_index,
                summary,
                selected_summary_maintenance_lifecycle: selected
                    .as_ref()
                    .map(|(lifecycle, _)| lifecycle.clone()),
                selected_maintenance_mode: selected.map(|(_, mode)| mode),
                evaluation_schedule,
                output_representation: OutputRepresentation::SummaryState,
                alternatives,
            }
        })
        .collect();
    let summary_total_cost = deployments.iter().try_fold(Cost::ZERO, |sum, deployment| {
        let selected = deployment.selected_summary_maintenance_lifecycle.as_ref()?;
        let cost = deployment
            .alternatives
            .iter()
            .find(|alternative| &alternative.summary_maintenance_lifecycle == selected)?
            .total_cost?;
        Some(Cost(sum.0 + cost.0))
    });
    Ok(SummaryMaintenanceLifecyclePlan {
        root,
        deployments,
        horizon,
        evaluation_rate: facts.evaluation_rate,
        update_rate: facts.update_rate,
        expected_reads: facts.reads,
        selected_raw_recompute: false,
        summary_total_cost,
        raw_recompute_total_cost: None,
    })
}

fn workload_facts(
    workload: &QueryWorkload,
    workload_entry_indices: &[usize],
    now_ms: u64,
    horizon: Option<Horizon>,
) -> Result<SummaryMaintenanceWorkloadFacts, SummaryMaintenanceLifecyclePlanError> {
    let mut one_time_invocations = 0u64;
    let mut recurring_reads = 0.0;
    let mut recurring_known = true;
    let mut evaluation_rate = 0.0;
    let mut has_evaluation_rate = false;
    let mut prepared_start: Option<TimestampMs> = None;
    let mut prepared_end: Option<TimestampMs> = None;
    let mut prepared_eligible = true;
    let mut requires_deletion = false;

    let entries: Vec<_> = workload.entries().collect();
    if workload_entry_indices.is_empty() {
        return Err(SummaryMaintenanceLifecyclePlanError::EmptyWorkloadDemand);
    }
    let mut seen_indices = HashSet::new();
    for &index in workload_entry_indices {
        if !seen_indices.insert(index) {
            return Err(SummaryMaintenanceLifecyclePlanError::DuplicateWorkloadEntry { index });
        }
        let entry = entries.get(index).ok_or(
            SummaryMaintenanceLifecyclePlanError::InvalidWorkloadEntry {
                index,
                entry_count: entries.len(),
            },
        )?;
        requires_deletion |= entry.time_selection.lookback.is_some()
            && entry.time_selection.as_of.is_none()
            && matches!(
                entry.time_selection.scope,
                asap_types::workload::QueryTimeScope::RealTime
                    | asap_types::workload::QueryTimeScope::Mixed
            );
        match &entry.recurrence {
            QueryRecurrence::OneTime {
                invocations,
                execute_at,
            } => {
                one_time_invocations = one_time_invocations.saturating_add(*invocations);
                let covered = if let (
                    Predictability::Predictable {
                        known_at: Some(known),
                    },
                    Some(execute),
                ) = (&entry.predictability, execute_at)
                {
                    if known < execute && now_ms < execute.0 {
                        let activate = TimestampMs(known.0.max(now_ms));
                        prepared_start =
                            Some(prepared_start.map_or(activate, |old| old.min(activate)));
                        prepared_end = Some(prepared_end.map_or(*execute, |old| old.max(*execute)));
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                prepared_eligible &= covered;
            }
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(interval)) => {
                prepared_eligible = false;
                let rate = 1000.0 / f64::from(interval.0);
                evaluation_rate += rate;
                has_evaluation_rate = true;
                if let Some(h) = horizon {
                    recurring_reads += h.0 * rate;
                } else {
                    recurring_known = false;
                }
            }
            QueryRecurrence::Repeated(RepeatedDemand::Scheduled(schedule)) => {
                prepared_eligible = false;
                if let Some(h) = horizon {
                    let end_ms = now_ms.saturating_add((h.0 * 1000.0) as u64);
                    let reads_in_horizon = schedule
                        .iter()
                        .filter(|at| at.0 >= now_ms && at.0 <= end_ms)
                        .count() as f64;
                    recurring_reads += reads_in_horizon;
                    evaluation_rate += reads_in_horizon / h.0;
                    has_evaluation_rate = true;
                } else {
                    recurring_known = false;
                }
            }
            QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(estimate)) => {
                prepared_eligible = false;
                if !estimate.is_fresh_at(now_ms) {
                    recurring_known = false;
                    continue;
                }
                let rate = match estimate.expected {
                    asap_types::workload::ExpectedDemand::AverageRate(rate) => Some(rate.0),
                    asap_types::workload::ExpectedDemand::InvocationCount(count) => {
                        let millis = estimate
                            .observation_window
                            .end
                            .0
                            .saturating_sub(estimate.observation_window.start.0);
                        (millis > 0).then_some(count as f64 / (millis as f64 / 1000.0))
                    }
                };
                if let Some(rate) = rate {
                    evaluation_rate += rate;
                    has_evaluation_rate = true;
                    if let Some(h) = horizon {
                        recurring_reads += h.0 * rate;
                    } else {
                        recurring_known = false;
                    }
                } else {
                    recurring_known = false;
                }
            }
            QueryRecurrence::Unknown => {
                prepared_eligible = false;
                recurring_known = false;
            }
        }
    }

    let data = workload.data_workload.as_ref();
    let arrival = data.map_or(DataArrival::Unknown, |data| data.arrival);
    let update_rate = data
        .and_then(|data| data.ingestion_rate.value_at(now_ms))
        .map(|rate| UpdateRate(rate.0));
    let reads = recurring_known.then_some(one_time_invocations as f64 + recurring_reads);
    Ok(SummaryMaintenanceWorkloadFacts {
        reads,
        one_time_invocations,
        evaluation_rate: has_evaluation_rate.then_some(EvaluationRate(evaluation_rate)),
        update_rate,
        arrival,
        prepared_window: prepared_start.zip(prepared_end),
        prepared_eligible,
        requires_deletion,
    })
}

fn alternatives_for(
    facts: &SummaryMaintenanceWorkloadFacts,
    horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    summary_capabilities: SummaryMaintenanceCapabilities,
    costs: SummaryMaintenanceLifecycleCostInputs,
) -> Vec<SummaryMaintenanceLifecycleAlternative> {
    let alternatives = vec![
        ephemeral(facts, capabilities, &costs),
        prepared(facts, capabilities, summary_capabilities, &costs),
        shared(facts, horizon, capabilities, summary_capabilities, &costs),
        continuous(facts, horizon, capabilities, summary_capabilities, &costs),
    ];
    alternatives
}

fn ephemeral(
    facts: &SummaryMaintenanceWorkloadFacts,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    costs: &SummaryMaintenanceLifecycleCostInputs,
) -> SummaryMaintenanceLifecycleAlternative {
    let lifecycle = SummaryMaintenanceLifecycle::Ephemeral;
    if !capabilities.ephemeral {
        return rejected(
            lifecycle,
            SummaryMaintenanceMode::DirectBuild,
            SummaryMaintenanceLifecycleRejection::UnsupportedByRuntime,
        );
    }
    let total_cost = zip_costs(&[
        costs.build_cost,
        costs.summary_read_cost,
        costs.retirement_cost,
    ])
    .zip(facts.reads)
    .map(|(per_read, reads)| Cost(per_read * reads));
    costed_or_unknown(
        lifecycle,
        SummaryMaintenanceMode::DirectBuild,
        total_cost,
        vec!["state is rebuilt per invocation".into()],
    )
}

fn prepared(
    facts: &SummaryMaintenanceWorkloadFacts,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    summary_capabilities: SummaryMaintenanceCapabilities,
    costs: &SummaryMaintenanceLifecycleCostInputs,
) -> SummaryMaintenanceLifecycleAlternative {
    if !facts.prepared_eligible {
        return rejected(
            SummaryMaintenanceLifecycle::Prepared {
                activate_at: TimestampMs(0),
                retire_at: TimestampMs(0),
            },
            retained_mode(facts),
            SummaryMaintenanceLifecycleRejection::RequiresPredictableOneTimeQuery,
        );
    }
    let Some((activate_at, retire_at)) = facts.prepared_window else {
        return rejected(
            SummaryMaintenanceLifecycle::Prepared {
                activate_at: TimestampMs(0),
                retire_at: TimestampMs(0),
            },
            retained_mode(facts),
            SummaryMaintenanceLifecycleRejection::RequiresPredictableOneTimeQuery,
        );
    };
    let lifecycle = SummaryMaintenanceLifecycle::Prepared {
        activate_at,
        retire_at,
    };
    if !capabilities.prepared {
        return rejected(
            lifecycle,
            retained_mode(facts),
            SummaryMaintenanceLifecycleRejection::UnsupportedByRuntime,
        );
    }
    if let Some(rejection) = maintenance_capability_rejection(facts, summary_capabilities) {
        return rejected(lifecycle, retained_mode(facts), rejection);
    }
    let seconds = retire_at.0.saturating_sub(activate_at.0) as f64 / 1000.0;
    let maintenance = maintenance_cost(facts, costs, seconds);
    let total_cost = match (
        costs.build_cost,
        costs.summary_read_cost,
        costs.retention_cost_rate,
        costs.retirement_cost,
        maintenance,
    ) {
        (Some(build), Some(read), Some(retention), Some(retire), Some(maintenance)) => Some(Cost(
            build.0
                + read.0 * facts.one_time_invocations as f64
                + retention.0 * seconds
                + retire.0
                + maintenance,
        )),
        _ => None,
    };
    costed_or_unknown(
        lifecycle,
        retained_mode(facts),
        total_cost,
        vec!["activation and retirement come from the declared schedule".into()],
    )
}

fn shared(
    facts: &SummaryMaintenanceWorkloadFacts,
    horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    summary_capabilities: SummaryMaintenanceCapabilities,
    costs: &SummaryMaintenanceLifecycleCostInputs,
) -> SummaryMaintenanceLifecycleAlternative {
    let lifecycle = SummaryMaintenanceLifecycle::Shared {
        retention: asap_types::workload::DurationMs(horizon.map_or(0, |h| (h.0 * 1000.0) as u64)),
    };
    if !capabilities.shared {
        return rejected(
            lifecycle,
            retained_mode(facts),
            SummaryMaintenanceLifecycleRejection::UnsupportedByRuntime,
        );
    }
    if let Some(rejection) = maintenance_capability_rejection(facts, summary_capabilities) {
        return rejected(lifecycle, retained_mode(facts), rejection);
    }
    if facts.reads.is_none_or(|reads| reads <= 1.0) {
        return rejected(
            lifecycle,
            retained_mode(facts),
            SummaryMaintenanceLifecycleRejection::RequiresMultipleReads,
        );
    }
    let Some(horizon) = horizon else {
        return rejected(
            lifecycle,
            retained_mode(facts),
            SummaryMaintenanceLifecycleRejection::RequiresHorizon,
        );
    };
    let total_cost = retained_cost(facts, costs, horizon.0);
    costed_or_unknown(
        lifecycle,
        retained_mode(facts),
        total_cost,
        vec!["one state is shared across reads".into()],
    )
}

fn continuous(
    facts: &SummaryMaintenanceWorkloadFacts,
    horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    summary_capabilities: SummaryMaintenanceCapabilities,
    costs: &SummaryMaintenanceLifecycleCostInputs,
) -> SummaryMaintenanceLifecycleAlternative {
    let lifecycle = SummaryMaintenanceLifecycle::ContinuouslyMaintained;
    if !capabilities.continuously_maintained {
        return rejected(
            lifecycle,
            SummaryMaintenanceMode::Incremental,
            SummaryMaintenanceLifecycleRejection::UnsupportedByRuntime,
        );
    }
    if !matches!(
        facts.arrival,
        DataArrival::ContinuouslyIngesting | DataArrival::Mixed
    ) {
        return rejected(
            lifecycle,
            SummaryMaintenanceMode::Incremental,
            SummaryMaintenanceLifecycleRejection::RequiresContinuousData,
        );
    }
    if facts.update_rate.is_none() {
        return rejected(
            lifecycle,
            SummaryMaintenanceMode::Incremental,
            SummaryMaintenanceLifecycleRejection::MissingOrStaleIngestionRate,
        );
    }
    if let Some(rejection) = maintenance_capability_rejection(facts, summary_capabilities) {
        return rejected(lifecycle, SummaryMaintenanceMode::Incremental, rejection);
    }
    let Some(horizon) = horizon else {
        return rejected(
            lifecycle,
            SummaryMaintenanceMode::Incremental,
            SummaryMaintenanceLifecycleRejection::RequiresHorizon,
        );
    };
    let total_cost = retained_cost(facts, costs, horizon.0);
    costed_or_unknown(
        lifecycle,
        SummaryMaintenanceMode::Incremental,
        total_cost,
        vec!["updates are applied for the optimization horizon".into()],
    )
}

fn maintenance_capability_rejection(
    facts: &SummaryMaintenanceWorkloadFacts,
    capabilities: SummaryMaintenanceCapabilities,
) -> Option<SummaryMaintenanceLifecycleRejection> {
    if matches!(
        facts.arrival,
        DataArrival::ContinuouslyIngesting | DataArrival::Mixed
    ) && !capabilities.incremental_update
    {
        Some(SummaryMaintenanceLifecycleRejection::SummaryDoesNotSupportIncrementalUpdates)
    } else if matches!(
        facts.arrival,
        DataArrival::ContinuouslyIngesting | DataArrival::Mixed
    ) && facts.requires_deletion
        && !capabilities.delete
    {
        Some(SummaryMaintenanceLifecycleRejection::SummaryDoesNotSupportDeletion)
    } else {
        None
    }
}

fn retained_cost(
    facts: &SummaryMaintenanceWorkloadFacts,
    costs: &SummaryMaintenanceLifecycleCostInputs,
    seconds: f64,
) -> Option<Cost> {
    let reads = facts.reads?;
    let maintenance = maintenance_cost(facts, costs, seconds)?;
    Some(Cost(
        costs.build_cost?.0
            + maintenance
            + reads * costs.summary_read_cost?.0
            + seconds * costs.retention_cost_rate?.0
            + costs.retirement_cost?.0,
    ))
}

fn maintenance_cost(
    facts: &SummaryMaintenanceWorkloadFacts,
    costs: &SummaryMaintenanceLifecycleCostInputs,
    seconds: f64,
) -> Option<f64> {
    match facts.arrival {
        DataArrival::AtRest => Some(0.0),
        DataArrival::ContinuouslyIngesting | DataArrival::Mixed => {
            Some(seconds * facts.update_rate?.0 * costs.maintenance_cost_per_update?.0)
        }
        DataArrival::Unknown => None,
    }
}

fn zip_costs(costs: &[Option<Cost>]) -> Option<f64> {
    costs
        .iter()
        .try_fold(0.0, |sum, cost| Some(sum + cost.as_ref()?.0))
}

fn costed_or_unknown(
    lifecycle: SummaryMaintenanceLifecycle,
    maintenance_mode: SummaryMaintenanceMode,
    total_cost: Option<Cost>,
    assumptions: Vec<String>,
) -> SummaryMaintenanceLifecycleAlternative {
    SummaryMaintenanceLifecycleAlternative {
        summary_maintenance_lifecycle: lifecycle,
        maintenance_mode,
        total_cost,
        rejection: total_cost
            .is_none()
            .then_some(SummaryMaintenanceLifecycleRejection::MissingCostEvidence),
        assumptions,
    }
}

fn rejected(
    lifecycle: SummaryMaintenanceLifecycle,
    maintenance_mode: SummaryMaintenanceMode,
    rejection: SummaryMaintenanceLifecycleRejection,
) -> SummaryMaintenanceLifecycleAlternative {
    SummaryMaintenanceLifecycleAlternative {
        summary_maintenance_lifecycle: lifecycle,
        maintenance_mode,
        total_cost: None,
        rejection: Some(rejection),
        assumptions: Vec::new(),
    }
}

fn retained_mode(facts: &SummaryMaintenanceWorkloadFacts) -> SummaryMaintenanceMode {
    match facts.arrival {
        DataArrival::ContinuouslyIngesting | DataArrival::Mixed => {
            SummaryMaintenanceMode::Incremental
        }
        DataArrival::AtRest | DataArrival::Unknown => SummaryMaintenanceMode::DirectBuild,
    }
}

fn collect_summary_aggs(
    node: &Rc<SummaryNode>,
    seen: &mut HashSet<*const SummaryNode>,
    output: &mut Vec<Rc<SummaryNode>>,
) {
    if !seen.insert(Rc::as_ptr(node)) {
        return;
    }
    match &node.expr {
        SummaryExpr::SummaryAgg { child, .. } => {
            output.push(Rc::clone(node));
            collect_summary_aggs(child, seen, output);
        }
        SummaryExpr::SummaryJoin { outer, inner, .. }
        | SummaryExpr::SummarySubtract {
            left: outer,
            right: inner,
        } => {
            collect_summary_aggs(outer, seen, output);
            collect_summary_aggs(inner, seen, output);
        }
        SummaryExpr::SummaryDelete { summary_input, .. }
        | SummaryExpr::SummaryEstimate { summary_input, .. } => {
            collect_summary_aggs(summary_input, seen, output)
        }
        SummaryExpr::SummaryMerge { children } => {
            for child in children {
                collect_summary_aggs(child, seen, output);
            }
        }
        SummaryExpr::KeepPreAsap(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::post_asap::{
        ExactKind, ExactParams, GroupingStrategy, ResultGuarantee, SummaryFamilyType, SummaryField,
        SummarySchema,
    };
    use asap_types::pre_asap::{Column, ColumnRef, DataType, QueryExpr, Reduction, Schema, Source};
    use asap_types::workload::{
        BatchEntry, DataWorkload, DurationMs, Evidence, EvidenceSource, Predictability, Query,
        QueryLanguage, QueryRequirements, Rate, RepeatingEntry, RepetitionInterval, TimeSelection,
    };

    struct UnitCosts;

    impl CostModel for UnitCosts {
        fn rank_candidates(
            &self,
            _intent: &asap_types::pre_asap::AggIntent,
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

    struct NoDelete;

    impl CostModel for NoDelete {
        fn rank_candidates(
            &self,
            _intent: &asap_types::pre_asap::AggIntent,
            candidates: &[asap_types::post_asap::SketchAlgorithm],
        ) -> Vec<asap_types::post_asap::SketchAlgorithm> {
            candidates.to_vec()
        }

        fn summary_maintenance_lifecycle_cost_inputs(
            &self,
            summary: &SummaryNode,
        ) -> SummaryMaintenanceLifecycleCostInputs {
            UnitCosts.summary_maintenance_lifecycle_cost_inputs(summary)
        }

        fn summary_maintenance_capabilities(
            &self,
            _summary: &SummaryNode,
        ) -> SummaryMaintenanceCapabilities {
            SummaryMaintenanceCapabilities {
                incremental_update: true,
                merge: true,
                delete: false,
            }
        }
    }

    fn query_root() -> Rc<QueryExpr> {
        query_root_for("m")
    }

    fn query_root_for(metric: &str) -> Rc<QueryExpr> {
        Rc::new(QueryExpr::Scan {
            source: Source::TimeSeries {
                metric: metric.into(),
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
        })
    }

    fn summary() -> Rc<SummaryNode> {
        let child = Rc::new(SummaryNode {
            expr: SummaryExpr::KeepPreAsap(query_root()),
            schema: SummarySchema {
                fields: vec![],
                time_index: None,
            },
            guarantee: Some(ResultGuarantee::exact("raw")),
        });
        let family = SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
        Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryAgg {
                child,
                family: family.clone(),
                col: ColumnRef::Named("value".into()),
                reduction: Reduction::by(vec![]),
                grouping: GroupingStrategy::default(),
            },
            schema: SummarySchema {
                fields: vec![SummaryField {
                    name: "state".into(),
                    dtype: family,
                    nullable: false,
                }],
                time_index: None,
            },
            guarantee: Some(ResultGuarantee::exact("sum")),
        })
    }

    fn batch(predictability: Predictability) -> BatchEntry {
        BatchEntry {
            query: Query("sum(m)".into()),
            requirements: QueryRequirements::default(),
            predictability,
            invocations: 1,
            execute_at: None,
            time_selection: TimeSelection::default(),
        }
    }

    fn workload(
        batches: Vec<BatchEntry>,
        repeating: Vec<RepeatingEntry>,
        data: DataWorkload,
    ) -> QueryWorkload {
        QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: (!batches.is_empty()).then_some(batches),
            repeating_queries: (!repeating.is_empty()).then_some(repeating),
            data_workload: Some(data),
        }
    }

    fn at_rest() -> DataWorkload {
        DataWorkload {
            arrival: DataArrival::AtRest,
            ..Default::default()
        }
    }

    fn continuous(observed_at_ms: u64, valid_for_ms: u64) -> DataWorkload {
        DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            ingestion_rate: Evidence {
                value: Some(Rate(1.0)),
                source: EvidenceSource::Observed,
                observed_at_ms: Some(observed_at_ms),
                valid_for_ms: Some(valid_for_ms),
            },
            ..Default::default()
        }
    }

    fn repeating() -> RepeatingEntry {
        RepeatingEntry {
            query: Query("sum(m)".into()),
            demand: RepeatedDemand::FixedInterval(RepetitionInterval(1_000)),
            requirements: QueryRequirements::default(),
            predictability: Predictability::Predictable { known_at: None },
            time_selection: TimeSelection::default(),
        }
    }

    #[test]
    fn unpredictable_one_time_at_rest_selects_ephemeral() {
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new(
                &workload(vec![batch(Predictability::AdHoc)], vec![], at_rest()),
                &[0],
            ),
            1_000,
            None,
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(plan.deployments.len(), 1);
        assert_eq!(
            plan.deployments[0].selected_summary_maintenance_lifecycle,
            Some(SummaryMaintenanceLifecycle::Ephemeral)
        );
        assert_eq!(
            plan.deployments[0].selected_maintenance_mode,
            Some(SummaryMaintenanceMode::DirectBuild)
        );
        assert_eq!(
            plan.deployments[0].alternatives[0].total_cost,
            Some(Cost(12.0))
        );
    }

    #[test]
    fn predictable_scheduled_one_time_offers_prepared_state() {
        let mut entry = batch(Predictability::Predictable {
            known_at: Some(TimestampMs(1_000)),
        });
        entry.execute_at = Some(TimestampMs(11_000));
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new(&workload(vec![entry], vec![], at_rest()), &[0]),
            1_000,
            None,
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        let prepared = &plan.deployments[0].alternatives[1];
        assert!(prepared.rejection.is_none());
        assert_eq!(prepared.total_cost, Some(Cost(13.0)));
    }

    #[test]
    fn prepared_state_starts_no_earlier_than_planning_time() {
        let mut entry = batch(Predictability::Predictable {
            known_at: Some(TimestampMs(1_000)),
        });
        entry.execute_at = Some(TimestampMs(11_000));
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new(&workload(vec![entry], vec![], at_rest()), &[0]),
            6_000,
            None,
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        let prepared = &plan.deployments[0].alternatives[1];
        assert_eq!(
            prepared.summary_maintenance_lifecycle,
            SummaryMaintenanceLifecycle::Prepared {
                activate_at: TimestampMs(6_000),
                retire_at: TimestampMs(11_000),
            }
        );
        assert_eq!(prepared.total_cost, Some(Cost(12.5)));
    }

    #[test]
    fn expired_one_time_execution_cannot_select_prepared_state() {
        let mut entry = batch(Predictability::Predictable {
            known_at: Some(TimestampMs(1_000)),
        });
        entry.execute_at = Some(TimestampMs(2_000));
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new(&workload(vec![entry], vec![], at_rest()), &[0]),
            3_000,
            None,
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(
            plan.deployments[0].alternatives[1].rejection,
            Some(SummaryMaintenanceLifecycleRejection::RequiresPredictableOneTimeQuery)
        );
    }

    #[test]
    fn repeated_at_rest_selects_shared_without_inventing_updates() {
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new(&workload(vec![], vec![repeating()], at_rest()), &[0]),
            1_000,
            Some(Horizon(10.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(
            plan.deployments[0].selected_summary_maintenance_lifecycle,
            Some(SummaryMaintenanceLifecycle::Shared {
                retention: DurationMs(10_000)
            })
        );
        assert_eq!(
            plan.deployments[0].selected_maintenance_mode,
            Some(SummaryMaintenanceMode::DirectBuild)
        );
        assert_eq!(
            plan.deployments[0].alternatives[3].rejection,
            Some(SummaryMaintenanceLifecycleRejection::RequiresContinuousData)
        );
        assert_eq!(plan.update_rate, None);
    }

    #[test]
    fn repeated_continuous_workload_can_select_continuous_maintenance() {
        let capabilities = SummaryMaintenanceLifecycleCapabilities {
            shared: false,
            ..SummaryMaintenanceLifecycleCapabilities::ALL
        };
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new(
                &workload(vec![], vec![repeating()], continuous(1_000, 60_000)),
                &[0],
            ),
            1_000,
            Some(Horizon(10.0)),
            capabilities,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(
            plan.deployments[0].selected_summary_maintenance_lifecycle,
            Some(SummaryMaintenanceLifecycle::ContinuouslyMaintained)
        );
        assert_eq!(
            plan.deployments[0].selected_maintenance_mode,
            Some(SummaryMaintenanceMode::Incremental)
        );
        assert_eq!(plan.evaluation_rate, Some(EvaluationRate(1.0)));
        assert_eq!(plan.update_rate, Some(UpdateRate(1.0)));
    }

    #[test]
    fn stale_ingestion_evidence_cannot_enable_continuous_maintenance() {
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new(
                &workload(vec![], vec![repeating()], continuous(1_000, 1_000)),
                &[0],
            ),
            3_000,
            Some(Horizon(10.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(
            plan.deployments[0].alternatives[3].rejection,
            Some(SummaryMaintenanceLifecycleRejection::MissingOrStaleIngestionRate)
        );
        assert_eq!(plan.update_rate, None);
    }

    #[test]
    fn unknown_costs_do_not_make_a_long_lived_lifecycle_win() {
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new(
                &workload(vec![], vec![repeating()], continuous(1_000, 60_000)),
                &[0],
            ),
            1_000,
            Some(Horizon(10.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &crate::cost_model::DefaultCostModel,
        )
        .unwrap();
        assert_eq!(
            plan.deployments[0].selected_summary_maintenance_lifecycle,
            None
        );
        assert!(plan.deployments[0]
            .alternatives
            .iter()
            .all(|alternative| alternative.rejection.is_some()));
    }

    #[test]
    fn unrelated_workload_entries_do_not_create_reuse_for_a_target() {
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new(
                &workload(
                    vec![batch(Predictability::AdHoc), batch(Predictability::AdHoc)],
                    vec![],
                    at_rest(),
                ),
                &[0],
            ),
            1_000,
            Some(Horizon(10.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(
            plan.deployments[0].selected_summary_maintenance_lifecycle,
            Some(SummaryMaintenanceLifecycle::Ephemeral)
        );
        assert_eq!(
            plan.deployments[0].alternatives[2].rejection,
            Some(SummaryMaintenanceLifecycleRejection::RequiresMultipleReads)
        );
    }

    #[test]
    fn scheduled_rate_counts_only_executions_inside_the_horizon() {
        let mut entry = repeating();
        entry.demand = RepeatedDemand::Scheduled(vec![
            TimestampMs(999),
            TimestampMs(5_000),
            TimestampMs(20_000),
        ]);
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new(&workload(vec![], vec![entry], at_rest()), &[0]),
            1_000,
            Some(Horizon(10.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(plan.evaluation_rate, Some(EvaluationRate(0.1)));
    }

    #[test]
    fn demand_binding_rejects_empty_and_duplicate_entries() {
        let workload = workload(vec![batch(Predictability::AdHoc)], vec![], at_rest());
        assert!(matches!(
            plan_summary_maintenance_lifecycles(
                summary(),
                WorkloadDemand::new(&workload, &[]),
                1_000,
                None,
                SummaryMaintenanceLifecycleCapabilities::ALL,
                &UnitCosts,
            ),
            Err(SummaryMaintenanceLifecyclePlanError::EmptyWorkloadDemand)
        ));
        assert!(matches!(
            plan_summary_maintenance_lifecycles(
                summary(),
                WorkloadDemand::new(&workload, &[0, 0]),
                1_000,
                None,
                SummaryMaintenanceLifecycleCapabilities::ALL,
                &UnitCosts,
            ),
            Err(SummaryMaintenanceLifecyclePlanError::DuplicateWorkloadEntry { index: 0 })
        ));
    }

    #[test]
    fn prepared_requires_every_bound_consumer_to_be_scheduled_and_predictable() {
        let mut predictable = batch(Predictability::Predictable {
            known_at: Some(TimestampMs(1_000)),
        });
        predictable.execute_at = Some(TimestampMs(2_000));
        let workload = workload(
            vec![predictable, batch(Predictability::AdHoc)],
            vec![],
            at_rest(),
        );
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new(&workload, &[0, 1]),
            1_000,
            Some(Horizon(10.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(
            plan.deployments[0].alternatives[1].rejection,
            Some(SummaryMaintenanceLifecycleRejection::RequiresPredictableOneTimeQuery)
        );
    }

    #[test]
    fn moving_realtime_maintenance_requires_summary_deletion_support() {
        let mut entry = repeating();
        entry.time_selection = TimeSelection {
            scope: asap_types::workload::QueryTimeScope::RealTime,
            lookback: Some(DurationMs(60_000)),
            as_of: None,
        };
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new(
                &workload(vec![], vec![entry], continuous(1_000, 60_000)),
                &[0],
            ),
            1_000,
            Some(Horizon(10.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &NoDelete,
        )
        .unwrap();
        assert_eq!(
            plan.deployments[0].alternatives[3].rejection,
            Some(SummaryMaintenanceLifecycleRejection::SummaryDoesNotSupportDeletion)
        );
    }

    #[test]
    fn normalized_workload_drives_plan_space_recurrence_profiles() {
        let root = query_root();
        let space = crate::replacement::search_workload(vec![("dashboard", Rc::clone(&root))]);
        let workload = workload(vec![], vec![repeating()], continuous(1_000, 60_000));
        let profiles = space
            .recurrence_profiles_from_workload(&workload, &[0], 1_000, Some(Horizon(10.0)))
            .unwrap();
        // `search_workload` canonicalizes roots through CSE; recurrence
        // profiles are keyed by that canonical post-CSE node.
        let profile = profiles.for_target(&space.roots[0].1);
        assert_eq!(profile.evaluation_rate, Some(EvaluationRate(1.0)));
        assert_eq!(profile.update_rate, Some(UpdateRate(1.0)));
        assert_eq!(profile.one_shot_consumers, 0);
    }

    #[test]
    fn recurrence_binding_is_explicit_when_root_order_differs_from_workload_order() {
        let repeating_root = query_root_for("dashboard");
        let batch_root = query_root_for("batch");
        let space = crate::replacement::search_workload(vec![
            ("dashboard", repeating_root),
            ("batch", batch_root),
        ]);
        let workload = workload(
            vec![batch(Predictability::AdHoc)],
            vec![repeating()],
            at_rest(),
        );
        let profiles = space
            .recurrence_profiles_from_workload(&workload, &[1, 0], 1_000, Some(Horizon(10.0)))
            .unwrap();
        let dashboard = profiles.for_target(&space.roots[0].1);
        let batch = profiles.for_target(&space.roots[1].1);
        assert_eq!(dashboard.evaluation_rate, Some(EvaluationRate(1.0)));
        assert_eq!(dashboard.one_shot_consumers, 0);
        assert_eq!(batch.evaluation_rate, None);
        assert_eq!(batch.one_shot_consumers, 1);
    }
}
