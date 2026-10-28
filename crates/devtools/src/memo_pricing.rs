//! Price a memo's enumerated choices as the selected plan is priced: each
//! choice's composed DAG (`memo::enumeration::compose`) goes through Stage 2
//! (`optimize_physical_asap`, every materialization assignment) and Stage 3
//! (`select_plan`, under the run's models), so a choice cheaper than the
//! selected plan shows that the search missed a plan the memo stands for.

use std::rc::Rc;

use asap_logical_asap_optimization::memo::candidates::CandidateMemo;
use asap_logical_asap_optimization::memo::enumeration::{compose, MemoChoice};
use asap_physical_asap_optimization::optimize_physical_asap;
use asap_plan_selection::{select_plan, PlanningModels, Selection, SelectionError};
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

/// One choice's result: valid with its cost, or why it is not valid.
#[derive(Debug, Clone, PartialEq)]
pub struct PricedChoice {
    /// Stage 2 implemented it and Stage 3 accepted one of its physical plans.
    pub valid: bool,
    /// Why it is not valid: the Stage 3 rejection cause of its first
    /// physical plan in snake_case (e.g. `accuracy_target_missed`), or
    /// `compose_failed`, `invalid_structure`, `stage2_failed`,
    /// `stage3_failed`.
    pub rejection: Option<String>,
    /// The rejection's one-line reason, with the numbers.
    pub rejection_reason: Option<String>,
    /// The cheapest cost per second of its valid physical plans under the
    /// run's cost model; `None` when not valid or under cost model 3.
    pub cost_per_second: Option<f64>,
    /// Valid and costs less than the selected plan (beyond rounding).
    pub cheaper_than_selected: bool,
}

/// What the choices are compared against: the run's selection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelectedBaseline {
    /// The selected plan's cost per second.
    pub cost_per_second: Option<f64>,
    /// The calibration factor the run priced with, frozen for every choice
    /// so each is priced on the same scale as the selected plan.
    pub calibration_factor: Option<f64>,
}

impl SelectedBaseline {
    /// The baseline of `selection`: its selected plan's cost and the
    /// calibration factor its search reported.
    pub fn of(selection: &Selection) -> Self {
        Self {
            cost_per_second: selection.costs[&selection.selected].cost_per_second,
            calibration_factor: selection
                .search
                .candidate_connections
                .as_ref()
                .map(|report| report.calibration_factor),
        }
    }
}

/// Costs within this fraction of each other are the same sum of charges
/// rounded differently (Stage 3's tie tolerance).
const TIE_TOLERANCE: f64 = 1e-9;

/// Each of `choices` of `memo`, composed, implemented and priced. Query `i`
/// is `stage0_roots[i]` with workload entry `entries[i]`; its operator
/// references are replaced by their root groups' composed DAGs.
pub fn price_choices(
    memo: &CandidateMemo,
    choices: &[MemoChoice],
    stage0_roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    selected: Option<SelectedBaseline>,
) -> Vec<PricedChoice> {
    let mut settings = models.plan_selection.clone();
    if settings.frozen_calibration_factor.is_none() {
        settings.frozen_calibration_factor = selected.and_then(|s| s.calibration_factor);
    }
    let models = models.with_plan_selection(&settings);
    let selected_cost = selected.and_then(|s| s.cost_per_second);
    choices
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            let mut priced = price_choice(memo, choice, index, stage0_roots, entries, data, models);
            priced.cheaper_than_selected = match (priced.cost_per_second, selected_cost) {
                (Some(cost), Some(selected)) => {
                    cost < selected - TIE_TOLERANCE * selected.abs().max(1.0)
                }
                _ => false,
            };
            priced
        })
        .collect()
}

fn price_choice(
    memo: &CandidateMemo,
    choice: &MemoChoice,
    index: usize,
    stage0_roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> PricedChoice {
    let invalid = |cause: &str, reason: String| PricedChoice {
        valid: false,
        rejection: Some(cause.into()),
        rejection_reason: Some(reason),
        cost_per_second: None,
        cheaper_than_selected: false,
    };
    let roots = match compose(memo, choice)
        .map_err(|error| error.to_string())
        .and_then(|composed| query_roots(memo, &composed, stage0_roots))
    {
        Ok(roots) => roots,
        Err(error) => return invalid("compose_failed", error),
    };
    if let Err(error) = roots.iter().try_for_each(QueryRoot::validate_structure) {
        return invalid("invalid_structure", error.to_string());
    }
    let id = format!("E{}", index + 1);
    let mut physical =
        match optimize_physical_asap(&id, &roots, entries, data, models.deployment_capabilities) {
            Ok(physical) if !physical.is_empty() => physical,
            Ok(_) => return invalid("stage2_failed", "no physical plan".into()),
            Err(error) => return invalid("stage2_failed", error.to_string()),
        };
    for (k, plan) in physical.iter_mut().enumerate() {
        plan.id = format!("{id}-{k}");
    }
    match select_plan(&physical, entries, data, models) {
        Ok(selection) => PricedChoice {
            valid: true,
            rejection: None,
            rejection_reason: None,
            cost_per_second: selection.costs[&selection.selected].cost_per_second,
            cheaper_than_selected: false,
        },
        Err(SelectionError::NoValidCandidate(rejected)) => match rejected.first() {
            Some(first) => invalid(
                serde_json::to_value(first.cause)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .as_deref()
                    .unwrap_or("rejected"),
                first.reason.clone(),
            ),
            None => invalid("rejected", "no valid physical plan".into()),
        },
        Err(error) => invalid("stage3_failed", error.to_string()),
    }
}

/// Each query's root with every operator reference replaced by the composed
/// DAG of the root group it names.
fn query_roots(
    memo: &CandidateMemo,
    composed: &[(
        asap_logical_asap_optimization::memo::MemoGroupId,
        Rc<OperatorNode>,
    )],
    stage0_roots: &[QueryRoot],
) -> Result<Vec<QueryRoot>, String> {
    let composed_of = |query: usize, original: &Rc<OperatorNode>| {
        composed
            .iter()
            .find(|(group, _)| {
                let group = &memo.groups[group.0 as usize];
                group.roots_of.contains(&query)
                    && (Rc::ptr_eq(&group.node, original) || *group.node == **original)
            })
            .map(|(_, node)| Rc::clone(node))
            .ok_or_else(|| format!("query {query}'s root has no root group"))
    };
    stage0_roots
        .iter()
        .enumerate()
        .map(|(query, root)| match root {
            QueryRoot::Operator(node) => composed_of(query, node).map(QueryRoot::Operator),
            QueryRoot::Scalar(expression) => {
                let mut error = None;
                let mapped = expression.map_operator_refs(&mut |node| {
                    composed_of(query, node).unwrap_or_else(|e| {
                        error = Some(e);
                        Rc::clone(node)
                    })
                });
                error.map_or(Ok(QueryRoot::Scalar(mapped)), Err)
            }
        })
        .collect()
}
