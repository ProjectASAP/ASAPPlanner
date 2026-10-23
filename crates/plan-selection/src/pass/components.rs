//! Independent components (joint-planning-search design, method 5): the
//! workload's queries split into groups that no candidate can link, each
//! searched on its own, and their selections combined.
//!
//! Two queries that read no common source cannot interact under any
//! alternative: every Pass 2 rule (identical expressions, summary
//! capability, rollup, top-k reuse) shares only work over the same input
//! rows, raw-data retention is kept per source, and every node's charge
//! reads only the queries and sources above and below it. Two queries over
//! one source interact only through a node both can read
//! ([`sharing_components`]). Under cost model 1 the workload's price is the sum
//! of its nodes', so the cheapest plan is the union of each component's
//! cheapest. Cost models 2 and 3 price or limit peak cores, a ceiling over
//! the whole workload's work, so their workloads are searched as one.
//!
//! The combined plan is rebuilt as one whole-workload candidate and checked
//! and priced by Stage 3 again. It is the exhaustive search's selection
//! only when no other combination ties with it within Stage 3's tolerance
//! (where the joint enumeration order would decide): each component's
//! runner-up must cost more than its selection by a margin above that
//! tolerance. Otherwise, or when the combined price is not the sum, the
//! workload is searched as one. The combination is not looked up among the
//! joint search's candidates: each component's Pass 2 rewrites touch only
//! its own nodes, so together they are a joint candidate whenever the joint
//! search combines those rules ([`combined_pass2`]), whatever the rules'
//! enumeration caps over the whole workload.

use std::collections::HashMap;
use std::rc::Rc;

use asap_logical_asap_optimization::asap_aware_cse::{
    capability_sharing_candidates, pane_candidates, rollup_candidates, sharing_candidates,
    topk_reuse_candidates, window_composition_pass2_candidates, with_union_scan_labels,
    SharedPaneChain,
};
use asap_logical_asap_optimization::per_computation_candidates::{
    compose_logical_candidate, generate_per_computation_candidates, PerComputationCandidates,
};
use asap_logical_asap_optimization::workload_candidates::Pass2Candidate;
use asap_physical_asap_optimization::optimize_physical_asap_assigned;
use asap_types::ir::operator::Source;
use asap_types::ir::{NonASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::physical::Materialization;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::query_candidates::{next_digits, query_digits};
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
        for node in starts.into_iter().flat_map(OperatorNode::reachable) {
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

/// The workload's queries grouped into components that no candidate can
/// link: [`source_components`], each split further into the queries that
/// can read a common node, in workload order, ordered by their first query.
/// `per_computation` is the Pass 1 inventory of the workload's roots;
/// `entries[q]` is query `q`'s workload entry.
///
/// Two queries over one source are linked when they read a common Pass 1
/// target, or when, for some choice of their own candidates, a Pass 2 rule
/// can make them read one node: the identical-expression rule (any
/// identical node, whole or in panes, with each metric's scans widened to
/// one label set), the summary-capability rule, the window-composition
/// rule's shared chains, or a reuse rule (rollup or exact top-k reuse).
/// Stage 2 can build any node two queries read once per evaluation, so
/// every such node is a link. Queries whose candidates have no node in
/// common under any rule share no charge: e.g. SQL queries over one table,
/// whose table scans have no unique key and are never made one node, with
/// different groupings.
///
/// Raw data kept for query-time scans is charged per source, for the
/// longest window any query scans: with a disk price, that charge links
/// every query over one source, and the components stay
/// [`source_components`].
pub(crate) fn sharing_components<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    models: &PlanningModels<'_>,
) -> Vec<Vec<usize>> {
    let roots: Vec<QueryRoot> = per_computation
        .roots
        .iter()
        .map(|(_, root)| root.clone())
        .collect();
    let by_source = source_components(&roots);
    let disk_priced = match &models.plan_selection.cost_model {
        CostModel::UsageOverTime(prices) => prices.cost_per_disk_byte_second != 0.0,
        _ => true,
    };
    if disk_priced {
        return by_source;
    }
    let grids: Vec<Option<u64>> = entries
        .iter()
        .map(QueryWorkloadEntry::evaluation_grid_ms)
        .collect();
    let digits: Vec<Vec<usize>> = roots
        .iter()
        .map(|root| query_digits(per_computation, root))
        .collect();
    let mut components = Vec::new();
    for queries in by_source {
        // Union-find over the component's positions.
        let mut parent: Vec<usize> = (0..queries.len()).collect();
        fn find(parent: &mut [usize], i: usize) -> usize {
            let mut root = i;
            while parent[root] != root {
                root = parent[root];
            }
            parent[i] = root;
            root
        }
        for i in 0..queries.len() {
            for j in i + 1..queries.len() {
                if find(&mut parent, i) == find(&mut parent, j) {
                    continue;
                }
                let (a, b) = (queries[i], queries[j]);
                if can_share_a_node(per_computation, &digits, &grids, a, b) {
                    let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                    parent[ri.max(rj)] = ri.min(rj);
                }
            }
        }
        let mut split: Vec<(usize, Vec<usize>)> = Vec::new();
        for (i, &query) in queries.iter().enumerate() {
            let root = find(&mut parent, i);
            match split.iter_mut().find(|(r, _)| *r == root) {
                Some((_, members)) => members.push(query),
                None => split.push((root, vec![query])),
            }
        }
        components.extend(split.into_iter().map(|(_, members)| members));
    }
    components.sort_by_key(|members| members[0]);
    components
}

/// Whether queries `a` and `b` of `per_computation` can read one node
/// ([`sharing_components`]): they read a common Pass 1 target, or for some
/// choice of their own candidates, composed together, a Pass 2 rule links
/// a node of each. `digits[q]` are query `q`'s digits of a whole-workload choice
/// ([`query_digits`]); `grids[q]` its evaluation grid.
fn can_share_a_node<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    digits: &[Vec<usize>],
    grids: &[Option<u64>],
    a: usize,
    b: usize,
) -> bool {
    if digits[a].iter().any(|d| digits[b].contains(d)) {
        return true;
    }
    let pair = PerComputationCandidates {
        roots: vec![
            per_computation.roots[a].clone(),
            per_computation.roots[b].clone(),
        ],
        targets: per_computation.targets.clone(),
    };
    let pair_digits: Vec<usize> = digits[a].iter().chain(&digits[b]).copied().collect();
    let pair_grids = [grids[a], grids[b]];
    let mut choice = vec![0; per_computation.targets.len()];
    loop {
        if !per_computation.absorbed_choice_varies(&choice) {
            if let Ok(composed) = compose_logical_candidate(&pair, &choice) {
                let roots: Vec<QueryRoot> = composed.into_iter().map(|(_, root)| root).collect();
                if rules_link_nodes(&roots, &pair_grids) {
                    return true;
                }
            }
        }
        if !next_digits(per_computation, &pair_digits, &mut choice) {
            return false;
        }
    }
}

/// Whether a Pass 2 rule, on the two-query candidate `roots`, makes both
/// queries read one node: whole, with the metric's scans widened to one
/// label set, or in any window-composition candidate (identical pane
/// chains, or one chain serving nested windows or several intervals).
fn rules_link_nodes(roots: &[QueryRoot], grids: &[Option<u64>]) -> bool {
    let reach = |root: &QueryRoot| -> std::collections::HashSet<*const OperatorNode> {
        root_nodes(root).iter().map(Rc::as_ptr).collect()
    };
    let links = |roots: &[QueryRoot]| {
        let (first, second) = (reach(&roots[0]), reach(&roots[1]));
        let spans = |nodes: &mut dyn Iterator<Item = &Rc<OperatorNode>>| {
            let (mut in_first, mut in_second) = (false, false);
            for node in nodes {
                in_first |= first.contains(&Rc::as_ptr(node));
                in_second |= second.contains(&Rc::as_ptr(node));
            }
            in_first && in_second
        };
        // A rule already made both read one node (a shared pane chain).
        first.iter().any(|node| second.contains(node))
            || sharing_candidates(roots)
                .iter()
                .any(|c| spans(&mut c.nodes.iter()))
            || capability_sharing_candidates(roots)
                .iter()
                .any(|c| spans(&mut c.members.iter()))
            || rollup_candidates(roots)
                .iter()
                .any(|c| spans(&mut [&c.coarse, &c.finer].into_iter()))
            || topk_reuse_candidates(roots)
                .iter()
                .any(|c| spans(&mut c.members.iter()))
    };
    if links(roots) {
        return true;
    }
    // Scans of one metric that project different labels, widened to one.
    if let Ok(Some(widened)) = with_union_scan_labels(roots) {
        if links(&widened) {
            return true;
        }
    }
    let panes = pane_candidates(roots, grids);
    if panes.is_empty() {
        return false;
    }
    window_composition_pass2_candidates(roots, &panes)
        .unwrap_or_default()
        .iter()
        .any(|in_panes| matches!(&in_panes.roots, Ok(in_panes) if links(in_panes)))
}

/// Every node `root` reaches.
fn root_nodes(root: &QueryRoot) -> Vec<Rc<OperatorNode>> {
    let starts: Vec<&Rc<OperatorNode>> = match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    };
    starts
        .into_iter()
        .flat_map(OperatorNode::reachable)
        .collect()
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
    let pass2 = combined_pass2(&runs).ok_or(SearchedAsOne::NotInJointSpace)?;
    // Stage 1 widens every metric's scans to one label set at once, so a
    // selection with widened scans is a joint candidate only alone.
    if components.len() > 1 {
        let composed: Vec<QueryRoot> = compose_logical_candidate(&joint_inventory, &joint_choice)
            .map_err(|_| SearchedAsOne::NotInJointSpace)?
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        if widens_a_scan(&combined, &composed) {
            return Err(SearchedAsOne::NotInJointSpace);
        }
    }
    let joint = combined;
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
    // A NaN price (no cost) does not add up either.
    let adds_up = (price - total).abs() <= 1e-9 * total.abs();
    if !adds_up {
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
    selection.search.interchangeable_queries = components
        .iter()
        .zip(&selections)
        .flat_map(|(component, s)| {
            s.search
                .interchangeable_queries
                .iter()
                .map(|class| class.iter().map(|&q| component[q]).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        })
        .collect();
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

/// Whether some query of `selected` reads a scan its root in `composed`
/// (its Pass 1 candidate's roots) does not read: one Stage 1 widened to
/// more labels ([`with_union_scan_labels`]).
fn widens_a_scan(selected: &[QueryRoot], composed: &[QueryRoot]) -> bool {
    let scans = |root: &QueryRoot| -> Vec<OperatorNode> {
        root_nodes(root)
            .into_iter()
            .filter(|node| matches!(node.non_asap(), Some(NonASAPOp::Scan { .. })))
            .map(|node| (*node).clone())
            .collect()
    };
    selected.iter().zip(composed).any(|(selected, composed)| {
        let original = scans(composed);
        scans(selected).iter().any(|scan| !original.contains(scan))
    })
}

/// What the Pass 2 rules changed in the components' selections, together:
/// `Some(None)` when no rule changed anything. `None` when the joint
/// search never combines those rewrites: it builds exact top-k reuse and
/// tumbling panes into no common candidate (Stage 1's reuse rules follow
/// only a rollup with panes). Each component's rewrites touch only its own
/// nodes, so the rest is a candidate of the joint search, up to its
/// enumeration caps, which bound each rule's list for one workload and are
/// not a property of the plan.
fn combined_pass2<Id>(runs: &[Stage1ToStage3Run<Id>]) -> Option<Option<Pass2Candidate>> {
    let mut combined: Option<Pass2Candidate> = None;
    for pass2 in runs
        .iter()
        .filter_map(|run| run.stage1_selected.as_ref()?.pass2.as_ref())
    {
        let into = combined.get_or_insert_with(|| Pass2Candidate {
            from_pass1_position: 0,
            shared_builds: Vec::new(),
            pane_candidates: Vec::new(),
            shared_pane_chains: Vec::new(),
            rollup_candidates: Vec::new(),
            topk_reuse_candidates: Vec::new(),
            sharing_candidates: Vec::new(),
        });
        into.shared_builds
            .extend(pass2.shared_builds.iter().cloned());
        // Each component's chain groups are positions in its own panes.
        let offset = into.pane_candidates.len();
        into.shared_pane_chains
            .extend(
                pass2
                    .shared_pane_chains
                    .iter()
                    .map(|chain| SharedPaneChain {
                        members: chain.members.iter().map(|&p| p + offset).collect(),
                        ..chain.clone()
                    }),
            );
        into.pane_candidates
            .extend(pass2.pane_candidates.iter().cloned());
        into.rollup_candidates
            .extend(pass2.rollup_candidates.iter().cloned());
        into.topk_reuse_candidates
            .extend(pass2.topk_reuse_candidates.iter().cloned());
        into.sharing_candidates
            .extend(pass2.sharing_candidates.iter().cloned());
    }
    match &combined {
        Some(pass2)
            if !pass2.topk_reuse_candidates.is_empty() && !pass2.pane_candidates.is_empty() =>
        {
            None
        }
        _ => Some(combined),
    }
}

/// `rejection` of a component's search with its query numbers (`q<n>` at
/// the start of its id or reason, counted within the component) renamed to
/// the workload's.
pub(crate) fn renumber(rejection: Rejection, component: &[usize]) -> Rejection {
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
