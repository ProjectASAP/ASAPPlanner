//! `asap-plan-selection` — #509 Stage 3 (MVP): plan selection, the only stage
//! that computes cost. Cargo enforces the stage order: this crate depends on
//! `asap-types`, Stage 1 and Stage 2, never on the facade or the executor.
//!
//! - [`cost`] — the [`CostModel`] trait, analytical and evidence-based
//!   pricing, recurrence, and the physical lowering and storage I/O they price.
//! - [`candidate_selection`] — the legacy cost-ranked selection over a Stage 1
//!   search (deleted under #580).
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
//! plus memory for ingestion-time state that query time reads. Prices come
//! from [`crate::cost::analytical_cost::estimate_operator`] over edge
//! statistics derived from the [`DataWorkload`] and a fixed default group
//! count, weighted by [`Stage3Calibration`]; summary build and estimation are
//! priced as rows × sketch depth and rows read out. These numbers are
//! illustrative, not calibrated. Latency bounds and deployment capabilities
//! are not checked yet.
//!
//! [`select_plan`] chooses over Stage 1's sharing variants without building
//! every combination: per variant, a dynamic program over target nesting (see
//! there). [`select_exhaustive`] builds and prices every combination, for
//! display and for checking the program. [`plan_stages`] runs the whole
//! pipeline from the frontends' roots.
pub mod candidate_selection;
pub mod cost;
#[cfg(test)]
mod test_support;

pub use candidate_selection::{
    CompositionDecision, CostedGlobalSelection, RankedTargetSubDAGCandidates, RecurrenceProfileMap,
};
pub use cost::cost_model::{
    maintenance_operation_plan_cost_rate, raw_recompute_cost_rate, read_operation_plan_cost_rate,
    CostModel, CostProvenance, CostUnit, DefaultCostModel, ExactCompositionCostInputs,
    ExactCompositionCostRequest, ValueOperationCapabilities,
};
pub use cost::recurrence::{
    evaluation_rate_of, total_cost, update_rate_from_data_workload, CostRate, EvaluationRate,
    Horizon, RecurrenceCostExplanation, RecurrenceError, RecurrenceProfile, RootRecurrence,
    UpdateRate,
};

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use asap_types::ir::export::{
    NonASAPOpKind, PhysicalASAPDAG, PhysicalASAPNodeId, PhysicalASAPOperatorPayload as Payload,
};
use asap_types::ir::operator::Reduction;
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
pub use asap_logical_optimizer::pass2::identical_expressions::SharingVariant;
use asap_logical_optimizer::pass2::identical_expressions::{
    share_identical_expressions, stage1_logical_candidates,
};
use asap_physical_optimizer::implementation::physical_candidates::{
    stage2_physical, PhysicalCandidate,
};

/// Cost per second of wall time; one cost unit is one CPU-millisecond under
/// [`Stage3Calibration::ILLUSTRATIVE`]. See
/// `docs/design_docs/proposals/stage3-cost-model.md`.
pub const COST_PER_SECOND: &str = "cost_per_second";

/// Groups assumed for every `by (...)` reduction, absent group-count evidence.
const DEFAULT_GROUP_COUNT: u64 = 100;
/// Used only when the data workload does not declare them.
const DEFAULT_SERIES: u64 = 1_000;
const DEFAULT_ROWS_PER_SECOND: f64 = 1_000.0;
const DEFAULT_LOOKBACK_MS: u64 = 60_000;
/// Most combinations built for display, and for selection when the dynamic
/// program's assumptions do not hold.
pub const MAX_ENUMERATED_CANDIDATES: usize = 64;

static DEFAULT_COST_MODEL: DefaultCostModel = DefaultCostModel;
static DEFAULT_ACCURACY_MODEL: DefaultAccuracyModel = DefaultAccuracyModel;
static NO_ACCURACY_EVIDENCE: NoAccuracyEvidence = NoAccuracyEvidence;

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
    pub version: &'static str,
}

impl Stage3Calibration {
    /// 1 ns of CPU per operation; 1 GB retained costs 1/8 vCPU
    /// (125 CPU-ms per second); one-off work is amortized over 1 h.
    pub const ILLUSTRATIVE: Self = Self {
        cost_per_cpu_op: 1e-6,
        cost_per_scan_byte: 1e-7,
        cost_per_retained_byte_second: 1.25e-7,
        horizon_s: 3_600.0,
        version: "illustrative-v2",
    };

    fn validate(&self) -> Result<(), AnalyticalCostError> {
        for (name, value) in [
            ("cost_per_cpu_op", self.cost_per_cpu_op),
            ("cost_per_scan_byte", self.cost_per_scan_byte),
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
/// a built-in default, evidence about a particular deployment cannot.
///
/// Stage 3 prices plans analytically, so the stage pipeline does not read
/// `cost`; only the legacy replacement search does (#580).
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct PlanningModels<'a> {
    pub cost: &'a dyn CostModel,
    pub accuracy: &'a dyn AccuracyModel,
    pub evidence: &'a dyn AccuracyEvidenceProvider,
    pub calibration: Stage3Calibration,
}

impl<'a> PlanningModels<'a> {
    pub fn new(
        cost: &'a dyn CostModel,
        accuracy: &'a dyn AccuracyModel,
        evidence: &'a dyn AccuracyEvidenceProvider,
    ) -> Self {
        Self {
            cost,
            accuracy,
            evidence,
            calibration: Stage3Calibration::ILLUSTRATIVE,
        }
    }

    /// The built-in models. `DefaultCostModel` does not override
    /// `estimate_cost`, so this configuration ranks structurally and is not a
    /// measured deployment cost.
    pub fn builtin() -> PlanningModels<'static> {
        PlanningModels {
            cost: &DEFAULT_COST_MODEL,
            accuracy: &DEFAULT_ACCURACY_MODEL,
            evidence: &NO_ACCURACY_EVIDENCE,
            calibration: Stage3Calibration::ILLUSTRATIVE,
        }
    }

    pub fn with_cost(mut self, cost: &'a dyn CostModel) -> Self {
        self.cost = cost;
        self
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
    /// and there were too many combinations to enumerate.
    TreeDpNotGuaranteedOptimal { reason: String },
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
    if let Some(reason) = accuracy_violation(candidate, demand, models) {
        return Err(reason);
    }
    price(&candidate.dag, demand, data, &models.calibration)
        .map_err(|(node, error)| format!("node {node:?}: {error}"))
}

/// One Stage 1 sharing variant as selection sees it: candidates of variant
/// `v` are numbered after every candidate of the variants before it, so ids
/// stay unique across variants.
struct Variant<'a, Id> {
    inventory: &'a LocalLogicalCandidates<Id>,
    /// Identical sub-DAGs are merged, after composition too.
    shared: bool,
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
                shared: v.shared,
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
        shared: false,
        inventory,
    }]
}

/// Stage 1 → Stage 2 for `choice` in the independent variant, named
/// `P<index+1>` from `L<index+1>`. `Err` is the reason the candidate cannot
/// be built.
pub fn realize_choice<Id: Clone>(
    inventory: &LocalLogicalCandidates<Id>,
    choice: &[usize],
) -> Result<(Vec<(Id, QueryRoot)>, PhysicalCandidate), String> {
    realize(
        Variant {
            inventory,
            shared: false,
            offset: 0,
        },
        choice,
    )
}

/// Stage 1 → Stage 2 for `choice` in `variant`. A shared variant also
/// merges identical sub-DAGs after composition, so queries that chose the
/// same summary producer reach one node; the returned Stage 1 candidate is
/// the merged one.
fn realize<Id: Clone>(
    variant: Variant<'_, Id>,
    choice: &[usize],
) -> Result<(Vec<(Id, QueryRoot)>, PhysicalCandidate), String> {
    let index = variant.number(choice);
    let mut logical = compose_logical_candidate(variant.inventory, choice)
        .map_err(|e| format!("Stage 1: {e}"))?;
    if variant.shared {
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
    let mut candidate =
        stage2_physical(&format!("L{index}"), &roots).map_err(|e| format!("Stage 2: {e}"))?;
    candidate.id = format!("P{index}");
    candidate.label = format!("{choice:?}");
    Ok((logical, candidate))
}

/// One built combination; `physical` is `None` when it could not be built.
#[derive(Debug, Clone)]
pub struct EnumeratedCandidate<Id> {
    /// From the shared variant (Pass 2's identical-expression rule).
    pub shared: bool,
    pub choice: Vec<usize>,
    pub logical: Option<Vec<(Id, QueryRoot)>>,
    pub physical: Option<PhysicalCandidate>,
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
    for variant in variants {
        let left = max.saturating_sub(candidates.len());
        for choice in enumerate_choices(variant.inventory, left) {
            let (logical, physical) = match realize(*variant, &choice) {
                Ok((logical, physical)) => (Some(logical), Some(physical)),
                Err(reason) => {
                    failed.push(Rejection {
                        id: format!("P{}", variant.number(&choice)),
                        valid: false,
                        reason,
                    });
                    // Composition may have succeeded: keep it for display.
                    (
                        compose_logical_candidate(variant.inventory, &choice).ok(),
                        None,
                    )
                }
            };
            candidates.push(EnumeratedCandidate {
                shared: variant.shared,
                choice,
                logical,
                physical,
            });
        }
    }
    let physical: Vec<_> = candidates
        .iter()
        .filter_map(|c| c.physical.clone())
        .collect();
    let selection = match stage3_select(&physical, demand, data, models) {
        Ok(mut selection) => {
            selection.rejected.extend(failed);
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
    /// From the shared variant (Pass 2's identical-expression rule).
    pub shared: bool,
    pub choice: Vec<usize>,
    /// The chosen Stage 1 candidate (identical sub-DAGs merged when `shared`).
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
        let evaluate = |choice: &[usize]| -> Result<f64, String> {
            let (_, candidate) = realize(variant, choice)?;
            assess(&candidate, demand, data, &models).map(|cost| cost.total)
        };
        let plan = match select_variant(variant, demand, data, models, &evaluate) {
            Ok(plan) => plan,
            Err(SelectionError::NoValidCandidate(reasons)) => {
                failures.extend(reasons);
                continue;
            }
            Err(other) => return Err(other),
        };
        if let SelectionMethod::TreeDpNotGuaranteedOptimal { reason } = &plan.selection.method {
            not_optimal.get_or_insert_with(|| reason.clone());
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
/// built, or, in a shared variant, two targets reading one input build
/// identical producers that are then merged. That coupling is checked:
/// every pair of choices for a target and a target beneath it (and, when
/// shared, for two targets reading a common input) is built, and must cost
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
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (t, by_choice) in reads.iter().enumerate() {
        for &u in by_choice.iter().flatten() {
            if !pairs.contains(&(t, u)) {
                pairs.push((t, u));
            }
        }
    }
    if variant.shared {
        pairs.extend(common_input_pairs(inventory, &beneath));
    }
    let mut coupling = None;
    'pairs: for &(t, u) in &pairs {
        for c in 1..local[t].len() {
            if !reads[t][c].contains(&u) {
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
/// node: in a shared variant their producers may be merged.
fn common_input_pairs<Id>(
    inventory: &LocalLogicalCandidates<Id>,
    beneath: &[Vec<usize>],
) -> Vec<(usize, usize)> {
    let inputs: Vec<Vec<*const OperatorNode>> = inventory
        .targets
        .iter()
        .map(|t| t.target.children().into_iter().map(Rc::as_ptr).collect())
        .collect();
    let mut pairs = Vec::new();
    for t in 0..inputs.len() {
        for u in t + 1..inputs.len() {
            let nested = beneath[t].contains(&u) || beneath[u].contains(&t);
            if !nested && inputs[t].iter().any(|p| inputs[u].contains(p)) {
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

/// Build `choice` in `variant` and run Stage 3 on it.
fn finish<Id: Clone>(
    variant: Variant<'_, Id>,
    demand: &[RootDemand],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
    choice: Vec<usize>,
    method: SelectionMethod,
) -> Result<SelectedPlan<Id>, String> {
    let (logical, physical) = realize(variant, &choice)?;
    let cost = assess(&physical, demand, data, models)?;
    Ok(SelectedPlan {
        shared: variant.shared,
        choice,
        logical,
        selection: Selection {
            selected: physical.id.clone(),
            costs: BTreeMap::from([(physical.id.clone(), cost)]),
            rejected: Vec::new(),
            method,
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
                    .as_ref()
                    .is_some_and(|p| p.id == enumeration.selection.selected)
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
/// identical-expression rule), Stage 2 and Stage 3. The facade and the
/// `stage_pipeline` devtool both run this. `display` builds and prices up to
/// that many candidates for display as well (0: none).
pub fn plan_stages<Id: Clone>(
    roots: Vec<(Id, QueryRoot)>,
    demand: &[RootDemand],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    display: usize,
) -> Result<StagePipelineRun<Id>, SelectionError> {
    let stage1 = stage1_logical_candidates(roots, &data.metric_types)?;
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
            let Operator::ASAP(ASAPOp::SummaryAgg { family, input, .. }) = &summary_input.operator
            else {
                return Some(format!("q{}: estimate over a non-summary input", query + 1));
            };
            let name = family_name(family);
            // Count-Min's one-sided error bound assumes no negative updates.
            if matches!(family, FieldDataType::Sketch(kind, _)
                    if matches!(kind.algorithm(), SketchAlgorithm::Cms | SketchAlgorithm::CmsWithHeap))
                && !matches!(input.weight_domain, WeightDomain::NonNegative { .. })
            {
                return Some(format!(
                    "q{}: {name} needs non-negative update weights, and these are not proven \
                     non-negative",
                    query + 1
                ));
            }
            let Some(guarantee) = models.accuracy.local_guarantee(family, statistic) else {
                return Some(format!(
                    "q{}: no accuracy model for {name}; target {target:?}",
                    query + 1
                ));
            };
            if !models.accuracy.satisfies(&guarantee, target) {
                return Some(format!(
                    "q{}: {name} guarantees bound {:?}, failure probability {:?}, which misses \
                     target {target:?} (analytical guarantee; no accuracy evidence)",
                    query + 1,
                    guarantee.bound.evaluate(),
                    guarantee.failure_probability.evaluate(),
                ));
            }
        }
    }
    None
}

fn family_name(family: &FieldDataType) -> String {
    match family {
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
fn scan_extent_ms(dag: &PhysicalASAPDAG, id: PhysicalASAPNodeId, offset_ms: i64) -> Option<u64> {
    dag.edges
        .iter()
        .filter(|e| e.producer == id)
        .filter_map(|e| {
            let consumer = dag.nodes.iter().find(|n| n.id == e.consumer)?;
            match &consumer.payload {
                Payload::Relational {
                    operator: NonASAPOpKind::TimeShift { shift },
                } => scan_extent_ms(dag, consumer.id, offset_ms.saturating_add(shift.offset_ms)),
                Payload::Relational {
                    operator: NonASAPOpKind::TimeRange { range, .. },
                } => Some((range.as_millis() as u64).saturating_add(offset_ms.max(0) as u64)),
                _ => None,
            }
        })
        .max()
}

/// Price every node of `dag` once, per second of wall time. Nodes are
/// exported children first, so each node's input statistics are known when
/// it is reached. An ingestion-time node is priced over one second of
/// ingested rows; a query-time node per evaluation, times its evaluation
/// rate. State an ingestion-time node keeps for query-time readers is also
/// charged per retained byte per second.
fn price(
    dag: &PhysicalASAPDAG,
    demand: &[RootDemand],
    data: &DataWorkload,
    calibration: &Stage3Calibration,
) -> Result<CandidateCost, (PhysicalASAPNodeId, AnalyticalCostError)> {
    let first = dag
        .roots
        .first()
        .copied()
        .unwrap_or(asap_types::ir::export::LogicalASAPNodeId(0));
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
    let mut output: HashMap<PhysicalASAPNodeId, EdgeStatistics> = HashMap::new();
    let mut per_node = BTreeMap::new();
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
        let groups = |reduction: &Reduction| {
            let groups = match reduction {
                Reduction::Reduce(keys) if keys.keys().is_empty() && !keys.is_without() => 1,
                Reduction::Reduce(keys) if !keys.is_without() => DEFAULT_GROUP_COUNT,
                _ => shape.series,
            };
            // One second of ingested rows does not bound the groups a
            // maintained state holds.
            match ingestion {
                true => groups,
                false => groups.min(input.rows.max(1)),
            }
        };
        let (out, estimate, detail) = match &node.payload {
            Payload::Relational { operator } => match operator {
                NonASAPOpKind::Scan { .. } => {
                    // At ingestion time, one second of arriving rows; at
                    // query time, as far back as the ranges reading it reach.
                    let span_ms = match ingestion {
                        true => 1_000,
                        false => scan_extent_ms(dag, node.id, 0).unwrap_or(DEFAULT_LOOKBACK_MS),
                    };
                    let out = edge(((rows_per_ms * span_ms as f64).round() as u64).max(1));
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
                NonASAPOpKind::Aggregate {
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
                NonASAPOpKind::Sort { keys, partition_by } => {
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
                NonASAPOpKind::Limit {
                    n,
                    offset,
                    partition_by,
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
                // scan: a filter on the timestamp.
                NonASAPOpKind::TimeRange { range, .. } if !ingestion => {
                    let rows = (rows_per_ms * range.as_millis() as f64).round() as u64;
                    let out = edge(input.rows.min(rows.max(1)));
                    let estimate = estimate_operator(
                        PhysicalOperator::Filter {
                            predicate_operations_per_row: 1,
                        },
                        OperatorStatistics::Filter { edges: unary(out) },
                    );
                    (
                        out,
                        estimate,
                        format!("time range {range:?}: pass {} rows", out.rows),
                    )
                }
                other => {
                    let estimate = estimate_operator(
                        PhysicalOperator::PassThrough,
                        OperatorStatistics::PassThrough {
                            edges: unary(input),
                        },
                    );
                    let name = match other {
                        NonASAPOpKind::TimeRange { range, .. } => format!("time range {range:?}"),
                        _ => "operator".into(),
                    };
                    (input, estimate, format!("{name}: pass {} rows", input.rows))
                }
            },
            Payload::SummaryAgg {
                family, reduction, ..
            } => {
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
            Payload::SummaryEstimate { query } => {
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
            Payload::FinalizeExactAccumulator => (
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
            match read_at_query_time {
                // The window being built and the completed one.
                true => {
                    let retained = 2 * out.rows * state_bytes(node);
                    (
                        cost + calibration.cost_per_retained_byte_second * retained as f64,
                        format!("{detail}; ingestion time, retains {retained} bytes"),
                    )
                }
                false => (cost, format!("{detail}; ingestion time")),
            }
        } else {
            let roots = reached.get(&node.id).into_iter().flatten();
            let rate = evaluation_rate(roots.filter_map(|&r| demand.get(r)), calibration.horizon_s)
                .map_err(|error| (node.id, error))?;
            (cost * rate, format!("{detail}; x {rate:.4} evaluations/s"))
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
    Ok(CandidateCost {
        total: per_node.values().map(|n| n.cost).sum(),
        unit: COST_PER_SECOND,
        source: format!(
            "analytical-cost-v2 (illustrative statistics, calibration {})",
            calibration.version
        ),
        per_node,
    })
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
            Payload::SummaryAgg { reduction, .. } => {
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
fn state_bytes(node: &asap_types::ir::export::PhysicalASAPDAGNode) -> u64 {
    match &node.payload {
        Payload::SummaryAgg {
            family: family @ FieldDataType::Sketch(..),
            ..
        } => summary_shape(family).1,
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
            _ => (1, 1_024),
        },
        _ => (1, 8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower_promql;
    use asap_physical_optimizer::implementation::physical_candidates::stage2_physical;
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
                shared: false,
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
                shared: false,
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
                    shared: false,
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
                shared: true,
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
        asap_types::ir::export::compile_physical_asap_workload_with_node_ids(&timed)
            .unwrap()
            .dag
    }

    fn node_of(
        dag: &PhysicalASAPDAG,
        pick: impl Fn(&Payload) -> bool,
    ) -> &asap_types::ir::export::PhysicalASAPDAGNode {
        dag.nodes.iter().find(|n| pick(&n.payload)).unwrap()
    }

    fn is_scan(payload: &Payload) -> bool {
        matches!(
            payload,
            Payload::Relational {
                operator: NonASAPOpKind::Scan { .. }
            }
        )
    }

    fn is_build(payload: &Payload) -> bool {
        matches!(payload, Payload::SummaryAgg { .. })
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
        let Payload::SummaryAgg { family, .. } = &build.payload else {
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
        let stage1 = stage1_logical_candidates(roots, &Default::default()).unwrap();
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
        let total = |shared: bool| {
            let variant = *variants(&stage1)
                .iter()
                .find(|v| v.shared == shared)
                .unwrap();
            let raw = vec![0; variant.inventory.targets.len()];
            let (_, candidate) = realize(variant, &raw).unwrap();
            let cost = assess(&candidate, &demand, &data, &PlanningModels::builtin()).unwrap();
            let scan_rows: Vec<_> = candidate
                .dag
                .nodes
                .iter()
                .filter(|n| is_scan(&n.payload))
                .map(|n| cost.per_node[&n.id].rows)
                .collect();
            (cost.total, scan_rows)
        };
        let (shared, shared_scans) = total(true);
        let (separate, separate_scans) = total(false);
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
}
