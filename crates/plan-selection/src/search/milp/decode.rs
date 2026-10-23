//! Solution decoding (joint-planning-search design, "One solution, one
//! plan"): a MILP solution — the chosen choices `x` (a candidate at its
//! site thresholds `u`, with its private slots' materializations), the
//! groups built once `m`, the scans shared at a horizon — maps to the one
//! physical plan it describes. Incumbent evaluation then runs Stage 3 once
//! on that plan.
//!
//! What a solution decodes to:
//! - each chosen choice's candidate at its site thresholds, its private
//!   builds materialized as its assignment;
//! - a group built once (`m`) or a scan shared at a horizon is one node
//!   for every query; a scan a choice reads apart is that reader's own;
//! - every other identical node is one node (the identical-expression
//!   rule): not built once, Stage 3 costs it as its copies, recomputed per
//!   consumer, so a plan keeping two identical non-scan nodes apart costs
//!   the same or more.
//!
//! Known gaps, where a brute-force plan is not the decoding of any
//! solution (the model still bounds it from below):
//! - a group read apart whose shareable inner nodes are one node built
//!   once (the model's per-choice `shared_nodes` choice);
//! - scan-label widening (every metric's scans widened to one label set,
//!   the identical-expression rule's variant) is no model variable yet:
//!   the model charges a metric's scans as one widened scan may cost, so a
//!   solution also decodes to its widened plan, and incumbent evaluation
//!   runs on both.

use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use asap_logical_asap_optimization::asap_aware_cse::{
    sharing_candidates, with_shared_candidates, with_union_scan_labels,
};
use asap_logical_asap_optimization::per_computation_candidates::{
    recompose_guarantees, PerComputationCandidates,
};
use asap_physical_asap_optimization::{optimize_physical_asap_chosen, PhysicalCandidate};
use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode, QueryRoot};
use asap_types::physical::{Materialization, Medium};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::search::memo::{GroupId, ImplementationCandidate, Memo};
use crate::search::milp::model::Choice;
use crate::PlanningModels;

/// A decoded plan: its logical roots and its one physical candidate.
pub(super) struct Decoded {
    pub roots: Vec<QueryRoot>,
    pub physical: PhysicalCandidate,
}

/// A solution's decoded plans.
pub(super) struct Decoding {
    pub plans: Vec<Decoded>,
    /// Whether every combination of the free nodes' Stage 2 options was
    /// evaluated: otherwise the choice set has plans incumbent evaluation
    /// did not see, and the solution's objective stays a lower bound.
    pub every_combination: bool,
}

/// The plan the solution with `chosen` choices (indices into `choices`,
/// whose candidates at their thresholds are `versions`) and `built_once`
/// groups describes, named `id`, with up to `max_free_combinations`
/// combinations of its free nodes' options; `None` when it cannot be
/// composed.
#[allow(clippy::too_many_arguments)]
pub(super) fn decode<Id>(
    memo: &Memo,
    per_computation: &PerComputationCandidates<Id>,
    choices: &[Choice],
    versions: &[ImplementationCandidate],
    chosen: &[usize],
    built_once: &BTreeSet<GroupId>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    id: &str,
    max_free_combinations: usize,
) -> Option<Decoding> {
    let n = memo.groups.len();
    let mut choice_of: Vec<Option<usize>> = vec![None; n];
    for &c in chosen {
        choice_of[choices[c].group.0 as usize] = Some(c);
    }
    // Groups made one node for every query: built once, or a scan shared
    // at a horizon.
    let mut one_node: Vec<bool> = vec![false; n];
    for g in built_once {
        one_node[g.0 as usize] = true;
    }
    // Scans a choice reads apart: its own copy, not the scan group's node.
    let mut apart_scans: BTreeSet<(usize, usize)> = BTreeSet::new();
    for &c in chosen {
        for (scan, horizon) in &choices[c].scan_children {
            match horizon {
                Some(_) => one_node[scan.0 as usize] = true,
                None => {
                    apart_scans.insert((choices[c].group.0 as usize, scan.0 as usize));
                }
            }
        }
    }
    let plan: Vec<Option<ImplementationCandidate>> = choice_of
        .iter()
        .map(|c| c.map(|c| versions[c].clone()))
        .collect();
    let composed = compose(memo, per_computation, &plan, &one_node, &apart_scans)?;
    // Each node's guarantee composed again over the chosen implementations.
    let recomposed = recompose_guarantees(&composed.roots).ok()?;
    let mut table = composed.table;
    remap(&mut table, &composed.roots, &recomposed);
    // Shared builds resized to one size are one build.
    let group_choice: crate::search::memo::brute_force::GroupChoice = choice_of
        .iter()
        .map(|c| c.map(|c| choices[c].candidate))
        .collect();
    let resized = crate::search::memo::brute_force::resized_builds(memo, &group_choice, &plan);
    let resized_shared =
        crate::search::memo::rules::capability::share_equal_builds(&recomposed, &resized);
    remap(&mut table, &recomposed, &resized_shared);
    // Identical nodes are one node (the identical-expression rule), but a
    // scan read apart stays each reader's: a scan shared at a horizon is
    // already one node. Shared and not built once, a node costs what its
    // copies cost (recomputed per consumer); the model's shared slots and
    // nodes assume one node.
    let (mut plans, mut every_combination) = implemented(
        &resized_shared,
        &table,
        Some(false),
        &choice_of,
        choices,
        entries,
        data,
        models,
        id,
        max_free_combinations,
    );
    // Scan widening is no model variable yet (a decoding gap): the model
    // charges a metric's scans as one widened scan may cost, so the plan
    // with every metric's scans widened to one label set, and identical
    // scans one node, is evaluated too.
    if let Ok(Some(widened)) = with_union_scan_labels(&resized_shared) {
        let mut widened_table = table.clone();
        remap(&mut widened_table, &resized_shared, &widened);
        for share_scans in [true, false] {
            let (widened_plans, every) = implemented(
                &widened,
                &widened_table,
                Some(share_scans),
                &choice_of,
                choices,
                entries,
                data,
                models,
                &format!("{id}w{}", u8::from(share_scans)),
                max_free_combinations,
            );
            plans.extend(widened_plans);
            every_combination &= every;
        }
    }
    Some(Decoding {
        plans,
        every_combination,
    })
}

/// The physical plans of `roots` with identical nodes one node (scans too
/// when `share_scans`), each node materialized as `table` says the
/// solution chose, and whether every combination of the free nodes'
/// options is among them.
#[allow(clippy::too_many_arguments)]
fn implemented(
    roots: &[QueryRoot],
    table: &HashMap<*const OperatorNode, Role>,
    share_scans: Option<bool>,
    choice_of: &[Option<usize>],
    choices: &[Choice],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    id: &str,
    max_free_combinations: usize,
) -> (Vec<Decoded>, bool) {
    let shared = match share_scans {
        Some(share_scans) => {
            let candidates = sharing_candidates(roots);
            let subset: Vec<bool> = candidates
                .iter()
                .map(|c| {
                    share_scans || !matches!(c.nodes[0].non_asap(), Some(NonASAPOp::Scan { .. }))
                })
                .collect();
            with_shared_candidates(roots, &candidates, &subset)
        }
        None => roots.to_vec(),
    };
    let mut table = table.clone();
    remap(&mut table, roots, &shared);
    let wanted_of = |logical: *const OperatorNode| match table.get(&logical) {
        Some(Role::Slot { group, index }) => Some(
            choice_of[*group]
                .and_then(|c| choices[c].assignment.get(*index).copied())
                .unwrap_or(Materialization::NotMaterialized),
        ),
        Some(Role::BuiltOnce) => Some(Materialization::MatAtQuery(Medium::Memory)),
        None => None,
    };
    // The nodes the solution sets no materialization for (identical nodes
    // made one, which the model charges at the cheaper of once and per
    // consumer): Stage 2's options for each, in Stage 2's order.
    let mut free: Vec<Vec<Materialization>> = Vec::new();
    let _ = optimize_physical_asap_chosen(
        id,
        &shared,
        entries,
        data,
        models.deployment_capabilities,
        &mut |logical, _readers, options| match wanted_of(logical) {
            Some(wanted) if options.contains(&wanted) => wanted,
            Some(_) => Materialization::NotMaterialized,
            None => {
                if options.len() > 1 {
                    free.push(options.to_vec());
                }
                options[0]
            }
        },
    );
    // Every combination of the free nodes' options, up to
    // `max_free_combinations`; beyond, each free node recomputed or each
    // built once.
    let count = free
        .iter()
        .try_fold(1usize, |n, options| n.checked_mul(options.len()))
        .filter(|&n| n <= max_free_combinations);
    let every_combination = count.is_some();
    let combinations: Vec<Vec<Materialization>> = match count {
        Some(n) => (0..n)
            .map(|mut k| {
                free.iter()
                    .map(|options| {
                        let m = options[k % options.len()];
                        k /= options.len();
                        m
                    })
                    .collect()
            })
            .collect(),
        None => {
            let pick = |m: Materialization| -> Vec<Materialization> {
                free.iter()
                    .map(|options| if options.contains(&m) { m } else { options[0] })
                    .collect()
            };
            vec![
                pick(Materialization::NotMaterialized),
                pick(Materialization::MatAtQuery(Medium::Memory)),
            ]
        }
    };
    let mut out = Vec::new();
    for (k, combination) in combinations.into_iter().enumerate() {
        let mut next_free = 0usize;
        let physical = optimize_physical_asap_chosen(
            &format!("{id}-{k}"),
            &shared,
            entries,
            data,
            models.deployment_capabilities,
            &mut |logical, _readers, options| match wanted_of(logical) {
                Some(wanted) if options.contains(&wanted) => wanted,
                Some(_) => Materialization::NotMaterialized,
                None if options.len() > 1 => {
                    next_free += 1;
                    combination[next_free - 1]
                }
                None => options[0],
            },
        );
        if let Ok(Some(physical)) = physical {
            out.push(Decoded {
                roots: shared.clone(),
                physical,
            });
        }
    }
    (out, every_combination)
}

/// The most combinations of Stage 2 options of the nodes no solution
/// variable sets that incumbent evaluation runs on.
pub(super) const MAX_FREE_COMBINATIONS: usize = 16;

/// What a node of the decoded plan is to the solution.
#[derive(Debug, Clone, Copy)]
enum Role {
    /// The `index`-th private build of `group`'s chosen choice (in Stage
    /// 2's order: children first), materialized as the choice's
    /// `assignment[index]`.
    Slot { group: usize, index: usize },
    /// A group's own node, built once at query time.
    BuiltOnce,
}

struct Composed {
    roots: Vec<QueryRoot>,
    table: HashMap<*const OperatorNode, Role>,
}

/// The query roots of `plan`, as [`crate::search::memo::brute_force::composed_roots`] composes
/// them before recomposing guarantees, but with each `one_node` group built
/// once for every query, and the roles of the solution's nodes.
fn compose<Id>(
    memo: &Memo,
    per_computation: &PerComputationCandidates<Id>,
    plan: &[Option<ImplementationCandidate>],
    one_node: &[bool],
    apart_scans: &BTreeSet<(usize, usize)>,
) -> Option<Composed> {
    memo.topological_order()?;
    let queries = per_computation.roots.len();
    let mut below: Vec<Vec<bool>> = vec![vec![false; memo.groups.len()]; queries];
    for (g, group) in memo.groups.iter().enumerate() {
        for &q in &group.roots_of {
            crate::search::memo::brute_force::mark_below(memo, g, &mut below[q]);
        }
    }
    let scope_of = |scope: usize, c: usize| {
        if one_node[c] {
            queries
        } else if scope < queries && below[scope][c] {
            scope
        } else {
            (0..queries).find(|&q| below[q][c]).unwrap_or(queries)
        }
    };
    let building: Vec<Option<(usize, &ImplementationCandidate)>> = (0..memo.groups.len())
        .map(|g| {
            plan[g].as_ref().map(|c| (g, c)).or_else(|| {
                plan.iter()
                    .enumerate()
                    .filter_map(|(h, c)| c.as_ref().map(|c| (h, c)))
                    .find(|(_, c)| c.implements.contains(&GroupId(g as u32)))
            })
        })
        .collect();
    let mut state = State {
        built: vec![HashMap::new(); queries + 1],
        table: HashMap::new(),
        apart_scans,
    };
    let roots: Vec<QueryRoot> = per_computation
        .roots
        .iter()
        .enumerate()
        .map(|(q, (_, root))| {
            let group_of = |node: &Rc<OperatorNode>, state: &mut State| {
                let g = memo
                    .groups
                    .iter()
                    .position(|group| group.roots_of.contains(&q) && *group.node == **node)?;
                build(
                    memo,
                    &building,
                    one_node,
                    scope_of(q, g),
                    g,
                    &scope_of,
                    state,
                )
            };
            Some(match root {
                QueryRoot::Operator(node) => QueryRoot::Operator(group_of(node, &mut state)?),
                QueryRoot::Scalar(expr) => {
                    let mut missing = false;
                    let mapped = expr.map_operator_refs(&mut |node| {
                        group_of(node, &mut state).unwrap_or_else(|| {
                            missing = true;
                            Rc::clone(node)
                        })
                    });
                    if missing {
                        return None;
                    }
                    QueryRoot::Scalar(mapped)
                }
            })
        })
        .collect::<Option<_>>()?;
    Some(Composed {
        roots,
        table: state.table,
    })
}

struct State<'a> {
    built: Vec<HashMap<usize, Rc<OperatorNode>>>,
    table: HashMap<*const OperatorNode, Role>,
    /// (reading group, scan group) pairs whose scan is the reader's own copy.
    apart_scans: &'a BTreeSet<(usize, usize)>,
}

/// Group `g` built in `scope`, as [`crate::search::memo::brute_force`]'s `build`, recording
/// its private builds and, built once, its own node.
fn build(
    memo: &Memo,
    building: &[Option<(usize, &ImplementationCandidate)>],
    one_node: &[bool],
    scope: usize,
    g: usize,
    scope_of: &dyn Fn(usize, usize) -> usize,
    state: &mut State,
) -> Option<Rc<OperatorNode>> {
    if let Some(node) = state.built[scope].get(&g) {
        return Some(Rc::clone(node));
    }
    let (owner, candidate) = building[g]?;
    let mut replaced: HashMap<*const OperatorNode, Rc<OperatorNode>> = HashMap::new();
    for child in &candidate.children {
        let c = child.0 as usize;
        if state.apart_scans.contains(&(owner, c)) {
            // A scan read apart: a node of the reader's own.
            let copy = Rc::new(OperatorNode::clone(&memo.groups[c].node));
            replaced.insert(Rc::as_ptr(&memo.groups[c].node), copy);
            continue;
        }
        let node = build(
            memo,
            building,
            one_node,
            scope_of(scope, c),
            c,
            scope_of,
            state,
        )?;
        replaced.insert(Rc::as_ptr(&memo.groups[c].node), node);
    }
    let mut rebuilt = HashMap::new();
    let node = Rc::new(OperatorNode::clone(
        &crate::search::memo::brute_force::substituted(&candidate.node, &replaced, &mut rebuilt),
    ));
    // The candidate's private builds, children first, as Stage 2 orders
    // the choice's own sub-DAG; the root is a fresh node of its own.
    let children: BTreeSet<*const OperatorNode> = candidate
        .children
        .iter()
        .map(|c| Rc::as_ptr(&memo.groups[c.0 as usize].node))
        .collect();
    let mut seen = BTreeSet::new();
    let mut order = Vec::new();
    private_postorder(&candidate.node, &children, &mut seen, &mut order);
    let mut index = 0;
    for original in order {
        if !is_build(&original) {
            continue;
        }
        let new = if Rc::ptr_eq(&original, &candidate.node) {
            Rc::as_ptr(&node)
        } else {
            rebuilt
                .get(&Rc::as_ptr(&original))
                .map_or(Rc::as_ptr(&original), Rc::as_ptr)
        };
        state.table.insert(
            new,
            Role::Slot {
                group: owner,
                index,
            },
        );
        index += 1;
    }
    // Built once (the model's `once[g]`) overrides the slot assignment of
    // a group's own node that is a build. The model builds a group once
    // only through its own choice (`owner == g`).
    if one_node[g] && owner == g {
        state.table.insert(Rc::as_ptr(&node), Role::BuiltOnce);
    }
    for implemented in &candidate.implements {
        state.built[scope].insert(implemented.0 as usize, Rc::clone(&node));
    }
    Some(node)
}

/// `node`'s nodes, children first, without entering `children` (the child
/// groups' nodes).
fn private_postorder(
    node: &Rc<OperatorNode>,
    children: &BTreeSet<*const OperatorNode>,
    seen: &mut BTreeSet<*const OperatorNode>,
    order: &mut Vec<Rc<OperatorNode>>,
) {
    let ptr = Rc::as_ptr(node);
    if children.contains(&ptr) || !seen.insert(ptr) {
        return;
    }
    for child in node.children() {
        private_postorder(child, children, seen, order);
    }
    order.push(Rc::clone(node));
}

fn is_build(node: &OperatorNode) -> bool {
    matches!(
        node.asap(),
        Some(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
    )
}

/// Moves `table`'s entries from `before`'s nodes to the nodes in the same
/// places of `after` (the same roots rebuilt node by node).
fn remap(
    table: &mut HashMap<*const OperatorNode, Role>,
    before: &[QueryRoot],
    after: &[QueryRoot],
) {
    let mut moved: HashMap<*const OperatorNode, Role> = HashMap::new();
    let mut seen: BTreeSet<*const OperatorNode> = BTreeSet::new();
    fn walk(
        a: &Rc<OperatorNode>,
        b: &Rc<OperatorNode>,
        table: &HashMap<*const OperatorNode, Role>,
        moved: &mut HashMap<*const OperatorNode, Role>,
        seen: &mut BTreeSet<*const OperatorNode>,
    ) {
        if !seen.insert(Rc::as_ptr(a)) {
            return;
        }
        // A `Project` scan widening put above a node: the node is below it.
        let is_project = |n: &OperatorNode| matches!(n.non_asap(), Some(NonASAPOp::Project { .. }));
        if is_project(b) && !is_project(a) {
            if let [inner] = b.children().as_slice() {
                seen.remove(&Rc::as_ptr(a));
                return walk(a, inner, table, moved, seen);
            }
        }
        if let Some(role) = table.get(&Rc::as_ptr(a)) {
            moved.entry(Rc::as_ptr(b)).or_insert(*role);
        }
        for (x, y) in a.children().into_iter().zip(b.children()) {
            walk(x, y, table, moved, seen);
        }
    }
    for (a, b) in before.iter().zip(after) {
        let pairs: Vec<(&Rc<OperatorNode>, &Rc<OperatorNode>)> = match (a, b) {
            (QueryRoot::Operator(x), QueryRoot::Operator(y)) => vec![(x, y)],
            (QueryRoot::Scalar(x), QueryRoot::Scalar(y)) => x
                .operator_refs()
                .into_iter()
                .zip(y.operator_refs())
                .collect(),
            _ => Vec::new(),
        };
        for (x, y) in pairs {
            walk(x, y, table, &mut moved, &mut seen);
        }
    }
    *table = moved;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::milp::model::{model_input_of, solve, tests::workload};
    use asap_types::ir::properties::AccuracyTarget;

    const QUERIES: [&str; 2] = ["sum_over_time(m[5m]) * 2", "sum_over_time(m[5m]) / 3"];

    fn targets() -> [AccuracyTarget; 2] {
        [AccuracyTarget::exact(), AccuracyTarget::exact()]
    }

    /// The model's optimum over a shared sum decodes to plans, every
    /// combination of its free nodes' options among them.
    #[test]
    fn a_solution_decodes_to_its_plans() {
        let (per_computation, entries, data) = workload(&QUERIES, &targets(), None);
        let models = PlanningModels::builtin();
        let memo = crate::search::memo::brute_force::memo(&per_computation, &entries);
        let (input, versions) = model_input_of(&memo, &entries, &data, models).unwrap();
        let solution = solve(&input).unwrap().unwrap();
        let decoding = decode(
            &memo,
            &per_computation,
            &input.choices,
            &versions,
            &solution.chosen,
            &solution.built_once,
            &entries,
            &data,
            models,
            "t",
            MAX_FREE_COMBINATIONS,
        )
        .expect("the optimum decodes");
        assert!(!decoding.plans.is_empty());
        assert!(decoding.every_combination);
        for plan in &decoding.plans {
            assert_eq!(plan.physical.roots.len(), QUERIES.len());
        }
    }

    /// A group built once whose own node is a build: its node is built
    /// once (the model's `once`), not materialized as the choice's slot.
    #[test]
    fn a_group_built_once_whose_node_is_a_build_is_built_once() {
        let (per_computation, entries, _) = workload(&QUERIES, &targets(), None);
        let memo = crate::search::memo::brute_force::memo(&per_computation, &entries);
        // The shared sum's group, and a candidate of it cut down to its
        // build: a group whose own node is a build.
        let g = memo
            .groups
            .iter()
            .position(|group| group.parents.len() > 1)
            .expect("a shared group");
        fn build_below(
            node: &Rc<OperatorNode>,
            children: &[*const OperatorNode],
        ) -> Option<Rc<OperatorNode>> {
            if children.contains(&Rc::as_ptr(node)) {
                return None;
            }
            if is_build(node) {
                return Some(Rc::clone(node));
            }
            node.children()
                .into_iter()
                .find_map(|child| build_below(child, children))
        }
        let mut plan: Vec<Option<ImplementationCandidate>> = memo
            .candidates
            .iter()
            .map(|candidates| candidates.first().cloned())
            .collect();
        let (candidate, build) = memo.candidates[g]
            .iter()
            .find_map(|candidate| {
                let children: Vec<*const OperatorNode> = candidate
                    .children
                    .iter()
                    .map(|c| Rc::as_ptr(&memo.groups[c.0 as usize].node))
                    .collect();
                Some((candidate, build_below(&candidate.node, &children)?))
            })
            .expect("a candidate with a build");
        plan[g] = Some(ImplementationCandidate {
            node: build,
            ..candidate.clone()
        });
        let mut one_node = vec![false; memo.groups.len()];
        one_node[g] = true;
        let composed =
            compose(&memo, &per_computation, &plan, &one_node, &BTreeSet::new()).unwrap();
        let roles: Vec<Role> = composed.table.values().copied().collect();
        assert!(
            roles.iter().any(|r| matches!(r, Role::BuiltOnce)),
            "{roles:?}"
        );
        assert!(
            !roles
                .iter()
                .any(|r| matches!(r, Role::Slot { group, .. } if *group == g)),
            "{roles:?}"
        );
    }
}
