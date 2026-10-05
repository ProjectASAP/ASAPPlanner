//! `asap-plan-selection` — #509 Stage 3 (MVP): plan selection, the only stage
//! that computes cost. Cargo enforces the stage order: this crate depends on
//! `asap-types`, Stage 1 and Stage 2, never on the facade or the executor.
//!
//! - [`cost`] — analytical pricing, evaluation rates from recurrence, and the
//!   physical lowering and storage I/O profiles a deployment can price.
//!
//! Each Stage 2 candidate is checked against every query's accuracy target
//! with the accuracy model, and Count-Min is admitted only over weights proven
//! non-negative; a miss rejects the candidate as invalid, with a reason.
//! Every valid candidate is priced node by node over its physical DAG, so a
//! node shared by several queries is charged once, and the cheapest is
//! selected. The rest are reported valid but costlier. A candidate that
//! cannot be built or priced is rejected with its reason; it does not fail
//! the selection.
//!
//! Cost is per second of wall time (`docs/design_docs/proposals/stage3-cost-model.md`):
//! an ingestion-time node over the ingestion rate, a query-time node per
//! evaluation times the evaluation rate of the roots reaching it ([`RootDemand`]),
//! plus memory for ingestion-time state that query time reads and for
//! tumbling panes kept across evaluations at query time. Prices come
//! from [`crate::cost::analytical_cost::estimate_operator`] over edge
//! statistics derived from the [`DataWorkload`] and a fixed default group
//! count, weighted by [`Stage3Calibration`]; summary build and estimation are
//! priced as rows × sketch depth and rows read out. These numbers are
//! illustrative, not calibrated. A query's latency bound is checked against
//! the query-time work it waits for in one evaluation. A candidate needing
//! a capability the deployment lacks ([`DeploymentCapabilities`]) is
//! rejected, and so is one retaining more than the deployment's memory
//! budget. When the deployment does not keep raw data anyway, a plan reading
//! raw data at query time pays for retaining it (Q48).
//!
//! A logical candidate has several physical candidates, one per Stage 2
//! materialization choice; selection takes the cheapest valid one.
//! [`select_plan`] chooses over Stage 1's sharing variants without building
//! every combination: per variant, a dynamic program over target nesting (see
//! there). [`select_exhaustive`] builds and prices every combination, for
//! display and for checking the program. [`plan_stages`] runs the whole
//! pipeline from the frontends' roots.
pub mod cost;
#[cfg(test)]
mod test_support;

pub use asap_types::deployment::DeploymentCapabilities;
pub use cost::recurrence::{evaluation_rate_of, EvaluationRate, RecurrenceError};

use asap_types::ir::NonASAPOp;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use asap_types::ir::operator::Reduction;
use asap_types::ir::physical_export::{
    PhysicalASAPDAG, PhysicalASAPNodeId, PhysicalASAPOperatorPayload as Payload,
};
use asap_types::ir::schema::{DataType, Schema};
use asap_types::ir::schema::{
    FieldDataType, SketchAlgorithm, SketchParams, SketchStatistic, WeightDomain,
};
use asap_types::ir::{ASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::workload::{DataWorkload, QueryRecurrence, RepeatedDemand, RootDemand};
use thiserror::Error;

use crate::cost::analytical_cost::{
    estimate_operator, AnalyticalCostError, PhysicalOperator, ResourceCalibration, ResourceEstimate,
};
use crate::cost::physical_operator_statistics::{
    EdgeStatistics, OperatorStatistics, PartitionStatistics, UnaryEdgeStatistics,
};
use asap_logical_optimizer::accuracy::{
    AccuracyEvidenceProvider, AccuracyModel, DefaultAccuracyModel, NoAccuracyEvidence,
};
use asap_logical_optimizer::pass1::logical_candidates::{
    choice_index, combination_count, compose_logical_candidate, enumerate_choices, nested_targets,
    read_targets, LocalLogicalCandidates, LogicalCandidateError,
};
use asap_logical_optimizer::pass2::identical_expressions::{
    share_identical_expressions, stage1_logical_candidates,
};
pub use asap_logical_optimizer::pass2::identical_expressions::{Sharing, SharingVariant};
use asap_logical_optimizer::pass2::window_composition::{pane_source, WindowForm};
use asap_physical_optimizer::implementation::physical_candidates::{
    stage2_physical, PhysicalCandidate, Stage2Candidates,
};
use asap_physical_optimizer::materialization::MAX_PHYSICAL_PER_LOGICAL;

/// Cost per second of wall time; one cost unit is one CPU-millisecond under
/// [`Stage3Calibration::ILLUSTRATIVE`]. See
/// `docs/design_docs/proposals/stage3-cost-model.md`.
pub const COST_PER_SECOND: &str = "cost_per_second";
/// The analytical cost model Stage 3 prices with, as reported in each
/// candidate's cost `source`.
pub const COST_MODEL: &str = "analytical-cost-v2";

/// Groups assumed for every `by (...)` reduction, absent group-count evidence.
const DEFAULT_GROUP_COUNT: u64 = 100;
/// Used only when the data workload does not declare them.
const DEFAULT_SERIES: u64 = 1_000;
const DEFAULT_ROWS_PER_SECOND: f64 = 1_000.0;
const DEFAULT_LOOKBACK_MS: u64 = 60_000;
/// Most combinations built for display, and for selection when the dynamic
/// program's assumptions do not hold.
pub const MAX_ENUMERATED_CANDIDATES: usize = 64;

static DEFAULT_ACCURACY_MODEL: DefaultAccuracyModel = DefaultAccuracyModel;
static NO_ACCURACY_EVIDENCE: NoAccuracyEvidence = NoAccuracyEvidence;
static UNRESTRICTED: DeploymentCapabilities = DeploymentCapabilities::UNRESTRICTED;

/// Stage 3's price coefficients and amortization horizon. Values are
/// illustrative until calibrated from measurements.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stage3Calibration {
    pub cost_per_cpu_op: f64,
    pub cost_per_scan_byte: f64,
    /// Price of state retained across evaluations, per byte per second.
    pub cost_per_retained_byte_second: f64,
    /// Seconds over which one-off and unknown recurrence is amortized.
    pub horizon_s: f64,
    /// Response time of one cost unit of query-time work in one evaluation,
    /// for the latency check: one CPU-ms, run on one core.
    pub latency_ms_per_cost_unit: f64,
    pub version: &'static str,
}

impl Stage3Calibration {
    /// 1 ns of CPU per operation; 1 GB retained costs 1/8 vCPU
    /// (125 CPU-ms per second); one-off work is amortized over 1 h; an
    /// evaluation runs on one core, so a cost unit (a CPU-ms) is 1 ms of
    /// latency.
    pub const ILLUSTRATIVE: Self = Self {
        cost_per_cpu_op: 1e-6,
        cost_per_scan_byte: 1e-7,
        cost_per_retained_byte_second: 1.25e-7,
        horizon_s: 3_600.0,
        latency_ms_per_cost_unit: 1.0,
        version: "illustrative-v2",
    };

    fn validate(&self) -> Result<(), AnalyticalCostError> {
        for (name, value) in [
            ("cost_per_cpu_op", self.cost_per_cpu_op),
            ("cost_per_scan_byte", self.cost_per_scan_byte),
            ("latency_ms_per_cost_unit", self.latency_ms_per_cost_unit),
            (
                "cost_per_retained_byte_second",
                self.cost_per_retained_byte_second,
            ),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(AnalyticalCostError::InvalidCalibration(name, value));
            }
        }
        if !self.horizon_s.is_finite() || self.horizon_s <= 0.0 {
            return Err(AnalyticalCostError::InvalidCalibration(
                "horizon_s",
                self.horizon_s,
            ));
        }
        Ok(())
    }
}

/// Planning logic, as opposed to the scoped facts it consumes: a model can have
/// a built-in default, evidence about a particular deployment cannot. With
/// the deployment's capabilities, these are #509's deployment inputs.
/// Stage 3 prices plans analytically ([`Stage3Calibration`]).
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct PlanningModels<'a> {
    pub accuracy: &'a dyn AccuracyModel,
    pub evidence: &'a dyn AccuracyEvidenceProvider,
    pub calibration: Stage3Calibration,
    /// What the deployment can build, read out and keep; unrestricted by
    /// default.
    pub capabilities: &'a DeploymentCapabilities,
}

impl<'a> PlanningModels<'a> {
    pub fn new(
        accuracy: &'a dyn AccuracyModel,
        evidence: &'a dyn AccuracyEvidenceProvider,
    ) -> Self {
        Self {
            accuracy,
            evidence,
            calibration: Stage3Calibration::ILLUSTRATIVE,
            capabilities: &UNRESTRICTED,
        }
    }

    /// The built-in models. Stage 3's calibration is illustrative, not a
    /// measured deployment cost.
    pub fn builtin() -> PlanningModels<'static> {
        PlanningModels {
            accuracy: &DEFAULT_ACCURACY_MODEL,
            evidence: &NO_ACCURACY_EVIDENCE,
            calibration: Stage3Calibration::ILLUSTRATIVE,
            capabilities: &UNRESTRICTED,
        }
    }

    pub fn with_accuracy(mut self, accuracy: &'a dyn AccuracyModel) -> Self {
        self.accuracy = accuracy;
        self
    }

    pub fn with_evidence(mut self, evidence: &'a dyn AccuracyEvidenceProvider) -> Self {
        self.evidence = evidence;
        self
    }

    pub fn with_calibration(mut self, calibration: Stage3Calibration) -> Self {
        self.calibration = calibration;
        self
    }

    pub fn with_capabilities(mut self, capabilities: &'a DeploymentCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeCost {
    pub cost: f64,
    /// Estimated output rows.
    pub rows: u64,
    pub detail: String,
}

/// Whole-workload cost of one candidate: one entry per DAG node, `total` is
/// their sum.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateCost {
    pub total: f64,
    pub unit: &'static str,
    /// The cost model and its calibration version.
    pub source: String,
    pub per_node: BTreeMap<PhysicalASAPNodeId, NodeCost>,
}

/// A candidate that was not selected. `valid == false`: it failed a check or
/// could not be built or priced; `true`: it lost on cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Rejection {
    pub id: String,
    pub valid: bool,
    pub reason: String,
}

/// How the selected candidate was found.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectionMethod {
    /// Every candidate was built and priced.
    Exhaustive,
    /// The dynamic program over target nesting, whose assumptions held.
    TreeDp,
    /// The dynamic program's result although its assumptions did not hold
    /// and there were too many combinations to enumerate, or Stage 2
    /// searched some candidate's materialization greedily.
    TreeDpNotGuaranteedOptimal { reason: String },
    /// Every combination was built, but Stage 2 searched some candidate's
    /// materialization greedily.
    ExhaustiveGreedyMaterialization { reason: String },
}

/// Costs are present for valid candidates only.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub selected: String,
    pub costs: BTreeMap<String, CandidateCost>,
    pub rejected: Vec<Rejection>,
    pub method: SelectionMethod,
}

impl Selection {
    /// Whether no valid candidate is cheaper than the selected one.
    pub fn guaranteed_optimal(&self) -> bool {
        !matches!(
            self.method,
            SelectionMethod::TreeDpNotGuaranteedOptimal { .. }
                | SelectionMethod::ExhaustiveGreedyMaterialization { .. }
        )
    }
}

#[derive(Debug, Error)]
pub enum SelectionError {
    #[error("no valid candidate: {0:?}")]
    NoValidCandidate(Vec<Rejection>),
    #[error("Stage 1: {0}")]
    Stage1(#[from] LogicalCandidateError),
}

/// Reject candidates that miss a query's accuracy target or cannot be priced,
/// price the rest and select the cheapest (the first on ties). `demand[i]`
/// is the demand of `candidate.roots[i]`.
pub fn stage3_select(
    cands: &[PhysicalCandidate],
    demand: &[RootDemand],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<Selection, SelectionError> {
    let mut costs = BTreeMap::new();
    let mut rejected = Vec::new();
    let mut best: Option<(&str, f64)> = None;
    for candidate in cands {
        match assess(candidate, demand, data, &models) {
            Ok(cost) => {
                if best.is_none_or(|(_, total)| cost.total < total) {
                    best = Some((&candidate.id, cost.total));
                }
                costs.insert(candidate.id.clone(), cost);
            }
            Err(reason) => rejected.push(Rejection {
                id: candidate.id.clone(),
                valid: false,
                reason,
            }),
        }
    }
    let Some((selected, best_total)) = best else {
        return Err(SelectionError::NoValidCandidate(rejected));
    };
    for candidate in cands {
        if candidate.id != selected && costs.contains_key(&candidate.id) {
            rejected.push(Rejection {
                id: candidate.id.clone(),
                valid: true,
                reason: format!(
                    "costlier: {:.3} vs {:.3} {COST_PER_SECOND}",
                    costs[&candidate.id].total, best_total
                ),
            });
        }
    }
    Ok(Selection {
        selected: selected.to_string(),
        costs,
        rejected,
        method: SelectionMethod::Exhaustive,
    })
}

/// Stage 3's checks and price for one candidate; `Err` is the rejection reason.
fn assess(
    candidate: &PhysicalCandidate,
    demand: &[RootDemand],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> Result<CandidateCost, String> {
    if candidate.roots.len() != demand.len() {
        return Err(format!(
            "{} roots but {} root demands (accuracy targets)",
            candidate.roots.len(),
            demand.len()
        ));
    }
    if let Some(reason) = capability_violation(candidate, models.capabilities) {
        return Err(reason);
    }
    if let Some(reason) = accuracy_violation(candidate, demand, models) {
        return Err(reason);
    }
    let capabilities = models.capabilities;
    let raw_bytes_per_sample =
        (!capabilities.raw_data_retained).then_some(capabilities.raw_bytes_per_sample);
    let priced = price_nodes(
        &candidate.dag,
        demand,
        data,
        &models.calibration,
        raw_bytes_per_sample,
    )
    .map_err(|(node, error)| format!("node {node:?}: {error}"))?;
    let Priced {
        cost,
        per_evaluation,
        retained_bytes,
    } = priced;
    if let Some(budget) = capabilities.memory_budget_bytes {
        if retained_bytes > budget {
            return Err(format!(
                "retains {retained_bytes} bytes across evaluations, over the deployment's \
                 memory budget of {budget} bytes"
            ));
        }
    }
    if let Some(reason) =
        latency_violation(&candidate.dag, &per_evaluation, demand, &models.calibration)
    {
        return Err(reason);
    }
    Ok(cost)
}

/// The first query whose query-time work in one evaluation exceeds its
/// latency bound (S6), as a reason. The estimate is the per-evaluation cost
/// of the query-time nodes the query reaches, run on one core: ingestion-time
/// work is done before the query asks.
fn latency_violation(
    dag: &PhysicalASAPDAG,
    per_evaluation: &HashMap<PhysicalASAPNodeId, f64>,
    demand: &[RootDemand],
    calibration: &Stage3Calibration,
) -> Option<String> {
    let reached = reaching_roots(dag);
    demand.iter().enumerate().find_map(|(query, demand)| {
        let bound = demand.latency_ms?;
        let work: f64 = per_evaluation
            .iter()
            .filter(|(id, _)| reached.get(id).is_some_and(|roots| roots.contains(&query)))
            .map(|(_, cost)| cost)
            .sum();
        let latency = work * calibration.latency_ms_per_cost_unit;
        (latency > bound).then(|| {
            format!(
                "q{}: query-time work takes {latency:.1} ms per evaluation, over the {bound} ms \
                 latency bound",
                query + 1
            )
        })
    })
}

/// One Stage 1 sharing variant as selection sees it: candidates of variant
/// `v` are numbered after every candidate of the variants before it, so ids
/// stay unique across variants.
struct Variant<'a, Id> {
    inventory: &'a LocalLogicalCandidates<Id>,
    sharing: Sharing,
    /// Candidates numbered before this variant's.
    offset: usize,
}

// Manual impls: a derive would require `Id: Copy`.
impl<Id> Clone for Variant<'_, Id> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<Id> Copy for Variant<'_, Id> {}

fn variants<Id>(stage1: &[SharingVariant<Id>]) -> Vec<Variant<'_, Id>> {
    let mut offset = 0;
    stage1
        .iter()
        .map(|v| {
            let variant = Variant {
                inventory: &v.inventory,
                sharing: v.sharing,
                offset,
            };
            offset = offset.saturating_add(combination_count(&v.inventory));
            variant
        })
        .collect()
}

impl<Id> Variant<'_, Id> {
    /// 1-based candidate number of `choice`: `L<n>` and `P<n>`.
    fn number(&self, choice: &[usize]) -> usize {
        self.offset + choice_index(self.inventory, choice) + 1
    }
}

/// The independent variant alone: Pass 1 without Pass 2.
pub fn independent<Id>(inventory: LocalLogicalCandidates<Id>) -> Vec<SharingVariant<Id>> {
    vec![SharingVariant {
        sharing: Sharing::Independent,
        inventory,
    }]
}

/// Stage 1 → Stage 2 for `choice` in the independent variant, all query
/// time, named `P<index+1>` from `L<index+1>`. `Err` is the reason the
/// candidate cannot be built.
pub fn realize_choice<Id: Clone>(
    inventory: &LocalLogicalCandidates<Id>,
    choice: &[usize],
) -> Result<(Vec<(Id, QueryRoot)>, PhysicalCandidate), String> {
    let (logical, mut stage2) = realize(
        Variant {
            inventory,
            sharing: Sharing::Independent,
            offset: 0,
        },
        choice,
        &[],
        &DataWorkload::default(),
        &PlanningModels::builtin(),
    )?;
    Ok((logical, stage2.candidates.swap_remove(0)))
}

/// Stage 1 → Stage 2 for `choice` in `variant`. A sharing variant also
/// merges identical sub-DAGs after composition, so queries that chose the
/// same summary producer reach one node; the returned Stage 1 candidate is
/// the merged one. The physical candidates are `P<n>` (all query time) and
/// `P<n>-m<k>` for each materialization choice; above
/// [`MAX_PHYSICAL_PER_LOGICAL`] choices Stage 2 searches them greedily by
/// Stage 3's cost.
fn realize<Id: Clone>(
    variant: Variant<'_, Id>,
    choice: &[usize],
    demand: &[RootDemand],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> Result<(Vec<(Id, QueryRoot)>, Stage2Candidates), String> {
    let index = variant.number(choice);
    let mut logical = compose_logical_candidate(variant.inventory, choice)
        .map_err(|e| format!("Stage 1: {e}"))?;
    if variant.sharing.merges_after_composition() {
        if let Some(merged) = share_identical_expressions(&logical) {
            logical = merged;
        }
    }
    let roots = logical
        .iter()
        .map(|(_, root)| match root {
            QueryRoot::Operator(node) => Ok(node.clone()),
            QueryRoot::Scalar(_) => Err("Stage 2: scalar query roots are not physical yet"),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let score = |candidate: &PhysicalCandidate| {
        assess(candidate, demand, data, models)
            .ok()
            .map(|cost| cost.total)
    };
    let mut stage2 = stage2_physical(&format!("L{index}"), &roots, demand, data, &score)
        .map_err(|e| format!("Stage 2: {e}"))?;
    for (k, candidate) in stage2.candidates.iter_mut().enumerate() {
        candidate.id = match k {
            0 => format!("P{index}"),
            k => format!("P{index}-m{k}"),
        };
        candidate.label = match candidate.materialization.as_str() {
            "" => format!("{choice:?}"),
            m => format!("{choice:?} · {m}"),
        };
    }
    Ok((logical, stage2))
}

/// Why a selection is not guaranteed optimal when Stage 2 searched a
/// candidate's materialization greedily.
fn greedy_reason(id: &str) -> String {
    format!("{id}: more than {MAX_PHYSICAL_PER_LOGICAL} materialization choices, searched greedily")
}

/// Stage 3 over the physical candidates of one logical candidate: its
/// cheapest valid cost, or the reason the first (all query time) fails.
fn best_total(
    candidates: &[PhysicalCandidate],
    demand: &[RootDemand],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<f64, String> {
    match stage3_select(candidates, demand, data, models) {
        Ok(selection) => Ok(selection.costs[&selection.selected].total),
        Err(SelectionError::NoValidCandidate(rejected)) => Err(rejected
            .into_iter()
            .next()
            .map_or_else(|| "no physical candidate".into(), |r| r.reason)),
        Err(other) => Err(other.to_string()),
    }
}

/// One built combination; `physical` is empty when it could not be built,
/// else its Stage 2 candidates, all query time first.
#[derive(Debug, Clone)]
pub struct EnumeratedCandidate<Id> {
    /// The Pass 2 variant it comes from.
    pub sharing: Sharing,
    pub choice: Vec<usize>,
    pub logical: Option<Vec<(Id, QueryRoot)>>,
    pub physical: Vec<PhysicalCandidate>,
}

/// Every combination [`select_exhaustive`] built, and Stage 3 over them.
#[derive(Debug, Clone)]
pub struct Enumeration<Id> {
    /// Over every variant.
    pub combinations: usize,
    pub candidates: Vec<EnumeratedCandidate<Id>>,
    pub selection: Selection,
}

/// Build the first `max` combinations, variant by variant in enumeration
/// order, and select over them. One that cannot be built is rejected with
/// its reason.
pub fn select_exhaustive<Id: Clone>(
    stage1: &[SharingVariant<Id>],
    demand: &[RootDemand],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    max: usize,
) -> Result<Enumeration<Id>, SelectionError> {
    exhaustive(&variants(stage1), demand, data, models, max)
}

fn exhaustive<Id: Clone>(
    variants: &[Variant<'_, Id>],
    demand: &[RootDemand],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    max: usize,
) -> Result<Enumeration<Id>, SelectionError> {
    let mut candidates = Vec::new();
    let mut failed = Vec::new();
    let mut greedy = None;
    for variant in variants {
        let left = max.saturating_sub(candidates.len());
        for choice in enumerate_choices(variant.inventory, left) {
            let (logical, physical) = match realize(*variant, &choice, demand, data, &models) {
                Ok((logical, stage2)) => {
                    if !stage2.exhaustive {
                        greedy.get_or_insert_with(|| greedy_reason(&stage2.candidates[0].id));
                    }
                    (Some(logical), stage2.candidates)
                }
                Err(reason) => {
                    failed.push(Rejection {
                        id: format!("P{}", variant.number(&choice)),
                        valid: false,
                        reason,
                    });
                    // Composition may have succeeded: keep it for display.
                    (
                        compose_logical_candidate(variant.inventory, &choice).ok(),
                        Vec::new(),
                    )
                }
            };
            candidates.push(EnumeratedCandidate {
                sharing: variant.sharing,
                choice,
                logical,
                physical,
            });
        }
    }
    let physical: Vec<_> = candidates
        .iter()
        .flat_map(|c| c.physical.iter().cloned())
        .collect();
    let selection = match stage3_select(&physical, demand, data, models) {
        Ok(mut selection) => {
            selection.rejected.extend(failed);
            if let Some(reason) = greedy {
                selection.method = SelectionMethod::ExhaustiveGreedyMaterialization { reason };
            }
            selection
        }
        Err(SelectionError::NoValidCandidate(mut rejected)) => {
            rejected.extend(failed);
            return Err(SelectionError::NoValidCandidate(rejected));
        }
        Err(other) => return Err(other),
    };
    Ok(Enumeration {
        combinations: variants.iter().fold(0usize, |n, v| {
            n.saturating_add(combination_count(v.inventory))
        }),
        candidates,
        selection,
    })
}

/// The plan [`select_plan`] chose.
#[derive(Debug, Clone)]
pub struct SelectedPlan<Id> {
    /// The Pass 2 variant it comes from.
    pub sharing: Sharing,
    pub choice: Vec<usize>,
    /// The chosen Stage 1 candidate (identical sub-DAGs merged unless
    /// `sharing` is independent).
    pub logical: Vec<(Id, QueryRoot)>,
    /// Stage 2 of `logical`; `roots` follow `logical`'s order.
    pub physical: PhysicalCandidate,
    pub selection: Selection,
}

/// Relative tolerance when checking that costs add up.
const ADDITIVITY_TOLERANCE: f64 = 1e-9;

/// Choose a sharing variant and one alternative per target. Sharing prices a
/// shared node once, which is not a sum of per-target changes, so the
/// variant is an outer choice: the dynamic program of [`select_variant`]
/// runs once per variant and the cheapest result wins (the first on ties).
pub fn select_plan<Id: Clone>(
    stage1: &[SharingVariant<Id>],
    demand: &[RootDemand],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<SelectedPlan<Id>, SelectionError> {
    let mut best: Option<SelectedPlan<Id>> = None;
    let mut costs = BTreeMap::new();
    let mut rejected = Vec::new();
    let mut not_optimal = None;
    let mut failures = Vec::new();
    for variant in variants(stage1) {
        // The program sees each logical candidate at its cheapest
        // materialization.
        let greedy = std::cell::RefCell::new(None);
        let evaluate = |choice: &[usize]| -> Result<f64, String> {
            let (_, stage2) = realize(variant, choice, demand, data, &models)?;
            if !stage2.exhaustive {
                greedy
                    .borrow_mut()
                    .get_or_insert_with(|| greedy_reason(&stage2.candidates[0].id));
            }
            best_total(&stage2.candidates, demand, data, models)
        };
        let plan = match select_variant(variant, demand, data, models, &evaluate) {
            Ok(plan) => plan,
            Err(SelectionError::NoValidCandidate(reasons)) => {
                failures.extend(reasons);
                continue;
            }
            Err(other) => return Err(other),
        };
        match &plan.selection.method {
            SelectionMethod::TreeDpNotGuaranteedOptimal { reason }
            | SelectionMethod::ExhaustiveGreedyMaterialization { reason } => {
                not_optimal.get_or_insert_with(|| reason.clone());
            }
            _ => {}
        }
        if let Some(reason) = greedy.into_inner() {
            not_optimal.get_or_insert(reason);
        }
        costs.extend(plan.selection.costs.clone());
        rejected.extend(plan.selection.rejected.clone());
        let total = |p: &SelectedPlan<Id>| p.selection.costs[&p.selection.selected].total;
        match &best {
            Some(current) if total(current) <= total(&plan) => rejected.push(costlier(
                &plan.selection.selected,
                total(&plan),
                total(current),
            )),
            _ => {
                if let Some(previous) = best.take() {
                    rejected.push(costlier(
                        &previous.selection.selected,
                        total(&previous),
                        total(&plan),
                    ));
                }
                best = Some(plan);
            }
        }
    }
    let Some(mut plan) = best else {
        return Err(SelectionError::NoValidCandidate(failures));
    };
    rejected.extend(failures);
    plan.selection.costs = costs;
    plan.selection.rejected = rejected;
    if let Some(reason) = not_optimal {
        plan.selection.method = SelectionMethod::TreeDpNotGuaranteedOptimal { reason };
    }
    Ok(plan)
}

fn costlier(id: &str, total: f64, best: f64) -> Rejection {
    Rejection {
        id: id.to_string(),
        valid: true,
        reason: format!("costlier: {total:.3} vs {best:.3} {COST_PER_SECOND}"),
    }
}

/// Choose one alternative per target of one variant by a dynamic program
/// over target nesting, then build the winner and check it in full.
///
/// `best(t, c) = local(t, c) + Σ_{u beneath t, read by c} min_c' best(u, c')`,
/// where `local(t, c)` is the change in workload cost when only `t` takes
/// alternative `c`, and a choice is admissible when that one-target
/// candidate builds and passes Stage 3's checks. Most realizations read
/// their target's rewritten input, so they read every target beneath it; a
/// whole-expression alternative absorbs the target beneath instead
/// ([`read_targets`]), which then contributes nothing and takes its
/// pass-through.
///
/// The result is the exhaustive minimum when (1) cost is a sum over nodes,
/// (2) a choice changes only its target's own nodes, and (3) shared nodes do
/// not depend on choices. Stage 3 prices per node and sizes every
/// realization of a target alike, so these hold unless a target's choice
/// changes what a target reading its output costs or whether it can be
/// built, or, in a sharing variant, two targets reading one input build
/// identical producers that are then merged. That coupling is checked:
/// every pair of choices for a target and a target beneath it (and, in a
/// sharing variant, for two targets reading a common or equal input) is
/// built, and must cost
/// the sum of their single changes and be admissible exactly when both are.
/// On coupling, or when the winner fails the full check, every combination
/// of the variant is built instead if there are at most
/// [`MAX_ENUMERATED_CANDIDATES`]; otherwise the result is flagged as not
/// guaranteed optimal.
fn select_variant<Id: Clone>(
    variant: Variant<'_, Id>,
    demand: &[RootDemand],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    evaluate: &dyn Fn(&[usize]) -> Result<f64, String>,
) -> Result<SelectedPlan<Id>, SelectionError> {
    let inventory = variant.inventory;
    let width = inventory.targets.len();
    let with = |changes: &[(usize, usize)]| {
        let mut choice = vec![0; width];
        for &(target, alternative) in changes {
            choice[target] = alternative;
        }
        choice
    };
    // Alternative 0 is always the pass-through, so the base is the raw plan.
    let base = match evaluate(&with(&[])) {
        Ok(base) => base,
        Err(reason) => {
            return fallback(variant, demand, data, models, with(&[]), reason);
        }
    };
    let local: Vec<Vec<Result<f64, String>>> = inventory
        .targets
        .iter()
        .enumerate()
        .map(|(t, target)| {
            (0..target.alternatives.len())
                .map(|c| match c {
                    0 => Ok(0.0),
                    _ => evaluate(&with(&[(t, c)])).map(|total| total - base),
                })
                .collect()
        })
        .collect();
    let beneath = nested_targets(inventory);
    let reads: Vec<Vec<Vec<usize>>> = (0..width)
        .map(|t| {
            (0..local[t].len())
                .map(|c| read_targets(inventory, &beneath, t, c))
                .collect()
        })
        .collect();
    // `(t, u, nested)`: `u` is read by `t`, or (not nested) both read a
    // common input.
    let mut pairs: Vec<(usize, usize, bool)> = Vec::new();
    for (t, by_choice) in reads.iter().enumerate() {
        for &u in by_choice.iter().flatten() {
            if !pairs.contains(&(t, u, true)) {
                pairs.push((t, u, true));
            }
        }
    }
    if variant.sharing.merges_after_composition() {
        pairs.extend(
            common_input_pairs(inventory, &beneath)
                .into_iter()
                .map(|(t, u)| (t, u, false)),
        );
    }
    let mut coupling = None;
    'pairs: for &(t, u, nested) in &pairs {
        for c in 1..local[t].len() {
            if nested && !reads[t][c].contains(&u) {
                // `c` does not read `u`'s output (it absorbs it, or reads a
                // target beneath it only through another).
                continue;
            }
            for d in 1..local[u].len() {
                let joint = evaluate(&with(&[(t, c), (u, d)]));
                let coupled = match (&local[t][c], &local[u][d], &joint) {
                    (Ok(a), Ok(b), Ok(joint)) => {
                        let expected = base + a + b;
                        (joint - expected).abs()
                            > ADDITIVITY_TOLERANCE * joint.abs().max(expected.abs()).max(1.0)
                    }
                    (Ok(_), Ok(_), Err(_)) | (Err(_), _, Ok(_)) | (_, Err(_), Ok(_)) => true,
                    _ => false,
                };
                if coupled {
                    coupling = Some(format!(
                        "target {t} alternative {c} and target {u} alternative {d} do not \
                         combine additively"
                    ));
                    break 'pairs;
                }
            }
        }
    }
    let mut best: Vec<Option<(f64, usize)>> = vec![None; width];
    for t in 0..width {
        best_choice(t, &local, &reads, &mut best);
    }
    let mut choice: Vec<usize> = best.iter().map(|b| b.map_or(0, |(_, c)| c)).collect();
    for t in 0..width {
        if let Some(u) = inventory.targets[t].absorbs[choice[t]] {
            choice[u] = 0;
        }
    }
    if let Some(reason) = coupling {
        return fallback(variant, demand, data, models, choice, reason);
    }
    match finish(
        variant,
        demand,
        data,
        &models,
        choice.clone(),
        SelectionMethod::TreeDp,
    ) {
        Ok(plan) => Ok(plan),
        Err(reason) => fallback(variant, demand, data, models, choice, reason),
    }
}

/// Pairs of targets, neither beneath the other, that read a common input
/// node, or equal ones, or whose tumbling forms read one scan: in a sharing
/// variant their producers (or panes) may be merged.
fn common_input_pairs<Id>(
    inventory: &LocalLogicalCandidates<Id>,
    beneath: &[Vec<usize>],
) -> Vec<(usize, usize)> {
    let inputs: Vec<Vec<&Rc<OperatorNode>>> = inventory
        .targets
        .iter()
        .map(|t| t.target.children())
        .collect();
    let same = |a: &Rc<OperatorNode>, b: &Rc<OperatorNode>| Rc::ptr_eq(a, b) || a == b;
    let panes: Vec<Option<&Rc<OperatorNode>>> = inventory
        .targets
        .iter()
        .map(|t| {
            t.windows
                .iter()
                .any(|w| *w != WindowForm::Whole)
                .then(|| pane_source(&t.target))
                .flatten()
        })
        .collect();
    let mut pairs = Vec::new();
    for t in 0..inputs.len() {
        for u in t + 1..inputs.len() {
            let nested = beneath[t].contains(&u) || beneath[u].contains(&t);
            if !nested
                && (inputs[t]
                    .iter()
                    .any(|a| inputs[u].iter().any(|b| same(a, b)))
                    || panes[t].zip(panes[u]).is_some_and(|(a, b)| same(a, b)))
            {
                pairs.push((t, u));
            }
        }
    }
    pairs
}

/// `best(t) = min_c local(t, c) + Σ_{u read by c} best(u)`, memoized; the
/// first alternative wins ties, as in enumeration order. Alternative 0 (the
/// pass-through) is admissible whenever the base plan is. `reads[t][c]` is
/// [`read_targets`].
fn best_choice(
    t: usize,
    local: &[Vec<Result<f64, String>>],
    reads: &[Vec<Vec<usize>>],
    best: &mut [Option<(f64, usize)>],
) -> f64 {
    if let Some((cost, _)) = best[t] {
        return cost;
    }
    let mut inner = Vec::with_capacity(local[t].len());
    for read in &reads[t] {
        inner.push(
            read.iter()
                .map(|&u| best_choice(u, local, reads, best))
                .sum::<f64>(),
        );
    }
    let (cost, choice) = local[t]
        .iter()
        .enumerate()
        .filter_map(|(c, cost)| cost.as_ref().ok().map(|cost| (cost + inner[c], c)))
        .fold(
            (f64::INFINITY, 0),
            |min, next| if next.0 < min.0 { next } else { min },
        );
    best[t] = Some((cost, choice));
    cost
}

/// Build `choice` in `variant` and run Stage 3 on its physical candidates:
/// the cheapest valid one is selected.
fn finish<Id: Clone>(
    variant: Variant<'_, Id>,
    demand: &[RootDemand],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
    choice: Vec<usize>,
    method: SelectionMethod,
) -> Result<SelectedPlan<Id>, String> {
    let (logical, stage2) = realize(variant, &choice, demand, data, models)?;
    let selection = match stage3_select(&stage2.candidates, demand, data, *models) {
        Ok(selection) => selection,
        Err(SelectionError::NoValidCandidate(rejected)) => {
            return Err(rejected
                .into_iter()
                .next()
                .map_or_else(|| "no physical candidate".into(), |r| r.reason))
        }
        Err(other) => return Err(other.to_string()),
    };
    let method = match (stage2.exhaustive, method) {
        (false, SelectionMethod::TreeDp) => SelectionMethod::TreeDpNotGuaranteedOptimal {
            reason: greedy_reason(&stage2.candidates[0].id),
        },
        (false, SelectionMethod::Exhaustive) => SelectionMethod::ExhaustiveGreedyMaterialization {
            reason: greedy_reason(&stage2.candidates[0].id),
        },
        (_, method) => method,
    };
    let physical = stage2
        .candidates
        .into_iter()
        .find(|c| c.id == selection.selected)
        .expect("the selected candidate is one of them");
    Ok(SelectedPlan {
        sharing: variant.sharing,
        choice,
        logical,
        selection: Selection {
            method,
            ..selection
        },
        physical,
    })
}

/// Selection when the dynamic program's result cannot be trusted: every
/// combination of the variant if there are few, else `choice` flagged with
/// `reason`.
fn fallback<Id: Clone>(
    variant: Variant<'_, Id>,
    demand: &[RootDemand],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    choice: Vec<usize>,
    reason: String,
) -> Result<SelectedPlan<Id>, SelectionError> {
    if combination_count(variant.inventory) <= MAX_ENUMERATED_CANDIDATES {
        let enumeration = exhaustive(&[variant], demand, data, models, MAX_ENUMERATED_CANDIDATES)?;
        let winner = enumeration
            .candidates
            .iter()
            .find(|c| {
                c.physical
                    .iter()
                    .any(|p| p.id == enumeration.selection.selected)
            })
            .expect("the selected candidate was built");
        let mut plan = finish(
            variant,
            demand,
            data,
            &models,
            winner.choice.clone(),
            SelectionMethod::Exhaustive,
        )
        .map_err(|reason| {
            SelectionError::NoValidCandidate(vec![Rejection {
                id: enumeration.selection.selected.clone(),
                valid: false,
                reason,
            }])
        })?;
        plan.selection = Selection {
            costs: enumeration.selection.costs,
            rejected: enumeration.selection.rejected,
            ..plan.selection
        };
        return Ok(plan);
    }
    let method = SelectionMethod::TreeDpNotGuaranteedOptimal {
        reason: reason.clone(),
    };
    finish(variant, demand, data, &models, choice.clone(), method).map_err(|failure| {
        SelectionError::NoValidCandidate(vec![Rejection {
            id: format!("P{}", variant.number(&choice)),
            valid: false,
            reason: format!("{reason}; {failure}"),
        }])
    })
}

/// What [`plan_stages`] produced.
#[derive(Debug, Clone)]
pub struct StagePipelineRun<Id> {
    /// Stage 1: Pass 1's alternatives per sharing variant (Pass 2).
    pub stage1: Vec<SharingVariant<Id>>,
    /// Stages 2 and 3 for the selected candidate ([`select_plan`]).
    pub plan: SelectedPlan<Id>,
    /// Every candidate built and priced, when requested for display.
    pub enumeration: Option<Enumeration<Id>>,
}

/// The #509 stage pipeline over `roots`: Stage 1 (Pass 1 and Pass 2's
/// identical-expression and summary-capability rules and the
/// window-composition rule's tumbling panes and shared segments, from each
/// root's `demand`),
/// Stage 2 and Stage 3. The facade and the
/// `stage_pipeline` devtool both run this. `display` builds and prices up to
/// that many candidates for display as well (0: none).
pub fn plan_stages<Id: Clone>(
    roots: Vec<(Id, QueryRoot)>,
    demand: &[RootDemand],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    display: usize,
) -> Result<StagePipelineRun<Id>, SelectionError> {
    let stage1 = stage1_logical_candidates(roots, &data.metric_types, demand)?;
    let plan = select_plan(&stage1, demand, data, models)?;
    let enumeration = match display {
        0 => None,
        max => Some(select_exhaustive(&stage1, demand, data, models, max)?),
    };
    Ok(StagePipelineRun {
        stage1,
        plan,
        enumeration,
    })
}

/// The first capability `candidate` needs that the deployment lacks, as a
/// reason: maintaining state at ingestion time, keeping query-time state
/// across evaluations, building a summary, or reading a statistic out of
/// one.
fn capability_violation(
    candidate: &PhysicalCandidate,
    capabilities: &DeploymentCapabilities,
) -> Option<String> {
    if !capabilities.ingestion_time
        && candidate
            .dag
            .nodes
            .iter()
            .any(|n| !n.output_state.timing.is_query_time())
    {
        return Some("deployment cannot maintain state at ingestion time".into());
    }
    if !capabilities.query_time_retention && candidate.dag.nodes.iter().any(|n| n.kept) {
        return Some(
            "deployment cannot keep query-time state across evaluations (query time, kept)".into(),
        );
    }
    // Summaries first: a readout of a summary that cannot be built is moot.
    let reasons = |node: &Rc<OperatorNode>, readouts: bool| match &node.operator {
        Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) if !readouts => {
            capabilities.missing_summary(family)
        }
        Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input,
            query: statistic,
        }) if readouts => summary_builds(summary_input)
            .into_iter()
            .flatten()
            .find_map(|build| match &build.operator {
                Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) => {
                    capabilities.missing_readout(family, statistic)
                }
                _ => None,
            }),
        _ => None,
    };
    [false, true].into_iter().find_map(|readouts| {
        candidate
            .roots
            .iter()
            .enumerate()
            .find_map(|(query, root)| {
                OperatorNode::reachable(root)
                    .iter()
                    .find_map(|node| reasons(node, readouts))
                    .map(|reason| format!("q{}: {reason}", query + 1))
            })
    })
}

/// The first summary estimate that misses its query's target, as a reason.
fn accuracy_violation(
    candidate: &PhysicalCandidate,
    demand: &[RootDemand],
    models: &PlanningModels<'_>,
) -> Option<String> {
    for (query, (root, demand)) in candidate.roots.iter().zip(demand).enumerate() {
        let Some(target) = &demand.accuracy else {
            continue;
        };
        for node in OperatorNode::reachable(root) {
            let Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input,
                query: statistic,
            }) = &node.operator
            else {
                continue;
            };
            let Some(builds) = summary_builds(summary_input) else {
                return Some(format!("q{}: estimate over a non-summary input", query + 1));
            };
            if let Some(reason) = builds
                .iter()
                .find_map(|build| build_violation(build, statistic, target, models))
            {
                return Some(format!("q{}: {reason}", query + 1));
            }
        }
    }
    None
}

/// The `SummaryAgg`s whose states `state` holds: itself, or the inputs of
/// a `SummaryMerge` (tumbling panes), recursively. `None` when another
/// operator produces it.
fn summary_builds(state: &Rc<OperatorNode>) -> Option<Vec<&Rc<OperatorNode>>> {
    match &state.operator {
        Operator::ASAP(ASAPOp::SummaryAgg { .. }) => Some(vec![state]),
        Operator::ASAP(ASAPOp::SummaryMerge { children }) => children
            .iter()
            .map(summary_builds)
            .collect::<Option<Vec<_>>>()
            .map(|builds| builds.concat()),
        _ => None,
    }
}

/// Why one summary build cannot answer `statistic` within `target`. A
/// merge of mergeable states keeps the family's guarantee
/// ([`FieldDataType::family_merges`], which `SummaryMerge` requires), so
/// each build is checked against the family's analytical guarantee.
fn build_violation(
    build: &OperatorNode,
    statistic: &SketchStatistic,
    target: &asap_types::types::AccuracyTarget,
    models: &PlanningModels<'_>,
) -> Option<String> {
    let Operator::ASAP(ASAPOp::SummaryAgg { family, input, .. }) = &build.operator else {
        unreachable!("summary_builds returns builds");
    };
    let name = family_name(family);
    // Count-Min's one-sided error bound assumes no negative updates.
    if matches!(family, FieldDataType::Sketch(kind, _)
            if matches!(kind.algorithm(), SketchAlgorithm::Cms | SketchAlgorithm::CmsWithHeap))
        && !matches!(input.weight_domain, WeightDomain::NonNegative { .. })
    {
        return Some(format!(
            "{name} needs non-negative update weights, and these are not proven non-negative"
        ));
    }
    let Some(guarantee) = models.accuracy.local_guarantee(family, statistic) else {
        return Some(format!("no accuracy model for {name}; target {target:?}"));
    };
    if !models.accuracy.answers(statistic, &guarantee) {
        return Some(format!(
            "{name} guarantees a {:?} bound, which does not bound {statistic:?}; target {target:?}",
            guarantee.metric
        ));
    }
    if !models.accuracy.satisfies(&guarantee, target) {
        return Some(format!(
            "{name} guarantees bound {:?}, failure probability {:?}, which misses target \
             {target:?} (analytical guarantee; no accuracy evidence)",
            guarantee.bound.evaluate(),
            guarantee.failure_probability.evaluate(),
        ));
    }
    None
}

fn family_name(family: &FieldDataType) -> String {
    use asap_types::ir::schema::GroupingStrategy;
    match family {
        FieldDataType::Sketch(_, GroupingStrategy::SharedMultiSubpopulation { kind, .. }) => {
            format!("{kind:?}")
        }
        FieldDataType::Sketch(kind, _) => format!("{:?}", kind.algorithm()),
        FieldDataType::ExactAggregate(kind, _) => format!("exact {kind:?} accumulator"),
        other => format!("{other:?}"),
    }
}

/// Statistics the analytical model needs, derived once per workload.
struct Shape {
    series: u64,
    /// λ, rows ingested per second.
    rows_per_second: f64,
}

/// λ: the declared ingestion rate, else one sample per series per ingestion
/// interval, else the default.
fn ingestion_rate(data: &DataWorkload) -> Result<f64, AnalyticalCostError> {
    let rate = match (
        data.ingestion_rate.value,
        data.input_cardinality.value,
        data.data_ingestion_interval.value,
    ) {
        (Some(rate), _, _) => rate.0,
        (None, Some(series), Some(interval)) if interval.0 > 0 => {
            series as f64 * 1_000.0 / interval.0 as f64
        }
        _ => DEFAULT_ROWS_PER_SECOND,
    };
    if rate.is_finite() && rate >= 0.0 {
        Ok(rate)
    } else {
        Err(AnalyticalCostError::InvalidIngestionRate(rate))
    }
}

/// Evaluations per second of a query-time node read by `roots`. Repeating
/// roots with equal intervals are evaluated together, so each interval
/// counts once. One-off, scheduled and unknown roots run as one batch: the
/// most invocations among them, amortized over `horizon_s`.
fn evaluation_rate<'d>(
    roots: impl IntoIterator<Item = &'d RootDemand>,
    horizon_s: f64,
) -> Result<f64, AnalyticalCostError> {
    let mut intervals = std::collections::BTreeSet::new();
    let mut estimated = 0.0;
    let mut invocations = 0u64;
    for demand in roots {
        match &demand.recurrence {
            QueryRecurrence::Repeated(
                RepeatedDemand::FixedInterval(interval)
                | RepeatedDemand::FixedIntervalAt { interval, .. },
            ) => {
                intervals.insert(*interval);
            }
            QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(estimate)) => {
                let rate = estimate.expected_rate.0;
                if !rate.is_finite() || rate < 0.0 {
                    return Err(AnalyticalCostError::InvalidRecurrence);
                }
                estimated += rate;
            }
            QueryRecurrence::Repeated(RepeatedDemand::Scheduled(times)) => {
                invocations = invocations.max(times.len() as u64);
            }
            QueryRecurrence::OneTime {
                invocations: count, ..
            } => invocations = invocations.max(*count),
            QueryRecurrence::Unknown => invocations = invocations.max(1),
        }
    }
    let repeated = evaluation_rate_of(intervals)
        .map_err(|_| AnalyticalCostError::InvalidRecurrence)?
        .map_or(0.0, |rate| rate.0);
    Ok(repeated + estimated + invocations as f64 / horizon_s)
}

/// For every node, the indices of the roots that reach it.
fn reaching_roots(dag: &PhysicalASAPDAG) -> HashMap<PhysicalASAPNodeId, Vec<usize>> {
    let mut reached: HashMap<PhysicalASAPNodeId, Vec<usize>> = HashMap::new();
    for (index, &root) in dag.roots.iter().enumerate() {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let roots = reached.entry(id).or_default();
            if roots.last() == Some(&index) {
                continue;
            }
            roots.push(index);
            stack.extend(
                dag.edges
                    .iter()
                    .filter(|e| e.consumer == id)
                    .map(|e| e.producer),
            );
        }
    }
    reached
}

/// How far back the time ranges reading `id` reach, in milliseconds: each
/// range plus the offsets between it and `id`. `None` when no range reads it.
/// A range feeding only kept panes (`roles`) is not read again.
fn scan_extent_ms(
    dag: &PhysicalASAPDAG,
    roles: &HashMap<PhysicalASAPNodeId, PaneRole>,
    id: PhysicalASAPNodeId,
    offset_ms: i64,
) -> Option<u64> {
    dag.edges
        .iter()
        .filter(|e| e.producer == id)
        .filter(|e| {
            !matches!(
                roles.get(&e.consumer),
                Some(PaneRole::Retained | PaneRole::FeedsRetained)
            )
        })
        .filter_map(|e| {
            let consumer = dag.nodes.iter().find(|n| n.id == e.consumer)?;
            match &consumer.payload {
                Payload::NonASAP(NonASAPOp::TimeShift { shift, .. }) => scan_extent_ms(
                    dag,
                    roles,
                    consumer.id,
                    offset_ms.saturating_add(shift.offset_ms),
                ),
                Payload::NonASAP(NonASAPOp::TimeRange { range, .. }) => {
                    Some((range.as_millis() as u64).saturating_add(offset_ms.max(0) as u64))
                }
                _ => None,
            }
        })
        .max()
}

/// The role of an ingestion-time or kept node in a chain of tumbling panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneRole {
    /// The pane being built: at ingestion time every arriving row lands in
    /// it, and it retains `panes` completed panes plus itself; kept, each
    /// evaluation builds it and keeps it with `panes − 1` older ones.
    Newest { panes: u64 },
    /// An older pane: the newest pane of an earlier evaluation, kept. It is
    /// not built again.
    Retained,
    /// Feeds only retained panes: its work was done for the newest pane.
    FeedsRetained,
}

/// Ingestion-time or kept panes merged by a `SummaryMerge`: pane `i` is
/// pane 0 shifted back by `i` widths, so the chain is one pane built (as
/// rows arrive, or when an evaluation reads it) and kept for the later
/// evaluations. The newest pane is the one with the smallest shift.
fn pane_roles(dag: &PhysicalASAPDAG) -> HashMap<PhysicalASAPNodeId, PaneRole> {
    let nodes: HashMap<_, _> = dag.nodes.iter().map(|n| (n.id, n)).collect();
    let producers = |id| {
        dag.edges
            .iter()
            .filter(move |e| e.consumer == id)
            .map(|e| e.producer)
    };
    let materialized =
        |id: PhysicalASAPNodeId| !nodes[&id].output_state.timing.is_query_time() || nodes[&id].kept;
    // The shift of a pane over `TimeRange` over `TimeShift`, else 0.
    let shift = |pane| {
        producers(pane)
            .flat_map(producers)
            .find_map(|id| match &nodes[&id].payload {
                Payload::NonASAP(NonASAPOp::TimeShift { shift, .. }) => Some(shift.offset_ms),
                _ => None,
            })
            .unwrap_or(0)
    };
    let mut roles: HashMap<PhysicalASAPNodeId, PaneRole> = HashMap::new();
    for merge in dag
        .nodes
        .iter()
        .filter(|n| matches!(n.payload, Payload::ASAP(ASAPOp::SummaryMerge { .. })))
    {
        let panes: Vec<_> = producers(merge.id)
            .filter(|&id| {
                materialized(id)
                    && matches!(nodes[&id].payload, Payload::ASAP(ASAPOp::SummaryAgg { .. }))
            })
            .collect();
        let Some(&newest) = panes.iter().min_by_key(|&&id| shift(id)) else {
            continue;
        };
        let count = panes.len() as u64;
        for &pane in &panes {
            let role = if pane == newest {
                PaneRole::Newest { panes: count }
            } else {
                PaneRole::Retained
            };
            // A pane newest in one merge stays newest; the longest chain sets
            // the retention.
            let merged = match (roles.get(&pane).copied(), role) {
                (Some(PaneRole::Newest { panes: a }), PaneRole::Newest { panes: b }) => {
                    PaneRole::Newest { panes: a.max(b) }
                }
                (Some(newest @ PaneRole::Newest { .. }), _) => newest,
                (_, role) => role,
            };
            roles.insert(pane, merged);
        }
    }
    // Consumers first: a node whose consumers all are retained panes, or
    // feed only those, does no work of its own.
    for node in dag.nodes.iter().rev() {
        if roles.contains_key(&node.id) {
            continue;
        }
        let mut consumers = dag
            .edges
            .iter()
            .filter(|e| e.producer == node.id)
            .peekable();
        if consumers.peek().is_some()
            && consumers.all(|e| {
                matches!(
                    roles.get(&e.consumer),
                    Some(PaneRole::Retained | PaneRole::FeedsRetained)
                )
            })
        {
            roles.insert(node.id, PaneRole::FeedsRetained);
        }
    }
    roles
}

/// Price every node of `dag` once, per second of wall time. Nodes are
/// exported children first, so each node's input statistics are known when
/// it is reached. An ingestion-time node is priced over one second of
/// ingested rows; a query-time node per evaluation, times its evaluation
/// rate. State an ingestion-time node keeps for query-time readers, and the
/// older panes a kept chain keeps, are also charged per retained byte per
/// second; a kept chain builds only its newest pane at each evaluation.
#[cfg(test)]
fn price(
    dag: &PhysicalASAPDAG,
    demand: &[RootDemand],
    data: &DataWorkload,
    calibration: &Stage3Calibration,
) -> Result<CandidateCost, (PhysicalASAPNodeId, AnalyticalCostError)> {
    price_nodes(dag, demand, data, calibration, None).map(|priced| priced.cost)
}

/// A candidate's price, and what Stage 3's checks read from pricing.
struct Priced {
    cost: CandidateCost,
    /// The cost of one evaluation of each query-time node.
    per_evaluation: HashMap<PhysicalASAPNodeId, f64>,
    /// Bytes retained across evaluations: ingestion-time state that query
    /// time reads, kept query-time panes, and raw data retained for
    /// query-time scans.
    retained_bytes: u64,
}

/// [`price`], and what the checks read. `raw_bytes_per_sample` is `Some`
/// when the deployment does not keep raw data anyway: each query-time scan
/// then retains its raw samples, priced as memory (Q48).
fn price_nodes(
    dag: &PhysicalASAPDAG,
    demand: &[RootDemand],
    data: &DataWorkload,
    calibration: &Stage3Calibration,
    raw_bytes_per_sample: Option<u64>,
) -> Result<Priced, (PhysicalASAPNodeId, AnalyticalCostError)> {
    let first = dag.roots.first().copied().unwrap_or(0);
    calibration.validate().map_err(|error| (first, error))?;
    let series = data
        .input_cardinality
        .value
        .unwrap_or(DEFAULT_SERIES)
        .max(1);
    let shape = Shape {
        series,
        rows_per_second: ingestion_rate(data).map_err(|error| (first, error))?,
    };
    let rows_per_ms = shape.rows_per_second / 1_000.0;
    let resources = ResourceCalibration {
        cost_per_cpu_op: calibration.cost_per_cpu_op,
        cost_per_scan_byte: calibration.cost_per_scan_byte,
        // Transient query-time memory is not priced.
        cost_per_retained_byte: 0.0,
        version: calibration.version.into(),
    };
    let reached = reaching_roots(dag);
    let nodes: HashMap<_, _> = dag.nodes.iter().map(|n| (n.id, n)).collect();
    let roles = pane_roles(dag);
    let raw_retention = raw_retention(dag, &roles, rows_per_ms, raw_bytes_per_sample);
    let mut retained_bytes = 0u64;
    let mut output: HashMap<PhysicalASAPNodeId, EdgeStatistics> = HashMap::new();
    let mut per_node = BTreeMap::new();
    let mut per_evaluation = HashMap::new();
    for node in &dag.nodes {
        let ingestion = !node.output_state.timing.is_query_time();
        let inputs: Vec<_> = dag
            .edges
            .iter()
            .filter(|e| e.consumer == node.id)
            .map(|e| output[&e.producer])
            .collect();
        let input = inputs
            .first()
            .copied()
            .unwrap_or(EdgeStatistics { rows: 1, bytes: 1 });
        let width = row_bytes(&node.output_schema);
        let edge = |rows: u64| EdgeStatistics {
            rows,
            bytes: rows * width,
        };
        let unary = |output| UnaryEdgeStatistics {
            input,
            output,
            promql: None,
        };
        // The groups a state keeps, bounded at query time by the rows (or,
        // for a merge, the input states) it is built from.
        let bounded_groups = |reduction: &Reduction, rows: u64| {
            let groups = match reduction {
                Reduction::Reduce(keys) if keys.keys().is_empty() && !keys.is_without() => 1,
                Reduction::Reduce(keys) if !keys.is_without() => DEFAULT_GROUP_COUNT,
                _ => shape.series,
            };
            // One second of ingested rows does not bound the groups a
            // maintained state holds.
            match ingestion {
                true => groups,
                false => groups.min(rows.max(1)),
            }
        };
        let groups = |reduction: &Reduction| bounded_groups(reduction, input.rows);
        let (out, estimate, detail) =
            match &node.payload {
                Payload::NonASAP(operator) => match operator {
                    NonASAPOp::Scan { .. } => {
                        // At ingestion time, one second of arriving rows; at
                        // query time, as far back as the ranges reading it reach.
                        let span_ms = match ingestion {
                            true => 1_000,
                            false => scan_extent_ms(dag, &roles, node.id, 0)
                                .unwrap_or(DEFAULT_LOOKBACK_MS),
                        };
                        let out = edge(scan_rows(rows_per_ms, span_ms));
                        let estimate = estimate_operator(
                            PhysicalOperator::Scan,
                            OperatorStatistics::Scan {
                                edges: UnaryEdgeStatistics {
                                    input: out,
                                    output: out,
                                    promql: None,
                                },
                                source_read_bytes: out.bytes,
                            },
                        );
                        (out, estimate, format!("scan {} samples", out.rows))
                    }
                    NonASAPOp::Aggregate {
                        reduction,
                        measures,
                        ..
                    } => {
                        let group_count = groups(reduction);
                        let keys = match reduction {
                            Reduction::Reduce(keys) => keys.keys().len() as u64,
                            _ => 1,
                        };
                        let out = edge(group_count);
                        let estimate = estimate_operator(
                            PhysicalOperator::HashAggregate {
                                grouping_key_count: keys,
                                accumulator_count: measures.len().max(1) as u64,
                            },
                            OperatorStatistics::HashAggregate {
                                edges: unary(out),
                                group_count,
                                key_bytes: 16 * keys,
                                accumulator_bytes_per_group: 8,
                            },
                        );
                        (
                            out,
                            estimate,
                            format!(
                                "hash aggregate {} rows into {group_count} groups",
                                input.rows
                            ),
                        )
                    }
                    NonASAPOp::Sort {
                        keys, partition_by, ..
                    } => {
                        let partitions = if partition_by.keys().is_empty() {
                            1
                        } else {
                            DEFAULT_GROUP_COUNT.min(input.rows.max(1))
                        };
                        let estimate = estimate_operator(
                            PhysicalOperator::InMemoryComparisonSort {
                                ordering_key_count: keys.len() as u64,
                                partitioned: partitions > 1,
                            },
                            OperatorStatistics::InMemoryComparisonSort {
                                edges: unary(input),
                                input_partitioning: split(input, partitions),
                            },
                        );
                        (
                            input,
                            estimate,
                            format!("sort {} rows in {partitions} partitions", input.rows),
                        )
                    }
                    NonASAPOp::Limit {
                        n,
                        offset,
                        partition_by,
                        ..
                    } => {
                        let partitions = partition_count(!partition_by.keys().is_empty());
                        let limit = n.map_or(u64::MAX, |n| (n as u64).saturating_mul(partitions));
                        let offset = (*offset as u64).saturating_mul(partitions);
                        let out = edge(selected_rows(input.rows.saturating_sub(offset), limit));
                        let estimate = estimate_operator(
                            PhysicalOperator::Limit { limit, offset },
                            OperatorStatistics::Limit { edges: unary(out) },
                        );
                        (out, estimate, format!("limit to {} rows", out.rows))
                    }
                    // At query time a range keeps only its own span of a longer
                    // scan. Rows are ordered by time, so it seeks to that span
                    // and is charged for the rows it keeps, not those it skips
                    // (Q66).
                    NonASAPOp::TimeRange { range, .. } if !ingestion => {
                        let rows = (rows_per_ms * range.as_millis() as f64).round() as u64;
                        let out = edge(input.rows.min(rows.max(1)));
                        (
                            out,
                            Ok(ResourceEstimate::new(out.rows as f64, width, 0)),
                            format!(
                                "time range {range:?}: keep {} of {} rows",
                                out.rows, input.rows
                            ),
                        )
                    }
                    // A shift only re-labels time: the executor folds it into
                    // the time bounds of the read below it, so it does no
                    // per-row work (Q66).
                    NonASAPOp::TimeShift { shift, .. } => (
                        input,
                        Ok(ResourceEstimate::new(0.0, 0, 0)),
                        format!(
                            "time shift {} ms: re-label {} rows, free",
                            shift.offset_ms, input.rows
                        ),
                    ),
                    other => {
                        let estimate = estimate_operator(
                            PhysicalOperator::PassThrough,
                            OperatorStatistics::PassThrough {
                                edges: unary(input),
                            },
                        );
                        let name = match other {
                            NonASAPOp::TimeRange { range, .. } => format!("time range {range:?}"),
                            _ => "operator".into(),
                        };
                        (input, estimate, format!("{name}: pass {} rows", input.rows))
                    }
                },
                Payload::ASAP(ASAPOp::SummaryAgg {
                    family, reduction, ..
                }) => {
                    let group_count = groups(reduction);
                    let (depth, state_bytes) = summary_shape(family);
                    let out = EdgeStatistics {
                        rows: group_count,
                        bytes: group_count * state_bytes,
                    };
                    let ops = input.rows as f64 * depth as f64;
                    (
                        out,
                        Ok(ResourceEstimate::new(ops, out.bytes, 0)),
                        format!(
                            "build {} into {group_count} states: {} rows x depth {depth}",
                            family_name(family),
                            input.rows
                        ),
                    )
                }
                Payload::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
                    let rows = match query {
                        // The logical result, as an exact Sort → Limit sizes it.
                        // Items are at most the series: a whole-expression
                        // sketch reads several samples per ranked item.
                        SketchStatistic::TopK { k } => {
                            let (summarized, grouped) = summarized_rows(dag, &output, node.id);
                            selected_rows(
                                summarized.min(shape.series),
                                (*k as u64).saturating_mul(partition_count(grouped)),
                            )
                        }
                        _ => input.rows,
                    };
                    let out = edge(rows);
                    (
                        out,
                        Ok(ResourceEstimate::new(out.rows as f64, 0, 0)),
                        format!("estimate {} rows from {} states", out.rows, input.rows),
                    )
                }
                // One merge per input state. The output holds the union of the
                // inputs' groups, bounded as one build over all their rows is,
                // so readers see what they would see over the whole window.
                Payload::ASAP(ASAPOp::SummaryMerge { .. }) => {
                    let merged: u64 = inputs.iter().map(|edge| edge.rows).sum();
                    let reduction = dag.edges.iter().filter(|e| e.consumer == node.id).find_map(
                        |e| match &nodes[&e.producer].payload {
                            Payload::ASAP(ASAPOp::SummaryAgg { reduction, .. }) => Some(reduction),
                            _ => None,
                        },
                    );
                    let rows = reduction.map_or(merged, |r| bounded_groups(r, merged));
                    let out = EdgeStatistics {
                        rows,
                        bytes: rows * (input.bytes / input.rows.max(1)),
                    };
                    (
                        out,
                        Ok(ResourceEstimate::new(merged as f64, 0, 0)),
                        format!(
                            "merge {merged} states from {} inputs into {rows}",
                            inputs.len()
                        ),
                    )
                }
                Payload::ASAP(ASAPOp::FinalizeExactAccumulator { .. }) => (
                    edge(input.rows),
                    Ok(ResourceEstimate::new(input.rows as f64, 0, 0)),
                    format!("finalize {} accumulators", input.rows),
                ),
                _ => (
                    edge(input.rows),
                    Ok(ResourceEstimate::new(input.rows as f64, 0, 0)),
                    format!("{} rows", input.rows),
                ),
            };
        let cost = estimate
            .and_then(|estimate| estimate.calibrated_cost(&resources))
            .map_err(|error| (node.id, error))?;
        let (cost, detail) = if ingestion {
            let read_at_query_time = dag.edges.iter().any(|e| {
                e.producer == node.id && nodes[&e.consumer].output_state.timing.is_query_time()
            });
            let mut retain = |windows: u64, cost: f64, what: &str| {
                let retained = windows * out.rows * state_bytes(node);
                retained_bytes = retained_bytes.saturating_add(retained);
                (
                    cost + calibration.cost_per_retained_byte_second * retained as f64,
                    format!("{detail}; ingestion time, {what}, retains {retained} bytes"),
                )
            };
            match (roles.get(&node.id), read_at_query_time) {
                (Some(PaneRole::Retained), _) => (
                    0.0,
                    "pane kept from an earlier evaluation; built and retained as the newest \
                     pane"
                        .to_string(),
                ),
                (Some(PaneRole::FeedsRetained), _) => (
                    0.0,
                    "feeds only kept panes; done for the newest pane".to_string(),
                ),
                // The pane being built and every completed one the
                // longest window reads.
                (Some(PaneRole::Newest { panes }), _) => retain(
                    panes + 1,
                    cost,
                    &format!("newest of {panes} panes, keeps {panes} completed"),
                ),
                // The window being built and the completed one.
                (None, true) => retain(2, cost, "window being built and the completed one"),
                (None, false) => (cost, format!("{detail}; ingestion time")),
            }
        } else {
            let roots = reached.get(&node.id).into_iter().flatten();
            let rate = evaluation_rate(roots.filter_map(|&r| demand.get(r)), calibration.horizon_s)
                .map_err(|error| (node.id, error))?;
            match roles.get(&node.id) {
                Some(PaneRole::Retained) => (
                    0.0,
                    "pane kept from an earlier evaluation; built and kept as the newest pane"
                        .to_string(),
                ),
                Some(PaneRole::FeedsRetained) => (
                    0.0,
                    "feeds only kept panes; done for the newest pane".to_string(),
                ),
                // Built from its width of raw data at each evaluation, then
                // kept with the `panes − 1` older panes the next one reads.
                Some(PaneRole::Newest { panes }) => {
                    per_evaluation.insert(node.id, cost);
                    let retained = (panes - 1) * out.rows * state_bytes(node);
                    retained_bytes = retained_bytes.saturating_add(retained);
                    (
                        cost * rate + calibration.cost_per_retained_byte_second * retained as f64,
                        format!(
                            "{detail}; x {rate:.4} evaluations/s; query time, newest of {panes} \
                             panes, keeps {} older, retains {retained} bytes",
                            panes - 1
                        ),
                    )
                }
                None => {
                    per_evaluation.insert(node.id, cost);
                    let detail = format!("{detail}; x {rate:.4} evaluations/s");
                    match raw_retention.get(&node.id) {
                        Some(&bytes) => {
                            retained_bytes = retained_bytes.saturating_add(bytes);
                            (
                                cost * rate
                                    + calibration.cost_per_retained_byte_second * bytes as f64,
                                format!("{detail}; retains {bytes} bytes of raw data"),
                            )
                        }
                        None => (cost * rate, detail),
                    }
                }
            }
        };
        output.insert(node.id, out);
        per_node.insert(
            node.id,
            NodeCost {
                cost,
                rows: out.rows,
                detail,
            },
        );
    }
    Ok(Priced {
        cost: CandidateCost {
            total: per_node.values().map(|n| n.cost).sum(),
            unit: COST_PER_SECOND,
            source: format!(
                "{COST_MODEL} (illustrative statistics, calibration {})",
                calibration.version
            ),
            per_node,
        },
        per_evaluation,
        retained_bytes,
    })
}

/// Rows a scan over `span_ms` reads at `rows_per_ms`.
fn scan_rows(rows_per_ms: f64, span_ms: u64) -> u64 {
    ((rows_per_ms * span_ms as f64).round() as u64).max(1)
}

/// The raw bytes each query-time scan makes the deployment retain:
/// lookback × λ × bytes per sample. Empty when raw data is kept anyway
/// (`None`). A scan at ingestion time reads samples as they arrive and
/// retains none. Like its work, a scan's retention is charged once per
/// scan node, so it stays a sum over nodes; a scan shared by several
/// queries is one node.
fn raw_retention(
    dag: &PhysicalASAPDAG,
    roles: &HashMap<PhysicalASAPNodeId, PaneRole>,
    rows_per_ms: f64,
    raw_bytes_per_sample: Option<u64>,
) -> HashMap<PhysicalASAPNodeId, u64> {
    let Some(bytes_per_sample) = raw_bytes_per_sample else {
        return HashMap::new();
    };
    dag.nodes
        .iter()
        .filter(|n| {
            n.output_state.timing.is_query_time()
                && matches!(n.payload, Payload::NonASAP(NonASAPOp::Scan { .. }))
        })
        .map(|n| {
            let span_ms = scan_extent_ms(dag, roles, n.id, 0).unwrap_or(DEFAULT_LOOKBACK_MS);
            let rows = scan_rows(rows_per_ms, span_ms);
            (n.id, rows.saturating_mul(bytes_per_sample))
        })
        .collect()
}

/// Partitions a per-group ranking assumes, absent group-count evidence.
fn partition_count(grouped: bool) -> u64 {
    if grouped {
        DEFAULT_GROUP_COUNT
    } else {
        1
    }
}

/// Rows a limit of `limit` keeps from `input` rows. Every top-k realization
/// is sized by this, so its consumers are priced alike whichever is chosen.
fn selected_rows(input: u64, limit: u64) -> u64 {
    input.min(limit)
}

/// The rows the summary under estimate `id` read, and whether it groups them.
fn summarized_rows(
    dag: &PhysicalASAPDAG,
    output: &HashMap<PhysicalASAPNodeId, EdgeStatistics>,
    id: PhysicalASAPNodeId,
) -> (u64, bool) {
    let producer = |consumer| {
        dag.edges
            .iter()
            .find(|e| e.consumer == consumer)
            .map(|e| e.producer)
    };
    let state = producer(id);
    let grouped = state
        .and_then(|state| dag.nodes.iter().find(|n| n.id == state))
        .is_some_and(|n| match &n.payload {
            Payload::ASAP(ASAPOp::SummaryAgg { reduction, .. }) => {
                !matches!(reduction, Reduction::Reduce(keys) if keys.keys().is_empty() && !keys.is_without())
            }
            _ => false,
        });
    let rows = state
        .and_then(producer)
        .and_then(|input| output.get(&input))
        .map_or(1, |edge| edge.rows);
    (rows, grouped)
}

/// `input` split as evenly as integers allow into `partitions` parts.
fn split(input: EdgeStatistics, partitions: u64) -> PartitionStatistics {
    let part = |total: u64, i: u64| total / partitions + u64::from(i < total % partitions);
    PartitionStatistics {
        partitions: (0..partitions)
            .map(|i| EdgeStatistics {
                rows: part(input.rows, i),
                bytes: part(input.bytes, i),
            })
            .collect(),
    }
}

/// Plain values are 8 bytes, strings 16; summary columns are sized apart.
fn row_bytes(schema: &Schema) -> u64 {
    schema
        .fields
        .iter()
        .map(|f| match &f.dtype {
            FieldDataType::Plain(DataType::Utf8) => 16,
            _ => 8,
        })
        .sum::<u64>()
        .max(1)
}

/// Bytes per group of the state `node` keeps: the summary's state, or the
/// row of an exact state.
fn state_bytes(node: &asap_types::ir::physical_export::PhysicalASAPDAGNode) -> u64 {
    match &node.payload {
        Payload::ASAP(ASAPOp::SummaryAgg {
            family: family @ FieldDataType::Sketch(..),
            ..
        }) => summary_shape(family).1,
        _ => row_bytes(&node.output_schema),
    }
}

/// Update operations per input row and bytes per state.
fn summary_shape(family: &FieldDataType) -> (u64, u64) {
    match family {
        FieldDataType::Sketch(kind, _) => match kind.params() {
            SketchParams::Cms { width, depth } | SketchParams::CountSketch { width, depth } => {
                (u64::from(*depth), 8 * u64::from(*width) * u64::from(*depth))
            }
            SketchParams::CmsWithHeap {
                width,
                depth,
                heap_size,
            }
            | SketchParams::CountSketchWithHeap {
                width,
                depth,
                heap_size,
            } => (
                u64::from(*depth) + 1,
                8 * u64::from(*width) * u64::from(*depth) + 24 * u64::from(*heap_size),
            ),
            // An insert reaches about two layers; each updates `d` rows'
            // counters and sign-checks, then its heap.
            params @ SketchParams::UnivMon { sketch_rows, .. } => (
                2 * (2 * u64::from(*sketch_rows) + 4),
                asap_logical_optimizer::pass1::replacement::sketch_state_bytes(params)
                    .unwrap_or(u64::MAX),
            ),
            _ => (1, 1_024),
        },
        _ => (1, 8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower_promql;

    /// Stage 2's all-query-time candidate.
    fn stage2_physical(
        from: &str,
        roots: &[Rc<OperatorNode>],
    ) -> Result<
        PhysicalCandidate,
        asap_physical_optimizer::implementation::physical_candidates::Stage2Error,
    > {
        asap_physical_optimizer::implementation::physical_candidates::stage2_physical(
            from,
            roots,
            &[],
            &DataWorkload::default(),
            &|_| None,
        )
        .map(|mut stage2| stage2.candidates.swap_remove(0))
    }
    use asap_types::ir::QueryRoot;
    use asap_types::types::AccuracyTarget;
    use asap_types::workload::{Evidence, Predictability, Rate, RepetitionInterval};

    /// A root repeating every `interval_ms`.
    fn repeating(accuracy: Option<AccuracyTarget>, interval_ms: u32) -> RootDemand {
        RootDemand {
            accuracy,
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                RepetitionInterval(interval_ms),
            )),
            predictability: Predictability::Unknown,
            latency_ms: None,
        }
    }

    /// A root repeating every 10 s, as Example 1's panels do.
    fn every_10s(accuracy: Option<AccuracyTarget>) -> RootDemand {
        repeating(accuracy, 10_000)
    }

    fn data() -> DataWorkload {
        DataWorkload {
            ingestion_rate: Evidence {
                value: Some(Rate(10_000.0)),
                ..Default::default()
            },
            input_cardinality: Evidence {
                value: Some(10_000),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Exact (P1), CMS+heap (P2) and CountSketch+heap (P3) realizations of
    /// one approximate top-k query, in Pass 1 catalog order.
    fn candidates() -> Vec<PhysicalCandidate> {
        candidates_with(&Default::default())
    }

    fn candidates_with(
        metric_types: &BTreeMap<String, asap_types::workload::MetricType>,
    ) -> Vec<PhysicalCandidate> {
        let root = lower_promql(
            "topk by (job) (10, sum_over_time(m[1m]))",
            AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.001,
            },
        );
        let inventory =
            asap_logical_optimizer::pass1::logical_candidates::enumerate_local_logical_candidates(
                vec![(0, QueryRoot::Operator(root))],
                metric_types,
            )
            .unwrap();
        let topk = inventory
            .targets
            .iter()
            .position(|t| t.alternatives.len() > 2)
            .unwrap();
        [0, 1, 2]
            .into_iter()
            .map(|alternative| {
                let mut choice = vec![0; inventory.targets.len()];
                choice[topk] = alternative;
                let roots: Vec<_> =
                    asap_logical_optimizer::pass1::logical_candidates::compose_logical_candidate(
                        &inventory, &choice,
                    )
                    .unwrap()
                    .into_iter()
                    .map(|(_, root)| match root {
                        QueryRoot::Operator(node) => node,
                        QueryRoot::Scalar(_) => panic!("operator root"),
                    })
                    .collect();
                let mut candidate = stage2_physical("L", &roots).unwrap();
                candidate.id = format!("P{}", alternative + 1);
                candidate
            })
            .collect()
    }

    /// A summary whose analytical guarantee misses the target is rejected as
    /// invalid with a reason; the exact plan is then selected.
    #[test]
    fn accuracy_failing_candidate_is_rejected_as_invalid() {
        let candidates = candidates();
        let strict = AccuracyTarget::EpsilonDelta {
            epsilon: 1e-6,
            delta: 1e-9,
        };
        let selection = stage3_select(
            &candidates,
            &[every_10s(Some(strict))],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        assert_eq!(selection.selected, "P1");
        let rejected: BTreeMap<_, _> = selection
            .rejected
            .iter()
            .map(|r| (r.id.as_str(), r))
            .collect();
        assert_eq!(rejected.len(), 2);
        assert!(!rejected["P3"].valid);
        assert!(
            rejected["P3"].reason.contains("misses target"),
            "{}",
            rejected["P3"].reason
        );
        assert!(!selection.costs.contains_key("P3"));
    }

    /// Count-Min over weights not proven non-negative is invalid whatever
    /// the target.
    #[test]
    fn count_min_over_signed_weights_is_rejected_as_invalid() {
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        };
        let selection = stage3_select(
            &candidates(),
            &[every_10s(Some(target))],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        let p2 = selection.rejected.iter().find(|r| r.id == "P2").unwrap();
        assert!(!p2.valid);
        assert!(p2.reason.contains("non-negative"), "{}", p2.reason);
    }

    /// Count-Min over samples of a declared counter is valid: Stage 3 prices it.
    #[test]
    fn count_min_over_declared_counter_samples_is_valid() {
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        };
        let counter = [("m".to_string(), asap_types::workload::MetricType::Counter)].into();
        let selection = stage3_select(
            &candidates_with(&counter),
            &[every_10s(Some(target))],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        assert!(
            selection.costs.contains_key("P2"),
            "{:?}",
            selection.rejected
        );
        assert!(
            selection.rejected.iter().all(|r| r.valid),
            "{:?}",
            selection.rejected
        );
    }

    fn inventory(queries: &[&str]) -> LocalLogicalCandidates<usize> {
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        };
        let roots = queries
            .iter()
            .enumerate()
            .map(|(i, query)| {
                let root = lower_promql(query, target.clone());
                let root =
                    asap_types::ir::schema_support::with_promql_series_identity(&root).unwrap();
                (i, QueryRoot::Operator(root))
            })
            .collect();
        asap_logical_optimizer::pass1::logical_candidates::enumerate_local_logical_candidates(
            roots,
            &Default::default(),
        )
        .unwrap()
    }

    fn no_targets(inventory: &LocalLogicalCandidates<usize>) -> Vec<RootDemand> {
        vec![every_10s(None); inventory.roots.len()]
    }

    /// Real Stage 1 → 3 cost plus a penalty whenever the two named targets
    /// both leave their pass-through: an inner choice that changes the cost
    /// of the target reading it.
    fn coupled<'a>(
        inventory: &'a LocalLogicalCandidates<usize>,
        targets: &'a [RootDemand],
        data: &'a DataWorkload,
        (outer, inner): (usize, usize),
    ) -> impl Fn(&[usize]) -> Result<f64, String> + 'a {
        move |choice: &[usize]| {
            let (_, candidate) = realize_choice(inventory, choice)?;
            let total = assess(&candidate, targets, data, &PlanningModels::builtin())?.total;
            Ok(total
                + if choice[outer] > 0 && choice[inner] > 0 {
                    1.0
                } else {
                    0.0
                })
        }
    }

    /// The outer target and the target beneath it.
    fn nested_pair(inventory: &LocalLogicalCandidates<usize>) -> (usize, usize) {
        let beneath = nested_targets(inventory);
        beneath
            .iter()
            .enumerate()
            .find_map(|(t, inner)| inner.first().map(|&u| (t, u)))
            .unwrap()
    }

    /// Coupling between a target and the target it reads, over at most 64
    /// combinations, falls back to building every combination.
    #[test]
    fn coupling_over_few_combinations_selects_exhaustively() {
        let inventory = inventory(&["count(topk by (job) (10, sum_over_time(m[1m])))"]);
        assert!(combination_count(&inventory) <= MAX_ENUMERATED_CANDIDATES);
        let targets = no_targets(&inventory);
        let data = data();
        let evaluate = coupled(&inventory, &targets, &data, nested_pair(&inventory));
        let plan = select_variant(
            Variant {
                inventory: &inventory,
                sharing: Sharing::Independent,
                offset: 0,
            },
            &targets,
            &data,
            PlanningModels::builtin(),
            &evaluate,
        )
        .unwrap();
        assert_eq!(plan.selection.method, SelectionMethod::Exhaustive);
        let exhaustive = select_exhaustive(
            &independent(inventory.clone()),
            &targets,
            &data,
            PlanningModels::builtin(),
            MAX_ENUMERATED_CANDIDATES,
        )
        .unwrap();
        assert_eq!(plan.selection.selected, exhaustive.selection.selected);
    }

    /// Coupling over more than 64 combinations keeps the dynamic program's
    /// result and flags it as not guaranteed optimal.
    #[test]
    fn coupling_over_many_combinations_is_flagged() {
        let inventory = inventory(&[
            "count(topk by (job) (10, sum_over_time(m[1m])))",
            "sum by (job) (rate(m[1m]))",
        ]);
        assert!(combination_count(&inventory) > MAX_ENUMERATED_CANDIDATES);
        let targets = no_targets(&inventory);
        let data = data();
        let evaluate = coupled(&inventory, &targets, &data, nested_pair(&inventory));
        let plan = select_variant(
            Variant {
                inventory: &inventory,
                sharing: Sharing::Independent,
                offset: 0,
            },
            &targets,
            &data,
            PlanningModels::builtin(),
            &evaluate,
        )
        .unwrap();
        assert!(
            matches!(
                &plan.selection.method,
                SelectionMethod::TreeDpNotGuaranteedOptimal { reason } if reason.contains("additively")
            ),
            "{:?}",
            plan.selection.method
        );
        assert!(!plan.selection.guaranteed_optimal());
    }

    /// Without coupling the dynamic program's result stands, and it is the
    /// exhaustive minimum.
    #[test]
    fn uncoupled_selection_uses_the_dynamic_program() {
        let inventory = inventory(&["count(topk by (job) (10, sum_over_time(m[1m])))"]);
        let targets = no_targets(&inventory);
        let plan = select_plan(
            &independent(inventory.clone()),
            &targets,
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        assert_eq!(plan.selection.method, SelectionMethod::TreeDp);
        let exhaustive = select_exhaustive(
            &independent(inventory.clone()),
            &targets,
            &data(),
            PlanningModels::builtin(),
            MAX_ENUMERATED_CANDIDATES,
        )
        .unwrap();
        assert_eq!(plan.selection.selected, exhaustive.selection.selected);
    }

    /// A whole-expression top-k absorbs the `sum_over_time` beneath it. When
    /// it is the cheapest choice, the dynamic program selects it with the
    /// inner target at its pass-through (it contributes nothing), and agrees
    /// with brute force over every valid choice under the same costs.
    #[test]
    fn absorbing_alternative_drops_the_inner_target() {
        let inventory = inventory(&["topk by (job) (10, sum_over_time(m[1m]))"]);
        let (t, c, u) = inventory
            .targets
            .iter()
            .enumerate()
            .find_map(|(t, target)| {
                let c = target.alternatives.iter().enumerate().position(|(c, a)| {
                    target.absorbs[c].is_some()
                        && matches!(a, asap_logical_optimizer::Realization::Sketch(kind)
                            if *kind.algorithm() == SketchAlgorithm::CountSketchWithHeap)
                })?;
                Some((t, c, target.absorbs[c]?))
            })
            .expect("a whole-expression CountSketch alternative");
        let targets = no_targets(&inventory);
        let data = data();
        for bonus in [0.0, 1e4] {
            let evaluate = |choice: &[usize]| -> Result<f64, String> {
                let (_, candidate) = realize_choice(&inventory, choice)?;
                let total = assess(&candidate, &targets, &data, &PlanningModels::builtin())?.total;
                Ok(total - if choice[t] == c { bonus } else { 0.0 })
            };
            let plan = select_variant(
                Variant {
                    inventory: &inventory,
                    sharing: Sharing::Independent,
                    offset: 0,
                },
                &targets,
                &data,
                PlanningModels::builtin(),
                &evaluate,
            )
            .unwrap();
            assert_eq!(plan.selection.method, SelectionMethod::TreeDp);
            let brute = enumerate_choices(&inventory, usize::MAX)
                .into_iter()
                .map(|choice| (evaluate(&choice).unwrap(), choice))
                .fold(None::<(f64, Vec<usize>)>, |best, next| match best {
                    Some(best) if best.0 <= next.0 => Some(best),
                    _ => Some(next),
                })
                .unwrap();
            assert_eq!(plan.choice, brute.1, "bonus {bonus}");
            if bonus > 0.0 {
                assert_eq!((plan.choice[t], plan.choice[u]), (c, 0));
            }
        }
    }

    /// In a shared variant, two queries that chose structurally identical
    /// summary producers reach one state.
    #[test]
    fn identical_producers_are_merged_after_composition() {
        let inventory = inventory(&[
            "quantile_over_time(0.5, m[5m])",
            "quantile_over_time(0.99, m[5m])",
        ]);
        let kll = |t: &asap_logical_optimizer::pass1::logical_candidates::LocalLogicalTarget| {
            t.alternatives
                .iter()
                .position(|a| matches!(a, asap_logical_optimizer::Realization::Sketch(kind) if *kind.algorithm() == SketchAlgorithm::Kll))
                .unwrap()
        };
        let choice: Vec<_> = inventory.targets.iter().map(kll).collect();
        let plan = finish(
            Variant {
                inventory: &inventory,
                sharing: Sharing::IdenticalExpressions,
                offset: 0,
            },
            &no_targets(&inventory),
            &data(),
            &PlanningModels::builtin(),
            choice,
            SelectionMethod::Exhaustive,
        )
        .unwrap();
        let states: std::collections::HashSet<_> = plan
            .physical
            .roots
            .iter()
            .flat_map(OperatorNode::reachable)
            .filter(|n| matches!(n.operator, Operator::ASAP(ASAPOp::SummaryAgg { .. })))
            .map(|n| std::rc::Rc::as_ptr(&n))
            .collect();
        assert_eq!(states.len(), 1);
    }

    /// #509 Example 2's requirements on one UnivMon each: the built-in model
    /// certifies the L2 norm (Q3) from layer 0's F₂, but neither the
    /// distinct count (Q1) nor the entropy (Q2).
    #[test]
    fn univmon_certifies_l2_but_not_distinct_count_or_entropy() {
        for (query, epsilon, certified) in [
            ("distinct_over_time(src[1m])", 0.02, false),
            ("entropy_over_time(src[1m])", 0.05, false),
            ("l2_over_time(src[1m])", 0.01, true),
        ] {
            let target = AccuracyTarget::EpsilonDelta {
                epsilon,
                delta: 0.01,
            };
            let root = lower_promql(query, target.clone());
            let root = asap_types::ir::schema_support::with_promql_series_identity(&root).unwrap();
            let inventory =
                asap_logical_optimizer::pass1::logical_candidates::enumerate_local_logical_candidates(
                    vec![(0, QueryRoot::Operator(root))],
                    &Default::default(),
                )
                .unwrap();
            let choice: Vec<_> = inventory
                .targets
                .iter()
                .map(|t| {
                    t.alternatives
                        .iter()
                        .position(|a| matches!(a, asap_logical_optimizer::Realization::Sketch(kind) if *kind.algorithm() == SketchAlgorithm::UnivMon))
                        .expect("a UnivMon alternative")
                })
                .collect();
            let (_, candidate) = realize_choice(&inventory, &choice).unwrap();
            let violation = accuracy_violation(
                &candidate,
                &[every_10s(Some(target))],
                &PlanningModels::builtin(),
            );
            if certified {
                assert_eq!(violation, None, "{query}");
            } else {
                let reason = violation.expect(query);
                assert!(reason.contains("no accuracy model for UnivMon"), "{reason}");
            }
        }
    }

    /// A guarantee in another statistic's metric does not satisfy a target:
    /// CountSketch's L2 frequency bound answers the top-k scores it was
    /// built for, but not a distinct count, however loose the target.
    #[test]
    fn a_bound_in_another_metric_misses_the_target() {
        let candidates = candidates();
        let build = OperatorNode::reachable(&candidates[2].roots[0])
            .into_iter()
            .find(|node| {
                matches!(&node.operator, Operator::ASAP(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. })
                    if *kind.algorithm() == SketchAlgorithm::CountSketchWithHeap)
            })
            .expect("a CountSketch build");
        let loose = AccuracyTarget::EpsilonDelta {
            epsilon: 0.5,
            delta: 0.5,
        };
        let models = PlanningModels::builtin();
        assert_eq!(
            build_violation(&build, &SketchStatistic::TopK { k: 10 }, &loose, &models),
            None
        );
        let reason = build_violation(&build, &SketchStatistic::Cardinality, &loose, &models)
            .expect("an L2 frequency bound does not bound a distinct count");
        assert!(reason.contains("does not bound Cardinality"), "{reason}");
    }

    /// A candidate that cannot be checked is rejected with its reason; the
    /// others are still selected among.
    #[test]
    fn a_candidate_that_cannot_be_checked_is_rejected_not_fatal() {
        let mut candidates = candidates();
        let mut extra = candidates[0].clone();
        extra.id = "P4".into();
        extra.roots.push(extra.roots[0].clone());
        candidates.push(extra);
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        };
        let selection = stage3_select(
            &candidates,
            &[every_10s(Some(target))],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        let p4 = selection.rejected.iter().find(|r| r.id == "P4").unwrap();
        assert!(!p4.valid);
        assert!(p4.reason.contains("accuracy targets"), "{}", p4.reason);
    }

    /// Every top-k realization reports the same output rows, as the logical
    /// result sizes them, whether the input holds fewer rows than k × groups
    /// or more.
    #[test]
    fn every_topk_realization_reports_the_same_output_rows() {
        for series in [3, 1_000_000] {
            let data = DataWorkload {
                input_cardinality: asap_types::workload::Evidence {
                    value: Some(series),
                    ..Default::default()
                },
                ..data()
            };
            let rows: Vec<u64> = candidates()
                .iter()
                .map(|candidate| {
                    let cost = price(
                        &candidate.dag,
                        &[every_10s(None)],
                        &data,
                        &Stage3Calibration::ILLUSTRATIVE,
                    )
                    .unwrap();
                    cost.per_node[&candidate.dag.roots[0]].rows
                })
                .collect();
            assert!(rows.windows(2).all(|w| w[0] == w[1]), "{series}: {rows:?}");
        }
    }

    /// Each node is priced once, under its DAG id, and the total is the sum;
    /// every candidate is either selected or rejected as costlier.
    #[test]
    fn per_node_costs_cover_the_dag_and_sum_to_the_total() {
        let candidates = candidates();
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        };
        let selection = stage3_select(
            &candidates,
            &[every_10s(Some(target))],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        for candidate in candidates.iter().filter(|c| c.id != "P2") {
            let cost = &selection.costs[&candidate.id];
            let ids: Vec<_> = candidate.dag.nodes.iter().map(|n| n.id).collect();
            let mut keys: Vec<_> = cost.per_node.keys().copied().collect();
            keys.sort();
            let mut sorted = ids.clone();
            sorted.sort();
            assert_eq!(keys, sorted);
            let sum: f64 = cost.per_node.values().map(|n| n.cost).sum();
            assert_eq!(cost.total, sum);
            assert!(cost.total > 0.0);
        }
        let costlier: Vec<_> = selection.rejected.iter().filter(|r| r.valid).collect();
        assert_eq!(costlier.len(), 1);
        assert_ne!(costlier[0].id, selection.selected);
    }

    /// `query` with its KLL alternative chosen, every state maintained at
    /// `timing`, as a priced-ready physical DAG.
    fn kll_dag(query: &str, ingestion: bool) -> PhysicalASAPDAG {
        use asap_types::ir::{
            apply_materialization_timings, MaterializationAssignment, TimingMemo,
        };
        let inventory = inventory(&[query]);
        let choice: Vec<_> = inventory
            .targets
            .iter()
            .map(|t| {
                t.alternatives
                    .iter()
                    .position(|a| matches!(a, asap_logical_optimizer::Realization::Sketch(kind) if *kind.algorithm() == SketchAlgorithm::Kll))
                    .unwrap_or(0)
            })
            .collect();
        let roots: Vec<_> =
            asap_logical_optimizer::pass1::logical_candidates::compose_logical_candidate(
                &inventory, &choice,
            )
            .unwrap()
            .into_iter()
            .map(|(_, root)| match root {
                QueryRoot::Operator(node) => node,
                QueryRoot::Scalar(_) => panic!("operator root"),
            })
            .collect();
        let assignment = match ingestion {
            true => MaterializationAssignment::all_ingestion_time(),
            false => MaterializationAssignment::all_query_time(),
        };
        let mut memo = TimingMemo::new();
        let timed: Vec<_> = roots
            .iter()
            .map(|root| apply_materialization_timings(root, &assignment, &mut memo).unwrap())
            .collect();
        asap_types::ir::physical_export::compile_physical_asap_workload_with_node_ids(&timed)
            .unwrap()
            .dag
    }

    fn node_of(
        dag: &PhysicalASAPDAG,
        pick: impl Fn(&Payload) -> bool,
    ) -> &asap_types::ir::physical_export::PhysicalASAPDAGNode {
        dag.nodes.iter().find(|n| pick(&n.payload)).unwrap()
    }

    fn is_scan(payload: &Payload) -> bool {
        matches!(payload, Payload::NonASAP(NonASAPOp::Scan { .. }))
    }

    fn is_build(payload: &Payload) -> bool {
        matches!(payload, Payload::ASAP(ASAPOp::SummaryAgg { .. }))
    }

    /// A p99 over the last 5 min every minute, predictable, over `data`
    /// arriving continuously; latency bound `latency_ms`.
    fn pattern_b(
        data: DataWorkload,
        latency_ms: Option<f64>,
    ) -> (StagePipelineRun<usize>, Vec<RootDemand>, DataWorkload) {
        let demand = vec![RootDemand {
            accuracy: Some(AccuracyTarget::Epsilon(0.01)),
            predictability: Predictability::Predictable { known_at: None },
            latency_ms,
            ..repeating(None, 60_000)
        }];
        let data = DataWorkload {
            arrival: asap_types::workload::DataArrival::ContinuouslyIngesting,
            ..data
        };
        let root = lower_promql(
            "quantile_over_time(0.99, m[5m])",
            AccuracyTarget::Epsilon(0.01),
        );
        let root = asap_types::ir::schema_support::with_promql_series_identity(&root).unwrap();
        let run = plan_stages(
            vec![(0, QueryRoot::Operator(root))],
            &demand,
            &data,
            PlanningModels::builtin(),
            MAX_ENUMERATED_CANDIDATES,
        )
        .unwrap();
        (run, demand, data)
    }

    /// The maintained KLL panes of [`pattern_b`] and their cost.
    fn maintained_panes(run: &StagePipelineRun<usize>) -> (&PhysicalCandidate, &CandidateCost) {
        let enumeration = run.enumeration.as_ref().unwrap();
        let p = enumeration
            .candidates
            .iter()
            .flat_map(|c| &c.physical)
            .find(|p| p.materialization == "ingestion time: Kll ×5 panes")
            .expect("maintained KLL panes");
        (p, &enumeration.selection.costs[&p.id])
    }

    /// At ingestion time a chain of panes is one pane built as rows arrive
    /// and kept: the newest pane pays the build and the memory of itself
    /// and the 5 completed panes; the older panes and their inputs cost
    /// nothing, since they are earlier evaluations' newest panes.
    #[test]
    fn maintained_panes_build_once_and_retain_lookback_over_width_plus_one() {
        let (run, ..) = pattern_b(data(), None);
        let (p, cost) = maintained_panes(&run);
        let builds: Vec<_> = p
            .dag
            .nodes
            .iter()
            .filter(|n| is_build(&n.payload))
            .collect();
        assert_eq!(builds.len(), 5);
        let charged: Vec<_> = builds
            .iter()
            .filter(|n| cost.per_node[&n.id].cost > 0.0)
            .collect();
        let [newest] = charged[..] else {
            panic!("one charged pane: {}", charged.len())
        };
        let Payload::ASAP(ASAPOp::SummaryAgg { family, .. }) = &newest.payload else {
            unreachable!()
        };
        // data(): λ = 10 000 rows/s into 10 000 series.
        let node = &cost.per_node[&newest.id];
        let retained = 6 * 10_000 * summary_shape(family).1;
        let build = 10_000.0 * 1e-6;
        assert!(
            (node.cost - build - 1.25e-7 * retained as f64).abs() < 1e-9,
            "{}: {}",
            node.cost,
            node.detail
        );
        let ingestion: Vec<_> = p
            .dag
            .nodes
            .iter()
            .filter(|n| !n.output_state.timing.is_query_time() && cost.per_node[&n.id].cost > 0.0)
            .map(|n| cost.per_node[&n.id].detail.clone())
            .collect();
        // The scan, one range and the newest pane; the shift is free (Q66).
        assert_eq!(ingestion.len(), 3, "{ingestion:#?}");
    }

    /// Kept at query time (B3, Q59), each evaluation builds the newest pane
    /// from one pane width of raw data and keeps it with the 4 older panes
    /// the next evaluation reads: the newest pane pays the build per
    /// evaluation and the memory of 4 panes; the older panes and their
    /// inputs cost nothing, and the scan reads one pane width. Without
    /// `query_time_retention` the candidate is rejected.
    #[test]
    fn kept_panes_build_the_newest_and_retain_the_others() {
        let (run, demand, data) = pattern_b(data(), None);
        let enumeration = run.enumeration.as_ref().unwrap();
        let p = enumeration
            .candidates
            .iter()
            .flat_map(|c| &c.physical)
            .find(|p| p.materialization == "query time, kept: Kll ×5 panes")
            .expect("kept KLL panes");
        let cost = &enumeration.selection.costs[&p.id];
        let builds: Vec<_> = p.dag.nodes.iter().filter(|n| n.kept).collect();
        assert_eq!(builds.len(), 5);
        assert!(builds.iter().all(|n| is_build(&n.payload)));
        let charged: Vec<_> = builds
            .iter()
            .filter(|n| cost.per_node[&n.id].cost > 0.0)
            .collect();
        let [newest] = charged[..] else {
            panic!("one charged pane: {}", charged.len())
        };
        let Payload::ASAP(ASAPOp::SummaryAgg { family, .. }) = &newest.payload else {
            unreachable!()
        };
        // data(): λ = 10 000 rows/s into 10 000 series; a 1-min pane is
        // 600 000 rows, built once a minute.
        let node = &cost.per_node[&newest.id];
        let retained = 4 * 10_000 * summary_shape(family).1;
        let build = 600_000.0 * 1e-6 / 60.0;
        assert!(
            (node.cost - build - 1.25e-7 * retained as f64).abs() < 1e-9,
            "{}: {}",
            node.cost,
            node.detail
        );
        let scan = node_of(&p.dag, is_scan);
        assert_eq!(cost.per_node[&scan.id].rows, 600_000);
        let without = DeploymentCapabilities {
            query_time_retention: false,
            ..DeploymentCapabilities::UNRESTRICTED
        };
        let selection = stage3_select(
            std::slice::from_ref(p),
            &demand,
            &data,
            PlanningModels::builtin().with_capabilities(&without),
        );
        let Err(SelectionError::NoValidCandidate(rejected)) = selection else {
            panic!("rejected")
        };
        assert_eq!(
            rejected[0].reason,
            "deployment cannot keep query-time state across evaluations (query time, kept)"
        );
    }

    /// The latency check (S6) rejects a candidate whose query-time work in
    /// one evaluation, at one cost unit per ms, exceeds its query's bound,
    /// and names the query, the estimate and the bound.
    #[test]
    fn latency_bound_rejects_slow_query_time_work() {
        let (run, ..) = pattern_b(data(), None);
        let enumeration = run.enumeration.as_ref().unwrap();
        let all: Vec<_> = enumeration
            .candidates
            .iter()
            .flat_map(|c| &c.physical)
            .collect();
        assert!(enumeration.selection.rejected.iter().all(|r| r.valid));
        // Per evaluation, in cost units: per-second cost × 60 s for the
        // all-query-time candidates.
        let per_evaluation =
            |p: &PhysicalCandidate| enumeration.selection.costs[&p.id].total * 60.0;
        let query_time: Vec<&PhysicalCandidate> = all
            .iter()
            .copied()
            .filter(|p| p.materialization.is_empty())
            .collect();
        let (low, high) = query_time
            .iter()
            .map(|p| per_evaluation(p))
            .fold((f64::INFINITY, 0.0f64), |(lo, hi), x| {
                (lo.min(x), hi.max(x))
            });
        let bound = ((low + high) / 2.0).round();
        let (fast, slow): (Vec<_>, Vec<_>) = query_time
            .into_iter()
            .partition(|p| per_evaluation(p) < bound);
        assert!(!fast.is_empty() && !slow.is_empty(), "{low} {high}");
        let (bounded, ..) = pattern_b(data(), Some(bound));
        let selection = &bounded.enumeration.as_ref().unwrap().selection;
        for p in slow {
            let reason = &selection
                .rejected
                .iter()
                .find(|r| r.id == p.id)
                .unwrap()
                .reason;
            assert!(
                reason.starts_with("q1: query-time work takes")
                    && reason.ends_with(&format!("over the {bound} ms latency bound")),
                "{reason}"
            );
        }
        for p in fast {
            assert!(selection.costs.contains_key(&p.id), "{}", p.id);
        }
    }

    /// With few series and a high ingestion rate, maintaining the panes
    /// beats rebuilding the window at every evaluation, and the dynamic
    /// program, which sees each logical candidate at its cheapest
    /// materialization, selects what exhaustive selection does.
    #[test]
    fn maintained_panes_win_with_few_series_and_dp_agrees() {
        let few = DataWorkload {
            ingestion_rate: Evidence {
                value: Some(Rate(100_000.0)),
                ..Default::default()
            },
            input_cardinality: Evidence {
                value: Some(10),
                ..Default::default()
            },
            ..Default::default()
        };
        let (run, ..) = pattern_b(few, None);
        let enumeration = run.enumeration.as_ref().unwrap();
        let selected = &enumeration.selection.selected;
        assert!(selected.contains("-m"), "{selected}");
        assert_eq!(&run.plan.selection.selected, selected);
        assert!(!run.plan.physical.materialization.is_empty());
        assert!(run.plan.selection.guaranteed_optimal());
        let (run, ..) = pattern_b(data(), None);
        let (_, maintained) = maintained_panes(&run);
        let selection = &run.enumeration.as_ref().unwrap().selection;
        assert!(maintained.total > selection.costs[&selection.selected].total);
        assert_eq!(run.plan.selection.selected, selection.selected);
    }

    /// An ingestion-time node is priced over λ rows per second, whatever its
    /// readers' evaluation rate; a query-time node scales with that rate.
    #[test]
    fn ingestion_time_node_is_priced_at_the_ingestion_rate() {
        let dag = kll_dag("quantile_over_time(0.99, m[1m])", true);
        let scan = node_of(&dag, is_scan);
        assert!(!scan.output_state.timing.is_query_time());
        let at = |interval_ms| {
            price(
                &dag,
                &[repeating(None, interval_ms)],
                &data(),
                &Stage3Calibration::ILLUSTRATIVE,
            )
            .unwrap()
        };
        let (fast, slow) = (at(1_000), at(10_000));
        // data() declares λ = 10 000 rows/s.
        assert_eq!(fast.per_node[&scan.id].rows, 10_000);
        let build = node_of(&dag, is_build).id;
        for id in [scan.id, build] {
            assert_eq!(fast.per_node[&id].cost, slow.per_node[&id].cost);
        }
        let estimate = dag.roots[0];
        assert!(
            (fast.per_node[&estimate].cost - 10.0 * slow.per_node[&estimate].cost).abs() < 1e-12
        );
    }

    /// The memory term is w × retained bytes: two windows of every group's
    /// state, for ingestion-time state that query time reads.
    #[test]
    fn memory_term_is_weight_times_retained_bytes() {
        let dag = kll_dag("quantile_over_time(0.99, m[1m])", true);
        let build = node_of(&dag, is_build);
        let Payload::ASAP(ASAPOp::SummaryAgg { family, .. }) = &build.payload else {
            unreachable!()
        };
        let cost = |w| {
            let calibration = Stage3Calibration {
                cost_per_retained_byte_second: w,
                ..Stage3Calibration::ILLUSTRATIVE
            };
            price(&dag, &[every_10s(None)], &data(), &calibration).unwrap()
        };
        let (with, without) = (cost(1.25e-7), cost(0.0));
        // One state per series: data() declares 10 000.
        let groups = with.per_node[&build.id].rows;
        assert_eq!(groups, 10_000);
        let retained = 2 * groups * summary_shape(family).1;
        let term = with.per_node[&build.id].cost - without.per_node[&build.id].cost;
        assert!((term - 1.25e-7 * retained as f64).abs() < 1e-12, "{term}");
        assert!((with.total - without.total - term).abs() < 1e-12);
        // Query-time state is transient: no memory term.
        let query_time = kll_dag("quantile_over_time(0.99, m[1m])", false);
        let (a, b) = (
            price(
                &query_time,
                &[every_10s(None)],
                &data(),
                &Stage3Calibration::ILLUSTRATIVE,
            ),
            price(
                &query_time,
                &[every_10s(None)],
                &data(),
                &Stage3Calibration {
                    cost_per_retained_byte_second: 0.0,
                    ..Stage3Calibration::ILLUSTRATIVE
                },
            ),
        );
        assert_eq!(a.unwrap().total, b.unwrap().total);
    }

    /// A node reached by two roots with equal intervals is evaluated once
    /// per interval; different intervals add their rates.
    #[test]
    fn node_shared_by_roots_with_equal_intervals_is_charged_once() {
        let root = lower_promql("sum by (job) (rate(m[1m]))", AccuracyTarget::Exact);
        let one = stage2_physical("L", std::slice::from_ref(&root))
            .unwrap()
            .dag;
        let two = stage2_physical("L", &[root.clone(), root]).unwrap().dag;
        let total = |dag: &PhysicalASAPDAG, demand: &[RootDemand]| {
            price(dag, demand, &data(), &Stage3Calibration::ILLUSTRATIVE)
                .unwrap()
                .total
        };
        let single = total(&one, &[every_10s(None)]);
        assert_eq!(total(&two, &[every_10s(None), every_10s(None)]), single);
        let mixed = total(&two, &[every_10s(None), repeating(None, 20_000)]);
        assert!((mixed - 1.5 * single).abs() < 1e-12 * single.max(1.0));
    }

    /// One-off recurrence is amortized over the horizon H:
    /// invocations × per-evaluation cost / H.
    #[test]
    fn one_off_recurrence_is_amortized_over_the_horizon() {
        let root = lower_promql("sum by (job) (rate(m[1m]))", AccuracyTarget::Exact);
        let dag = stage2_physical("L", &[root]).unwrap().dag;
        let calibration = Stage3Calibration::ILLUSTRATIVE;
        let total = |recurrence| {
            let demand = RootDemand {
                recurrence,
                ..every_10s(None)
            };
            price(&dag, &[demand], &data(), &calibration).unwrap().total
        };
        // Once per second: the per-evaluation cost.
        let per_evaluation = total(QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
            RepetitionInterval(1_000),
        )));
        let once = total(QueryRecurrence::OneTime {
            invocations: 3,
            execute_at: None,
        });
        let expected = 3.0 * per_evaluation / calibration.horizon_s;
        assert!(
            (once - expected).abs() < 1e-12 * expected,
            "{once} vs {expected}"
        );
        assert_eq!(
            total(QueryRecurrence::Unknown),
            per_evaluation / calibration.horizon_s
        );
    }

    /// Regression (#509 Example 3, Pattern A): a scan is priced over the
    /// longest range plus offset reading it, and each range passes only its
    /// own span, so sharing one 5-year scan is not costlier than separate
    /// scans.
    #[test]
    fn shared_long_range_scan_is_not_costlier_than_separate_scans() {
        let queries = [
            "quantile_over_time(0.99, latency_ms[5y])",
            "quantile_over_time(0.99, latency_ms[1y])",
            "quantile_over_time(0.99, latency_ms[1y] offset 1y)",
            "quantile_over_time(0.99, latency_ms[1y] offset 2y)",
            "quantile_over_time(0.99, latency_ms[3y] offset 2y)",
        ];
        let roots = queries
            .iter()
            .enumerate()
            .map(|(i, query)| {
                let root = lower_promql(query, AccuracyTarget::Exact);
                let root =
                    asap_types::ir::schema_support::with_promql_series_identity(&root).unwrap();
                (i, QueryRoot::Operator(root))
            })
            .collect();
        let stage1 = stage1_logical_candidates(roots, &Default::default(), &[]).unwrap();
        let data = DataWorkload {
            ingestion_rate: Evidence {
                value: Some(Rate(1_000_000.0 / 15.0)),
                ..Default::default()
            },
            input_cardinality: Evidence {
                value: Some(1_000_000),
                ..Default::default()
            },
            ..Default::default()
        };
        let demand: Vec<_> = queries
            .iter()
            .map(|_| RootDemand {
                recurrence: QueryRecurrence::OneTime {
                    invocations: 1,
                    execute_at: None,
                },
                ..every_10s(None)
            })
            .collect();
        let total = |sharing: Sharing| {
            let variant = *variants(&stage1)
                .iter()
                .find(|v| v.sharing == sharing)
                .unwrap();
            let raw = vec![0; variant.inventory.targets.len()];
            let models = PlanningModels::builtin();
            let (_, mut stage2) = realize(variant, &raw, &demand, &data, &models).unwrap();
            let candidate = stage2.candidates.swap_remove(0);
            let cost = assess(&candidate, &demand, &data, &models).unwrap();
            let scan_rows: Vec<_> = candidate
                .dag
                .nodes
                .iter()
                .filter(|n| is_scan(&n.payload))
                .map(|n| cost.per_node[&n.id].rows)
                .collect();
            (cost.total, scan_rows)
        };
        let (shared, shared_scans) = total(Sharing::IdenticalExpressions);
        let (separate, separate_scans) = total(Sharing::Independent);
        let year = 365.0 * 24.0 * 3_600.0 * 1_000_000.0 / 15.0;
        assert_eq!(shared_scans.len(), 1);
        assert!(
            (shared_scans[0] as f64 / year - 5.0).abs() < 0.01,
            "{shared_scans:?}"
        );
        let mut years: Vec<_> = separate_scans
            .iter()
            .map(|&rows| (rows as f64 / year).round() as u64)
            .collect();
        years.sort();
        assert_eq!(years, [1, 2, 3, 5, 5]);
        assert!(shared <= separate, "shared {shared} vs separate {separate}");
    }

    /// Q66: over a shared 5-year scan, a time shift is free and each time
    /// range is charged for the rows it keeps: the 1-year range costs a
    /// fifth of the 5-year one, not the same.
    #[test]
    fn time_shift_is_free_and_time_range_pays_for_the_rows_it_keeps() {
        let queries = [
            "quantile_over_time(0.99, latency_ms[5y])",
            "quantile_over_time(0.99, latency_ms[1y] offset 2y)",
        ];
        let roots = queries
            .iter()
            .enumerate()
            .map(|(i, query)| {
                let root = lower_promql(query, AccuracyTarget::Exact);
                let root =
                    asap_types::ir::schema_support::with_promql_series_identity(&root).unwrap();
                (i, QueryRoot::Operator(root))
            })
            .collect();
        let stage1 = stage1_logical_candidates(roots, &Default::default(), &[]).unwrap();
        let demand = vec![every_10s(None); queries.len()];
        let data = data();
        let variant = *variants(&stage1)
            .iter()
            .find(|v| v.sharing == Sharing::IdenticalExpressions)
            .unwrap();
        let raw = vec![0; variant.inventory.targets.len()];
        let models = PlanningModels::builtin();
        let (_, mut stage2) = realize(variant, &raw, &demand, &data, &models).unwrap();
        let candidate = stage2.candidates.swap_remove(0);
        let cost = assess(&candidate, &demand, &data, &models).unwrap();
        let of = |pick: fn(&NonASAPOp<PhysicalASAPNodeId>) -> bool| -> Vec<&NodeCost> {
            candidate
                .dag
                .nodes
                .iter()
                .filter(|n| matches!(&n.payload, Payload::NonASAP(operator) if pick(operator)))
                .map(|n| &cost.per_node[&n.id])
                .collect()
        };
        let shifts = of(|op| matches!(op, NonASAPOp::TimeShift { .. }));
        assert_eq!(shifts.len(), 1);
        assert_eq!(shifts[0].cost, 0.0, "{}", shifts[0].detail);
        let scan = of(|op| matches!(op, NonASAPOp::Scan { .. }))[0].rows;
        let mut ranges = of(|op| matches!(op, NonASAPOp::TimeRange { .. }));
        ranges.sort_by_key(|n| n.rows);
        let [year, five_years] = ranges.as_slice() else {
            panic!("two time ranges");
        };
        assert_eq!(five_years.rows, scan);
        assert!((five_years.rows as f64 / year.rows as f64 - 5.0).abs() < 0.01);
        // Charged per kept row: the same price per row for both.
        let per_row = |n: &NodeCost| n.cost / n.rows as f64;
        assert!((per_row(year) / per_row(five_years) - 1.0).abs() < 1e-9);
        assert!(
            year.detail.contains(&format!("of {scan} rows")),
            "{}",
            year.detail
        );
    }

    // ── Deployment capabilities (C3, Q48) ────────────────────────────────

    fn reasons(selection: &Selection) -> BTreeMap<&str, &str> {
        selection
            .rejected
            .iter()
            .filter(|r| !r.valid)
            .map(|r| (r.id.as_str(), r.reason.as_str()))
            .collect()
    }

    /// A summary the deployment cannot build, or a readout it cannot
    /// compute, rejects the candidate as invalid, naming what is missing.
    #[test]
    fn missing_summary_or_readout_is_rejected_with_its_reason() {
        use asap_types::deployment::{InstanceLayout, SummaryFamily, SummarySupport};
        let counter = [("m".to_string(), asap_types::workload::MetricType::Counter)].into();
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        };
        // Both heap sketches can be built; only Count-Min's top-k is read.
        let support = |algorithm, readouts: &[_]| SummarySupport {
            family: SummaryFamily::Sketch(algorithm),
            layout: InstanceLayout::PerGroup,
            readouts: readouts.iter().copied().collect(),
        };
        use asap_types::deployment::Readout::TopK;
        let caps = DeploymentCapabilities {
            summaries: Some(vec![
                support(SketchAlgorithm::CmsWithHeap, &[TopK]),
                support(SketchAlgorithm::CountSketchWithHeap, &[]),
                SummarySupport {
                    family: SummaryFamily::Exact(asap_types::ir::schema::ExactKind::Sum),
                    ..support(SketchAlgorithm::Kll, &[])
                },
            ]),
            ..DeploymentCapabilities::UNRESTRICTED
        };
        let select = |caps| {
            stage3_select(
                &candidates_with(&counter),
                &[every_10s(Some(target.clone()))],
                &data(),
                PlanningModels::builtin().with_capabilities(caps),
            )
            .unwrap()
        };
        let selection = select(&caps);
        assert_eq!(
            reasons(&selection),
            BTreeMap::from([(
                "P3",
                "q1: deployment lacks a CountSketchWithHeap TopK readout"
            )])
        );
        assert!(selection.costs.contains_key("P2"));
        // Without the Count-Min + heap summary, P2 cannot be built either.
        let without_cms = DeploymentCapabilities {
            summaries: caps.summaries.clone().map(|mut s| {
                s.remove(0);
                s
            }),
            ..caps
        };
        let selection = select(&without_cms);
        assert_eq!(
            reasons(&selection)["P2"],
            "q1: deployment lacks a CmsWithHeap summary"
        );
    }

    /// A deployment that cannot maintain state at ingestion time rejects
    /// every candidate running a node then, and only those.
    #[test]
    fn ingestion_time_is_rejected_when_unsupported() {
        let (run, demand, data) = pattern_b(data(), None);
        let all: Vec<_> = run
            .enumeration
            .unwrap()
            .candidates
            .into_iter()
            .flat_map(|c| c.physical)
            .collect();
        let caps = DeploymentCapabilities {
            ingestion_time: false,
            ..DeploymentCapabilities::UNRESTRICTED
        };
        let selection = stage3_select(
            &all,
            &demand,
            &data,
            PlanningModels::builtin().with_capabilities(&caps),
        )
        .unwrap();
        let invalid = reasons(&selection);
        for p in &all {
            let maintained = p.materialization.starts_with("ingestion time");
            assert_eq!(invalid.contains_key(p.id.as_str()), maintained, "{}", p.id);
            if maintained {
                assert_eq!(
                    invalid[p.id.as_str()],
                    "deployment cannot maintain state at ingestion time"
                );
            }
        }
    }

    /// A candidate retaining more state than the deployment's memory budget
    /// is rejected; at the budget it is priced.
    #[test]
    fn memory_budget_is_enforced_on_retained_state() {
        let (run, demand, data) = pattern_b(data(), None);
        let (p, _) = maintained_panes(&run);
        let p = p.clone();
        // data(): 10 000 series; the newest pane keeps itself and 5 others.
        let Payload::ASAP(ASAPOp::SummaryAgg { family, .. }) = &node_of(&p.dag, is_build).payload
        else {
            unreachable!()
        };
        let retained = 6 * 10_000 * summary_shape(family).1;
        let select = |budget| {
            let caps = DeploymentCapabilities {
                memory_budget_bytes: Some(budget),
                ..DeploymentCapabilities::UNRESTRICTED
            };
            stage3_select(
                std::slice::from_ref(&p),
                &demand,
                &data,
                PlanningModels::builtin().with_capabilities(&caps),
            )
        };
        assert!(select(retained).is_ok());
        let Err(SelectionError::NoValidCandidate(rejected)) = select(retained - 1) else {
            panic!("over the budget")
        };
        assert_eq!(
            rejected[0].reason,
            format!(
                "retains {retained} bytes across evaluations, over the deployment's memory \
                 budget of {} bytes",
                retained - 1
            )
        );
    }

    /// When the deployment does not keep raw data anyway, a query-time scan
    /// pays w × lookback × λ × bytes per sample; an ingestion-time scan, and
    /// any scan when raw data is kept, pays nothing for retention.
    #[test]
    fn raw_retention_is_charged_only_without_raw_data_and_not_for_maintained_inputs() {
        let query = "quantile_over_time(0.99, m[1m])";
        let priced = |ingestion, raw| {
            price_nodes(
                &kll_dag(query, ingestion),
                &[every_10s(None)],
                &data(),
                &Stage3Calibration::ILLUSTRATIVE,
                raw,
            )
            .unwrap()
        };
        // data(): λ = 10 000 rows/s over 1 min, 16 bytes per sample.
        let raw_bytes = 60 * 10_000 * 16;
        let (kept, not_kept) = (priced(false, None), priced(false, Some(16)));
        let scan = node_of(&kll_dag(query, false), is_scan).id;
        let charge = not_kept.cost.per_node[&scan].cost - kept.cost.per_node[&scan].cost;
        assert!(
            (charge - 1.25e-7 * raw_bytes as f64).abs() < 1e-12,
            "{charge}"
        );
        assert!((not_kept.cost.total - kept.cost.total - charge).abs() < 1e-12);
        assert_eq!(not_kept.retained_bytes - kept.retained_bytes, raw_bytes);
        assert!(not_kept.cost.per_node[&scan]
            .detail
            .ends_with(&format!("retains {raw_bytes} bytes of raw data")));
        let (kept, not_kept) = (priced(true, None), priced(true, Some(16)));
        assert_eq!(kept.cost, not_kept.cost);
        assert_eq!(kept.retained_bytes, not_kept.retained_bytes);
    }
}
