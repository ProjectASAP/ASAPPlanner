//! Price a memo's enumerated choices as the selected plan is priced: each
//! choice's composed DAG (`memo::enumeration::build_logical_asap_dags_from_chosen_candidates`) goes through Stage 2
//! (`optimize_physical_asap`, every materialization assignment) and Stage 3
//! (the reference pricer, `asap_plan_selection::reference_pricer`, under the
//! run's models), and the plan it selects for
//! it is installed as `plan_installable` installs one (`installable_plan_of`,
//! which also compiles it with the executor), so a choice cheaper than the
//! selected plan shows that the search missed a plan the memo stands for.

use std::collections::HashMap;
use std::rc::Rc;

use asap_logical_asap_optimization::{
    build_logical_asap_dags_from_chosen_candidates, ChosenCandidatePerMemoGroup,
};
use asap_physical_asap_optimization::optimize_physical_asap;
use asap_plan_selection::reference_pricer::{select_among_plans, WholePlanSelection};
use asap_plan_selection::{ErrorSelectingPlan, Selection};
use asap_planner::{installable_plan_of, InstallablePlanError, PlanningModels, PlanningRun};
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{NonASAPOp, OperatorNode, QueryRoot};
use asap_types::physical::PhysicalWorkloadPlan;
use asap_types::stages::MemoCandidates;
use asap_types::stages::MemoGroupId;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

/// One choice's result: valid with its cost, or why it is not valid.
#[derive(Debug, Clone, PartialEq)]
pub struct PricedChoice {
    /// Stage 2 implemented it, Stage 3 accepted one of its physical plans,
    /// and the plan Stage 3 selected installs (a standalone scalar query,
    /// which no choice changes, aside).
    pub valid: bool,
    /// Why it is not valid: the Stage 3 rejection cause of its first
    /// physical plan in snake_case (e.g. `accuracy_target_missed`), or
    /// `compose_failed`, `invalid_structure`, `stage2_failed`,
    /// `stage3_failed`, `rejected` (no plan and no rejection listed),
    /// `not_executable` (the executor cannot compile the selected plan's
    /// view refresh or query run), or `not_installable` (installation
    /// refuses the selected plan for another reason, e.g. an unsupported
    /// raw-data window).
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
    /// The baseline of `selection`: its selected plan's cost. Stage 3
    /// reports no calibration factor, so a measured cost table without a
    /// frozen factor is priced at each choice's own fit.
    pub fn of(selection: &Selection) -> Self {
        Self {
            cost_per_second: selection.cost.cost_per_second,
            calibration_factor: None,
        }
    }
}

/// Costs within this fraction of each other are the same sum of charges
/// rounded differently (Stage 3's relative tie tolerance).
const TIE_TOLERANCE: f64 = 1e-9;

/// Each of `choices` of `memo`, composed, implemented and priced. Query `i`
/// is `stage0_roots[i]` with workload entry `entries[i]`; its operator
/// references are replaced by their root groups' composed DAGs.
pub fn price_choices(
    memo: &MemoCandidates,
    choices: &[ChosenCandidatePerMemoGroup],
    stage0_roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    selected: Option<SelectedBaseline>,
) -> Vec<PricedChoice> {
    let mut settings = models.cost_inputs.clone();
    if settings.frozen_calibration_factor.is_none() {
        settings.frozen_calibration_factor = selected.and_then(|s| s.calibration_factor);
    }
    let models = models.with_cost_inputs(&settings);
    let selected_cost = selected.and_then(|s| s.cost_per_second);
    // Which root group each query's operator references name is the same
    // for every choice.
    let root_groups = root_groups_of_queries(memo, stage0_roots);
    choices
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            let roots = build_logical_asap_dags_from_chosen_candidates(memo, choice)
                .map_err(|error| error.to_string())
                .and_then(|composed| query_roots(&composed, stage0_roots, &root_groups));
            let mut priced = price_choice(roots, index, entries, data, models);
            priced.cheaper_than_selected = match (priced.cost_per_second, selected_cost) {
                (Some(cost), Some(selected)) => cost < selected - TIE_TOLERANCE * selected.abs(),
                _ => false,
            };
            priced
        })
        .collect()
}

/// One choice, its query roots composed (or why they do not compose),
/// implemented and priced.
fn price_choice(
    roots: Result<Vec<QueryRoot>, String>,
    index: usize,
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
    let roots = match roots {
        Ok(roots) => roots,
        Err(error) => return invalid("compose_failed", error),
    };
    if let Err(error) = roots.iter().try_for_each(QueryRoot::validate_structure) {
        return invalid("invalid_structure", error.to_string());
    }
    let physical = match physical_plans_of(&roots, index, entries, data, models) {
        Ok(physical) => physical,
        Err(error) => return invalid("stage2_failed", error),
    };
    match reference_select(&physical, entries, data, models) {
        Ok(selection) => {
            match installable_plan_of(&selection.plan, None, &selection.pane_retention_ms, entries)
            {
                // A standalone scalar query, which a runtime cannot answer yet,
                // does not depend on the choice.
                Ok(_) | Err(InstallablePlanError::UnsupportedQueryRoot { .. }) => PricedChoice {
                    valid: true,
                    rejection: None,
                    rejection_reason: None,
                    cost_per_second: selection.costs[&selection.selected].cost_per_second,
                    cheaper_than_selected: false,
                },
                Err(
                    error @ (InstallablePlanError::MaterializedViewNotExecutable { .. }
                    | InstallablePlanError::QueryRunNotExecutable { .. }),
                ) => invalid("not_executable", error.to_string()),
                Err(error) => invalid("not_installable", error.to_string()),
            }
        }
        Err(ErrorSelectingPlan::NoValidCandidate(rejected)) => match rejected.first() {
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

/// Every physical plan Stage 2 offers for choice `index`'s composed `roots`
/// (each materialization assignment), named `E<index+1>-<k>`.
pub fn physical_plans_of(
    roots: &[QueryRoot],
    index: usize,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<Vec<PhysicalWorkloadPlan>, String> {
    let id = format!("E{}", index + 1);
    let mut physical = optimize_physical_asap(
        &id,
        roots,
        entries,
        data,
        models.deployment_capabilities.query_time_results_retained,
    )
    .map_err(|error| error.to_string())?;
    retain_deployable(&mut physical, models);
    if physical.is_empty() {
        return Err("no physical plan".into());
    }
    for (k, plan) in physical.iter_mut().enumerate() {
        plan.id = format!("{id}-{k}");
        plan.from_logical = id.clone();
    }
    Ok(physical)
}

/// Drops the plans of `physical` that Stage 2 would not offer under
/// `models.deployment_capabilities`: without raw data retained, a plan that
/// scans raw data at query time (Stage 2's `PruneReason::RawDataNotRetained`;
/// `optimize_physical_asap` enumerates every assignment and leaves this
/// rule to Stage 2's catalog, and the reference pricer no longer checks it).
pub fn retain_deployable(physical: &mut Vec<PhysicalWorkloadPlan>, models: PlanningModels<'_>) {
    if models.deployment_capabilities.raw_data_retained {
        return;
    }
    physical.retain(|plan| {
        plan.dag.nodes.iter().all(|node| {
            !matches!(
                node.payload,
                PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { .. })
            ) || node.output_state.timing == ExecutionTiming::IngestionTime
        })
    });
}

/// The reference pricer over `physical` under `models`: every plan checked,
/// priced and ranked, every candidate kept.
pub fn reference_select(
    physical: &[PhysicalWorkloadPlan],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<WholePlanSelection, ErrorSelectingPlan> {
    select_among_plans(
        physical,
        entries,
        data,
        &models.accuracy_check_inputs(),
        models.cost_inputs,
    )
}

/// One enumerated memo choice of a reference enumeration: its composed
/// logical roots (or why they do not compose), under id `E<n>`.
#[derive(Debug)]
pub struct EnumeratedChoice {
    pub id: String,
    pub roots: Result<Vec<QueryRoot>, String>,
}

/// `stage_pipeline --exhaustive`: the first `max_choices` choices of the
/// selected round's memo (`run.memo_steps.concrete_states`), each composed,
/// implemented by Stage 2 with every materialization assignment, and all
/// their physical plans priced together by the reference pricer.
#[derive(Debug)]
pub struct ReferenceEnumeration {
    pub choices: Vec<EnumeratedChoice>,
    pub physical: Vec<PhysicalWorkloadPlan>,
    /// Why a composed choice has no physical plan, by choice id.
    pub stage2_failed: Vec<(String, String)>,
    /// Whether the memo has more choices than `max_choices`.
    pub capped: bool,
    pub selection: Result<WholePlanSelection, ErrorSelectingPlan>,
}

/// The reference enumeration of `run` ([`ReferenceEnumeration`]); `None`
/// without a memo.
pub fn reference_enumeration(
    run: &PlanningRun,
    stage0_roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    max_choices: usize,
) -> Option<ReferenceEnumeration> {
    use asap_logical_asap_optimization::enumerate;
    let memo = &run.memo_steps.as_ref()?.concrete_states;
    let enumeration = enumerate(memo, max_choices.saturating_add(1));
    let capped = !enumeration.complete || enumeration.all_chosen_candidates.len() > max_choices;
    let root_groups = root_groups_of_queries(memo, stage0_roots);
    let mut choices = Vec::new();
    let mut physical = Vec::new();
    let mut stage2_failed = Vec::new();
    for (index, choice) in enumeration
        .all_chosen_candidates
        .iter()
        .take(max_choices)
        .enumerate()
    {
        let id = format!("E{}", index + 1);
        let roots = build_logical_asap_dags_from_chosen_candidates(memo, choice)
            .map_err(|error| error.to_string())
            .and_then(|composed| query_roots(&composed, stage0_roots, &root_groups))
            .and_then(|roots| {
                roots
                    .iter()
                    .try_for_each(QueryRoot::validate_structure)
                    .map_err(|error| error.to_string())?;
                Ok(roots)
            });
        if let Ok(roots) = &roots {
            match physical_plans_of(roots, index, entries, data, models) {
                Ok(plans) => physical.extend(plans),
                Err(error) => stage2_failed.push((id.clone(), error)),
            }
        }
        choices.push(EnumeratedChoice { id, roots });
    }
    let selection = reference_select(&physical, entries, data, models);
    Some(ReferenceEnumeration {
        choices,
        physical,
        stage2_failed,
        capped,
        selection,
    })
}

/// For each query, the root group of each of its operator references, in
/// `operator_refs` order: the one group the query's root (in the memo's
/// `query_roots`) reads when its root is one operator, else the group
/// whose node is the reference.
fn root_groups_of_queries(
    memo: &MemoCandidates,
    stage0_roots: &[QueryRoot],
) -> Result<Vec<Vec<MemoGroupId>>, String> {
    stage0_roots
        .iter()
        .enumerate()
        .map(|(query, root)| {
            let groups: Vec<_> = memo
                .query_roots
                .get(query)
                .map(|root| {
                    root.operator_refs()
                        .into_iter()
                        .copied()
                        .collect::<std::collections::BTreeSet<_>>()
                })
                .unwrap_or_default()
                .into_iter()
                .collect();
            match root {
                QueryRoot::Operator(_) if groups.len() == 1 => Ok(groups),
                QueryRoot::Operator(node) => Ok(vec![group_of(memo, &groups, node, query)?]),
                QueryRoot::Scalar(expression) => expression
                    .operator_refs()
                    .into_iter()
                    .map(|node| group_of(memo, &groups, node, query))
                    .collect(),
            }
        })
        .collect()
}

/// The group among `groups` whose node is `node`: the same node, else an
/// equal one (the memo's group nodes are the frontends' nodes with common
/// sub-DAGs shared).
fn group_of(
    memo: &MemoCandidates,
    groups: &[MemoGroupId],
    node: &Rc<OperatorNode>,
    query: usize,
) -> Result<MemoGroupId, String> {
    let node_of = |g: &&MemoGroupId| &memo.groups[g.0 as usize].logical_node;
    groups
        .iter()
        .find(|g| Rc::ptr_eq(node_of(g), node))
        .or_else(|| groups.iter().find(|g| **node_of(g) == **node))
        .copied()
        .ok_or_else(|| format!("query {query}'s root has no root group"))
}

/// Each query's root with every operator reference replaced by the composed
/// DAG of its root group (`root_groups`).
fn query_roots(
    composed: &[(MemoGroupId, Rc<OperatorNode>)],
    stage0_roots: &[QueryRoot],
    root_groups: &Result<Vec<Vec<MemoGroupId>>, String>,
) -> Result<Vec<QueryRoot>, String> {
    let root_groups = root_groups.as_ref().map_err(String::clone)?;
    let composed: HashMap<_, _> = composed.iter().cloned().collect();
    stage0_roots
        .iter()
        .zip(root_groups)
        .map(|(root, groups)| {
            let mut next = groups.iter().map(|group| Rc::clone(&composed[group]));
            Ok(match root {
                QueryRoot::Operator(_) => QueryRoot::Operator(next.next().expect("one group")),
                QueryRoot::Scalar(expression) => QueryRoot::Scalar(
                    expression.map_operator_refs(&mut |_| next.next().expect("a group per ref")),
                ),
            })
        })
        .collect()
}
