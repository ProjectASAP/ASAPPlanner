//! Workload-aware physical summary-maintenance lifecycle planning.
//!
//! A **summary-maintenance lifecycle** is the physical policy for when one
//! materialized summary state is created, retained or shared, updated as data
//! arrives, and retired. It is deliberately narrower than the end-to-end data
//! lifecycle and independent of query recurrence: recurrence is workload
//! evidence used to choose a lifecycle, not a lifecycle itself.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryMaintenanceLifecycleCapabilities {
    pub ephemeral: bool,
    pub prepared: bool,
    pub shared: bool,
    pub continuously_maintained: bool,
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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SummaryMaintenanceLifecycleCostInputs {
    pub build_cost: Option<Cost>,
    pub maintenance_cost_per_update: Option<Cost>,
    pub summary_read_cost: Option<Cost>,
    pub retention_cost_rate: Option<CostRate>,
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
    MissingCostEvidence,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SummaryMaintenanceLifecycleAlternative {
    pub summary_maintenance_lifecycle: SummaryMaintenanceLifecycle,
    /// How this lifecycle obtains and refreshes its summary state.
    pub maintenance_mode: SummaryMaintenanceMode,
    pub total_cost: Option<Cost>,
    pub rejection: Option<SummaryMaintenanceLifecycleRejection>,
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
    pub summary_index: usize,
    pub summary: Rc<SummaryNode>,
    pub selected_summary_maintenance_lifecycle: Option<SummaryMaintenanceLifecycle>,
    pub selected_maintenance_mode: Option<SummaryMaintenanceMode>,
    pub evaluation_schedule: Option<EvaluationSchedule>,
    pub output_representation: OutputRepresentation,
    pub alternatives: Vec<SummaryMaintenanceLifecycleAlternative>,
}

#[derive(Debug, Clone)]
pub struct SummaryMaintenanceLifecyclePlan {
    pub root: Rc<SummaryNode>,
    pub deployments: Vec<SummaryMaintenanceDeployment>,
    pub horizon: Option<Horizon>,
    pub evaluation_rate: Option<EvaluationRate>,
    pub update_rate: Option<UpdateRate>,
}

#[derive(Debug, thiserror::Error)]
pub enum SummaryMaintenanceLifecyclePlanError {
    #[error(transparent)]
    InvalidWorkload(#[from] WorkloadError),
    #[error("optimization horizon must be finite and strictly positive")]
    InvalidHorizon,
}

#[derive(Debug)]
struct WorkloadFacts {
    reads: Option<f64>,
    one_time_invocations: u64,
    evaluation_rate: Option<EvaluationRate>,
    update_rate: Option<UpdateRate>,
    arrival: DataArrival,
    prepared_window: Option<(TimestampMs, TimestampMs)>,
}

/// Validate a materialized plan, enumerate lifecycle alternatives for each
/// unique summary state, and select the cheapest legal alternative whose cost
/// is fully known.
pub fn plan_summary_maintenance_lifecycles(
    root: Rc<SummaryNode>,
    workload: &QueryWorkload,
    now_ms: u64,
    horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    cost_model: &dyn CostModel,
) -> Result<SummaryMaintenanceLifecyclePlan, SummaryMaintenanceLifecyclePlanError> {
    workload.validate()?;
    if horizon.is_some_and(|h| !h.0.is_finite() || h.0 <= 0.0) {
        return Err(SummaryMaintenanceLifecyclePlanError::InvalidHorizon);
    }
    let facts = workload_facts(workload, now_ms, horizon);
    let mut summaries = Vec::new();
    collect_summary_aggs(&root, &mut HashSet::new(), &mut summaries);
    let deployments = summaries
        .into_iter()
        .enumerate()
        .map(|(summary_index, summary)| {
            let alternatives = alternatives_for(
                &facts,
                horizon,
                capabilities,
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
    Ok(SummaryMaintenanceLifecyclePlan {
        root,
        deployments,
        horizon,
        evaluation_rate: facts.evaluation_rate,
        update_rate: facts.update_rate,
    })
}

fn workload_facts(
    workload: &QueryWorkload,
    now_ms: u64,
    horizon: Option<Horizon>,
) -> WorkloadFacts {
    let mut one_time_invocations = 0u64;
    let mut recurring_reads = 0.0;
    let mut recurring_known = true;
    let mut evaluation_rate = 0.0;
    let mut has_evaluation_rate = false;
    let mut prepared_start: Option<TimestampMs> = None;
    let mut prepared_end: Option<TimestampMs> = None;

    for entry in workload.entries() {
        match &entry.recurrence {
            QueryRecurrence::OneTime {
                invocations,
                execute_at,
            } => {
                one_time_invocations = one_time_invocations.saturating_add(*invocations);
                if let (
                    Predictability::Predictable {
                        known_at: Some(known),
                    },
                    Some(execute),
                ) = (&entry.predictability, execute_at)
                {
                    if known < execute {
                        prepared_start = Some(prepared_start.map_or(*known, |old| old.min(*known)));
                        prepared_end = Some(prepared_end.map_or(*execute, |old| old.max(*execute)));
                    }
                }
            }
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(interval)) => {
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
                if let Some(h) = horizon {
                    let end_ms = now_ms.saturating_add((h.0 * 1000.0) as u64);
                    recurring_reads += schedule
                        .iter()
                        .filter(|at| at.0 >= now_ms && at.0 <= end_ms)
                        .count() as f64;
                    evaluation_rate += schedule.len() as f64 / h.0;
                    has_evaluation_rate = true;
                } else {
                    recurring_known = false;
                }
            }
            QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(estimate)) => {
                let fresh = match (estimate.observed_at, estimate.valid_for) {
                    (Some(observed), Some(valid_for)) => {
                        now_ms <= observed.0.saturating_add(valid_for.0)
                    }
                    (None, Some(_)) => false,
                    _ => true,
                };
                if !fresh {
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
            QueryRecurrence::Unknown => recurring_known = false,
        }
    }

    let data = workload.data_workload.as_ref();
    let arrival = data.map_or(DataArrival::Unknown, |data| data.arrival);
    let update_rate = data
        .and_then(|data| data.ingestion_rate.value_at(now_ms))
        .map(|rate| UpdateRate(rate.0));
    let reads = recurring_known.then_some(one_time_invocations as f64 + recurring_reads);
    WorkloadFacts {
        reads,
        one_time_invocations,
        evaluation_rate: has_evaluation_rate.then_some(EvaluationRate(evaluation_rate)),
        update_rate,
        arrival,
        prepared_window: prepared_start.zip(prepared_end),
    }
}

fn alternatives_for(
    facts: &WorkloadFacts,
    horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    costs: SummaryMaintenanceLifecycleCostInputs,
) -> Vec<SummaryMaintenanceLifecycleAlternative> {
    let alternatives = vec![
        ephemeral(facts, capabilities, &costs),
        prepared(facts, capabilities, &costs),
        shared(facts, horizon, capabilities, &costs),
        continuous(facts, horizon, capabilities, &costs),
    ];
    alternatives
}

fn ephemeral(
    facts: &WorkloadFacts,
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
    facts: &WorkloadFacts,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    costs: &SummaryMaintenanceLifecycleCostInputs,
) -> SummaryMaintenanceLifecycleAlternative {
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
    facts: &WorkloadFacts,
    horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
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
    facts: &WorkloadFacts,
    horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
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

fn retained_cost(
    facts: &WorkloadFacts,
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
    facts: &WorkloadFacts,
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

fn retained_mode(facts: &WorkloadFacts) -> SummaryMaintenanceMode {
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
    }

    fn query_root() -> Rc<QueryExpr> {
        Rc::new(QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
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
            &workload(vec![batch(Predictability::AdHoc)], vec![], at_rest()),
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
            &workload(vec![entry], vec![], at_rest()),
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
    fn repeated_at_rest_selects_shared_without_inventing_updates() {
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            &workload(vec![], vec![repeating()], at_rest()),
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
            &workload(vec![], vec![repeating()], continuous(1_000, 60_000)),
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
            &workload(vec![], vec![repeating()], continuous(1_000, 1_000)),
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
            &workload(vec![], vec![repeating()], continuous(1_000, 60_000)),
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
}
