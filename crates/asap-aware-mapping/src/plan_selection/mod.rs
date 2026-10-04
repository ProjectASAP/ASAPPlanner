//! #509 Stage 3 (MVP): plan selection, the only stage that computes cost.
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
//! Prices come from [`crate::analytical_cost::estimate_operator`] over edge
//! statistics derived from the [`DataWorkload`] and a fixed default group
//! count; summary build and estimation are priced as rows × sketch depth and
//! rows read out. These numbers are illustrative, not calibrated. Latency
//! bounds and deployment capabilities are not checked yet.
//!
//! [`select_plan`] chooses over a whole Stage 1 inventory without building
//! every combination: a dynamic program over target nesting (see there).
//! [`select_exhaustive`] builds and prices every combination, for display and
//! for checking the program.
pub mod candidate_selection;

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use asap_types::ir::cse::share_common_sub_dags;
use asap_types::ir::export::{
    NonASAPOpKind, PhysicalASAPDAG, PhysicalASAPNodeId, PhysicalASAPOperatorPayload as Payload,
};
use asap_types::ir::operator::Reduction;
use asap_types::ir::schema::{DataType, Schema};
use asap_types::ir::schema::{
    FieldDataType, SketchAlgorithm, SketchParams, SketchStatistic, WeightDomain,
};
use asap_types::ir::{ASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::types::AccuracyTarget;
use asap_types::workload::DataWorkload;
use thiserror::Error;

use crate::analytical_cost::{
    estimate_operator, AnalyticalCostError, PhysicalOperator, ResourceCalibration, ResourceEstimate,
};
use crate::cost_model::{CostModel, DefaultCostModel};
use crate::physical_candidates::{stage2_physical, PhysicalCandidate};
use crate::physical_operator_statistics::{
    EdgeStatistics, OperatorStatistics, PartitionStatistics, UnaryEdgeStatistics,
};
use asap_logical_optimizer::accuracy::{
    AccuracyEvidenceProvider, AccuracyModel, DefaultAccuracyModel, NoAccuracyEvidence,
};
use asap_logical_optimizer::pass1::logical_candidates::{
    choice_index, combination_count, compose_logical_candidate, enumerate_choices, nested_targets,
    LocalLogicalCandidates,
};

pub const COST_UNIT: &str = "cpu_ms_per_workload_evaluation";
pub const COST_SOURCE: &str = "analytical-cost-v1 (illustrative statistics)";

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
    pub source: &'static str,
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
}

/// Reject candidates that miss a query's accuracy target or cannot be priced,
/// price the rest and select the cheapest (the first on ties). `targets[i]`
/// is the requirement of `candidate.roots[i]`; `None` imposes none.
pub fn stage3_select(
    cands: &[PhysicalCandidate],
    targets: &[Option<AccuracyTarget>],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<Selection, SelectionError> {
    let mut costs = BTreeMap::new();
    let mut rejected = Vec::new();
    let mut best: Option<(&str, f64)> = None;
    for candidate in cands {
        match assess(candidate, targets, data, &models) {
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
                    "costlier: {:.3} vs {:.3} {COST_UNIT}",
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
    targets: &[Option<AccuracyTarget>],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> Result<CandidateCost, String> {
    if candidate.roots.len() != targets.len() {
        return Err(format!(
            "{} roots but {} accuracy targets",
            candidate.roots.len(),
            targets.len()
        ));
    }
    if let Some(reason) = accuracy_violation(candidate, targets, models) {
        return Err(reason);
    }
    price(&candidate.dag, data).map_err(|(node, error)| format!("node {node:?}: {error}"))
}

/// Stage 1 → Stage 2 for `choice`, named `P<index+1>` from `L<index+1>`.
/// `Err` is the reason the candidate cannot be built.
pub fn realize_choice<Id: Clone>(
    inventory: &LocalLogicalCandidates<Id>,
    choice: &[usize],
) -> Result<(Vec<(Id, QueryRoot)>, PhysicalCandidate), String> {
    realize(inventory, choice, false)
}

/// As [`realize_choice`]; `merge` first interns structurally identical
/// sub-DAGs across roots, so queries that chose the same summary producer
/// reach one node.
fn realize<Id: Clone>(
    inventory: &LocalLogicalCandidates<Id>,
    choice: &[usize],
    merge: bool,
) -> Result<(Vec<(Id, QueryRoot)>, PhysicalCandidate), String> {
    let index = choice_index(inventory, choice) + 1;
    let logical =
        compose_logical_candidate(inventory, choice).map_err(|e| format!("Stage 1: {e}"))?;
    let mut operators = logical
        .iter()
        .map(|(id, root)| match root {
            QueryRoot::Operator(node) => Ok((id.clone(), node.clone())),
            QueryRoot::Scalar(_) => Err("Stage 2: scalar query roots are not physical yet"),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if merge {
        operators = share_common_sub_dags(operators);
    }
    let roots: Vec<Rc<OperatorNode>> = operators.into_iter().map(|(_, node)| node).collect();
    let mut candidate =
        stage2_physical(&format!("L{index}"), &roots).map_err(|e| format!("Stage 2: {e}"))?;
    candidate.id = format!("P{index}");
    candidate.label = format!("{choice:?}");
    Ok((logical, candidate))
}

/// One built combination; `physical` is `None` when it could not be built.
#[derive(Debug, Clone)]
pub struct EnumeratedCandidate<Id> {
    pub choice: Vec<usize>,
    pub logical: Option<Vec<(Id, QueryRoot)>>,
    pub physical: Option<PhysicalCandidate>,
}

/// Every combination [`select_exhaustive`] built, and Stage 3 over them.
#[derive(Debug, Clone)]
pub struct Enumeration<Id> {
    pub combinations: usize,
    pub candidates: Vec<EnumeratedCandidate<Id>>,
    pub selection: Selection,
}

/// Build the first `max` combinations in enumeration order and select over
/// them. One that cannot be built is rejected with its reason.
pub fn select_exhaustive<Id: Clone>(
    inventory: &LocalLogicalCandidates<Id>,
    targets: &[Option<AccuracyTarget>],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    max: usize,
) -> Result<Enumeration<Id>, SelectionError> {
    let mut candidates = Vec::new();
    let mut failed = Vec::new();
    for choice in enumerate_choices(inventory, max) {
        let (logical, physical) = match realize_choice(inventory, &choice) {
            Ok((logical, physical)) => (Some(logical), Some(physical)),
            Err(reason) => {
                let index = choice_index(inventory, &choice) + 1;
                failed.push(Rejection {
                    id: format!("P{index}"),
                    valid: false,
                    reason,
                });
                // Composition may have succeeded: keep it for display.
                (compose_logical_candidate(inventory, &choice).ok(), None)
            }
        };
        candidates.push(EnumeratedCandidate {
            choice,
            logical,
            physical,
        });
    }
    let physical: Vec<_> = candidates
        .iter()
        .filter_map(|c| c.physical.clone())
        .collect();
    let selection = match stage3_select(&physical, targets, data, models) {
        Ok(mut selection) => {
            selection.rejected.extend(failed);
            selection
        }
        Err(SelectionError::NoValidCandidate(mut rejected)) => {
            rejected.extend(failed);
            return Err(SelectionError::NoValidCandidate(rejected));
        }
    };
    Ok(Enumeration {
        combinations: combination_count(inventory),
        candidates,
        selection,
    })
}

/// The plan [`select_plan`] chose.
#[derive(Debug, Clone)]
pub struct SelectedPlan<Id> {
    pub choice: Vec<usize>,
    /// The chosen Stage 1 candidate, before identical producers are merged.
    pub logical: Vec<(Id, QueryRoot)>,
    /// Stage 2 of `logical` after merging identical sub-DAGs across roots;
    /// `roots` follow `logical`'s order.
    pub physical: PhysicalCandidate,
    pub selection: Selection,
}

/// Relative tolerance when checking that costs add up.
const ADDITIVITY_TOLERANCE: f64 = 1e-9;

/// Choose one alternative per target by a dynamic program over target
/// nesting, then build the winner and check it in full.
///
/// `best(t, c) = local(t, c) + Σ_{u beneath t, read by c} min_c' best(u, c')`,
/// where `local(t, c)` is the change in workload cost when only `t` takes
/// alternative `c`, and a choice is admissible when that one-target
/// candidate builds and passes Stage 3's checks. Every Stage 1 realization
/// reads its target's rewritten input, so every choice reads every target
/// beneath it, and the minimum is taken per target.
///
/// The result is the exhaustive minimum when (1) cost is a sum over nodes,
/// (2) a choice changes only its target's own nodes, and (3) shared nodes do
/// not depend on choices. Stage 3 prices per node and sizes every
/// realization of a target alike, so these hold unless a target's choice
/// changes what a target reading its output costs or whether it can be
/// built. That coupling is checked: every pair of choices for a target and a
/// target beneath it is built, and must cost the sum of their single
/// changes and be admissible exactly when both are. On coupling, or when the
/// winner fails the full check, every combination is built instead if there
/// are at most [`MAX_ENUMERATED_CANDIDATES`]; otherwise the result is
/// flagged as not guaranteed optimal.
pub fn select_plan<Id: Clone>(
    inventory: &LocalLogicalCandidates<Id>,
    targets: &[Option<AccuracyTarget>],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<SelectedPlan<Id>, SelectionError> {
    let evaluate = |choice: &[usize]| -> Result<f64, String> {
        let (_, candidate) = realize_choice(inventory, choice)?;
        assess(&candidate, targets, data, &models).map(|cost| cost.total)
    };
    select_plan_with(inventory, targets, data, models, &evaluate)
}

/// [`select_plan`] with the workload cost of a choice given by `evaluate`.
fn select_plan_with<Id: Clone>(
    inventory: &LocalLogicalCandidates<Id>,
    targets: &[Option<AccuracyTarget>],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    evaluate: &dyn Fn(&[usize]) -> Result<f64, String>,
) -> Result<SelectedPlan<Id>, SelectionError> {
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
            return fallback(inventory, targets, data, models, with(&[]), reason);
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
    let mut coupling = None;
    'pairs: for (t, inner) in beneath.iter().enumerate() {
        for &u in inner {
            for c in 1..local[t].len() {
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
    }
    let mut best: Vec<Option<(f64, usize)>> = vec![None; width];
    for t in 0..width {
        best_choice(t, &local, &beneath, &mut best);
    }
    let choice: Vec<usize> = best.iter().map(|b| b.map_or(0, |(_, c)| c)).collect();
    if let Some(reason) = coupling {
        return fallback(inventory, targets, data, models, choice, reason);
    }
    match finish(
        inventory,
        targets,
        data,
        &models,
        choice.clone(),
        SelectionMethod::TreeDp,
    ) {
        Ok(plan) => Ok(plan),
        Err(reason) => fallback(inventory, targets, data, models, choice, reason),
    }
}

/// `best(t) = min_c local(t, c) + Σ_{u beneath t} best(u)`, memoized; the
/// first alternative wins ties, as in enumeration order. Alternative 0 (the
/// pass-through) is admissible whenever the base plan is.
fn best_choice(
    t: usize,
    local: &[Vec<Result<f64, String>>],
    beneath: &[Vec<usize>],
    best: &mut [Option<(f64, usize)>],
) -> f64 {
    if let Some((cost, _)) = best[t] {
        return cost;
    }
    let inner: f64 = beneath[t]
        .iter()
        .map(|&u| best_choice(u, local, beneath, best))
        .sum();
    let (cost, choice) = local[t]
        .iter()
        .enumerate()
        .filter_map(|(c, cost)| cost.as_ref().ok().map(|cost| (cost + inner, c)))
        .fold(
            (f64::INFINITY, 0),
            |min, next| if next.0 < min.0 { next } else { min },
        );
    best[t] = Some((cost, choice));
    cost
}

/// Build `choice` with identical producers merged and run Stage 3 on it.
fn finish<Id: Clone>(
    inventory: &LocalLogicalCandidates<Id>,
    targets: &[Option<AccuracyTarget>],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
    choice: Vec<usize>,
    method: SelectionMethod,
) -> Result<SelectedPlan<Id>, String> {
    let (logical, physical) = realize(inventory, &choice, true)?;
    let cost = assess(&physical, targets, data, models)?;
    Ok(SelectedPlan {
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
/// combination if there are few, else `choice` flagged with `reason`.
fn fallback<Id: Clone>(
    inventory: &LocalLogicalCandidates<Id>,
    targets: &[Option<AccuracyTarget>],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    choice: Vec<usize>,
    reason: String,
) -> Result<SelectedPlan<Id>, SelectionError> {
    if combination_count(inventory) <= MAX_ENUMERATED_CANDIDATES {
        let enumeration =
            select_exhaustive(inventory, targets, data, models, MAX_ENUMERATED_CANDIDATES)?;
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
            inventory,
            targets,
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
    finish(inventory, targets, data, &models, choice.clone(), method).map_err(|failure| {
        SelectionError::NoValidCandidate(vec![Rejection {
            id: format!("P{}", choice_index(inventory, &choice) + 1),
            valid: false,
            reason: format!("{reason}; {failure}"),
        }])
    })
}

/// The first summary estimate that misses its query's target, as a reason.
fn accuracy_violation(
    candidate: &PhysicalCandidate,
    targets: &[Option<AccuracyTarget>],
    models: &PlanningModels<'_>,
) -> Option<String> {
    for (query, (root, target)) in candidate.roots.iter().zip(targets).enumerate() {
        let Some(target) = target else { continue };
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
    rows_per_ms: f64,
}

/// Price every node of `dag` once. Nodes are exported children first, so
/// each node's input statistics are known when it is reached.
fn price(
    dag: &PhysicalASAPDAG,
    data: &DataWorkload,
) -> Result<CandidateCost, (PhysicalASAPNodeId, AnalyticalCostError)> {
    let series = data
        .input_cardinality
        .value
        .unwrap_or(DEFAULT_SERIES)
        .max(1);
    let shape = Shape {
        series,
        rows_per_ms: data
            .ingestion_rate
            .value
            .map_or(DEFAULT_ROWS_PER_SECOND, |rate| rate.0)
            / 1_000.0,
    };
    let calibration = ResourceCalibration {
        cost_per_cpu_op: 1e-6,
        cost_per_scan_byte: 1e-7,
        cost_per_retained_byte: 0.0,
        version: "illustrative-v1".into(),
    };
    let nodes: HashMap<_, _> = dag.nodes.iter().map(|n| (n.id, n)).collect();
    let mut output: HashMap<PhysicalASAPNodeId, EdgeStatistics> = HashMap::new();
    let mut per_node = BTreeMap::new();
    for node in &dag.nodes {
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
            match reduction {
                Reduction::Reduce(keys) if keys.keys().is_empty() && !keys.is_without() => 1,
                Reduction::Reduce(keys) if !keys.is_without() => DEFAULT_GROUP_COUNT,
                _ => shape.series,
            }
            .min(input.rows.max(1))
        };
        let (out, estimate, detail) = match &node.payload {
            Payload::Relational { operator } => match operator {
                NonASAPOpKind::Scan { .. } => {
                    // A scan reads what its time range keeps.
                    let lookback = dag
                        .edges
                        .iter()
                        .filter(|e| e.producer == node.id)
                        .filter_map(|e| match &nodes[&e.consumer].payload {
                            Payload::Relational {
                                operator: NonASAPOpKind::TimeRange { range, .. },
                            } => Some(range.as_millis() as u64),
                            _ => None,
                        })
                        .max()
                        .unwrap_or(DEFAULT_LOOKBACK_MS);
                    let out = edge(((shape.rows_per_ms * lookback as f64).round() as u64).max(1));
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
                    SketchStatistic::TopK { k } => {
                        let (summarized, grouped) = summarized_rows(dag, &output, node.id);
                        selected_rows(
                            summarized,
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
            .and_then(|estimate| estimate.calibrated_cost(&calibration))
            .map_err(|error| (node.id, error))?;
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
        unit: COST_UNIT,
        source: COST_SOURCE,
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
    use crate::physical_candidates::stage2_physical;
    use crate::test_support::lower_promql;
    use asap_types::ir::QueryRoot;
    use asap_types::workload::{Evidence, Rate};

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
            &[Some(strict)],
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
            &[Some(target)],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        let p2 = selection.rejected.iter().find(|r| r.id == "P2").unwrap();
        assert!(!p2.valid);
        assert!(p2.reason.contains("non-negative"), "{}", p2.reason);
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
        asap_logical_optimizer::pass1::logical_candidates::enumerate_local_logical_candidates(roots)
            .unwrap()
    }

    fn no_targets(inventory: &LocalLogicalCandidates<usize>) -> Vec<Option<AccuracyTarget>> {
        vec![None; inventory.roots.len()]
    }

    /// Real Stage 1 → 3 cost plus a penalty whenever the two named targets
    /// both leave their pass-through: an inner choice that changes the cost
    /// of the target reading it.
    fn coupled<'a>(
        inventory: &'a LocalLogicalCandidates<usize>,
        targets: &'a [Option<AccuracyTarget>],
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
        let plan = select_plan_with(
            &inventory,
            &targets,
            &data,
            PlanningModels::builtin(),
            &evaluate,
        )
        .unwrap();
        assert_eq!(plan.selection.method, SelectionMethod::Exhaustive);
        let exhaustive = select_exhaustive(
            &inventory,
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
        let plan = select_plan_with(
            &inventory,
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
        let plan = select_plan(&inventory, &targets, &data(), PlanningModels::builtin()).unwrap();
        assert_eq!(plan.selection.method, SelectionMethod::TreeDp);
        let exhaustive = select_exhaustive(
            &inventory,
            &targets,
            &data(),
            PlanningModels::builtin(),
            MAX_ENUMERATED_CANDIDATES,
        )
        .unwrap();
        assert_eq!(plan.selection.selected, exhaustive.selection.selected);
    }

    /// Two queries that chose structurally identical summary producers reach
    /// one state in the selected plan.
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
            &inventory,
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
            &[Some(target)],
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
                    let cost = price(&candidate.dag, &data).unwrap();
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
            &[Some(target)],
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
}
