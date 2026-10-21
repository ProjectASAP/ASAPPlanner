//! Independent components (joint-planning-search design, method 5): the
//! workload's queries split into groups that read disjoint sources, each
//! searched on its own, and their selections combined.
//!
//! Two queries that read no common source cannot interact under any
//! alternative: every Pass 2 rule (identical expressions, summary
//! capability, rollup, top-k reuse) shares only work over the same input
//! rows, raw-data retention is kept per source, and every node's charge
//! reads only the queries and sources above and below it. Under cost model
//! 1 the workload's price is the sum of its nodes', so the cheapest plan is
//! the union of each component's cheapest. Cost models 2 and 3 price or
//! limit peak cores, a ceiling over the whole workload's work, so their
//! workloads are searched as one.
//!
//! The combined plan is rebuilt as one whole-workload candidate and checked
//! and priced by Stage 3 again. It is the exhaustive search's selection
//! only when no other combination ties with it within Stage 3's tolerance
//! (where the joint enumeration order would decide): each component's
//! runner-up must cost more than its selection by a margin above that
//! tolerance. Otherwise, or when the combined price is not the sum, the
//! workload is searched as one.

use std::collections::HashMap;
use std::rc::Rc;

use asap_logical_asap_optimization::per_computation_candidates::{
    generate_per_computation_candidates, PerComputationCandidates,
};
use asap_logical_asap_optimization::workload_candidates::stage1_candidates_of_choice;
use asap_physical_asap_optimization::optimize_physical_asap_assigned;
use asap_types::ir::flat::flatten;
use asap_types::ir::operator::Source;
use asap_types::ir::{NonASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::physical::Materialization;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::stage1_to_stage3::{
    run_stage1_to_stage3_from_pass1, selected_logical, LogicalCandidate, Stage1ToStage3Run,
    Stage1ToStage3Search,
};
use crate::{
    ComponentReport, CostModel, PlanSelector, PlanningModels, QuerySearchReport, Rejection,
    SearchedAsOne, SelectionError,
};

/// The margin, relative to the workload's price, by which each
/// component's runner-up must cost more than its selection: above Stage
/// 3's tie tolerance (1e-9 of the best price), with room for the rounding
/// of summing the same charges in another grouping.
const UNIQUE_MARGIN: f64 = 4e-9;

/// The workload's queries grouped into components that read disjoint
/// sources, each in workload order, ordered by their first query. A query
/// that reads no source is a component of its own.
pub(crate) fn source_components(roots: &[QueryRoot]) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..roots.len()).collect();
    fn find(parent: &mut [usize], q: usize) -> usize {
        let mut root = q;
        while parent[root] != root {
            root = parent[root];
        }
        let mut q = q;
        while parent[q] != root {
            let next = parent[q];
            parent[q] = root;
            q = next;
        }
        root
    }
    // The first query read each source, by name (data facts are keyed by
    // name, whatever the source's kind).
    let mut first_reader: HashMap<String, usize> = HashMap::new();
    for (query, root) in roots.iter().enumerate() {
        let starts: Vec<&Rc<OperatorNode>> = match root {
            QueryRoot::Operator(node) => vec![node],
            QueryRoot::Scalar(expr) => expr.operator_refs(),
        };
        for node in starts.into_iter().flat_map(|n| OperatorNode::reachable(n)) {
            let Operator::NonASAP(NonASAPOp::Scan { source, .. }) = &node.operator else {
                continue;
            };
            let name = match source {
                Source::TimeSeries { metric } => metric.clone(),
                Source::Table { table_ref } => table_ref.clone(),
            };
            let other = *first_reader.entry(name).or_insert(query);
            let (a, b) = (find(&mut parent, query), find(&mut parent, other));
            parent[a.max(b)] = a.min(b);
        }
    }
    let mut components: Vec<Vec<usize>> = Vec::new();
    let mut index_of: HashMap<usize, usize> = HashMap::new();
    for query in 0..roots.len() {
        let root = find(&mut parent, query);
        let index = *index_of.entry(root).or_insert_with(|| {
            components.push(Vec::new());
            components.len() - 1
        });
        components[index].push(query);
    }
    components
}

/// Why `per_computation`'s workload under `models` and `search` is
/// searched as one, or `None` when its `components` may be searched
/// separately.
pub(crate) fn searched_as_one(
    components: &[Vec<usize>],
    models: &PlanningModels<'_>,
    search: Stage1ToStage3Search,
) -> Option<SearchedAsOne> {
    if search.exhaustive || search.keep_every_candidate {
        return Some(SearchedAsOne::EveryCandidateRequested);
    }
    if !matches!(
        models.plan_selection.cost_model,
        CostModel::UsageOverTime(_)
    ) {
        return Some(SearchedAsOne::CostNotAdditive);
    }
    if !models
        .plan_selection
        .measured_operation_costs
        .operators
        .is_empty()
    {
        return Some(SearchedAsOne::CalibratedOverTheWorkload);
    }
    if components.len() < 2 {
        return Some(SearchedAsOne::OneComponent);
    }
    None
}

/// Search each of `components` of `per_computation`'s workload on its own
/// and combine their selections into one whole-workload candidate, checked
/// and priced by Stage 3. `Err(reason)` when the combination is not proven
/// to be the joint search's selection: the caller searches the workload as
/// one.
pub(crate) fn search_components<Id: Clone + std::fmt::Debug>(
    per_computation: &PerComputationCandidates<Id>,
    components: &[Vec<usize>],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<Stage1ToStage3Run<Id>, SearchedAsOne> {
    let roots: Vec<QueryRoot> = per_computation
        .roots
        .iter()
        .map(|(_, root)| root.clone())
        .collect();
    let mut runs = Vec::with_capacity(components.len());
    for queries in components {
        let component_roots: Vec<(usize, QueryRoot)> =
            queries.iter().map(|&q| (q, roots[q].clone())).collect();
        let component_entries: Vec<QueryWorkloadEntry> =
            queries.iter().map(|&q| entries[q].clone()).collect();
        // Pass 1 succeeded for the whole workload, so for each part of it.
        let Ok(inventory) = generate_per_computation_candidates(component_roots) else {
            return Err(SearchedAsOne::NotAdditive);
        };
        let run = run_stage1_to_stage3_from_pass1(
            inventory,
            &component_entries,
            data,
            models,
            Stage1ToStage3Search::default(),
        );
        runs.push(run);
    }
    // A component without a valid candidate leaves the workload without one.
    if runs.iter().any(|run| run.stage3_plan_selection.is_err()) {
        let mut rejected: Vec<Rejection> = Vec::new();
        for (queries, run) in components.iter().zip(&runs) {
            match &run.stage3_plan_selection {
                Err(SelectionError::NoValidCandidate(found)) => {
                    rejected.extend(found.iter().cloned().map(|r| renumber(r, queries)))
                }
                Err(_) => return Err(SearchedAsOne::NotAdditive),
                Ok(_) => {}
            }
        }
        return Ok(Stage1ToStage3Run {
            stage1_pass1_per_computation: per_computation
                .clone()
                .without_unsupported_summaries(models.deployment_capabilities),
            combinations: per_computation
                .clone()
                .without_unsupported_summaries(models.deployment_capabilities)
                .combinations(),
            stage1_logical_asap_optimization: Vec::new(),
            stage2_physical_asap_optimization: Vec::new(),
            stage2_pruned: Vec::new(),
            stage3_plan_selection: Err(SelectionError::NoValidCandidate(rejected)),
            stage3_selected: None,
            stage3_selected_logical: None,
            stage1_selected: None,
        });
    }
    let selections: Vec<_> = runs
        .iter()
        .map(|run| run.stage3_plan_selection.as_ref().expect("checked above"))
        .collect();
    let prices: Vec<f64> = selections
        .iter()
        .map(|s| s.costs[&s.selected].cost_per_second.unwrap_or(f64::NAN))
        .collect();
    let total: f64 = prices.iter().sum();
    let unique = selections.iter().zip(&prices).all(|(s, &price)| {
        s.search
            .runner_up_cost_per_second
            .is_none_or(|runner_up| runner_up - price > UNIQUE_MARGIN * total.abs())
    });
    if !total.is_finite() || !unique {
        return Err(SearchedAsOne::NearTie);
    }
    // The whole-workload candidate the combination is: the joint search's
    // Pass 1 choice of each component's selected choice, and among its Stage
    // 1 candidates the one with each query's roots from its component.
    let joint_inventory = per_computation
        .clone()
        .without_unsupported_summaries(models.deployment_capabilities);
    let mut component_of = vec![0; roots.len()];
    let mut combined: Vec<Option<QueryRoot>> = vec![None; roots.len()];
    for (c, (queries, run)) in components.iter().zip(&runs).enumerate() {
        let selected = run
            .stage1_selected
            .as_ref()
            .ok_or(SearchedAsOne::NotAdditive)?;
        let selected_roots = selected
            .roots
            .as_ref()
            .map_err(|_| SearchedAsOne::NotAdditive)?;
        for (&q, root) in queries.iter().zip(selected_roots) {
            combined[q] = Some(root.clone());
            component_of[q] = c;
        }
    }
    let combined: Vec<QueryRoot> = combined
        .into_iter()
        .map(|r| r.expect("every query"))
        .collect();
    let joint_choice: Option<Vec<usize>> = joint_inventory
        .targets
        .iter()
        .map(|target| {
            runs.iter().find_map(|run| {
                let inventory = &run.stage1_pass1_per_computation;
                let u = inventory.targets.iter().position(|t| {
                    Rc::ptr_eq(&t.aggregate, &target.aggregate) && t.measure == target.measure
                })?;
                (inventory.targets[u].choice_count() == target.choice_count())
                    .then(|| run.stage1_selected.as_ref().map(|l| l.choice[u]))
                    .flatten()
            })
        })
        .collect();
    let joint_choice = joint_choice.ok_or(SearchedAsOne::NotInJointSpace)?;
    let flat = flatten(&combined).0;
    let skip_raw = !models.deployment_capabilities.raw_data_retained;
    let viable = |roots: &[QueryRoot]| {
        super::stage1_to_stage3::viable(roots, entries, data, models, skip_raw)
    };
    let (joint, pass2) =
        stage1_candidates_of_choice(&joint_inventory, entries, &joint_choice, &viable)
            .into_iter()
            .filter_map(|candidate| Some((candidate.roots.ok()?, candidate.pass2)))
            .find(|(roots, _)| flatten(roots).0 == flat)
            .ok_or(SearchedAsOne::NotInJointSpace)?;
    let mut assignments: Vec<std::vec::IntoIter<Materialization>> = runs
        .iter()
        .map(|run| {
            run.stage3_selected
                .as_ref()
                .map(|p| p.assignment.clone())
                .unwrap_or_default()
                .into_iter()
        })
        .collect();
    let id = selections
        .iter()
        .map(|s| s.selected.as_str())
        .collect::<Vec<_>>()
        .join("+");
    let mut choose = |readers: &[usize]| assignments[component_of[*readers.first()?]].next();
    let built = optimize_physical_asap_assigned(
        "L1",
        &joint,
        entries,
        data,
        models.deployment_capabilities,
        &mut choose,
    );
    let Ok(Some(mut candidate)) = built else {
        return Err(SearchedAsOne::NotAdditive);
    };
    if assignments.iter_mut().any(|rest| rest.next().is_some()) {
        return Err(SearchedAsOne::NotAdditive);
    }
    candidate.id = id;
    let mut selector =
        PlanSelector::new(entries, data, models, false).map_err(|_| SearchedAsOne::NotAdditive)?;
    selector
        .offer(&candidate)
        .map_err(|_| SearchedAsOne::NotAdditive)?;
    let (mut selection, selected) = selector.finish().map_err(|_| SearchedAsOne::NotAdditive)?;
    let price = selection.costs[&selection.selected]
        .cost_per_second
        .unwrap_or(f64::NAN);
    if !((price - total).abs() <= 1e-9 * total.abs()) {
        return Err(SearchedAsOne::NotAdditive);
    }
    // The report: each component's search, and each query's own-candidate
    // checks in workload order.
    let mut queries = vec![QuerySearchReport::default(); roots.len()];
    for (component, selection) in components.iter().zip(&selections) {
        for (&q, report) in component.iter().zip(&selection.search.queries) {
            queries[q] = report.clone();
        }
    }
    let mut rejected = Vec::new();
    for (component, s) in components.iter().zip(&selections) {
        rejected.extend(s.rejected.iter().cloned().map(|r| renumber(r, component)));
    }
    selection.rejected = rejected;
    selection.search.queries = queries;
    selection.search.candidates_checked += selections
        .iter()
        .map(|s| s.search.candidates_checked)
        .sum::<u64>();
    selection.search.components = components
        .iter()
        .zip(&selections)
        .map(|(queries, s)| ComponentReport {
            queries: queries.clone(),
            candidates_checked: s.search.candidates_checked,
            selected: s.selected.clone(),
        })
        .collect();
    selection.search.runner_up_cost_per_second = None;
    selection.search.raw_scan_assignments_skipped = selections
        .iter()
        .map(|s| s.search.raw_scan_assignments_skipped)
        .sum();
    let stage3_selected_logical = selected_logical(&joint, entries, data);
    Ok(Stage1ToStage3Run {
        stage1_pass1_per_computation: per_computation
            .clone()
            .without_unsupported_summaries(models.deployment_capabilities),
        combinations: per_computation
            .clone()
            .without_unsupported_summaries(models.deployment_capabilities)
            .combinations(),
        stage1_logical_asap_optimization: Vec::new(),
        stage2_physical_asap_optimization: Vec::new(),
        stage2_pruned: Vec::new(),
        stage3_plan_selection: Ok(selection),
        stage3_selected: Some(selected),
        stage3_selected_logical,
        stage1_selected: Some(LogicalCandidate {
            id: "L1".to_string(),
            choice: joint_choice,
            roots: Ok(joint),
            pass2,
        }),
    })
}

/// `rejection` of a component's search with its query numbers (`q<n>` at
/// the start of its id or reason, counted within the component) renamed to
/// the workload's.
fn renumber(rejection: Rejection, component: &[usize]) -> Rejection {
    let rename = |text: &str| -> String {
        let Some(rest) = text.strip_prefix('q') else {
            return text.to_string();
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        match digits.parse::<usize>() {
            Ok(n) if n >= 1 && n <= component.len() => {
                format!("q{}{}", component[n - 1] + 1, &rest[digits.len()..])
            }
            _ => text.to_string(),
        }
    };
    Rejection {
        id: rename(&rejection.id),
        cause: rejection.cause,
        reason: rename(&rejection.reason),
    }
}
