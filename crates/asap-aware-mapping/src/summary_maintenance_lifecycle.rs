//! Workload-aware summary-maintenance lifecycle planning.
//!
//! A **summary-maintenance lifecycle** is the planner policy for when one
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
//! materialized plan, and for every maintained population (`MaintainPopulation`)
//! that is not an input of a `SummaryAgg`. [`SummaryMaintenanceMode`] is an orthogonal detail of
//! the selected deployment: state is either built directly or updated
//! incrementally. Unknown evidence stays unknown and therefore cannot make a
//! long-lived alternative win.

use asap_types::ir::cse::share_common_sub_dags;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::export::{
    compile_physical_asap_dag_with_node_ids, compile_physical_asap_workload_with_node_ids,
    PhysicalASAPDAG, PhysicalASAPDAGValidationError, PhysicalASAPNodeId,
};
use asap_types::ir::timing::{apply_lifecycle_timings, LifecycleAssignment, TimingMemo};
use asap_types::ir::{ASAPOp, Operator, OperatorNode};
use asap_types::post_asap::{
    EvaluationSchedule, ExecutionDataStateError, ExecutionTiming, OutputRepresentation,
    ResultGuarantee, SummaryMaintenanceLifecycle, SummaryMaintenanceLifecycleGuarantee,
    SummaryMaintenanceMode, SummaryWindowFramework,
};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    DataArrival, DataWorkload, Predictability, QueryRecurrence, QueryWorkload, RepeatedDemand,
    TimestampMs, WorkloadError,
};

use crate::analytical_cost::AnalyticalCostError;
use crate::cost_model::{
    CompleteSummaryCandidateEstimate, Cost, CostModel, CostedSummaryDeployment,
};
use crate::physical_operator_statistics::evaluations_in_horizon;
use crate::recurrence::{
    CostRate, EvaluationRate, Horizon, RecurrenceError, RecurrenceProfile, UpdateRate,
};
use crate::replacement::{
    CandidateCostOverrides, CandidateLogicalASAPDAGs, GlobalSelection, RealizationError,
    Replacement, ReplacementProvenance,
};

/// Summary-maintenance lifecycle shapes supported by the target runtime.
///
/// These independent flags describe the set of lifecycle alternatives the
/// runtime implements, not simultaneous states of one deployment. Multiple
/// flags may be `true` (a runtime can support both ephemeral and prepared
/// state, for example); the planner still selects exactly one mutually
/// exclusive [`SummaryMaintenanceLifecycle`] for each deployment. A supported
/// alternative may still be rejected because workload evidence is missing or
/// its cost is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryMaintenanceLifecycleCapabilities {
    /// The runtime can build a fresh state for each invocation and retire it
    /// after that invocation finishes.
    pub supports_ephemeral: bool,
    /// The runtime can build state before a predictable execution and retain
    /// it until that scheduled execution window ends.
    pub supports_prepared: bool,
    /// The runtime can retain one state and reuse it across multiple reads.
    pub supports_shared: bool,
    /// The runtime can keep state current by applying arriving data updates.
    pub supports_continuously_maintained: bool,
}

/// State operations supported by one concrete summary family and
/// representation.
///
/// This differs from [`SummaryMaintenanceLifecycleCapabilities`]: these flags
/// describe what the summary algorithm itself can do, while lifecycle
/// capabilities describe what deployment policies the target runtime can
/// orchestrate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SummaryMaintenanceCapabilities {
    /// Existing state can incorporate arriving input without a full rebuild.
    pub incremental_update: bool,
    /// Two independently built states can be combined into one equivalent
    /// state.
    pub merge: bool,
    /// Expired or retracted input can be removed from existing state.
    pub delete: bool,
}

impl SummaryMaintenanceLifecycleCapabilities {
    pub const ALL: Self = Self {
        supports_ephemeral: true,
        supports_prepared: true,
        supports_shared: true,
        supports_continuously_maintained: true,
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
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

/// One candidate lifecycle policy for a particular summary deployment.
///
/// `total_cost: None` never means zero: it means the planner lacks enough
/// evidence to cost the candidate. Such a candidate is not selectable and its
/// `rejection` explains why.
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryMaintenanceLifecycleAlternative {
    /// State creation, retention, sharing, update, and retirement policy.
    pub summary_maintenance_lifecycle: SummaryMaintenanceLifecycle,
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

/// One unique retained-state deployment. Shared `Rc` nodes are emitted once.
#[derive(Debug, Clone)]
pub struct SummaryMaintenanceDeployment {
    /// Identity of this summary in the exported post-ASAP semantic DAG.
    /// It is scoped to one plan version and is not a summary definition or
    /// summary instance identity.
    pub post_asap_node_id: PhysicalASAPNodeId,
    /// The unique materialized `SummaryAgg` represented by this deployment.
    pub summary: Rc<OperatorNode>,
    /// Lifecycle, evaluation, and representation commitment selected for this
    /// state, or `None` when no alternative is selectable.
    pub summary_maintenance_lifecycle_guarantee: Option<SummaryMaintenanceLifecycleGuarantee>,
    /// Abstract window primitive selected for this state. Concrete runtime
    /// implementation, placement, and identity remain downstream decisions.
    pub selected_window_framework: Option<SummaryWindowFramework>,
    /// Every lifecycle shape considered, including rejected and uncosted ones.
    pub alternatives: Vec<SummaryMaintenanceLifecycleAlternative>,
}

/// Workload-aware lifecycle and window-framework decisions for every unique
/// summary state reachable from one materialized post-ASAP root.
#[derive(Debug, Clone)]
pub struct SummaryMaintenanceLifecyclePlan {
    /// Root of the materialized post-ASAP DAG being deployed.
    pub root: Rc<OperatorNode>,
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
    /// Total demand inside the horizon, when every recurrence is known.
    pub expected_reads: Option<f64>,
    /// Whether global costing preferred rebuilding the raw expression over all
    /// summary deployments.
    pub selected_raw_recompute: bool,
    /// Provider-owned identity of the selected complete physical deployment
    /// (for example a tumbling, sliding, or exponential-histogram plan).
    pub selected_window_implementation_id: Option<String>,
    /// Cost of the selected set of summary deployments, when fully known.
    pub summary_total_cost: Option<Cost>,
    /// Composed accuracy guarantee supplied by the selected physical window
    /// evidence, when the window framework introduces approximation.
    pub window_accuracy_guarantee: Option<ResultGuarantee>,
    /// Cost of evaluating the original expression for the same demand, when
    /// fully known.
    pub raw_recompute_total_cost: Option<Cost>,
}

/// Why a lifecycle plan cannot assign execution timing to its DAG.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SummaryMaintenanceTimingError {
    #[error(transparent)]
    InvalidPostAsapDAG(#[from] ExecutionDataStateError),
    #[error("summary {0:?} has no selected lifecycle")]
    UnselectedLifecycle(PhysicalASAPNodeId),
    /// A maintained population outside any `SummaryAgg`'s inputs has no
    /// deployment, so its timing would be guessed. Enumeration always emits
    /// one; this arises only for a plan whose root or deployments were edited.
    #[error("node {0:?} maintains state that has no summary-maintenance lifecycle")]
    UnplannedMaintainedState(PhysicalASAPNodeId),
    #[error(transparent)]
    InvalidPhases(#[from] PhysicalASAPDAGValidationError),
}

impl SummaryMaintenanceLifecyclePlan {
    /// The post-ASAP DAG of [`Self::root`] with every node's timing derived
    /// from the selected lifecycles, so physical compilation places it.
    ///
    /// A retained (non-`Ephemeral`) state outlives one query, so it and every
    /// input it consumes run at ingestion time. Every other node runs at query
    /// time: evaluations and consumers of retained state, and each `Ephemeral`
    /// state not consumed by retained state together with its inputs, whose
    /// raw data the deployment must supply as a query source. This applies to
    /// maintained populations as to `SummaryAgg` states; a population feeding
    /// a `SummaryAgg` is one of its inputs. Timings already on the root are
    /// ignored.
    pub fn execution_timed_dag(&self) -> Result<PhysicalASAPDAG, SummaryMaintenanceTimingError> {
        execution_timed_workload_dag(&[self])
    }
}

/// One physical ASAP DAG for a workload: a root per plan, in order, with
/// sub-DAGs shared between plans exported once. Timing follows the selected
/// lifecycles of every plan's deployments, as in
/// [`SummaryMaintenanceLifecyclePlan::execution_timed_dag`].
pub fn execution_timed_workload_dag(
    plans: &[&SummaryMaintenanceLifecyclePlan],
) -> Result<PhysicalASAPDAG, SummaryMaintenanceTimingError> {
    // One memo, so a node shared by several roots is timed and exported once.
    let mut memo = TimingMemo::new();
    let assignment = LifecycleAssignment::default_maintained();
    let timed = plans
        .iter()
        .map(|plan| apply_lifecycle_timings(&plan.root, &assignment, &mut memo))
        .collect::<Result<Vec<_>, _>>()?;
    let compiled = compile_physical_asap_workload_with_node_ids(&timed)?;
    let id_of = |node: &Rc<OperatorNode>| {
        compiled
            .node_ids
            .node_id(memo.timed(node).expect("plan node was timed"))
            .expect("timed plan node belongs to the compiled DAG")
    };
    let deployments: Vec<_> = plans
        .iter()
        .flat_map(|plan| &plan.deployments)
        .map(|deployment| (id_of(&deployment.summary), deployment))
        .collect();
    for plan in plans {
        for population in &standalone_populations(&plan.root) {
            let id = id_of(population);
            if !deployments.iter().any(|(deployed, _)| *deployed == id) {
                return Err(SummaryMaintenanceTimingError::UnplannedMaintainedState(id));
            }
        }
    }
    let dag = compiled.dag;
    let mut pending = Vec::new();
    for (id, deployment) in &deployments {
        let guarantee = deployment
            .summary_maintenance_lifecycle_guarantee
            .as_ref()
            .ok_or(SummaryMaintenanceTimingError::UnselectedLifecycle(*id))?;
        if guarantee.summary_maintenance_lifecycle != SummaryMaintenanceLifecycle::Ephemeral {
            pending.push(*id);
        }
    }
    let mut ingestion = HashSet::new();
    while let Some(id) = pending.pop() {
        if ingestion.insert(id) {
            pending.extend(
                dag.edges
                    .iter()
                    .filter(|edge| edge.consumer == id)
                    .map(|edge| edge.producer),
            );
        }
    }
    let phases = dag
        .nodes
        .iter()
        .map(|node| {
            let timing = if ingestion.contains(&node.id) {
                ExecutionTiming::IngestionTime
            } else {
                ExecutionTiming::QueryTime
            };
            (node.id, timing)
        })
        .collect();
    Ok(dag.with_execution_phases(&phases)?)
}

/// Explicit association between a materialized target and the normalized
/// workload entries whose demand consumes it.
///
/// [`QueryWorkload`] remains the source of query demand, while source-data
/// evidence is supplied independently. Indices avoid copying normalized entry
/// definitions while ensuring unrelated entries do not influence a target's
/// lifecycle decision.
#[derive(Debug, Clone, Copy)]
pub struct WorkloadDemand<'a> {
    /// Original normalized query workload.
    pub workload: &'a QueryWorkload,
    /// Independent source-data evidence, when the caller has it.
    pub data_workload: Option<&'a DataWorkload>,
    /// Indices from [`QueryWorkload::entries`] that consume this target.
    pub entry_indices: &'a [usize],
}

impl<'a> WorkloadDemand<'a> {
    /// Bind query demand without source-data evidence. Callers that have a
    /// [`DataWorkload`] should use [`Self::new_with_data`] so ingestion facts
    /// are not silently discarded.
    pub const fn new_without_data(workload: &'a QueryWorkload, entry_indices: &'a [usize]) -> Self {
        Self {
            workload,
            data_workload: None,
            entry_indices,
        }
    }

    pub const fn new_with_data(
        workload: &'a QueryWorkload,
        data_workload: &'a DataWorkload,
        entry_indices: &'a [usize],
    ) -> Self {
        Self {
            workload,
            data_workload: Some(data_workload),
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
    #[error(transparent)]
    InvalidPostAsapDAG(#[from] ExecutionDataStateError),
}

#[derive(Debug, thiserror::Error)]
pub enum SummaryMaintenanceLifecycleAssemblyError {
    #[error(transparent)]
    AssembleDAG(#[from] RealizationError),
    #[error(transparent)]
    SummaryMaintenance(#[from] SummaryMaintenanceLifecyclePlanError),
}

/// Failure while deriving workload-aware candidate costs before global
/// selection.
#[derive(Debug, thiserror::Error)]
pub enum SummaryMaintenanceLifecycleSelectionError {
    #[error(transparent)]
    Recurrence(#[from] RecurrenceError),
    #[error(transparent)]
    SummaryMaintenance(#[from] SummaryMaintenanceLifecyclePlanError),
}

/// Every lifecycle alternative for each unique retained state of one fixed
/// root, before any lifecycle is chosen.
///
/// Planner selection ([`plan_summary_maintenance_lifecycles`]) and a
/// deployment's explicit choice ([`Self::select`]) both finish from this value,
/// so they produce the same [`SummaryMaintenanceLifecyclePlan`] shape.
pub struct SummaryMaintenanceLifecycleCandidates<'a> {
    /// Unselected plan: deployments carry alternatives but no guarantee or
    /// window framework.
    plan: SummaryMaintenanceLifecyclePlan,
    components: Vec<usize>,
    arrival: DataArrival,
    required_accuracy: Vec<AccuracyTarget>,
    cost_model: &'a dyn CostModel,
    comparison_target: Option<&'a OperatorNode>,
}

/// Why an explicit per-state lifecycle choice cannot be bound.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SummaryMaintenanceLifecycleChoiceError {
    #[error("summary {0:?} is not a deployment of this root")]
    UnknownSummary(PhysicalASAPNodeId),
    #[error("summary {0:?} is chosen more than once")]
    DuplicateChoice(PhysicalASAPNodeId),
    #[error("summary {0:?} has no chosen lifecycle")]
    MissingChoice(PhysicalASAPNodeId),
    #[error("chosen lifecycle is not an enumerated alternative of summary {0:?}")]
    NotAnAlternative(PhysicalASAPNodeId),
    #[error("chosen lifecycle of summary {post_asap_node_id:?} is rejected: {rejection:?}")]
    Rejected {
        post_asap_node_id: PhysicalASAPNodeId,
        rejection: Option<SummaryMaintenanceLifecycleRejection>,
    },
    #[error("summary states on one maintenance path have different evaluation schedules")]
    IncompatibleEvaluationSchedules,
    #[error("the cost model supplied no complete estimate for the chosen combination")]
    NoCompleteEstimate,
}

impl SummaryMaintenanceLifecycleCandidates<'_> {
    /// One entry per unique retained state (see
    /// [`SummaryMaintenanceLifecyclePlan::deployments`]), with every
    /// alternative and its rejection; no lifecycle or window framework is
    /// selected.
    pub fn deployments(&self) -> &[SummaryMaintenanceDeployment] {
        &self.plan.deployments
    }

    /// Guarantee that binding `lifecycle` would attach under this workload's
    /// data arrival, so a caller can price an alternative before choosing it.
    pub fn guarantee(
        &self,
        lifecycle: &SummaryMaintenanceLifecycle,
    ) -> SummaryMaintenanceLifecycleGuarantee {
        lifecycle_guarantee(lifecycle, self.arrival)
    }

    fn context(&self) -> CompleteCostContext<'_> {
        CompleteCostContext {
            root: &self.plan.root,
            components: &self.components,
            cost_model: self.cost_model,
            comparison_target: self.comparison_target,
            horizon: self.plan.horizon,
            expected_reads: self.plan.expected_reads,
            required_accuracy: &self.required_accuracy,
        }
    }

    fn finish(
        mut self,
        estimate: Option<CompleteSummaryCandidateEstimate>,
    ) -> SummaryMaintenanceLifecyclePlan {
        if let Some(estimate) = estimate {
            self.plan.summary_total_cost = Some(estimate.cost);
            self.plan.selected_window_implementation_id = estimate.physical_plan_id;
            self.plan.window_accuracy_guarantee = estimate.window_accuracy_guarantee;
        }
        self.plan
    }

    /// Planner's choice: the cheapest complete combination of eligible
    /// alternatives.
    fn select_cheapest(mut self) -> SummaryMaintenanceLifecyclePlan {
        let estimate = select_complete_lifecycle_combination(
            &self.plan.root,
            &mut self.plan.deployments,
            &self.components,
            self.arrival,
            self.cost_model,
            self.comparison_target,
            self.plan.horizon,
            self.plan.expected_reads,
            &self.required_accuracy,
        );
        self.finish(estimate)
    }

    /// Bind one caller-chosen lifecycle per summary state. Each choice must be
    /// an alternative Planner itself could select; the complete estimate is
    /// then obtained exactly as for Planner selection, so window framework and
    /// cost are the model's and unknown cost is never replaced by zero.
    pub fn select(
        mut self,
        choices: &[(PhysicalASAPNodeId, SummaryMaintenanceLifecycle)],
    ) -> Result<SummaryMaintenanceLifecyclePlan, SummaryMaintenanceLifecycleChoiceError> {
        use SummaryMaintenanceLifecycleChoiceError as E;
        let deployments = &self.plan.deployments;
        let mut chosen: Vec<Option<&SummaryMaintenanceLifecycleAlternative>> =
            vec![None; deployments.len()];
        let context = self.context();
        for (id, lifecycle) in choices {
            let index = deployments
                .iter()
                .position(|deployment| deployment.post_asap_node_id == *id)
                .ok_or(E::UnknownSummary(*id))?;
            if chosen[index].is_some() {
                return Err(E::DuplicateChoice(*id));
            }
            let alternative = deployments[index]
                .alternatives
                .iter()
                .find(|alternative| alternative.summary_maintenance_lifecycle == *lifecycle)
                .ok_or(E::NotAnAlternative(*id))?;
            if !context.eligible(alternative) {
                return Err(E::Rejected {
                    post_asap_node_id: *id,
                    rejection: alternative.rejection.clone(),
                });
            }
            chosen[index] = Some(alternative);
        }
        let selected = chosen
            .into_iter()
            .enumerate()
            .map(|(index, alternative)| {
                let alternative =
                    alternative.ok_or(E::MissingChoice(deployments[index].post_asap_node_id))?;
                Ok((
                    index,
                    lifecycle_guarantee(&alternative.summary_maintenance_lifecycle, self.arrival),
                    // Reached only for costed alternatives or when the
                    // complete hook is authoritative, matching Planner search.
                    alternative.total_cost.unwrap_or(Cost::ZERO),
                ))
            })
            .collect::<Result<Vec<_>, E>>()?;
        if selected.is_empty() {
            return Ok(self.finish(None));
        }
        if !context.schedules_compatible(&selected) {
            return Err(E::IncompatibleEvaluationSchedules);
        }
        let estimate = context
            .estimate(deployments, &selected)
            .ok_or(E::NoCompleteEstimate)?;
        let guarantees = selected
            .into_iter()
            .map(|(index, guarantee, _)| (index, guarantee))
            .collect();
        apply_selection(&mut self.plan.deployments, guarantees, &estimate);
        Ok(self.finish(Some(estimate)))
    }
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
    required_accuracy: Vec<AccuracyTarget>,
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
    root: Rc<OperatorNode>,
    demand: WorkloadDemand<'_>,
    now_ms: u64,
    horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    cost_model: &dyn CostModel,
) -> Result<SummaryMaintenanceLifecyclePlan, SummaryMaintenanceLifecyclePlanError> {
    Ok(enumerate_summary_maintenance_lifecycles(
        root,
        demand,
        now_ms,
        horizon,
        capabilities,
        cost_model,
    )?
    .select_cheapest())
}

/// Validate a materialized plan and enumerate lifecycle alternatives for each
/// unique summary state without choosing one. A deployment that prices the
/// alternatives itself binds its choice with
/// [`SummaryMaintenanceLifecycleCandidates::select`].
pub fn enumerate_summary_maintenance_lifecycles<'a>(
    root: Rc<OperatorNode>,
    demand: WorkloadDemand<'_>,
    now_ms: u64,
    horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    cost_model: &'a dyn CostModel,
) -> Result<SummaryMaintenanceLifecycleCandidates<'a>, SummaryMaintenanceLifecyclePlanError> {
    enumerate_with_profile(
        root,
        demand,
        now_ms,
        horizon,
        capabilities,
        cost_model,
        None,
        None,
    )
}

/// Internal candidate-costing form. The workload binding supplies temporal
/// eligibility and data-arrival facts; `profile` supplies effective uses after
/// DAG path multiplicity has been propagated by `CandidateLogicalASAPDAGs`.
#[expect(clippy::too_many_arguments, reason = "internal bound planning context")]
fn enumerate_with_profile<'a>(
    root: Rc<OperatorNode>,
    demand: WorkloadDemand<'_>,
    now_ms: u64,
    horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    cost_model: &'a dyn CostModel,
    profile: Option<RecurrenceProfile>,
    comparison_target: Option<&'a OperatorNode>,
) -> Result<SummaryMaintenanceLifecycleCandidates<'a>, SummaryMaintenanceLifecyclePlanError> {
    demand.workload.validate()?;
    if let Some(data) = demand.data_workload {
        data.validate()?;
    }
    if horizon.is_some_and(|h| !h.0.is_finite() || h.0 <= 0.0) {
        return Err(SummaryMaintenanceLifecyclePlanError::InvalidHorizon);
    }
    let mut facts = workload_facts(
        demand.workload,
        demand.data_workload,
        demand.entry_indices,
        now_ms,
        horizon,
    )?;
    if let Some(profile) = profile {
        facts.one_time_invocations = u64::try_from(profile.one_shot_consumers).unwrap_or(u64::MAX);
        facts.evaluation_rate = profile.evaluation_rate;
        facts.update_rate = profile.update_rate;
        facts.reads = match (profile.evaluation_rate, horizon) {
            (Some(rate), Some(horizon)) => {
                Some(profile.one_shot_consumers as f64 + rate.0 * horizon.0)
            }
            (Some(_), None) => None,
            (None, _) if profile.one_shot_consumers > 0 => Some(profile.one_shot_consumers as f64),
            // Preserve unknown recurrence from the normalized workload. An
            // empty profile does not prove that the target is never read.
            (None, _) => facts.reads,
        };
    }
    let mut summaries = Vec::new();
    collect_states(
        &root,
        &mut HashSet::new(),
        &mut summaries,
        StateKind::SummaryAgg,
    );
    summaries.extend(standalone_populations(&root));
    let mut timing_memo = TimingMemo::new();
    let timed_root = apply_lifecycle_timings(
        &root,
        &LifecycleAssignment::default_maintained(),
        &mut timing_memo,
    )?;
    let node_ids = compile_physical_asap_dag_with_node_ids(&timed_root)?.node_ids;
    let components = summary_state_components(&summaries);
    let deployments: Vec<SummaryMaintenanceDeployment> = summaries
        .into_iter()
        .map(|summary| {
            let capabilities = if OperatorNode::reachable(&summary).iter().any(|node| {
                matches!(
                    node.non_asap(),
                    Some(asap_types::ir::NonASAPOp::BinaryOp { .. })
                ) && asap_types::ir::timing::validate_default(node, ExecutionTiming::IngestionTime)
                    .is_err()
            }) {
                SummaryMaintenanceLifecycleCapabilities {
                    supports_ephemeral: capabilities.supports_ephemeral,
                    supports_prepared: false,
                    supports_shared: false,
                    supports_continuously_maintained: false,
                }
            } else {
                capabilities
            };
            let alternatives = alternatives_for(
                &facts,
                horizon,
                capabilities,
                cost_model.summary_maintenance_capabilities(&summary),
                cost_model.summary_maintenance_lifecycle_cost_inputs_for_horizon(&summary, horizon),
            );
            SummaryMaintenanceDeployment {
                post_asap_node_id: timing_memo
                    .timed(&summary)
                    .and_then(|timed| node_ids.node_id(timed))
                    .expect("collected summary belongs to the compiled DAG"),
                summary,
                summary_maintenance_lifecycle_guarantee: None,
                selected_window_framework: None,
                alternatives,
            }
        })
        .collect();
    let selected_raw_recompute = !root.contains_asap();
    Ok(SummaryMaintenanceLifecycleCandidates {
        plan: SummaryMaintenanceLifecyclePlan {
            root,
            deployments,
            horizon,
            evaluation_rate: facts.evaluation_rate,
            update_rate: facts.update_rate,
            expected_reads: facts.reads,
            selected_raw_recompute,
            selected_window_implementation_id: None,
            summary_total_cost: None,
            window_accuracy_guarantee: None,
            raw_recompute_total_cost: None,
        },
        components,
        arrival: facts.arrival,
        required_accuracy: facts.required_accuracy,
        cost_model,
        comparison_target,
    })
}

/// Rank semantic summary siblings using the cheapest legal
/// summary-maintenance lifecycle for each candidate before final global
/// selection. The candidate space stays compact; only cost overrides are
/// attached, so shared `Rc` identity and exact-composition commitments remain
/// the responsibility of `GlobalSelection`.
///
/// Summary candidates of different targets whose outermost `SummaryAgg` is
/// structurally identical (for example p50 and p99 over one KLL) form a class.
/// When [`shared_state_cost`] can cost that state once against the union of
/// the targets' entries, each member is offered an equal split of it instead
/// of its independent cost. If selection then leaves any member of a class on
/// another choice, that class reverts to independent costs and selection runs
/// once more.
pub fn global_selection_with_summary_maintenance_lifecycles<'a, Id>(
    space: &'a CandidateLogicalASAPDAGs<Id>,
    demand: WorkloadDemand<'_>,
    now_ms: u64,
    horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    cost_model: &dyn CostModel,
) -> Result<GlobalSelection<'a>, SummaryMaintenanceLifecycleSelectionError> {
    let WorkloadDemand {
        workload,
        data_workload,
        entry_indices: root_workload_entries,
    } = demand;
    let profiles = space.recurrence_profiles_from_workload(
        workload,
        data_workload,
        root_workload_entries,
        now_ms,
        horizon,
    )?;
    let bindings = space.workload_entries_by_target(workload, root_workload_entries)?;
    let mut costs = CandidateCostOverrides::default();
    // Finalized summary candidates, as sharing-class members.
    let mut members = Vec::new();
    for group in space.target_subdag_candidates() {
        let Some(entry_indices) = bindings.get(&Rc::as_ptr(&group.target)) else {
            continue;
        };
        for candidate in &group.candidates {
            // Only summary realizations carry a maintenance lifecycle; a
            // logical rewrite or CSE share/recompute candidate does not.
            let Replacement::SubDAG(summary) = &candidate.replacement else {
                continue;
            };
            if candidate.provenance != ReplacementProvenance::SummaryRealization {
                continue;
            }
            costs.finalize_target(&group.target);
            let plan = enumerate_with_profile(
                Rc::clone(summary),
                WorkloadDemand {
                    workload,
                    data_workload,
                    entry_indices,
                },
                now_ms,
                horizon,
                capabilities,
                cost_model,
                Some(profiles.for_target(&group.target)),
                Some(&group.target),
            )?
            .select_cheapest();
            let raw = plan
                .expected_reads
                .and_then(|reads| cost_model.raw_query_recompute_total_cost(&group.target, reads));
            // Final comparison is atomic: without the raw side, no summary
            // override is published even when that summary alone is costed.
            if let Some(raw) = raw {
                costs.insert_raw(&group.target, raw);
                if !plan.deployments.is_empty() {
                    if let Some(total) = plan.summary_total_cost {
                        costs.insert(&group.target, candidate, total);
                    }
                    members.push((group, candidate, Rc::clone(summary)));
                }
            }
        }
    }

    // Intern every member once; members whose outermost state (the
    // `SummaryAgg` every other state of the candidate feeds) interns to the
    // same node share it. Classes are kept in first-member order.
    let interned = share_common_sub_dags(
        members
            .iter()
            .enumerate()
            .map(|(index, (_, _, summary))| (index, Rc::clone(summary)))
            .collect(),
    );
    let mut classes: Vec<(Rc<OperatorNode>, Vec<usize>)> = Vec::new();
    for (index, root) in interned {
        let states = summary_states(&root);
        let Some(state) = states
            .iter()
            .find(|state| summary_states(state).len() == states.len())
        else {
            continue;
        };
        if !standalone_populations(&root).is_empty() {
            continue;
        }
        match classes.iter_mut().find(|(s, _)| Rc::ptr_eq(s, state)) {
            Some((_, class)) => class.push(index),
            None => classes.push((Rc::clone(state), vec![index])),
        }
    }
    let mut shared = Vec::new();
    for (state, class) in classes {
        let mut targets: Vec<&Rc<OperatorNode>> = Vec::new();
        for &index in &class {
            let target = &members[index].0.target;
            if !targets.iter().any(|t| Rc::ptr_eq(t, target)) {
                targets.push(target);
            }
        }
        if targets.len() < 2 {
            continue;
        }
        let mut entries: Vec<usize> = targets
            .iter()
            .flat_map(|target| bindings[&Rc::as_ptr(target)].iter().copied())
            .collect();
        entries.sort_unstable();
        entries.dedup();
        let Some(cost) = shared_state_cost(
            &state,
            WorkloadDemand {
                workload,
                data_workload,
                entry_indices: &entries,
            },
            now_ms,
            horizon,
            capabilities,
            cost_model,
        )?
        else {
            continue;
        };
        shared.push((class, Cost(cost.0 / targets.len() as f64)));
    }

    let with_shared = |kept: &[(Vec<usize>, Cost)]| {
        let mut costs = costs.clone();
        for (class, split) in kept {
            for &index in class {
                let (group, candidate, _) = &members[index];
                costs.insert(&group.target, candidate, *split);
            }
        }
        costs
    };
    let selection = space.global_selection_with_candidate_costs(
        cost_model,
        &profiles,
        horizon,
        &with_shared(&shared),
    )?;
    let before = shared.len();
    shared.retain(|(class, _)| {
        class.iter().all(|&index| {
            let target = &members[index].0.target;
            let chosen = selection.for_target(target).and_then(|s| s.chosen);
            class.iter().any(|&other| {
                Rc::ptr_eq(&members[other].0.target, target)
                    && chosen.is_some_and(|chosen| std::ptr::eq(chosen, members[other].1))
            })
        })
    });
    if shared.len() == before {
        return Ok(selection);
    }
    Ok(space.global_selection_with_candidate_costs(
        cost_model,
        &profiles,
        horizon,
        &with_shared(&shared),
    )?)
}

/// Cost of one `SummaryAgg` state maintained once for every entry in
/// `demand`, or `None` when no lifecycle alternative is selectable for it.
/// No comparison target is supplied: the state serves several queries.
pub(crate) fn shared_state_cost(
    state: &Rc<OperatorNode>,
    demand: WorkloadDemand<'_>,
    now_ms: u64,
    horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    cost_model: &dyn CostModel,
) -> Result<Option<Cost>, SummaryMaintenanceLifecyclePlanError> {
    Ok(enumerate_with_profile(
        Rc::clone(state),
        demand,
        now_ms,
        horizon,
        capabilities,
        cost_model,
        None,
        None,
    )?
    .select_cheapest()
    .summary_total_cost)
}

/// Every unique `SummaryAgg` reachable from `root`.
pub(crate) fn summary_states(root: &Rc<OperatorNode>) -> Vec<Rc<OperatorNode>> {
    let mut states = Vec::new();
    collect_states(
        root,
        &mut HashSet::new(),
        &mut states,
        StateKind::SummaryAgg,
    );
    states
}

/// Assemble a globally selected phase-valid DAG and attach workload-aware
/// summary maintenance decisions. This does not create or maintain runtime state.
pub fn assemble_selected_dag_with_summary_maintenance_lifecycles(
    selection: &GlobalSelection<'_>,
    target: &Rc<OperatorNode>,
    demand: WorkloadDemand<'_>,
    now_ms: u64,
    horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    cost_model: &dyn CostModel,
) -> Result<Option<SummaryMaintenanceLifecyclePlan>, SummaryMaintenanceLifecycleAssemblyError> {
    selection
        .assemble_selected_dag(target)?
        .map(|root| {
            plan_assembled_dag(
                root,
                target,
                demand,
                now_ms,
                horizon,
                capabilities,
                cost_model,
            )
        })
        .transpose()
}

/// The lifecycle half of
/// [`assemble_selected_dag_with_summary_maintenance_lifecycles`], for a root
/// the caller already assembled (and possibly interned across queries).
pub(crate) fn plan_assembled_dag(
    root: Rc<OperatorNode>,
    target: &Rc<OperatorNode>,
    demand: WorkloadDemand<'_>,
    now_ms: u64,
    horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    cost_model: &dyn CostModel,
) -> Result<SummaryMaintenanceLifecyclePlan, SummaryMaintenanceLifecycleAssemblyError> {
    let mut plan = enumerate_with_profile(
        root,
        demand,
        now_ms,
        horizon,
        capabilities,
        cost_model,
        None,
        Some(target),
    )?
    .select_cheapest();
    plan.raw_recompute_total_cost = plan
        .expected_reads
        .and_then(|reads| cost_model.raw_query_recompute_total_cost(target, reads));
    if !plan.selected_raw_recompute
        && plan.raw_recompute_total_cost.is_none_or(|raw| {
            plan.summary_total_cost
                .is_none_or(|summary| raw.0 <= summary.0)
        })
    {
        plan.root = crate::replacement::retain_exact(target)?;
        plan.deployments.clear();
        plan.selected_raw_recompute = true;
        plan.selected_window_implementation_id = None;
        plan.summary_total_cost = None;
        plan.window_accuracy_guarantee = None;
    }
    Ok(plan)
}

fn workload_facts(
    workload: &QueryWorkload,
    data_workload: Option<&DataWorkload>,
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
    let mut required_accuracy = Vec::new();

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
        required_accuracy.push(entry.requirements.accuracy.target());
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
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(interval))
            | QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt { interval, .. }) => {
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
                let rate = estimate.expected_rate.0;
                evaluation_rate += rate;
                has_evaluation_rate = true;
                if let Some(h) = horizon {
                    recurring_reads += h.0 * rate;
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

    let data = data_workload;
    let arrival = data.map_or(DataArrival::Unknown, |data| data.arrival);
    let update_rate = data
        .and_then(|data| data.ingestion_rate.value_at(now_ms))
        .map(|rate| UpdateRate(rate.0));
    let reads = if let Some(horizon) = horizon {
        let horizon_ms = horizon.0 * 1_000.0;
        if !horizon_ms.is_finite()
            || horizon_ms <= 0.0
            || horizon_ms > u64::MAX as f64
            || horizon_ms.fract() != 0.0
        {
            None
        } else {
            workload_entry_indices
                .iter()
                .try_fold(0_u64, |total, index| {
                    let entry = entries.get(*index)?;
                    match evaluations_in_horizon(&entry.recurrence, now_ms, horizon_ms as u64) {
                        Ok(count) => total.checked_add(count),
                        Err(AnalyticalCostError::NoEvaluationsInHorizon) => Some(total),
                        Err(_) => None,
                    }
                })
                .map(|count| count as f64)
        }
    } else {
        recurring_known.then_some(one_time_invocations as f64 + recurring_reads)
    };
    Ok(SummaryMaintenanceWorkloadFacts {
        required_accuracy,
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
    if !capabilities.supports_ephemeral {
        return rejected(
            lifecycle,
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
            SummaryMaintenanceLifecycleRejection::RequiresPredictableOneTimeQuery,
        );
    }
    let Some((activate_at, retire_at)) = facts.prepared_window else {
        return rejected(
            SummaryMaintenanceLifecycle::Prepared {
                activate_at: TimestampMs(0),
                retire_at: TimestampMs(0),
            },
            SummaryMaintenanceLifecycleRejection::RequiresPredictableOneTimeQuery,
        );
    };
    let lifecycle = SummaryMaintenanceLifecycle::Prepared {
        activate_at,
        retire_at,
    };
    if !capabilities.supports_prepared {
        return rejected(
            lifecycle,
            SummaryMaintenanceLifecycleRejection::UnsupportedByRuntime,
        );
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
    facts: &SummaryMaintenanceWorkloadFacts,
    horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities,
    summary_capabilities: SummaryMaintenanceCapabilities,
    costs: &SummaryMaintenanceLifecycleCostInputs,
) -> SummaryMaintenanceLifecycleAlternative {
    let lifecycle = SummaryMaintenanceLifecycle::Shared {
        retention: asap_types::workload::DurationMs(horizon.map_or(0, |h| (h.0 * 1000.0) as u64)),
    };
    if !capabilities.supports_shared {
        return rejected(
            lifecycle,
            SummaryMaintenanceLifecycleRejection::UnsupportedByRuntime,
        );
    }
    if let Some(rejection) = maintenance_capability_rejection(facts, summary_capabilities) {
        return rejected(lifecycle, rejection);
    }
    if facts.reads.is_none_or(|reads| reads <= 1.0) {
        return rejected(
            lifecycle,
            SummaryMaintenanceLifecycleRejection::RequiresMultipleReads,
        );
    }
    let Some(horizon) = horizon else {
        return rejected(
            lifecycle,
            SummaryMaintenanceLifecycleRejection::RequiresHorizon,
        );
    };
    let total_cost = retained_cost(facts, costs, horizon.0);
    costed_or_unknown(
        lifecycle,
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
    if !capabilities.supports_continuously_maintained {
        return rejected(
            lifecycle,
            SummaryMaintenanceLifecycleRejection::UnsupportedByRuntime,
        );
    }
    if !matches!(
        facts.arrival,
        DataArrival::ContinuouslyIngesting | DataArrival::Mixed
    ) {
        return rejected(
            lifecycle,
            SummaryMaintenanceLifecycleRejection::RequiresContinuousData,
        );
    }
    if facts.update_rate.is_none() {
        return rejected(
            lifecycle,
            SummaryMaintenanceLifecycleRejection::MissingOrStaleIngestionRate,
        );
    }
    if let Some(rejection) = maintenance_capability_rejection(facts, summary_capabilities) {
        return rejected(lifecycle, rejection);
    }
    let Some(horizon) = horizon else {
        return rejected(
            lifecycle,
            SummaryMaintenanceLifecycleRejection::RequiresHorizon,
        );
    };
    let total_cost = retained_cost(facts, costs, horizon.0);
    costed_or_unknown(
        lifecycle,
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
    summary_maintenance_lifecycle: SummaryMaintenanceLifecycle,
    total_cost: Option<Cost>,
    assumptions: Vec<String>,
) -> SummaryMaintenanceLifecycleAlternative {
    SummaryMaintenanceLifecycleAlternative {
        summary_maintenance_lifecycle,
        total_cost,
        rejection: total_cost
            .is_none()
            .then_some(SummaryMaintenanceLifecycleRejection::MissingCostEvidence),
        assumptions,
    }
}

fn rejected(
    summary_maintenance_lifecycle: SummaryMaintenanceLifecycle,
    rejection: SummaryMaintenanceLifecycleRejection,
) -> SummaryMaintenanceLifecycleAlternative {
    SummaryMaintenanceLifecycleAlternative {
        summary_maintenance_lifecycle,
        total_cost: None,
        rejection: Some(rejection),
        assumptions: Vec::new(),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum StateKind {
    SummaryAgg,
    Population,
}

/// Collect every unique node of `kind` reachable from `node`.
fn collect_states(
    node: &Rc<OperatorNode>,
    seen: &mut HashSet<*const OperatorNode>,
    output: &mut Vec<Rc<OperatorNode>>,
    kind: StateKind,
) {
    if !seen.insert(Rc::as_ptr(node)) {
        return;
    }
    if matches!(
        (&node.operator, kind),
        (
            Operator::ASAP(ASAPOp::SummaryAgg { .. }),
            StateKind::SummaryAgg
        ) | (
            Operator::ASAP(ASAPOp::MaintainPopulation { .. }),
            StateKind::Population
        )
    ) {
        output.push(Rc::clone(node));
    }
    for child in node.children() {
        collect_states(child, seen, output, kind);
    }
}

/// Maintained populations that are not an input of any `SummaryAgg`. A
/// population feeding summary state is on that state's maintenance path, so
/// that state's lifecycle times it, even when a evaluation also reads it directly.
fn standalone_populations(root: &Rc<OperatorNode>) -> Vec<Rc<OperatorNode>> {
    let mut summaries = Vec::new();
    collect_states(
        root,
        &mut HashSet::new(),
        &mut summaries,
        StateKind::SummaryAgg,
    );
    let mut nested = Vec::new();
    let mut seen = HashSet::new();
    for summary in &summaries {
        collect_states(summary, &mut seen, &mut nested, StateKind::Population);
    }
    let nested: HashSet<_> = nested.iter().map(Rc::as_ptr).collect();
    let mut populations = Vec::new();
    collect_states(
        root,
        &mut HashSet::new(),
        &mut populations,
        StateKind::Population,
    );
    populations.retain(|population| !nested.contains(&Rc::as_ptr(population)));
    populations
}

pub(crate) fn evaluation_schedule(
    lifecycle: &SummaryMaintenanceLifecycle,
    arrival: DataArrival,
) -> EvaluationSchedule {
    match lifecycle {
        SummaryMaintenanceLifecycle::Ephemeral => EvaluationSchedule::OneShot,
        SummaryMaintenanceLifecycle::Prepared { .. }
        | SummaryMaintenanceLifecycle::Shared { .. }
            if matches!(
                arrival,
                DataArrival::ContinuouslyIngesting | DataArrival::Mixed
            ) =>
        {
            EvaluationSchedule::PerUpdate
        }
        SummaryMaintenanceLifecycle::Prepared { .. } => EvaluationSchedule::OneShot,
        SummaryMaintenanceLifecycle::Shared { .. } => EvaluationSchedule::OnRead,
        SummaryMaintenanceLifecycle::ContinuouslyMaintained => EvaluationSchedule::PerUpdate,
    }
}

/// Summary states composed on one maintenance path must be produced on the
/// same schedule. Return a component id for each collected state.
fn summary_state_components(summaries: &[Rc<OperatorNode>]) -> Vec<usize> {
    let indices: HashMap<_, _> = summaries
        .iter()
        .enumerate()
        .map(|(index, summary)| (Rc::as_ptr(summary), index))
        .collect();
    let mut parents: Vec<_> = (0..summaries.len()).collect();

    fn find(parents: &mut [usize], index: usize) -> usize {
        if parents[index] != index {
            parents[index] = find(parents, parents[index]);
        }
        parents[index]
    }

    for (parent_index, summary) in summaries.iter().enumerate() {
        let Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) = &summary.operator else {
            continue;
        };
        if !matches!(
            child.operator,
            Operator::ASAP(
                ASAPOp::SummaryAgg { .. }
                    | ASAPOp::SummaryJoin { .. }
                    | ASAPOp::SummarySubtract { .. }
                    | ASAPOp::SummaryDelete { .. }
                    | ASAPOp::SummaryMerge { .. }
            )
        ) {
            continue;
        }
        let mut descendants = Vec::new();
        collect_states(
            child,
            &mut HashSet::new(),
            &mut descendants,
            StateKind::SummaryAgg,
        );
        for descendant in descendants {
            let child_index = indices[&Rc::as_ptr(&descendant)];
            let parent_root = find(&mut parents, parent_index);
            let child_root = find(&mut parents, child_index);
            parents[child_root] = parent_root;
        }
    }
    (0..parents.len())
        .map(|index| find(&mut parents, index))
        .collect()
}

/// Inputs shared by every complete lifecycle-combination evaluation of one
/// root, whether Planner searches combinations or a caller supplies one.
struct CompleteCostContext<'a> {
    root: &'a OperatorNode,
    components: &'a [usize],
    cost_model: &'a dyn CostModel,
    comparison_target: Option<&'a OperatorNode>,
    horizon: Option<Horizon>,
    expected_reads: Option<f64>,
    required_accuracy: &'a [AccuracyTarget],
}

impl CompleteCostContext<'_> {
    /// Planner's own admission rule for one alternative. Uncosted alternatives
    /// are admitted only when the complete-candidate hook is authoritative.
    fn eligible(&self, alternative: &SummaryMaintenanceLifecycleAlternative) -> bool {
        alternative.selectable()
            || (self
                .cost_model
                .complete_summary_candidate_estimate_covers_lifecycle_costs()
                && alternative.rejection
                    == Some(SummaryMaintenanceLifecycleRejection::MissingCostEvidence))
    }

    /// `selected` holds one entry per deployment, in deployment order.
    fn schedules_compatible(
        &self,
        selected: &[(usize, SummaryMaintenanceLifecycleGuarantee, Cost)],
    ) -> bool {
        !selected.iter().enumerate().any(|(left, (_, a, _))| {
            selected.iter().enumerate().any(|(right, (_, b, _))| {
                self.components[left] == self.components[right]
                    && a.evaluation_schedule != b.evaluation_schedule
            })
        })
    }

    fn estimate(
        &self,
        deployments: &[SummaryMaintenanceDeployment],
        selected: &[(usize, SummaryMaintenanceLifecycleGuarantee, Cost)],
    ) -> Option<CompleteSummaryCandidateEstimate> {
        if !self.schedules_compatible(selected) {
            return None;
        }
        let costed: Vec<_> = selected
            .iter()
            .map(|(index, guarantee, cost)| CostedSummaryDeployment {
                summary: &deployments[*index].summary,
                guarantee,
                selected_cost: *cost,
            })
            .collect();
        let estimate = self.cost_model.complete_summary_candidate_estimate(
            self.root,
            self.comparison_target,
            &costed,
            self.horizon,
            self.expected_reads,
            self.required_accuracy,
        )?;
        (estimate.window_frameworks.len() == deployments.len()).then_some(estimate)
    }
}

fn lifecycle_guarantee(
    lifecycle: &SummaryMaintenanceLifecycle,
    arrival: DataArrival,
) -> SummaryMaintenanceLifecycleGuarantee {
    SummaryMaintenanceLifecycleGuarantee {
        summary_maintenance_mode: maintenance_mode(lifecycle, arrival),
        evaluation_schedule: evaluation_schedule(lifecycle, arrival),
        summary_maintenance_lifecycle: lifecycle.clone(),
        output_representation: OutputRepresentation::SummaryState,
    }
}

fn apply_selection(
    deployments: &mut [SummaryMaintenanceDeployment],
    guarantees: Vec<(usize, SummaryMaintenanceLifecycleGuarantee)>,
    estimate: &CompleteSummaryCandidateEstimate,
) {
    for (index, guarantee) in guarantees {
        deployments[index].summary_maintenance_lifecycle_guarantee = Some(guarantee);
    }
    for (deployment, framework) in deployments
        .iter_mut()
        .zip(estimate.window_frameworks.iter().cloned())
    {
        deployment.selected_window_framework = framework;
    }
}

#[expect(clippy::too_many_arguments, reason = "complete combination context")]
fn select_complete_lifecycle_combination(
    root: &OperatorNode,
    deployments: &mut [SummaryMaintenanceDeployment],
    components: &[usize],
    arrival: DataArrival,
    cost_model: &dyn CostModel,
    comparison_target: Option<&OperatorNode>,
    horizon: Option<Horizon>,
    expected_reads: Option<f64>,
    required_accuracy: &[AccuracyTarget],
) -> Option<CompleteSummaryCandidateEstimate> {
    const MAX_COMPLETE_LIFECYCLE_COMBINATIONS: usize = 4_096;
    if deployments.is_empty() {
        return None;
    }
    let context = CompleteCostContext {
        root,
        components,
        cost_model,
        comparison_target,
        horizon,
        expected_reads,
        required_accuracy,
    };
    // The whole-candidate hook is intentionally arbitrary, so partial costs
    // cannot soundly prune the search. Bound exhaustive enumeration and fail
    // closed instead of allowing an adversarial DAG to consume exponential
    // planner time.
    let combinations = deployments
        .iter()
        .try_fold(1_usize, |product, deployment| {
            let selectable = deployment
                .alternatives
                .iter()
                .filter(|alternative| context.eligible(alternative))
                .count();
            product.checked_mul(selectable)
        })?;
    if combinations == 0 || combinations > MAX_COMPLETE_LIFECYCLE_COMBINATIONS {
        return None;
    }
    type Best = Option<(
        CompleteSummaryCandidateEstimate,
        Vec<(usize, SummaryMaintenanceLifecycleGuarantee)>,
    )>;
    fn visit(
        index: usize,
        context: &CompleteCostContext<'_>,
        deployments: &[SummaryMaintenanceDeployment],
        arrival: DataArrival,
        selected: &mut Vec<(usize, SummaryMaintenanceLifecycleGuarantee, Cost)>,
        best: &mut Best,
    ) {
        if index == deployments.len() {
            let Some(estimate) = context.estimate(deployments, selected) else {
                return;
            };
            if best
                .as_ref()
                .is_none_or(|(best_estimate, _)| estimate.cost.0 < best_estimate.cost.0)
            {
                *best = Some((
                    estimate,
                    selected
                        .iter()
                        .map(|(index, guarantee, _)| (*index, guarantee.clone()))
                        .collect(),
                ));
            }
            return;
        }
        for alternative in deployments[index]
            .alternatives
            .iter()
            .filter(|alternative| context.eligible(alternative))
        {
            selected.push((
                index,
                lifecycle_guarantee(&alternative.summary_maintenance_lifecycle, arrival),
                alternative.total_cost.unwrap_or(Cost::ZERO),
            ));
            visit(index + 1, context, deployments, arrival, selected, best);
            selected.pop();
        }
    }

    let mut best = None;
    visit(
        0,
        &context,
        deployments,
        arrival,
        &mut Vec::new(),
        &mut best,
    );
    let (estimate, guarantees) = best?;
    apply_selection(deployments, guarantees, &estimate);
    Some(estimate)
}

pub(crate) fn maintenance_mode(
    lifecycle: &SummaryMaintenanceLifecycle,
    arrival: DataArrival,
) -> SummaryMaintenanceMode {
    match lifecycle {
        SummaryMaintenanceLifecycle::Ephemeral => SummaryMaintenanceMode::DirectBuild,
        SummaryMaintenanceLifecycle::ContinuouslyMaintained => SummaryMaintenanceMode::Incremental,
        SummaryMaintenanceLifecycle::Prepared { .. }
        | SummaryMaintenanceLifecycle::Shared { .. } => match arrival {
            DataArrival::ContinuouslyIngesting | DataArrival::Mixed => {
                SummaryMaintenanceMode::Incremental
            }
            DataArrival::AtRest | DataArrival::Unknown => SummaryMaintenanceMode::DirectBuild,
        },
    }
}

#[cfg(test)]
mod tests {
    // Independent data evidence must be validated at both planning boundaries.
    #[test]
    fn rejects_invalid_parallel_data_evidence() {
        let query = workload(vec![batch(Predictability::AdHoc)], vec![], at_rest());
        let space = crate::replacement::search_workload(vec![("q", quantile_query())]);
        for rate in [1.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut data = at_rest();
            data.ingestion_rate.value = Some(Rate(rate));
            assert!(space
                .recurrence_profiles_from_workload(&query, Some(&data), &[0], 0, None)
                .is_err());
            assert!(plan_summary_maintenance_lifecycles(
                summary(),
                WorkloadDemand::new_with_data(&query, &data, &[0]),
                0,
                None,
                SummaryMaintenanceLifecycleCapabilities::ALL,
                &crate::cost_model::DefaultCostModel,
            )
            .is_err());
        }
    }
    use super::*;
    use asap_types::ir::export::{NonASAPOpKind, PhysicalASAPOperatorPayload};
    use asap_types::ir::{BinaryOperator, NonASAPOp};
    use asap_types::post_asap::{
        ExactKind, ExactParams, Field, FieldDataType, GroupingStrategy, ResultGuarantee, Schema,
        SketchAlgorithm,
    };
    use asap_types::pre_asap::AggIntent;
    use asap_types::pre_asap::{
        ArithmeticOpKind, BinaryOpKind, ColumnRef, DataType, Reduction, Source,
    };
    use asap_types::types::AccuracyTarget;
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
            _summary: &OperatorNode,
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
            _summary: &OperatorNode,
        ) -> SummaryMaintenanceCapabilities {
            SummaryMaintenanceCapabilities {
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

        fn summary_maintenance_lifecycle_cost_inputs(
            &self,
            summary: &OperatorNode,
        ) -> SummaryMaintenanceLifecycleCostInputs {
            UnitCosts.summary_maintenance_lifecycle_cost_inputs(summary)
        }

        fn summary_maintenance_capabilities(
            &self,
            summary: &OperatorNode,
        ) -> SummaryMaintenanceCapabilities {
            UnitCosts.summary_maintenance_capabilities(summary)
        }

        fn raw_query_recompute_cost(&self, _target: &OperatorNode) -> Option<Cost> {
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

        fn summary_maintenance_lifecycle_cost_inputs(
            &self,
            summary: &OperatorNode,
        ) -> SummaryMaintenanceLifecycleCostInputs {
            UnitCosts.summary_maintenance_lifecycle_cost_inputs(summary)
        }

        fn summary_maintenance_capabilities(
            &self,
            _summary: &OperatorNode,
        ) -> SummaryMaintenanceCapabilities {
            SummaryMaintenanceCapabilities {
                incremental_update: true,
                merge: true,
                delete: false,
            }
        }
    }

    struct SummaryMaintenancePrefersDdSketch;

    impl CostModel for SummaryMaintenancePrefersDdSketch {
        fn raw_query_recompute_total_cost(
            &self,
            _target: &OperatorNode,
            _expected_reads: f64,
        ) -> Option<Cost> {
            Some(Cost(1_000.0))
        }

        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchAlgorithm],
        ) -> Vec<SketchAlgorithm> {
            // Preserve semantic mapping's KLL-first order. The lifecycle
            // total below must be what changes the final choice.
            candidates.to_vec()
        }

        fn summary_maintenance_lifecycle_cost_inputs(
            &self,
            summary: &OperatorNode,
        ) -> SummaryMaintenanceLifecycleCostInputs {
            let build = match sketch_algorithm(summary) {
                Some(SketchAlgorithm::Kll) => 100.0,
                Some(SketchAlgorithm::DDSketch) => 1.0,
                _ => 10.0,
            };
            SummaryMaintenanceLifecycleCostInputs {
                build_cost: Some(Cost(build)),
                maintenance_cost_per_update: Some(Cost(1.0)),
                summary_read_cost: Some(Cost(1.0)),
                retention_cost_rate: Some(CostRate(0.1)),
                retirement_cost: Some(Cost(1.0)),
            }
        }
    }

    struct IncompatibleNestedCosts;

    struct WholeCandidatePrefersContinuous;

    impl CostModel for WholeCandidatePrefersContinuous {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchAlgorithm],
        ) -> Vec<SketchAlgorithm> {
            candidates.to_vec()
        }

        fn complete_summary_candidate_cost(
            &self,
            _root: &OperatorNode,
            _target: Option<&OperatorNode>,
            deployments: &[CostedSummaryDeployment<'_>],
            _horizon: Option<Horizon>,
            _expected_reads: Option<f64>,
            _required_accuracy: &[AccuracyTarget],
        ) -> Option<Cost> {
            Some(
                if deployments.iter().all(|deployment| {
                    matches!(
                        deployment.guarantee.summary_maintenance_lifecycle,
                        SummaryMaintenanceLifecycle::ContinuouslyMaintained
                    )
                }) {
                    Cost(1.0)
                } else {
                    Cost(100.0)
                },
            )
        }
    }

    impl CostModel for IncompatibleNestedCosts {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchAlgorithm],
        ) -> Vec<SketchAlgorithm> {
            candidates.to_vec()
        }

        fn summary_maintenance_lifecycle_cost_inputs(
            &self,
            summary: &OperatorNode,
        ) -> SummaryMaintenanceLifecycleCostInputs {
            // A leaf summary is one built directly over kept pre-ASAP rows
            // (its child is not an ASAP node); a nested one reads state.
            let is_leaf = matches!(
                &summary.operator,
                Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) if !child.is_asap()
            );
            SummaryMaintenanceLifecycleCostInputs {
                build_cost: Some(Cost(if is_leaf { 1.0 } else { 100.0 })),
                maintenance_cost_per_update: Some(Cost(if is_leaf { 100.0 } else { 0.0 })),
                summary_read_cost: Some(Cost::ZERO),
                retention_cost_rate: Some(CostRate(0.0)),
                retirement_cost: Some(Cost::ZERO),
            }
        }

        fn summary_maintenance_capabilities(
            &self,
            _summary: &OperatorNode,
        ) -> SummaryMaintenanceCapabilities {
            SummaryMaintenanceCapabilities {
                incremental_update: true,
                merge: true,
                delete: true,
            }
        }
    }

    /// The sketch algorithm of the first `SummaryAgg` reachable from `node`
    /// (through a evaluation or any relational operator kept above it).
    fn sketch_algorithm(node: &OperatorNode) -> Option<SketchAlgorithm> {
        if let Operator::ASAP(ASAPOp::SummaryAgg {
            family: FieldDataType::Sketch(kind, _),
            ..
        }) = &node.operator
        {
            return Some(kind.algorithm().clone());
        }
        node.children()
            .into_iter()
            .find_map(|child| sketch_algorithm(child))
    }

    fn query_root() -> Rc<OperatorNode> {
        query_root_for("m")
    }

    fn query_root_for(metric: &str) -> Rc<OperatorNode> {
        OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries {
                metric: metric.into(),
            },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        }))
        .unwrap()
    }

    fn sum_query() -> Rc<OperatorNode> {
        OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![]),
            measures: vec![AggIntent::Sum { col: None }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: query_root(),
        }))
        .unwrap()
    }

    fn quantile_query() -> Rc<OperatorNode> {
        OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![]),
            measures: vec![AggIntent::Quantile {
                col: None,
                q: 0.99,
                accuracy: AccuracyTarget::Epsilon(0.1),
            }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: query_root(),
        }))
        .unwrap()
    }

    /// An exact sum accumulator over the kept pre-ASAP scan.
    fn summary() -> Rc<OperatorNode> {
        let child = Rc::new(
            query_root()
                .as_ref()
                .clone()
                .with_guarantee(Some(ResultGuarantee::exact("raw"))),
        );
        let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
        std::rc::Rc::new(
            OperatorNode::with_schema(
                asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg {
                    child,
                    family: family.clone(),
                    input: asap_types::post_asap::SummaryUpdate::column(ColumnRef::Named(
                        "value".into(),
                    )),
                    reduction: Reduction::by(vec![]),
                    grouping: GroupingStrategy::default(),
                    filter: None,
                }),
                Schema::lifted(vec![Field::new("state", family, false)], None),
            )
            .with_guarantee(Some(ResultGuarantee::exact("sum"))),
        )
    }

    fn nested_summary() -> Rc<OperatorNode> {
        let child = summary();
        let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
        std::rc::Rc::new(
            OperatorNode::with_schema(
                asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg {
                    child,
                    family: family.clone(),
                    input: asap_types::post_asap::SummaryUpdate::column(ColumnRef::Named(
                        "state".into(),
                    )),
                    reduction: Reduction::by(vec![]),
                    grouping: GroupingStrategy::default(),
                    filter: None,
                }),
                Schema::lifted(vec![Field::new("state", family, false)], None),
            )
            .with_guarantee(Some(ResultGuarantee::exact("nested sum"))),
        )
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
        _data: DataWorkload,
    ) -> QueryWorkload {
        QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: (!batches.is_empty()).then_some(batches),
            repeating_queries: (!repeating.is_empty()).then_some(repeating),
        }
    }

    fn at_rest() -> DataWorkload {
        DataWorkload {
            arrival: DataArrival::AtRest,
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(1_000)),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn continuous(observed_at_ms: u64, valid_for_ms: u64) -> DataWorkload {
        DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(1_000)),
                ..Default::default()
            },
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

    fn selected_summary_maintenance_lifecycle(
        deployment: &SummaryMaintenanceDeployment,
    ) -> Option<&SummaryMaintenanceLifecycle> {
        deployment
            .summary_maintenance_lifecycle_guarantee
            .as_ref()
            .map(|guarantee| &guarantee.summary_maintenance_lifecycle)
    }

    #[test]
    fn fixed_interval_reads_use_the_physical_horizon_multiplicity() {
        let mut query = repeating();
        query.demand = RepeatedDemand::FixedInterval(RepetitionInterval(600));
        let workload = workload(vec![], vec![query], at_rest());

        let facts =
            workload_facts(&workload, Some(&at_rest()), &[0], 0, Some(Horizon(1.0))).unwrap();

        assert_eq!(facts.reads, Some(1.0));
    }

    #[test]
    fn unpredictable_one_time_at_rest_selects_ephemeral() {
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new_without_data(
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
            selected_summary_maintenance_lifecycle(&plan.deployments[0]),
            Some(&SummaryMaintenanceLifecycle::Ephemeral)
        );
        let guarantee = plan.deployments[0]
            .summary_maintenance_lifecycle_guarantee
            .as_ref()
            .unwrap();
        assert_eq!(guarantee.evaluation_schedule, EvaluationSchedule::OneShot);
        assert_eq!(
            guarantee.summary_maintenance_mode,
            SummaryMaintenanceMode::DirectBuild
        );
        assert_eq!(
            guarantee.output_representation,
            OutputRepresentation::SummaryState
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
            WorkloadDemand::new_with_data(
                &workload(vec![entry], vec![], at_rest()),
                &at_rest(),
                &[0],
            ),
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
            WorkloadDemand::new_with_data(
                &workload(vec![entry], vec![], at_rest()),
                &at_rest(),
                &[0],
            ),
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
            WorkloadDemand::new_without_data(&workload(vec![entry], vec![], at_rest()), &[0]),
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
    fn nested_summary_lifecycles_have_compatible_evaluation_schedules() {
        let workload = workload(vec![], vec![repeating()], continuous(1_000, 20_000));
        let plan = plan_summary_maintenance_lifecycles(
            nested_summary(),
            WorkloadDemand::new_without_data(&workload, &[0]),
            1_000,
            Some(Horizon(10.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &IncompatibleNestedCosts,
        )
        .unwrap();

        assert_eq!(plan.deployments.len(), 2);
        let schedules: HashSet<_> = plan
            .deployments
            .iter()
            .map(|deployment| {
                deployment
                    .summary_maintenance_lifecycle_guarantee
                    .as_ref()
                    .unwrap()
                    .evaluation_schedule
            })
            .collect();
        assert_eq!(schedules.len(), 1);
    }

    #[test]
    fn repeated_at_rest_selects_shared_without_inventing_updates() {
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new_with_data(
                &workload(vec![], vec![repeating()], at_rest()),
                &at_rest(),
                &[0],
            ),
            1_000,
            Some(Horizon(10.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(
            selected_summary_maintenance_lifecycle(&plan.deployments[0]),
            Some(&SummaryMaintenanceLifecycle::Shared {
                retention: DurationMs(10_000)
            })
        );
        assert_eq!(
            plan.deployments[0]
                .summary_maintenance_lifecycle_guarantee
                .as_ref()
                .unwrap()
                .summary_maintenance_mode,
            SummaryMaintenanceMode::DirectBuild
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
            supports_shared: false,
            ..SummaryMaintenanceLifecycleCapabilities::ALL
        };
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new_with_data(
                &workload(vec![], vec![repeating()], continuous(1_000, 60_000)),
                &continuous(1_000, 60_000),
                &[0],
            ),
            1_000,
            Some(Horizon(10.0)),
            capabilities,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(
            selected_summary_maintenance_lifecycle(&plan.deployments[0]),
            Some(&SummaryMaintenanceLifecycle::ContinuouslyMaintained)
        );
        assert_eq!(
            plan.deployments[0]
                .summary_maintenance_lifecycle_guarantee
                .as_ref()
                .unwrap()
                .summary_maintenance_mode,
            SummaryMaintenanceMode::Incremental
        );
        assert_eq!(plan.evaluation_rate, Some(EvaluationRate(1.0)));
        assert_eq!(plan.update_rate, Some(UpdateRate(1.0)));
    }

    #[test]
    fn stale_ingestion_evidence_cannot_enable_continuous_maintenance() {
        let plan = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new_with_data(
                &workload(vec![], vec![repeating()], continuous(1_000, 1_000)),
                &continuous(1_000, 1_000),
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
            WorkloadDemand::new_without_data(
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
            selected_summary_maintenance_lifecycle(&plan.deployments[0]),
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
            WorkloadDemand::new_without_data(
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
            selected_summary_maintenance_lifecycle(&plan.deployments[0]),
            Some(&SummaryMaintenanceLifecycle::Ephemeral)
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
            WorkloadDemand::new_without_data(&workload(vec![], vec![entry], at_rest()), &[0]),
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
                WorkloadDemand::new_without_data(&workload, &[]),
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
                WorkloadDemand::new_without_data(&workload, &[0, 0]),
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
            WorkloadDemand::new_without_data(&workload, &[0, 1]),
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
            WorkloadDemand::new_with_data(
                &workload(vec![], vec![entry], continuous(1_000, 60_000)),
                &continuous(1_000, 60_000),
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
    fn lifecycle_cost_can_fall_back_to_raw_recomputation() {
        let target = sum_query();
        let space = crate::replacement::search_workload(vec![("q", Rc::clone(&target))]);
        let selection = space.global_selection(&RawCheaper);
        let workload = workload(vec![batch(Predictability::AdHoc)], vec![], at_rest());
        let plan = assemble_selected_dag_with_summary_maintenance_lifecycles(
            &selection,
            &space.roots[0].1,
            WorkloadDemand::new_without_data(&workload, &[0]),
            1_000,
            None,
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &RawCheaper,
        )
        .unwrap()
        .unwrap();
        assert!(plan.selected_raw_recompute);
        assert_eq!(plan.raw_recompute_total_cost, Some(Cost(1.0)));
        assert_eq!(plan.summary_total_cost, None);
        assert!(plan.deployments.is_empty());
        // The logical query stays exact; deployment assigns execution timing.
        assert!(!plan.root.contains_asap());
        assert!(matches!(
            plan.root.non_asap(),
            Some(NonASAPOp::Aggregate { .. })
        ));
        assert!(plan.root.timing.is_none());

        let exported =
            crate::summary_maintenance_dag_export::export_summary_maintenance_plan(&plan);
        assert!(exported.selected_raw_recompute);
        assert_eq!(exported.raw_recompute_total_cost, Some(1.0));
        assert_eq!(exported.summary_total_cost, None);
        assert!(exported.deployments.is_empty());
    }

    #[test]
    fn whole_candidate_cost_is_evaluated_before_selecting_a_lifecycle() {
        let root = summary();
        let mut deployments = vec![SummaryMaintenanceDeployment {
            post_asap_node_id: asap_types::ir::export::LogicalASAPNodeId(0),
            summary: Rc::clone(&root),
            summary_maintenance_lifecycle_guarantee: None,
            selected_window_framework: None,
            alternatives: vec![
                SummaryMaintenanceLifecycleAlternative {
                    summary_maintenance_lifecycle: SummaryMaintenanceLifecycle::Ephemeral,
                    total_cost: Some(Cost(1.0)),
                    rejection: None,
                    assumptions: vec![],
                },
                SummaryMaintenanceLifecycleAlternative {
                    summary_maintenance_lifecycle:
                        SummaryMaintenanceLifecycle::ContinuouslyMaintained,
                    total_cost: Some(Cost(10.0)),
                    rejection: None,
                    assumptions: vec![],
                },
            ],
        }];

        let total = select_complete_lifecycle_combination(
            &root,
            &mut deployments,
            &[0],
            DataArrival::ContinuouslyIngesting,
            &WholeCandidatePrefersContinuous,
            None,
            Some(Horizon(10.0)),
            Some(2.0),
            &[],
        );

        assert_eq!(total.map(|estimate| estimate.cost), Some(Cost(1.0)));
        assert!(matches!(
            deployments[0]
                .summary_maintenance_lifecycle_guarantee
                .as_ref()
                .unwrap()
                .summary_maintenance_lifecycle,
            SummaryMaintenanceLifecycle::ContinuouslyMaintained
        ));
    }

    #[test]
    fn complete_lifecycle_enumeration_fails_closed_above_safe_bound() {
        let root = summary();
        let alternatives = vec![
            SummaryMaintenanceLifecycleAlternative {
                summary_maintenance_lifecycle: SummaryMaintenanceLifecycle::Ephemeral,
                total_cost: Some(Cost(1.0)),
                rejection: None,
                assumptions: vec![],
            },
            SummaryMaintenanceLifecycleAlternative {
                summary_maintenance_lifecycle: SummaryMaintenanceLifecycle::ContinuouslyMaintained,
                total_cost: Some(Cost(2.0)),
                rejection: None,
                assumptions: vec![],
            },
        ];
        let mut deployments: Vec<_> = (0..13)
            .map(|summary_index| SummaryMaintenanceDeployment {
                post_asap_node_id: asap_types::ir::export::LogicalASAPNodeId(summary_index as u32),
                summary: Rc::clone(&root),
                summary_maintenance_lifecycle_guarantee: None,
                selected_window_framework: None,
                alternatives: alternatives.clone(),
            })
            .collect();
        assert_eq!(
            select_complete_lifecycle_combination(
                &root,
                &mut deployments,
                &(0..13).collect::<Vec<_>>(),
                DataArrival::ContinuouslyIngesting,
                &WholeCandidatePrefersContinuous,
                None,
                Some(Horizon(10.0)),
                Some(2.0),
                &[],
            ),
            None
        );
        assert!(deployments
            .iter()
            .all(|deployment| deployment.summary_maintenance_lifecycle_guarantee.is_none()));
    }

    #[test]
    fn materialization_falls_back_to_raw_when_raw_cost_is_unavailable() {
        let target = quantile_query();
        let space = crate::replacement::search_workload(vec![("q", target)]);
        let selection = space.global_selection(&UnitCosts);
        let workload = workload(vec![batch(Predictability::AdHoc)], vec![], at_rest());
        let plan = assemble_selected_dag_with_summary_maintenance_lifecycles(
            &selection,
            &space.roots[0].1,
            WorkloadDemand::new_without_data(&workload, &[0]),
            1_000,
            None,
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap()
        .unwrap();
        assert!(plan.selected_raw_recompute);
        assert!(plan.raw_recompute_total_cost.is_none());
        assert!(!plan.root.contains_asap());
        assert!(matches!(
            plan.root.non_asap(),
            Some(NonASAPOp::Aggregate { .. })
        ));
    }

    #[test]
    fn unmatched_target_is_reported_as_raw_recomputation() {
        let target = query_root();
        let space = crate::replacement::search_workload(vec![("q", Rc::clone(&target))]);
        let selection = space.global_selection(&RawCheaper);
        let workload = workload(vec![batch(Predictability::AdHoc)], vec![], at_rest());
        let plan = assemble_selected_dag_with_summary_maintenance_lifecycles(
            &selection,
            &space.roots[0].1,
            WorkloadDemand::new_without_data(&workload, &[0]),
            1_000,
            None,
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &RawCheaper,
        )
        .unwrap()
        .unwrap();

        assert!(plan.selected_raw_recompute);
        assert_eq!(plan.raw_recompute_total_cost, Some(Cost(1.0)));
        assert_eq!(plan.summary_total_cost, None);
        assert!(plan.deployments.is_empty());
        assert!(!plan.root.contains_asap());
        assert!(matches!(plan.root.non_asap(), Some(NonASAPOp::Scan { .. })));
    }

    #[test]
    fn lifecycle_cost_reorders_semantic_summary_candidates_before_materialization() {
        let target = quantile_query();
        let space = crate::replacement::search_workload(vec![("q", target)]);
        let workload = workload(vec![batch(Predictability::AdHoc)], vec![], at_rest());

        let selection = global_selection_with_summary_maintenance_lifecycles(
            &space,
            WorkloadDemand::new_with_data(&workload, &at_rest(), &[0]),
            1_000,
            None,
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &SummaryMaintenancePrefersDdSketch,
        )
        .unwrap();
        let materialized = selection
            .assemble_selected_dag(&space.roots[0].1)
            .unwrap()
            .unwrap();

        assert_eq!(
            sketch_algorithm(&materialized),
            Some(SketchAlgorithm::DDSketch)
        );
    }

    #[test]
    fn lifecycle_cost_counts_one_shared_summary_node_once() {
        // One shared exact accumulator read twice by the same root: a
        // query-time `sum + sum` over one finalized state. (`SummaryMerge`
        // is reserved in the unified IR, so the sharing is expressed through
        // a relational consumer instead.)
        let shared = summary();
        let finalized = Rc::new(
            OperatorNode::new(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
                child: Rc::clone(&shared),
            }))
            .unwrap(),
        );
        let root =
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::BinaryOp {
                operator: BinaryOperator {
                    checked_relative_division: false,
                    checked_finite_division: false,
                    kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Add),
                    vector_match: None,
                },
                return_bool: false,
                lhs: Rc::clone(&finalized),
                rhs: finalized,
            }))
            .unwrap();
        let workload = workload(
            vec![batch(Predictability::AdHoc), batch(Predictability::AdHoc)],
            vec![],
            at_rest(),
        );
        let horizon = Some(Horizon(10.0));
        let plan = plan_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new_with_data(&workload, &at_rest(), &[0, 1]),
            1_000,
            horizon,
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(plan.deployments.len(), 1);
        assert!(matches!(
            selected_summary_maintenance_lifecycle(&plan.deployments[0]),
            Some(SummaryMaintenanceLifecycle::Shared { .. })
        ));
    }

    /// A state costs 10 however often it is read. Recomputing p50 raw costs
    /// 1 and p99 costs 8.
    struct P50PrefersRaw;

    impl CostModel for P50PrefersRaw {
        fn rank_candidates(
            &self,
            _intent: &AggIntent,
            candidates: &[SketchAlgorithm],
        ) -> Vec<SketchAlgorithm> {
            candidates.to_vec()
        }

        fn summary_maintenance_lifecycle_cost_inputs(
            &self,
            _summary: &OperatorNode,
        ) -> SummaryMaintenanceLifecycleCostInputs {
            SummaryMaintenanceLifecycleCostInputs {
                build_cost: Some(Cost(10.0)),
                maintenance_cost_per_update: Some(Cost::ZERO),
                summary_read_cost: Some(Cost::ZERO),
                retention_cost_rate: Some(CostRate(0.0)),
                retirement_cost: Some(Cost::ZERO),
            }
        }

        fn summary_maintenance_capabilities(
            &self,
            summary: &OperatorNode,
        ) -> SummaryMaintenanceCapabilities {
            UnitCosts.summary_maintenance_capabilities(summary)
        }

        fn raw_query_recompute_total_cost(
            &self,
            target: &OperatorNode,
            _expected_reads: f64,
        ) -> Option<Cost> {
            match target.non_asap() {
                Some(NonASAPOp::Aggregate { measures, .. }) => match measures[..] {
                    [AggIntent::Quantile { q: 0.5, .. }] => Some(Cost(1.0)),
                    _ => Some(Cost(8.0)),
                },
                _ => None,
            }
        }
    }

    /// p50 and p99 form a sharing class over one state (5 each), but p50's
    /// raw recompute (1) still wins. The class reverts, so p99 is reselected
    /// at its independent cost (10) and recomputes raw (8), as it does alone.
    /// Checked at selection: the assembled plan's own raw comparison would
    /// recompute p99 raw either way.
    #[test]
    fn sharing_class_reverts_when_a_member_selects_elsewhere() {
        let quantile = |q| {
            OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: Reduction::by(vec![]),
                measures: vec![AggIntent::Quantile {
                    col: None,
                    q,
                    accuracy: AccuracyTarget::Epsilon(0.1),
                }],
                // A shared output name keeps p50 and p99 on one state.
                output_names: vec!["value".into()],
                filters: vec![],
                having: None,
                child: query_root(),
            }))
            .unwrap()
        };
        let workload = workload(vec![], vec![repeating(), repeating()], at_rest());
        // Whether each root selected a summary rather than raw recompute.
        let summaries = |space: &CandidateLogicalASAPDAGs<&str>, entries: &[usize]| {
            let selection = global_selection_with_summary_maintenance_lifecycles(
                space,
                WorkloadDemand::new_with_data(&workload, &at_rest(), entries),
                1_000,
                Some(Horizon(10.0)),
                SummaryMaintenanceLifecycleCapabilities::ALL,
                &P50PrefersRaw,
            )
            .unwrap();
            space
                .roots
                .iter()
                .map(|(_, target)| selection.for_target(target).unwrap().chosen.is_some())
                .collect::<Vec<_>>()
        };

        let space = crate::replacement::search_workload(vec![
            ("p50", quantile(0.5)),
            ("p99", quantile(0.99)),
        ]);
        let alone = crate::replacement::search_workload(vec![("p99", quantile(0.99))]);
        assert_eq!(summaries(&space, &[0, 1]), vec![false, false]);
        assert_eq!(summaries(&alone, &[1]), vec![false]);
    }

    #[test]
    fn normalized_workload_drives_candidate_logical_asap_dags_recurrence_profiles() {
        let root = query_root();
        let space = crate::replacement::search_workload(vec![("dashboard", Rc::clone(&root))]);
        let workload = workload(vec![], vec![repeating()], continuous(1_000, 60_000));
        let profiles = space
            .recurrence_profiles_from_workload(
                &workload,
                Some(&continuous(1_000, 60_000)),
                &[0],
                1_000,
                Some(Horizon(10.0)),
            )
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
            .recurrence_profiles_from_workload(&workload, None, &[1, 0], 1_000, Some(Horizon(10.0)))
            .unwrap();
        let dashboard = profiles.for_target(&space.roots[0].1);
        let batch = profiles.for_target(&space.roots[1].1);
        assert_eq!(dashboard.evaluation_rate, Some(EvaluationRate(1.0)));
        assert_eq!(dashboard.one_shot_consumers, 0);
        assert_eq!(batch.evaluation_rate, None);
        assert_eq!(batch.one_shot_consumers, 1);
    }

    fn continuous_candidates<'a>(
        workload: &QueryWorkload,
        data: &DataWorkload,
        model: &'a dyn CostModel,
    ) -> SummaryMaintenanceLifecycleCandidates<'a> {
        enumerate_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new_with_data(workload, data, &[0]),
            1_000,
            Some(Horizon(10.0)),
            SummaryMaintenanceLifecycleCapabilities {
                supports_shared: false,
                ..SummaryMaintenanceLifecycleCapabilities::ALL
            },
            model,
        )
        .unwrap()
    }

    fn choose(
        candidates: &SummaryMaintenanceLifecycleCandidates<'_>,
        lifecycle: SummaryMaintenanceLifecycle,
    ) -> Vec<(PhysicalASAPNodeId, SummaryMaintenanceLifecycle)> {
        candidates
            .deployments()
            .iter()
            .map(|deployment| (deployment.post_asap_node_id, lifecycle.clone()))
            .collect()
    }

    // Enumeration reports all four lifecycle kinds with their rejections and
    // selects nothing.
    #[test]
    fn enumeration_exposes_every_lifecycle_without_selecting() {
        let data = continuous(1_000, 60_000);
        let workload = workload(vec![], vec![repeating()], data.clone());
        let candidates = continuous_candidates(&workload, &data, &UnitCosts);
        let [deployment] = candidates.deployments() else {
            panic!("one summary state");
        };
        assert_eq!(deployment.summary_maintenance_lifecycle_guarantee, None);
        assert_eq!(deployment.selected_window_framework, None);
        let outcome: Vec<_> = deployment
            .alternatives
            .iter()
            .map(|alternative| {
                (
                    &alternative.summary_maintenance_lifecycle,
                    alternative.rejection.clone(),
                    alternative.total_cost.is_some(),
                )
            })
            .collect();
        assert!(matches!(
            outcome.as_slice(),
            [
                (SummaryMaintenanceLifecycle::Ephemeral, None, true),
                (
                    SummaryMaintenanceLifecycle::Prepared { .. },
                    Some(SummaryMaintenanceLifecycleRejection::RequiresPredictableOneTimeQuery),
                    false
                ),
                (
                    SummaryMaintenanceLifecycle::Shared { .. },
                    Some(SummaryMaintenanceLifecycleRejection::UnsupportedByRuntime),
                    false
                ),
                (
                    SummaryMaintenanceLifecycle::ContinuouslyMaintained,
                    None,
                    true
                ),
            ]
        ));
        let guarantee = candidates.guarantee(&SummaryMaintenanceLifecycle::ContinuouslyMaintained);
        assert_eq!(
            guarantee.summary_maintenance_mode,
            SummaryMaintenanceMode::Incremental
        );
        assert_eq!(guarantee.evaluation_schedule, EvaluationSchedule::PerUpdate);
    }

    // Explicitly choosing Planner's own selection reproduces Planner's plan.
    #[test]
    fn explicit_choice_of_planner_selection_reproduces_planner_plan() {
        let data = continuous(1_000, 60_000);
        let workload = workload(vec![], vec![repeating()], data.clone());
        let planned = plan_summary_maintenance_lifecycles(
            summary(),
            WorkloadDemand::new_with_data(&workload, &data, &[0]),
            1_000,
            Some(Horizon(10.0)),
            SummaryMaintenanceLifecycleCapabilities {
                supports_shared: false,
                ..SummaryMaintenanceLifecycleCapabilities::ALL
            },
            &UnitCosts,
        )
        .unwrap();
        let candidates = continuous_candidates(&workload, &data, &UnitCosts);
        let choice = choose(
            &candidates,
            SummaryMaintenanceLifecycle::ContinuouslyMaintained,
        );
        let chosen = candidates.select(&choice).unwrap();
        assert_eq!(format!("{chosen:?}"), format!("{planned:?}"));
    }

    // A deployment may bind a legal alternative Planner's estimate does not
    // prefer; the plan carries that alternative's guarantee and cost.
    #[test]
    fn explicit_choice_may_bind_a_costlier_legal_alternative() {
        let data = continuous(1_000, 60_000);
        let workload = workload(vec![], vec![repeating()], data.clone());
        let candidates = continuous_candidates(&workload, &data, &UnitCosts);
        let ephemeral_cost = candidates.deployments()[0].alternatives[0].total_cost;
        let choice = choose(&candidates, SummaryMaintenanceLifecycle::Ephemeral);
        let plan = candidates.select(&choice).unwrap();
        assert_eq!(
            selected_summary_maintenance_lifecycle(&plan.deployments[0]),
            Some(&SummaryMaintenanceLifecycle::Ephemeral)
        );
        assert_eq!(plan.summary_total_cost, ephemeral_cost);
    }

    // Choices that Planner could not select, or that do not cover exactly the
    // enumerated states, are refused rather than bound.
    #[test]
    fn explicit_choice_rejects_illegal_or_incomplete_choices() {
        use SummaryMaintenanceLifecycleChoiceError as E;
        let data = continuous(1_000, 60_000);
        let workload = workload(vec![], vec![repeating()], data.clone());
        let select = |model: &dyn CostModel, choice: &dyn Fn(PhysicalASAPNodeId) -> Vec<_>| {
            let candidates = continuous_candidates(&workload, &data, model);
            let id = candidates.deployments()[0].post_asap_node_id;
            (id, candidates.select(&choice(id)).unwrap_err())
        };
        let shared = SummaryMaintenanceLifecycle::Shared {
            retention: DurationMs(10_000),
        };
        let (id, error) = select(&UnitCosts, &|id| vec![(id, shared.clone())]);
        assert_eq!(
            error,
            E::Rejected {
                post_asap_node_id: id,
                rejection: Some(SummaryMaintenanceLifecycleRejection::UnsupportedByRuntime),
            }
        );
        let continuous = SummaryMaintenanceLifecycle::ContinuouslyMaintained;
        let (id, error) = select(&crate::cost_model::DefaultCostModel, &|id| {
            vec![(id, SummaryMaintenanceLifecycle::Ephemeral)]
        });
        assert_eq!(
            error,
            E::Rejected {
                post_asap_node_id: id,
                rejection: Some(SummaryMaintenanceLifecycleRejection::MissingCostEvidence),
            }
        );
        let (id, error) = select(&UnitCosts, &|id| {
            vec![(
                id,
                SummaryMaintenanceLifecycle::Shared {
                    retention: DurationMs(1),
                },
            )]
        });
        assert_eq!(error, E::NotAnAlternative(id));
        let (id, error) = select(&UnitCosts, &|_| vec![]);
        assert_eq!(error, E::MissingChoice(id));
        let (id, error) = select(&UnitCosts, &|id| {
            vec![(id, continuous.clone()), (id, continuous.clone())]
        });
        assert_eq!(error, E::DuplicateChoice(id));
        let (_, error) = select(&UnitCosts, &|_| {
            vec![(
                asap_types::ir::export::LogicalASAPNodeId(u32::MAX),
                continuous.clone(),
            )]
        });
        assert_eq!(
            error,
            E::UnknownSummary(asap_types::ir::export::LogicalASAPNodeId(u32::MAX))
        );
    }

    // Nested states on one maintenance path must share an evaluation schedule.
    #[test]
    fn explicit_choice_rejects_incompatible_nested_schedules() {
        let workload = workload(vec![], vec![repeating()], continuous(1_000, 20_000));
        let candidates = enumerate_summary_maintenance_lifecycles(
            nested_summary(),
            WorkloadDemand::new_with_data(&workload, &continuous(1_000, 20_000), &[0]),
            1_000,
            Some(Horizon(10.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &IncompatibleNestedCosts,
        )
        .unwrap();
        let [outer, inner] = candidates.deployments() else {
            panic!("two summary states");
        };
        let choice = vec![
            (
                outer.post_asap_node_id,
                SummaryMaintenanceLifecycle::Ephemeral,
            ),
            (
                inner.post_asap_node_id,
                SummaryMaintenanceLifecycle::ContinuouslyMaintained,
            ),
        ];
        assert_eq!(
            candidates.select(&choice).unwrap_err(),
            SummaryMaintenanceLifecycleChoiceError::IncompatibleEvaluationSchedules
        );
    }

    // A multi-summary root yields one candidate entry per unique state, with
    // a shared `Rc` state listed once.
    #[test]
    fn enumeration_lists_each_unique_summary_state_once() {
        let shared = summary();
        let root = test_binary(
            test_binary(evaluation(&shared), evaluation(&shared)),
            evaluation(&summary()),
        );
        let workload = workload(vec![batch(Predictability::AdHoc)], vec![], at_rest());
        let candidates = enumerate_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new_with_data(&workload, &at_rest(), &[0]),
            1_000,
            None,
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        let ids: HashSet<_> = candidates
            .deployments()
            .iter()
            .map(|deployment| deployment.post_asap_node_id)
            .collect();
        assert_eq!(candidates.deployments().len(), 2);
        assert_eq!(ids.len(), 2);
        assert!(candidates
            .deployments()
            .iter()
            .any(|deployment| Rc::ptr_eq(&deployment.summary, &shared)));
    }

    fn evaluation(state: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        std::rc::Rc::new(
            OperatorNode::with_schema(
                asap_types::ir::Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
                    child: Rc::clone(state),
                }),
                Schema::lifted(
                    vec![Field::new(
                        "value",
                        FieldDataType::Plain(DataType::Float64),
                        false,
                    )],
                    None,
                ),
            )
            .with_guarantee(Some(ResultGuarantee::exact("sum"))),
        )
    }

    fn test_binary(lhs: Rc<OperatorNode>, rhs: Rc<OperatorNode>) -> Rc<OperatorNode> {
        let schema = lhs.schema.clone();
        Rc::new(OperatorNode::with_schema(
            Operator::NonASAP(NonASAPOp::BinaryOp {
                lhs,
                rhs,
                return_bool: false,
                operator: BinaryOperator {
                    kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Add),
                    vector_match: None,
                    checked_relative_division: false,
                    checked_finite_division: false,
                },
            }),
            schema,
        ))
    }

    fn lifecycle_matching(
        alternatives: &[SummaryMaintenanceLifecycleAlternative],
        kind: fn(&SummaryMaintenanceLifecycle) -> bool,
    ) -> SummaryMaintenanceLifecycle {
        alternatives
            .iter()
            .map(|alternative| &alternative.summary_maintenance_lifecycle)
            .find(|lifecycle| kind(lifecycle))
            .expect("lifecycle kind is an alternative")
            .clone()
    }

    /// Bind the lifecycle `choose` picks for every state of `root`, then
    /// derive the timed DAG.
    fn timed_dag(
        root: Rc<OperatorNode>,
        workload: &QueryWorkload,
        data: &DataWorkload,
        horizon: Option<Horizon>,
        choose: impl Fn(&SummaryMaintenanceDeployment) -> SummaryMaintenanceLifecycle,
    ) -> PhysicalASAPDAG {
        let candidates = enumerate_summary_maintenance_lifecycles(
            root,
            WorkloadDemand::new_with_data(workload, data, &[0]),
            1_000,
            horizon,
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        let choice: Vec<_> = candidates
            .deployments()
            .iter()
            .map(|deployment| (deployment.post_asap_node_id, choose(deployment)))
            .collect();
        let dag = candidates
            .select(&choice)
            .unwrap()
            .execution_timed_dag()
            .unwrap();
        dag.validate().unwrap();
        dag
    }

    /// Operator kinds in node-id order, each paired with its timing.
    fn timings(dag: &PhysicalASAPDAG) -> Vec<(&'static str, ExecutionTiming)> {
        dag.nodes
            .iter()
            .map(|node| {
                let kind = match node.payload {
                    PhysicalASAPOperatorPayload::Relational {
                        operator: NonASAPOpKind::BinaryOp { .. },
                    } => "binary",
                    PhysicalASAPOperatorPayload::Relational { .. } => "raw",
                    PhysicalASAPOperatorPayload::SummaryAgg { .. } => "state",
                    PhysicalASAPOperatorPayload::FinalizeExactAccumulator
                    | PhysicalASAPOperatorPayload::EvaluatePopulation { .. } => "evaluation",

                    _ => "other",
                };
                (kind, node.output_state.timing)
            })
            .collect()
    }

    const INGEST: ExecutionTiming = ExecutionTiming::IngestionTime;
    const QUERY: ExecutionTiming = ExecutionTiming::QueryTime;

    // Every retained lifecycle kind runs its state and inputs at ingestion
    // time and its evaluation at query time.
    #[test]
    fn retained_lifecycles_time_state_and_inputs_at_ingestion() {
        let mut scheduled = batch(Predictability::Predictable {
            known_at: Some(TimestampMs(1_000)),
        });
        scheduled.execute_at = Some(TimestampMs(11_000));
        type Case = (
            QueryWorkload,
            DataWorkload,
            Option<Horizon>,
            fn(&SummaryMaintenanceLifecycle) -> bool,
        );
        let cases: [Case; 3] = [
            (
                workload(vec![], vec![repeating()], continuous(1_000, 60_000)),
                continuous(1_000, 60_000),
                Some(Horizon(10.0)),
                |lifecycle| {
                    matches!(
                        lifecycle,
                        SummaryMaintenanceLifecycle::ContinuouslyMaintained
                    )
                },
            ),
            (
                workload(vec![], vec![repeating()], at_rest()),
                at_rest(),
                Some(Horizon(10.0)),
                |lifecycle| matches!(lifecycle, SummaryMaintenanceLifecycle::Shared { .. }),
            ),
            (
                workload(vec![scheduled], vec![], at_rest()),
                at_rest(),
                None,
                |lifecycle| matches!(lifecycle, SummaryMaintenanceLifecycle::Prepared { .. }),
            ),
        ];
        for (workload, data, horizon, kind) in cases {
            let dag = timed_dag(
                evaluation(&summary()),
                &workload,
                &data,
                horizon,
                |deployment| lifecycle_matching(&deployment.alternatives, kind),
            );
            assert_eq!(
                timings(&dag),
                [("raw", INGEST), ("state", INGEST), ("evaluation", QUERY)]
            );
        }
    }

    // An Ephemeral state, its raw input, and its evaluation all run at query time.
    #[test]
    fn ephemeral_lifecycle_times_state_and_downstream_at_query() {
        let workload = workload(vec![batch(Predictability::AdHoc)], vec![], at_rest());
        let dag = timed_dag(evaluation(&summary()), &workload, &at_rest(), None, |_| {
            SummaryMaintenanceLifecycle::Ephemeral
        });
        assert_eq!(
            timings(&dag),
            [("raw", QUERY), ("state", QUERY), ("evaluation", QUERY)]
        );
    }

    // One state read by two consumers is one deployment; its timing follows
    // that single choice while both consumers run at query time.
    #[test]
    fn shared_state_is_timed_once_for_all_consumers() {
        let state = summary();
        let lhs = evaluation(&state);
        let rhs = Rc::new(lhs.as_ref().clone());
        let root = test_binary(lhs, rhs);
        let data = continuous(1_000, 60_000);
        let workload = workload(vec![], vec![repeating()], data.clone());
        let dag = timed_dag(root, &workload, &data, Some(Horizon(10.0)), |deployment| {
            assert!(Rc::ptr_eq(&deployment.summary, &state));
            SummaryMaintenanceLifecycle::ContinuouslyMaintained
        });
        assert_eq!(
            timings(&dag),
            [
                ("raw", INGEST),
                ("state", INGEST),
                ("evaluation", QUERY),
                ("evaluation", QUERY),
                ("binary", QUERY),
            ]
        );
    }

    // An Ephemeral state consumed by retained state is built on the retained
    // state's ingestion path; it is not retained, but cannot run at query time.
    #[test]
    fn ephemeral_state_feeding_retained_state_runs_at_ingestion() {
        let mut scheduled = batch(Predictability::Predictable {
            known_at: Some(TimestampMs(1_000)),
        });
        scheduled.execute_at = Some(TimestampMs(11_000));
        let root = nested_summary();
        let workload = workload(vec![scheduled], vec![], at_rest());
        let dag = timed_dag(
            Rc::clone(&root),
            &workload,
            &at_rest(),
            None,
            |deployment| {
                if Rc::ptr_eq(&deployment.summary, &root) {
                    lifecycle_matching(&deployment.alternatives, |lifecycle| {
                        matches!(lifecycle, SummaryMaintenanceLifecycle::Prepared { .. })
                    })
                } else {
                    SummaryMaintenanceLifecycle::Ephemeral
                }
            },
        );
        assert_eq!(
            timings(&dag),
            [("raw", INGEST), ("state", INGEST), ("state", INGEST)]
        );
    }

    // Timing is not derived for a state without a selected lifecycle, and a
    // raw-recompute plan runs entirely at query time.
    #[test]
    fn timing_requires_a_selected_lifecycle_for_every_state() {
        let workload = workload(vec![batch(Predictability::AdHoc)], vec![], at_rest());
        let data = at_rest();
        let demand = WorkloadDemand::new_with_data(&workload, &data, &[0]);
        let plan = plan_summary_maintenance_lifecycles(
            evaluation(&summary()),
            demand,
            1_000,
            None,
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &crate::cost_model::DefaultCostModel,
        )
        .unwrap();
        assert_eq!(
            plan.execution_timed_dag().unwrap_err(),
            SummaryMaintenanceTimingError::UnselectedLifecycle(
                plan.deployments[0].post_asap_node_id
            )
        );
        let raw = plan_summary_maintenance_lifecycles(
            crate::replacement::retain_exact(&sum_query()).unwrap(),
            demand,
            1_000,
            None,
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        assert_eq!(
            timings(&raw.execution_timed_dag().unwrap()),
            [("raw", QUERY), ("raw", QUERY)]
        );
    }

    /// A strategy-built `sum(a)` over one maintained current-series population.
    fn population_evaluation() -> Rc<OperatorNode> {
        let target = crate::test_support::lower_promql("sum(a)", AccuracyTarget::Exact);
        crate::maintained_population::MaintainedPopulationStrategy::new(std::slice::from_ref(
            &target,
        ))
        .candidate(&target)
        .unwrap()
    }

    fn is_population(node: &OperatorNode) -> bool {
        matches!(
            node.operator,
            Operator::ASAP(ASAPOp::MaintainPopulation { .. })
        )
    }

    fn population_timings(dag: &PhysicalASAPDAG) -> Vec<(&'static str, ExecutionTiming)> {
        dag.nodes
            .iter()
            .zip(timings(dag))
            .map(|(node, (kind, timing))| match node.payload {
                PhysicalASAPOperatorPayload::MaintainPopulation { .. } => ("population", timing),
                _ => (kind, timing),
            })
            .collect()
    }

    // A maintained population is enumerated as retained state, with costs
    // from the caller's model for both the maintained and the rebuilt choice.
    #[test]
    fn enumeration_includes_maintained_population() {
        let data = continuous(1_000, 60_000);
        let workload = workload(vec![], vec![repeating()], data.clone());
        let candidates = enumerate_summary_maintenance_lifecycles(
            population_evaluation(),
            WorkloadDemand::new_with_data(&workload, &data, &[0]),
            1_000,
            Some(Horizon(10.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        let [deployment] = candidates.deployments() else {
            panic!("one population state");
        };
        assert!(is_population(&deployment.summary));
        let cost = |lifecycle: SummaryMaintenanceLifecycle| {
            deployment
                .alternatives
                .iter()
                .find(|alternative| alternative.summary_maintenance_lifecycle == lifecycle)
                .and_then(|alternative| alternative.total_cost)
        };
        // Ephemeral: (build 10 + read 1 + retire 1) x 10 reads. Maintained over
        // 10 s at 1 update/s: build 10 + updates 10 + reads 10 + retention 1 + retire 1.
        assert_eq!(
            cost(SummaryMaintenanceLifecycle::Ephemeral),
            Some(Cost(120.0))
        );
        assert_eq!(
            cost(SummaryMaintenanceLifecycle::ContinuouslyMaintained),
            Some(Cost(32.0))
        );
    }

    // Without cost evidence a population's alternatives stay unknown: Planner
    // selects none and timing is refused rather than guessed.
    #[test]
    fn population_without_cost_evidence_stays_unselected() {
        let data = continuous(1_000, 60_000);
        let workload = workload(vec![], vec![repeating()], data.clone());
        let plan = plan_summary_maintenance_lifecycles(
            population_evaluation(),
            WorkloadDemand::new_with_data(&workload, &data, &[0]),
            1_000,
            Some(Horizon(10.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &crate::cost_model::DefaultCostModel,
        )
        .unwrap();
        let [deployment] = plan.deployments.as_slice() else {
            panic!("one population state");
        };
        assert!(deployment
            .alternatives
            .iter()
            .all(|alternative| alternative.total_cost.is_none()));
        assert!(deployment.summary_maintenance_lifecycle_guarantee.is_none());
        assert_eq!(
            plan.execution_timed_dag().unwrap_err(),
            SummaryMaintenanceTimingError::UnselectedLifecycle(deployment.post_asap_node_id)
        );
    }

    // A retained population and its raw input run at ingestion time; an
    // Ephemeral population is rebuilt from raw input at query time.
    #[test]
    fn population_lifecycle_choice_decides_its_timing() {
        let data = continuous(1_000, 60_000);
        let workload = workload(vec![], vec![repeating()], data.clone());
        let timed = |lifecycle: SummaryMaintenanceLifecycle| {
            population_timings(&timed_dag(
                population_evaluation(),
                &workload,
                &data,
                Some(Horizon(10.0)),
                |_| lifecycle.clone(),
            ))
        };
        assert_eq!(
            timed(SummaryMaintenanceLifecycle::ContinuouslyMaintained),
            [
                ("raw", INGEST),
                ("raw", INGEST),
                ("population", INGEST),
                ("evaluation", QUERY)
            ]
        );
        assert_eq!(
            timed(SummaryMaintenanceLifecycle::Ephemeral),
            [
                ("raw", QUERY),
                ("raw", QUERY),
                ("population", QUERY),
                ("evaluation", QUERY)
            ]
        );
    }

    // When retaining is cheaper, Planner's own selection keeps the population
    // maintained at ingestion time, as realization strategies placed it before
    // population timing became a lifecycle decision.
    #[test]
    fn planner_selection_retains_population_at_ingestion() {
        let data = continuous(1_000, 60_000);
        let workload = workload(vec![], vec![repeating()], data.clone());
        let plan = plan_summary_maintenance_lifecycles(
            population_evaluation(),
            WorkloadDemand::new_with_data(&workload, &data, &[0]),
            1_000,
            Some(Horizon(10.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        // Shared and ContinuouslyMaintained tie at 32; the first wins.
        assert!(matches!(
            selected_summary_maintenance_lifecycle(&plan.deployments[0]),
            Some(SummaryMaintenanceLifecycle::Shared { .. })
        ));
        assert_eq!(
            population_timings(&plan.execution_timed_dag().unwrap()),
            [
                ("raw", INGEST),
                ("raw", INGEST),
                ("population", INGEST),
                ("evaluation", QUERY)
            ]
        );
    }

    // A population feeding summary state is that state's input, not a separate
    // deployment: the state's lifecycle times it.
    #[test]
    fn population_feeding_summary_state_follows_that_state() {
        let Operator::ASAP(ASAPOp::EvaluatePopulation {
            child: population, ..
        }) = &population_evaluation().operator
        else {
            unreachable!()
        };
        let state = summary();
        let Operator::ASAP(ASAPOp::SummaryAgg {
            family,
            input,
            reduction,
            grouping,
            ..
        }) = &state.operator
        else {
            unreachable!()
        };
        let state = Rc::new(OperatorNode {
            operator: Operator::ASAP(ASAPOp::SummaryAgg {
                child: Rc::clone(population),
                family: family.clone(),
                input: input.clone(),
                reduction: reduction.clone(),
                grouping: grouping.clone(),
                filter: None,
            }),
            ..state.as_ref().clone()
        });
        let data = continuous(1_000, 60_000);
        let workload = workload(vec![], vec![repeating()], data.clone());
        let timed = |lifecycle: SummaryMaintenanceLifecycle| {
            population_timings(&timed_dag(
                evaluation(&state),
                &workload,
                &data,
                Some(Horizon(10.0)),
                |deployment| {
                    assert!(Rc::ptr_eq(&deployment.summary, &state));
                    lifecycle.clone()
                },
            ))
        };
        assert_eq!(
            timed(SummaryMaintenanceLifecycle::ContinuouslyMaintained),
            [
                ("raw", INGEST),
                ("raw", INGEST),
                ("population", INGEST),
                ("state", INGEST),
                ("evaluation", QUERY)
            ]
        );
        assert_eq!(
            timed(SummaryMaintenanceLifecycle::Ephemeral),
            [
                ("raw", QUERY),
                ("raw", QUERY),
                ("population", QUERY),
                ("state", QUERY),
                ("evaluation", QUERY)
            ]
        );
    }

    // A population both read directly and consumed by summary state is that
    // state's input in either traversal order: not a separate deployment, and
    // timed by the state's lifecycle.
    #[test]
    fn shared_population_follows_its_summary_consumer() {
        let direct = population_evaluation();
        let Operator::ASAP(ASAPOp::EvaluatePopulation {
            child: population, ..
        }) = &direct.operator
        else {
            unreachable!()
        };
        let state = summary();
        let Operator::ASAP(ASAPOp::SummaryAgg {
            family,
            input,
            reduction,
            grouping,
            ..
        }) = &state.operator
        else {
            unreachable!()
        };
        let state = Rc::new(OperatorNode {
            operator: Operator::ASAP(ASAPOp::SummaryAgg {
                child: Rc::clone(population),
                family: family.clone(),
                input: input.clone(),
                reduction: reduction.clone(),
                grouping: grouping.clone(),
                filter: None,
            }),
            ..state.as_ref().clone()
        });
        let binary = test_binary;
        let data = continuous(1_000, 60_000);
        let workload = workload(vec![], vec![repeating()], data.clone());
        for root in [
            binary(Rc::clone(&direct), evaluation(&state)),
            binary(evaluation(&state), Rc::clone(&direct)),
        ] {
            for lifecycle in [
                SummaryMaintenanceLifecycle::ContinuouslyMaintained,
                SummaryMaintenanceLifecycle::Ephemeral,
            ] {
                let dag = timed_dag(
                    Rc::clone(&root),
                    &workload,
                    &data,
                    Some(Horizon(10.0)),
                    |deployment| {
                        assert!(Rc::ptr_eq(&deployment.summary, &state));
                        lifecycle.clone()
                    },
                );
                let expected = if lifecycle == SummaryMaintenanceLifecycle::Ephemeral {
                    QUERY
                } else {
                    INGEST
                };
                for (kind, timing) in population_timings(&dag) {
                    if matches!(kind, "raw" | "population" | "state") {
                        assert_eq!(timing, expected, "{kind}");
                    } else {
                        assert_eq!(timing, QUERY, "{kind}");
                    }
                }
            }
        }
    }

    // A plan whose population deployment was removed after enumeration is
    // refused rather than timed by a guess.
    #[test]
    fn timing_refuses_population_without_deployment() {
        let data = continuous(1_000, 60_000);
        let workload = workload(vec![], vec![repeating()], data.clone());
        let mut plan = plan_summary_maintenance_lifecycles(
            population_evaluation(),
            WorkloadDemand::new_with_data(&workload, &data, &[0]),
            1_000,
            Some(Horizon(10.0)),
            SummaryMaintenanceLifecycleCapabilities::ALL,
            &UnitCosts,
        )
        .unwrap();
        let id = plan.deployments.remove(0).post_asap_node_id;
        assert_eq!(
            plan.execution_timed_dag(),
            Err(SummaryMaintenanceTimingError::UnplannedMaintainedState(id))
        );
    }
}
