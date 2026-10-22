//! Clusters, coupling thresholds with branch and bound, the dynamic
//! program, build-late (design "Memo-group search", steps 3–7).
//!
//! [`brute_force`] is the reference the decomposed search is checked
//! against: every consistent choice of one candidate per needed group and
//! one threshold per coupling site, composed into a plan and priced by
//! Stage 2 and Stage 3 in every materialization.

use std::collections::HashMap;
use std::rc::Rc;

use asap_logical_asap_optimization::asap_aware_cse::{
    share_identical_expressions, sharing_candidates,
};
use asap_logical_asap_optimization::per_computation_candidates::{
    recompose_guarantees, PerComputationCandidates,
};
use asap_physical_asap_optimization::{optimize_physical_asap, PhysicalCandidate};
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::{groups, CouplingKind, GroupId, ImplementationCandidate, Memo};
use crate::{PlanSelector, PlanningModels, Selection, SelectionError};

/// The workload's memo: groups, Pass 1 candidates and the Pass 2
/// transformations' candidates.
pub(crate) fn memo<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
) -> Memo {
    let mut memo = groups::memo_of(per_computation);
    super::capability::add_shared_builds(&mut memo);
    super::windows::add_pass2_candidates(&mut memo, entries);
    super::scans::add_scan_widening(&mut memo);
    super::scans::add_raw_horizons(&mut memo);
    super::scans::scan_materializations(&mut memo);
    memo
}

/// A one-line summary of `memo` for logs: groups, shared groups (two or
/// more parents or queries), candidates, distinct coupling sites.
pub(crate) fn stats(memo: &Memo) -> String {
    let shared = memo
        .groups
        .iter()
        .filter(|g| g.parents.len() > 1 || (g.parents.is_empty() && g.roots_of.len() > 1))
        .count();
    let candidates: usize = memo.candidates.iter().map(Vec::len).sum();
    let mut sites: Vec<u64> = memo
        .candidates
        .iter()
        .flatten()
        .flat_map(|c| c.coupling_sites.iter().map(|s| s.key))
        .collect();
    sites.sort_unstable();
    sites.dedup();
    let most = memo.candidates.iter().map(Vec::len).max().unwrap_or(0);
    format!(
        "groups {} shared {} candidates {} (most in one group {}) coupling sites {}",
        memo.groups.len(),
        shared,
        candidates,
        most,
        sites.len()
    )
}

/// One plan of the memo: a candidate per needed group (`None` for a group
/// no chosen candidate needs, or one a fused candidate implements).
type GroupChoice = Vec<Option<usize>>;

/// Every consistent choice of one candidate per needed group, parents
/// before children: a group is needed when it is a query's root or a child
/// of a chosen candidate, and not implemented by a chosen fused candidate.
fn group_choices(memo: &Memo) -> Option<Vec<GroupChoice>> {
    let order = memo.topological_order()?;
    let parents_first: Vec<GroupId> = order.into_iter().rev().collect();
    let n = memo.groups.len();
    let mut needed = vec![0u32; n];
    let mut covered = vec![0u32; n];
    for (g, group) in memo.groups.iter().enumerate() {
        if !group.roots_of.is_empty() {
            needed[g] += 1;
        }
    }
    let mut choice: GroupChoice = vec![None; n];
    let mut out = Vec::new();
    fn walk(
        memo: &Memo,
        order: &[GroupId],
        at: usize,
        needed: &mut [u32],
        covered: &mut [u32],
        choice: &mut GroupChoice,
        out: &mut Vec<GroupChoice>,
    ) {
        let Some(&GroupId(g)) = order.get(at) else {
            out.push(choice.clone());
            return;
        };
        let g = g as usize;
        if needed[g] == 0 || covered[g] > 0 {
            walk(memo, order, at + 1, needed, covered, choice, out);
            return;
        }
        for (index, candidate) in memo.candidates[g].iter().enumerate() {
            for absorbed in &candidate.implements[1..] {
                covered[absorbed.0 as usize] += 1;
            }
            for child in &candidate.children {
                needed[child.0 as usize] += 1;
            }
            choice[g] = Some(index);
            walk(memo, order, at + 1, needed, covered, choice, out);
            choice[g] = None;
            for child in &candidate.children {
                needed[child.0 as usize] -= 1;
            }
            for absorbed in &candidate.implements[1..] {
                covered[absorbed.0 as usize] -= 1;
            }
        }
    }
    walk(
        memo,
        &parents_first,
        0,
        &mut needed,
        &mut covered,
        &mut choice,
        &mut out,
    );
    Some(out)
}

/// The chosen candidates of `choice`, each rewritten at a threshold of each
/// of its coupling sites: every combination of thresholds every reader may
/// read. A site without a rewrite (raw horizon, a shared build until it can
/// be resized) leaves its readers as they are; Stage 3 prices it.
fn at_thresholds(memo: &Memo, choice: &GroupChoice) -> Vec<Vec<Option<ImplementationCandidate>>> {
    let chosen: Vec<Option<ImplementationCandidate>> = choice
        .iter()
        .enumerate()
        .map(|(g, c)| c.map(|c| memo.candidates[g][c].clone()))
        .collect();
    // The sites the chosen candidates read, each once.
    let mut sites = Vec::new();
    for candidate in chosen.iter().flatten() {
        for site in &candidate.coupling_sites {
            if !sites.contains(site) {
                sites.push(*site);
            }
        }
    }
    let mut plans = vec![chosen];
    for site in sites {
        let mut next = Vec::new();
        for plan in plans {
            match site.kind {
                CouplingKind::PaneChain => {
                    for threshold in super::windows::chain_thresholds(memo, &site) {
                        let mut rewritten = plan.clone();
                        let mut valid = true;
                        for candidate in rewritten.iter_mut().flatten() {
                            if !candidate.coupling_sites.contains(&site) {
                                continue;
                            }
                            let readers: Vec<_> = super::windows::chain_readers(&candidate.node)
                                .into_iter()
                                .filter(|r| r.key == site.key)
                                .collect();
                            if !readers
                                .iter()
                                .all(|r| super::windows::may_read(&threshold, r))
                            {
                                valid = false;
                                break;
                            }
                            match super::windows::read_at_threshold(candidate, &site, &threshold) {
                                Ok(node) => candidate.node = node,
                                Err(_) => {
                                    valid = false;
                                    break;
                                }
                            }
                        }
                        if valid {
                            next.push(rewritten);
                        }
                    }
                }
                CouplingKind::ScanLabels => {
                    for threshold in super::scans::label_thresholds(memo, &site) {
                        let mut rewritten = plan.clone();
                        let mut valid = true;
                        for candidate in rewritten.iter_mut().flatten() {
                            if !candidate.coupling_sites.contains(&site) {
                                continue;
                            }
                            match super::scans::widened_at_threshold(
                                memo, candidate, &site, &threshold,
                            ) {
                                Ok(Some(widened)) => *candidate = widened,
                                _ => {
                                    valid = false;
                                    break;
                                }
                            }
                        }
                        if valid {
                            next.push(rewritten);
                        }
                    }
                }
                CouplingKind::SharedBuildSize | CouplingKind::RawHorizon => next.push(plan),
            }
        }
        plans = next;
    }
    plans
}

/// The query roots of a plan: each chosen candidate's node with every
/// child group's node replaced by that group's built node, children first,
/// so a group read by several parents is one node.
fn composed_roots<Id>(
    memo: &Memo,
    per_computation: &PerComputationCandidates<Id>,
    chosen: &[Option<ImplementationCandidate>],
) -> Option<Vec<QueryRoot>> {
    let order = memo.topological_order()?;
    let mut built: HashMap<*const OperatorNode, Rc<OperatorNode>> = HashMap::new();
    let mut built_of: Vec<Option<Rc<OperatorNode>>> = vec![None; memo.groups.len()];
    for GroupId(g) in order {
        let g = g as usize;
        let Some(candidate) = &chosen[g] else {
            continue;
        };
        let mut rebuilt = HashMap::new();
        let node = substituted(&candidate.node, &built, &mut rebuilt);
        for implemented in &candidate.implements {
            built.insert(
                Rc::as_ptr(&memo.groups[implemented.0 as usize].node),
                Rc::clone(&node),
            );
        }
        built_of[g] = Some(node);
    }
    per_computation
        .roots
        .iter()
        .enumerate()
        .map(|(q, (_, root))| {
            let group_of = |node: &Rc<OperatorNode>| -> Option<Rc<OperatorNode>> {
                memo.groups
                    .iter()
                    .enumerate()
                    .find(|(_, group)| group.roots_of.contains(&q) && *group.node == **node)
                    .and_then(|(g, _)| built_of[g].clone())
            };
            Some(match root {
                QueryRoot::Operator(node) => QueryRoot::Operator(group_of(node)?),
                QueryRoot::Scalar(expr) => {
                    let mut missing = false;
                    let mapped = expr.map_operator_refs(&mut |node| {
                        group_of(node).unwrap_or_else(|| {
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
        .collect()
}

/// `node` with every node `built` maps (by address) replaced by its built
/// node, rebuilt above them; `rebuilt` memoizes by address.
fn substituted(
    node: &Rc<OperatorNode>,
    built: &HashMap<*const OperatorNode, Rc<OperatorNode>>,
    rebuilt: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Rc<OperatorNode> {
    if let Some(replacement) = built.get(&Rc::as_ptr(node)) {
        return Rc::clone(replacement);
    }
    if let Some(done) = rebuilt.get(&Rc::as_ptr(node)) {
        return Rc::clone(done);
    }
    let mut changed = false;
    let operator = node.operator.map_children(|child| {
        let new = substituted(child, built, rebuilt);
        changed |= !Rc::ptr_eq(&new, child);
        new
    });
    let new = match changed {
        false => Rc::clone(node),
        true => Rc::new(
            OperatorNode::with_schema(operator, node.schema.clone())
                .with_guarantee(node.guarantee.clone())
                .with_timing(node.timing)
                .with_materialization(node.materialization),
        ),
    };
    rebuilt.insert(Rc::as_ptr(node), Rc::clone(&new));
    new
}

/// The reference search: every plan of the memo ([`group_choices`] ×
/// [`at_thresholds`]), its identical nodes made one node, implemented in
/// every Stage 2 materialization and priced by Stage 3; the cheapest
/// valid one, with its logical roots and the number of physical candidates
/// checked.
pub(crate) fn brute_force<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<(Selection, PhysicalCandidate, Vec<QueryRoot>), SelectionError> {
    let memo = memo(per_computation, entries);
    let choices = group_choices(&memo).unwrap_or_default();
    let mut selector = PlanSelector::new(entries, data, models, false)?;
    let mut logical: HashMap<String, Vec<QueryRoot>> = HashMap::new();
    let mut number = 0usize;
    for choice in &choices {
        let chosen: Vec<(GroupId, &ImplementationCandidate)> = choice
            .iter()
            .enumerate()
            .filter_map(|(g, c)| Some((GroupId(g as u32), &memo.candidates[g][(*c)?])))
            .collect();
        if !super::windows::requirements_met(&chosen) {
            continue;
        }
        for plan in at_thresholds(&memo, choice) {
            let Some(roots) = composed_roots(&memo, per_computation, &plan) else {
                continue;
            };
            // Each candidate's guarantees were composed over its own
            // children's Pass 1 choice: compose them again over the chosen
            // implementations.
            let Ok(roots) = recompose_guarantees(&roots) else {
                continue;
            };
            let shareable = sharing_candidates(&roots);
            let roots = match shareable.is_empty() {
                true => roots,
                false => share_identical_expressions(&roots, &shareable),
            };
            number += 1;
            let id = format!("L{number}");
            let Ok(implemented) =
                optimize_physical_asap(&id, &roots, entries, data, models.deployment_capabilities)
            else {
                continue;
            };
            for (k, mut physical) in implemented.into_iter().enumerate() {
                physical.id = match k {
                    0 => format!("P{number}"),
                    k => format!("P{number}-m{k}"),
                };
                selector.offer(&physical)?;
            }
            if selector.keeps_from_logical(&id) {
                logical.insert(id, roots);
            }
            logical.retain(|id, _| selector.keeps_from_logical(id));
        }
    }
    let (selection, selected) = selector.finish()?;
    let roots = logical
        .remove(&selected.from_logical)
        .expect("the selection's logical roots are kept");
    Ok((selection, selected, roots))
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::schema_support::with_promql_series_identity;
    use asap_types::workload::{
        DataArrival, DurationMs, MetricType, Predictability, QueryRecurrence, QueryRequirements,
        RepeatedDemand, RepetitionInterval, SourceFacts,
    };

    #[test]
    #[ignore = "debugging aid"]
    fn debug_topk_memo() {
        let queries = ["topk by (job) (3, sum_over_time(m[1m]))"];
        let roots: Vec<(usize, QueryRoot)> = queries
            .iter()
            .map(|q| {
                let root = lower_promql(q, AccuracyTarget::max_error(0.05));
                QueryRoot::Operator(with_promql_series_identity(&root).unwrap())
            })
            .enumerate()
            .collect();
        let pc = generate_per_computation_candidates(roots).unwrap();
        let entries: Vec<QueryWorkloadEntry> = queries
            .iter()
            .map(|q| QueryWorkloadEntry {
                query: asap_types::workload::Query((*q).into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyTarget::max_error(0.05),
                    max_latency_ms: None,
                },
                predictability: Predictability::Predictable { known_at: None },
                recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                    RepetitionInterval(60_000),
                )),
                range_evaluation: None,
            })
            .collect();
        let data = DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 2_000.0 / 15.0,
            },
            sources: [(
                "m".to_string(),
                SourceFacts {
                    series_count: Some(1_000),
                    metric_type: Some(MetricType::Gauge),
                    sample_interval: Some(DurationMs(15_000)),
                    ..SourceFacts::default()
                },
            )]
            .into(),
        };
        let memo = memo(&pc, &entries);
        eprintln!("{}", stats(&memo));
        for (g, group) in memo.groups.iter().enumerate() {
            eprintln!(
                "G{g} {:?} children {:?} roots_of {:?} candidates {}",
                group.node.operator.kind_name(),
                group.children,
                group.roots_of,
                memo.candidates[g].len()
            );
            for c in &memo.candidates[g] {
                eprintln!(
                    "   {:?} implements {:?} children {:?}",
                    c.origin, c.implements, c.children
                );
            }
        }
        let choices = group_choices(&memo).unwrap();
        eprintln!("choices {}", choices.len());
        for choice in &choices {
            for plan in at_thresholds(&memo, choice) {
                let roots = composed_roots(&memo, &pc, &plan);
                eprintln!("choice {choice:?} composed {}", roots.is_some());
                if let Some(roots) = roots {
                    let r =
                        optimize_physical_asap("L", &roots, &entries, &data, &Default::default());
                    eprintln!(
                        "  stage2 {:?}",
                        r.as_ref().map(|v| v.len()).map_err(|e| e.to_string())
                    );
                }
            }
        }
    }
}
