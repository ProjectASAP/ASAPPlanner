//! Workload-aware physical lifecycle planning for summary state.
//!
//! Phase validation from PR #300 answers whether a post-ASAP DAG can execute.
//! This module answers how each unique `SummaryAgg` state is deployed for the
//! supplied query and data workloads. Unknown evidence stays unknown and
//! therefore cannot make a long-lived lifecycle win.

use std::collections::HashSet;
use std::rc::Rc;

use asap_types::post_asap::{validate_execution_phases, StateLifecycle, SummaryExpr, SummaryNode};
use asap_types::post_asap::{EvaluationSchedule, OutputRepresentation};
use asap_types::pre_asap::QueryExpr;
use asap_types::workload::{
    DataArrival, Predictability, QueryRecurrence, QueryWorkload, RepeatedDemand, TimestampMs,
    WorkloadError,
};

use crate::cost_model::{Cost, CostModel};
use crate::recurrence::{CostRate, EvaluationRate, Horizon, UpdateRate};
use crate::replacement::{GlobalSelection, ImplementError};

/// Runtime lifecycle shapes available to the planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleCapabilities {
    pub ephemeral: bool,
    pub prepared: bool,
    pub shared: bool,
    pub continuously_maintained: bool,
}

/// Capabilities of one concrete summary family/state representation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SummaryLifecycleCapabilities {
    pub incremental_update: bool,
    pub merge: bool,
    pub delete: bool,
}

impl LifecycleCapabilities {
    pub const ALL: Self = Self {
        ephemeral: true,
        prepared: true,
        shared: true,
        continuously_maintained: true,
    };
}

impl Default for LifecycleCapabilities {
    fn default() -> Self {
        Self::ALL
    }
}

/// Primitive costs for one concrete summary state. Every field is optional:
/// missing statistics produce an uncosted alternative, never a zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LifecycleCostInputs {
    pub build_cost: Option<Cost>,
    pub maintenance_cost_per_update: Option<Cost>,
    pub summary_read_cost: Option<Cost>,
    pub retention_cost_rate: Option<CostRate>,
    pub retirement_cost: Option<Cost>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleRejection {
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

#[derive(Debug, Clone, PartialEq)]
pub struct LifecycleAlternative {
    pub lifecycle: StateLifecycle,
    pub total_cost: Option<Cost>,
    pub rejection: Option<LifecycleRejection>,
    pub assumptions: Vec<String>,
}

impl LifecycleAlternative {
    fn selectable(&self) -> bool {
        self.rejection.is_none() && self.total_cost.is_some()
    }
}

/// One unique summary-state deployment. Shared `Rc` nodes are emitted once.
#[derive(Debug, Clone)]
pub struct StateDeployment {
    pub summary_index: usize,
    pub summary: Rc<SummaryNode>,
    pub selected: Option<StateLifecycle>,
    pub evaluation_schedule: Option<EvaluationSchedule>,
    pub output_representation: OutputRepresentation,
    pub alternatives: Vec<LifecycleAlternative>,
}

#[derive(Debug, Clone)]
pub struct LifecyclePlan {
    pub root: Rc<SummaryNode>,
    pub deployments: Vec<StateDeployment>,
    pub horizon: Option<Horizon>,
    pub evaluation_rate: Option<EvaluationRate>,
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
pub enum LifecyclePlanError {
    #[error(transparent)]
    InvalidWorkload(#[from] WorkloadError),
    #[error(transparent)]
    InvalidExecutionPhases(#[from] asap_types::post_asap::PhaseError),
    #[error("optimization horizon must be finite and strictly positive")]
    InvalidHorizon,
    #[error("workload entry index {index} is out of bounds for {entry_count} entries")]
    InvalidWorkloadEntry { index: usize, entry_count: usize },
    #[error("a workload-demand binding must contain at least one entry")]
    EmptyWorkloadDemand,
    #[error("workload entry index {index} appears more than once in one demand binding")]
    DuplicateWorkloadEntry { index: usize },
}

#[derive(Debug, thiserror::Error)]
pub enum MaterializeLifecycleError {
    #[error(transparent)]
    Materialize(#[from] ImplementError),
    #[error(transparent)]
    Lifecycle(#[from] LifecyclePlanError),
}

#[derive(Debug)]
struct WorkloadFacts {
    reads: Option<f64>,
    one_time_invocations: u64,
    evaluation_rate: Option<EvaluationRate>,
    update_rate: Option<UpdateRate>,
    arrival: DataArrival,
    prepared_window: Option<(TimestampMs, TimestampMs)>,
    prepared_eligible: bool,
    requires_deletion: bool,
}

/// Validate a materialized plan, enumerate lifecycle alternatives for each
/// unique summary state, and select the cheapest legal alternative whose cost
/// is fully known.
pub fn plan_summary_lifecycles(
    root: Rc<SummaryNode>,
    demand: WorkloadDemand<'_>,
    now_ms: u64,
    horizon: Option<Horizon>,
    capabilities: LifecycleCapabilities,
    cost_model: &dyn CostModel,
) -> Result<LifecyclePlan, LifecyclePlanError> {
    demand.workload.validate()?;
    validate_execution_phases(&root)?;
    if horizon.is_some_and(|h| !h.0.is_finite() || h.0 <= 0.0) {
        return Err(LifecyclePlanError::InvalidHorizon);
    }
    let facts = workload_facts(demand.workload, demand.entry_indices, now_ms, horizon)?;
    let mut summaries = Vec::new();
    collect_summary_aggs(&root, &mut HashSet::new(), &mut summaries);
    let deployments: Vec<StateDeployment> = summaries
        .into_iter()
        .enumerate()
        .map(|(summary_index, summary)| {
            let alternatives = alternatives_for(
                &facts,
                horizon,
                capabilities,
                cost_model.summary_lifecycle_capabilities(&summary),
                cost_model.summary_lifecycle_cost_inputs(&summary),
            );
            let selected = alternatives
                .iter()
                .filter(|candidate| candidate.selectable())
                .min_by(|a, b| a.total_cost.unwrap().0.total_cmp(&b.total_cost.unwrap().0))
                .map(|candidate| candidate.lifecycle.clone());
            let evaluation_schedule = selected.as_ref().map(|lifecycle| match lifecycle {
                StateLifecycle::Ephemeral => EvaluationSchedule::OneShot,
                StateLifecycle::Prepared { .. } | StateLifecycle::Shared { .. }
                    if matches!(
                        facts.arrival,
                        DataArrival::ContinuouslyIngesting | DataArrival::Mixed
                    ) =>
                {
                    EvaluationSchedule::PerUpdate
                }
                StateLifecycle::Prepared { .. } => EvaluationSchedule::OneShot,
                StateLifecycle::Shared { .. } => EvaluationSchedule::OnRead,
                StateLifecycle::ContinuouslyMaintained => EvaluationSchedule::PerUpdate,
            });
            StateDeployment {
                summary_index,
                summary,
                selected,
                evaluation_schedule,
                output_representation: OutputRepresentation::SummaryState,
                alternatives,
            }
        })
        .collect();
    let summary_total_cost = deployments.iter().try_fold(Cost::ZERO, |sum, deployment| {
        let selected = deployment.selected.as_ref()?;
        let cost = deployment
            .alternatives
            .iter()
            .find(|alternative| &alternative.lifecycle == selected)?
            .total_cost?;
        Some(Cost(sum.0 + cost.0))
    });
    Ok(LifecyclePlan {
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

/// Materialize PR #300's globally selected phase-valid DAG and immediately
/// attach workload-aware lifecycle deployments.
pub fn materialize_with_lifecycles(
    selection: &GlobalSelection<'_>,
    target: &Rc<QueryExpr>,
    demand: WorkloadDemand<'_>,
    now_ms: u64,
    horizon: Option<Horizon>,
    capabilities: LifecycleCapabilities,
    cost_model: &dyn CostModel,
) -> Result<Option<LifecyclePlan>, MaterializeLifecycleError> {
    selection
        .materialize(target)?
        .map(|root| {
            let mut plan =
                plan_summary_lifecycles(root, demand, now_ms, horizon, capabilities, cost_model)?;
            plan.raw_recompute_total_cost = cost_model
                .raw_query_recompute_cost(target)
                .zip(plan.expected_reads)
                .map(|(per_read, reads)| Cost(per_read.0 * reads));
            if plan.raw_recompute_total_cost.is_some_and(|raw| {
                plan.summary_total_cost
                    .is_none_or(|summary| raw.0 <= summary.0)
            }) {
                plan.root = crate::replacement::keep_pre_asap(target)?;
                plan.deployments.clear();
                plan.selected_raw_recompute = true;
            }
            Ok(plan)
        })
        .transpose()
}

fn workload_facts(
    workload: &QueryWorkload,
    workload_entry_indices: &[usize],
    now_ms: u64,
    horizon: Option<Horizon>,
) -> Result<WorkloadFacts, LifecyclePlanError> {
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
        return Err(LifecyclePlanError::EmptyWorkloadDemand);
    }
    let mut seen_indices = HashSet::new();
    for &index in workload_entry_indices {
        if !seen_indices.insert(index) {
            return Err(LifecyclePlanError::DuplicateWorkloadEntry { index });
        }
        let entry = entries
            .get(index)
            .ok_or(LifecyclePlanError::InvalidWorkloadEntry {
                index,
                entry_count: entries.len(),
            })?;
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
                    if known < execute {
                        prepared_start = Some(prepared_start.map_or(*known, |old| old.min(*known)));
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
    Ok(WorkloadFacts {
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
    facts: &WorkloadFacts,
    horizon: Option<Horizon>,
    capabilities: LifecycleCapabilities,
    summary_capabilities: SummaryLifecycleCapabilities,
    costs: LifecycleCostInputs,
) -> Vec<LifecycleAlternative> {
    let alternatives = vec![
        ephemeral(facts, capabilities, &costs),
        prepared(facts, capabilities, summary_capabilities, &costs),
        shared(facts, horizon, capabilities, summary_capabilities, &costs),
        continuous(facts, horizon, capabilities, summary_capabilities, &costs),
    ];
    alternatives
}

fn ephemeral(
    facts: &WorkloadFacts,
    capabilities: LifecycleCapabilities,
    costs: &LifecycleCostInputs,
) -> LifecycleAlternative {
    let lifecycle = StateLifecycle::Ephemeral;
    if !capabilities.ephemeral {
        return rejected(lifecycle, LifecycleRejection::UnsupportedByRuntime);
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
        total_cost,
        vec!["state is rebuilt per invocation".into()],
    )
}

fn prepared(
    facts: &WorkloadFacts,
    capabilities: LifecycleCapabilities,
    summary_capabilities: SummaryLifecycleCapabilities,
    costs: &LifecycleCostInputs,
) -> LifecycleAlternative {
    if !facts.prepared_eligible {
        return rejected(
            StateLifecycle::Prepared {
                activate_at: TimestampMs(0),
                retire_at: TimestampMs(0),
            },
            LifecycleRejection::RequiresPredictableOneTimeQuery,
        );
    }
    let Some((activate_at, retire_at)) = facts.prepared_window else {
        return rejected(
            StateLifecycle::Prepared {
                activate_at: TimestampMs(0),
                retire_at: TimestampMs(0),
            },
            LifecycleRejection::RequiresPredictableOneTimeQuery,
        );
    };
    let lifecycle = StateLifecycle::Prepared {
        activate_at,
        retire_at,
    };
    if !capabilities.prepared {
        return rejected(lifecycle, LifecycleRejection::UnsupportedByRuntime);
    }
    if let Some(rejection) = maintenance_capability_rejection(facts, summary_capabilities) {
        return rejected(lifecycle, rejection);
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
        total_cost,
        vec!["activation and retirement come from the declared schedule".into()],
    )
}

fn shared(
    facts: &WorkloadFacts,
    horizon: Option<Horizon>,
    capabilities: LifecycleCapabilities,
    summary_capabilities: SummaryLifecycleCapabilities,
    costs: &LifecycleCostInputs,
) -> LifecycleAlternative {
    let lifecycle = StateLifecycle::Shared {
        retention: asap_types::workload::DurationMs(horizon.map_or(0, |h| (h.0 * 1000.0) as u64)),
    };
    if !capabilities.shared {
        return rejected(lifecycle, LifecycleRejection::UnsupportedByRuntime);
    }
    if let Some(rejection) = maintenance_capability_rejection(facts, summary_capabilities) {
        return rejected(lifecycle, rejection);
    }
    if facts.reads.is_none_or(|reads| reads <= 1.0) {
        return rejected(lifecycle, LifecycleRejection::RequiresMultipleReads);
    }
    let Some(horizon) = horizon else {
        return rejected(lifecycle, LifecycleRejection::RequiresHorizon);
    };
    let total_cost = retained_cost(facts, costs, horizon.0);
    costed_or_unknown(
        lifecycle,
        total_cost,
        vec!["one state is shared across reads".into()],
    )
}

fn continuous(
    facts: &WorkloadFacts,
    horizon: Option<Horizon>,
    capabilities: LifecycleCapabilities,
    summary_capabilities: SummaryLifecycleCapabilities,
    costs: &LifecycleCostInputs,
) -> LifecycleAlternative {
    let lifecycle = StateLifecycle::ContinuouslyMaintained;
    if !capabilities.continuously_maintained {
        return rejected(lifecycle, LifecycleRejection::UnsupportedByRuntime);
    }
    if !matches!(
        facts.arrival,
        DataArrival::ContinuouslyIngesting | DataArrival::Mixed
    ) {
        return rejected(lifecycle, LifecycleRejection::RequiresContinuousData);
    }
    if facts.update_rate.is_none() {
        return rejected(lifecycle, LifecycleRejection::MissingOrStaleIngestionRate);
    }
    if let Some(rejection) = maintenance_capability_rejection(facts, summary_capabilities) {
        return rejected(lifecycle, rejection);
    }
    let Some(horizon) = horizon else {
        return rejected(lifecycle, LifecycleRejection::RequiresHorizon);
    };
    let total_cost = retained_cost(facts, costs, horizon.0);
    costed_or_unknown(
        lifecycle,
        total_cost,
        vec!["updates are applied for the optimization horizon".into()],
    )
}

fn maintenance_capability_rejection(
    facts: &WorkloadFacts,
    capabilities: SummaryLifecycleCapabilities,
) -> Option<LifecycleRejection> {
    if matches!(
        facts.arrival,
        DataArrival::ContinuouslyIngesting | DataArrival::Mixed
    ) && !capabilities.incremental_update
    {
        Some(LifecycleRejection::SummaryDoesNotSupportIncrementalUpdates)
    } else if matches!(
        facts.arrival,
        DataArrival::ContinuouslyIngesting | DataArrival::Mixed
    ) && facts.requires_deletion
        && !capabilities.delete
    {
        Some(LifecycleRejection::SummaryDoesNotSupportDeletion)
    } else {
        None
    }
}

fn retained_cost(facts: &WorkloadFacts, costs: &LifecycleCostInputs, seconds: f64) -> Option<Cost> {
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
    facts: &WorkloadFacts,
    costs: &LifecycleCostInputs,
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
    lifecycle: StateLifecycle,
    total_cost: Option<Cost>,
    assumptions: Vec<String>,
) -> LifecycleAlternative {
    LifecycleAlternative {
        lifecycle,
        total_cost,
        rejection: total_cost
            .is_none()
            .then_some(LifecycleRejection::MissingCostEvidence),
        assumptions,
    }
}

fn rejected(lifecycle: StateLifecycle, rejection: LifecycleRejection) -> LifecycleAlternative {
    LifecycleAlternative {
        lifecycle,
        total_cost: None,
        rejection: Some(rejection),
        assumptions: Vec::new(),
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
        SummaryExpr::UpdateTransform { child, .. }
        | SummaryExpr::ReadoutPostProcess { child, .. } => {
            collect_summary_aggs(child, seen, output)
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
    use asap_types::pre_asap::AggIntent;
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

        fn summary_lifecycle_cost_inputs(&self, _summary: &SummaryNode) -> LifecycleCostInputs {
            LifecycleCostInputs {
                build_cost: Some(Cost(10.0)),
                maintenance_cost_per_update: Some(Cost(1.0)),
                summary_read_cost: Some(Cost(1.0)),
                retention_cost_rate: Some(CostRate(0.1)),
                retirement_cost: Some(Cost(1.0)),
            }
        }

        fn summary_lifecycle_capabilities(
            &self,
            _summary: &SummaryNode,
        ) -> SummaryLifecycleCapabilities {
            SummaryLifecycleCapabilities {
                incremental_update: true,
                merge: true,
                delete: true,
            }
        }
    }

    struct RawCheaper;

    impl CostModel for RawCheaper {
        fn rank_candidates(
            &self,
            _intent: &asap_types::pre_asap::AggIntent,
            candidates: &[asap_types::post_asap::SketchAlgorithm],
        ) -> Vec<asap_types::post_asap::SketchAlgorithm> {
            candidates.to_vec()
        }

        fn summary_lifecycle_cost_inputs(&self, summary: &SummaryNode) -> LifecycleCostInputs {
            UnitCosts.summary_lifecycle_cost_inputs(summary)
        }

        fn summary_lifecycle_capabilities(
            &self,
            summary: &SummaryNode,
        ) -> SummaryLifecycleCapabilities {
            UnitCosts.summary_lifecycle_capabilities(summary)
        }

        fn raw_query_recompute_cost(&self, _target: &QueryExpr) -> Option<Cost> {
            Some(Cost(1.0))
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

        fn summary_lifecycle_cost_inputs(&self, summary: &SummaryNode) -> LifecycleCostInputs {
            UnitCosts.summary_lifecycle_cost_inputs(summary)
        }

        fn summary_lifecycle_capabilities(
            &self,
            _summary: &SummaryNode,
        ) -> SummaryLifecycleCapabilities {
            SummaryLifecycleCapabilities {
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

    fn sum_query() -> Rc<QueryExpr> {
        Rc::new(QueryExpr::Aggregate {
            reduction: Reduction::by(vec![]),
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![],
            having: None,
            child: query_root(),
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
        let plan = plan_summary_lifecycles(
            summary(),
            WorkloadDemand::new(
                &workload(vec![batch(Predictability::AdHoc)], vec![], at_rest()),
                &[0],
            ),
            1_000,
            None,
            LifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(plan.deployments.len(), 1);
        assert_eq!(
            plan.deployments[0].selected,
            Some(StateLifecycle::Ephemeral)
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
        let plan = plan_summary_lifecycles(
            summary(),
            WorkloadDemand::new(&workload(vec![entry], vec![], at_rest()), &[0]),
            1_000,
            None,
            LifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        let prepared = &plan.deployments[0].alternatives[1];
        assert!(prepared.rejection.is_none());
        assert_eq!(prepared.total_cost, Some(Cost(13.0)));
    }

    #[test]
    fn repeated_at_rest_selects_shared_without_inventing_updates() {
        let plan = plan_summary_lifecycles(
            summary(),
            WorkloadDemand::new(&workload(vec![], vec![repeating()], at_rest()), &[0]),
            1_000,
            Some(Horizon(10.0)),
            LifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(
            plan.deployments[0].selected,
            Some(StateLifecycle::Shared {
                retention: DurationMs(10_000)
            })
        );
        assert_eq!(
            plan.deployments[0].alternatives[3].rejection,
            Some(LifecycleRejection::RequiresContinuousData)
        );
        assert_eq!(plan.update_rate, None);
    }

    #[test]
    fn repeated_continuous_workload_can_select_continuous_maintenance() {
        let capabilities = LifecycleCapabilities {
            shared: false,
            ..LifecycleCapabilities::ALL
        };
        let plan = plan_summary_lifecycles(
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
            plan.deployments[0].selected,
            Some(StateLifecycle::ContinuouslyMaintained)
        );
        assert_eq!(plan.evaluation_rate, Some(EvaluationRate(1.0)));
        assert_eq!(plan.update_rate, Some(UpdateRate(1.0)));
    }

    #[test]
    fn stale_ingestion_evidence_cannot_enable_continuous_maintenance() {
        let plan = plan_summary_lifecycles(
            summary(),
            WorkloadDemand::new(
                &workload(vec![], vec![repeating()], continuous(1_000, 1_000)),
                &[0],
            ),
            3_000,
            Some(Horizon(10.0)),
            LifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(
            plan.deployments[0].alternatives[3].rejection,
            Some(LifecycleRejection::MissingOrStaleIngestionRate)
        );
        assert_eq!(plan.update_rate, None);
    }

    #[test]
    fn unknown_costs_do_not_make_a_long_lived_lifecycle_win() {
        let plan = plan_summary_lifecycles(
            summary(),
            WorkloadDemand::new(
                &workload(vec![], vec![repeating()], continuous(1_000, 60_000)),
                &[0],
            ),
            1_000,
            Some(Horizon(10.0)),
            LifecycleCapabilities::ALL,
            &crate::cost_model::DefaultCostModel,
        )
        .unwrap();
        assert_eq!(plan.deployments[0].selected, None);
        assert!(plan.deployments[0]
            .alternatives
            .iter()
            .all(|alternative| alternative.rejection.is_some()));
    }

    #[test]
    fn unrelated_workload_entries_do_not_create_reuse_for_a_target() {
        let plan = plan_summary_lifecycles(
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
            LifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(
            plan.deployments[0].selected,
            Some(StateLifecycle::Ephemeral)
        );
        assert_eq!(
            plan.deployments[0].alternatives[2].rejection,
            Some(LifecycleRejection::RequiresMultipleReads)
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
        let plan = plan_summary_lifecycles(
            summary(),
            WorkloadDemand::new(&workload(vec![], vec![entry], at_rest()), &[0]),
            1_000,
            Some(Horizon(10.0)),
            LifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(plan.evaluation_rate, Some(EvaluationRate(0.1)));
    }

    #[test]
    fn demand_binding_rejects_empty_and_duplicate_entries() {
        let workload = workload(vec![batch(Predictability::AdHoc)], vec![], at_rest());
        assert!(matches!(
            plan_summary_lifecycles(
                summary(),
                WorkloadDemand::new(&workload, &[]),
                1_000,
                None,
                LifecycleCapabilities::ALL,
                &UnitCosts,
            ),
            Err(LifecyclePlanError::EmptyWorkloadDemand)
        ));
        assert!(matches!(
            plan_summary_lifecycles(
                summary(),
                WorkloadDemand::new(&workload, &[0, 0]),
                1_000,
                None,
                LifecycleCapabilities::ALL,
                &UnitCosts,
            ),
            Err(LifecyclePlanError::DuplicateWorkloadEntry { index: 0 })
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
        let plan = plan_summary_lifecycles(
            summary(),
            WorkloadDemand::new(&workload, &[0, 1]),
            1_000,
            Some(Horizon(10.0)),
            LifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(
            plan.deployments[0].alternatives[1].rejection,
            Some(LifecycleRejection::RequiresPredictableOneTimeQuery)
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
        let plan = plan_summary_lifecycles(
            summary(),
            WorkloadDemand::new(
                &workload(vec![], vec![entry], continuous(1_000, 60_000)),
                &[0],
            ),
            1_000,
            Some(Horizon(10.0)),
            LifecycleCapabilities::ALL,
            &NoDelete,
        )
        .unwrap();
        assert_eq!(
            plan.deployments[0].alternatives[3].rejection,
            Some(LifecycleRejection::SummaryDoesNotSupportDeletion)
        );
    }

    #[test]
    fn lifecycle_cost_can_fall_back_to_raw_recomputation() {
        let target = sum_query();
        let space = crate::replacement::search_workload(vec![("q", Rc::clone(&target))]);
        let selection = space.global_selection(&RawCheaper);
        let workload = workload(vec![batch(Predictability::AdHoc)], vec![], at_rest());
        let plan = materialize_with_lifecycles(
            &selection,
            &space.roots[0].1,
            WorkloadDemand::new(&workload, &[0]),
            1_000,
            None,
            LifecycleCapabilities::ALL,
            &RawCheaper,
        )
        .unwrap()
        .unwrap();
        assert!(plan.selected_raw_recompute);
        assert_eq!(plan.raw_recompute_total_cost, Some(Cost(1.0)));
        assert!(plan.deployments.is_empty());
        assert!(matches!(plan.root.expr, SummaryExpr::KeepPreAsap(_)));
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
