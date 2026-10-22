//! Clusters, coupling thresholds with branch and bound, the dynamic
//! program, build-late (design "Memo-group search", steps 3–7).
//!
//! [`brute_force`] is the reference the decomposed search is checked
//! against: every consistent choice of one candidate per needed group and
//! one threshold per coupling site, composed into a plan and priced by
//! Stage 2 and Stage 3 in every materialization.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use asap_logical_asap_optimization::asap_aware_cse::{
    identical_expression_pass2_candidates, sharing_candidates, with_union_scan_labels,
};
use asap_logical_asap_optimization::per_computation_candidates::{
    recompose_guarantees, PerComputationCandidates,
};
use asap_physical_asap_optimization::{optimize_physical_asap, PhysicalCandidate};
use asap_types::ir::{NonASAPOp, OperatorNode, QueryRoot};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::{groups, CouplingKind, GroupId, ImplementationCandidate, Memo};
use crate::{MemoStats, PlanSelector, PlanningModels, Selection, SelectionError};

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

/// The size of `memo`, built in `build_ms` milliseconds, for the
/// selection report: groups, shared groups (read by two or more parents,
/// or the root of two or more queries), candidates by origin and distinct
/// coupling sites by kind.
pub(crate) fn stats(memo: &Memo, build_ms: f64) -> MemoStats {
    let shared_groups = memo
        .groups
        .iter()
        .filter(|g| g.parents.len() > 1 || (g.parents.is_empty() && g.roots_of.len() > 1))
        .count();
    let mut candidates_by_origin: BTreeMap<String, usize> = BTreeMap::new();
    let mut sites = BTreeSet::new();
    for candidate in memo.candidates.iter().flatten() {
        *candidates_by_origin
            .entry(format!("{:?}", candidate.origin))
            .or_default() += 1;
        sites.extend(candidate.coupling_sites.iter().copied());
    }
    let mut coupling_sites_by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for site in sites {
        *coupling_sites_by_kind
            .entry(format!("{:?}", site.kind))
            .or_default() += 1;
    }
    MemoStats {
        groups: memo.groups.len(),
        shared_groups,
        candidates: memo.candidates.iter().map(Vec::len).sum(),
        candidates_by_origin,
        coupling_sites_by_kind,
        build_ms,
    }
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
/// read. A raw-horizon site has no rewrite and leaves its readers as they
/// are; Stage 3 prices it.
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
                CouplingKind::SharedBuildSize => {
                    for threshold in super::capability::size_thresholds(memo, &site) {
                        let mut rewritten = plan.clone();
                        let mut valid = true;
                        for candidate in rewritten.iter_mut().flatten() {
                            if !candidate.coupling_sites.contains(&site) {
                                continue;
                            }
                            match super::capability::resized_at_threshold(
                                memo, candidate, &site, &threshold,
                            ) {
                                Ok(Some(node)) => candidate.node = node,
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
                CouplingKind::RawHorizon => next.push(plan),
            }
        }
        plans = next;
    }
    plans
}

/// `roots` (each query's own nodes, as Pass 1 composes them) and its
/// identical-expression Pass 2 candidates, as the exhaustive search lists
/// them: every closed set of identical nodes made one node, and, when a
/// metric's scans project different labels, the candidates over every
/// metric's scans widened to one label set that share a scan.
fn identical_expression_variants(roots: Vec<QueryRoot>) -> Vec<Vec<QueryRoot>> {
    let mut variants = Vec::new();
    for shared in identical_expression_pass2_candidates(&roots, &sharing_candidates(&roots)) {
        variants.push(shared.roots);
    }
    if let Ok(Some(widened)) = with_union_scan_labels(&roots) {
        for shared in identical_expression_pass2_candidates(&widened, &sharing_candidates(&widened))
        {
            let shares_a_scan = shared
                .sharing_candidates
                .iter()
                .any(|c| matches!(c.nodes[0].non_asap(), Some(NonASAPOp::Scan { .. })));
            if shares_a_scan {
                variants.push(shared.roots);
            }
        }
    }
    variants.insert(0, roots);
    variants
}

/// The query roots of a plan, each query's own nodes as Pass 1 composes
/// them: each chosen candidate's node with every child group's node
/// replaced by that group's built node, children first. A group below
/// several queries' roots is built once per query (the identical-expression
/// rule shares them: [`identical_expression_variants`]); a group a Pass 2
/// candidate reads from outside its query (a rollup's finer group, a
/// ranking, a host chain) is built once, by the first query below whose
/// root it is, or once for all.
fn composed_roots<Id>(
    memo: &Memo,
    per_computation: &PerComputationCandidates<Id>,
    chosen: &[Option<ImplementationCandidate>],
) -> Option<Vec<QueryRoot>> {
    memo.topological_order()?;
    let queries = per_computation.roots.len();
    // Per query, the groups below its root through the groups' children.
    let mut below: Vec<Vec<bool>> = vec![vec![false; memo.groups.len()]; queries];
    for (g, group) in memo.groups.iter().enumerate() {
        for &q in &group.roots_of {
            mark_below(memo, g, &mut below[q]);
        }
    }
    // A child below the reading query's root is built in that query; any
    // other in the first query it is below, or once for all (`queries`).
    let scope_of = |scope: usize, c: usize| {
        if scope < queries && below[scope][c] {
            scope
        } else {
            (0..queries).find(|&q| below[q][c]).unwrap_or(queries)
        }
    };
    // The candidate building each group: its own, or the fused candidate
    // implementing it.
    let building: Vec<Option<&ImplementationCandidate>> = (0..memo.groups.len())
        .map(|g| {
            chosen[g].as_ref().or_else(|| {
                chosen
                    .iter()
                    .flatten()
                    .find(|c| c.implements.contains(&GroupId(g as u32)))
            })
        })
        .collect();
    let mut built: Vec<HashMap<usize, Rc<OperatorNode>>> = vec![HashMap::new(); queries + 1];
    let roots: Vec<QueryRoot> = per_computation
        .roots
        .iter()
        .enumerate()
        .map(|(q, (_, root))| {
            let mut group_of = |node: &Rc<OperatorNode>| -> Option<Rc<OperatorNode>> {
                let g = memo
                    .groups
                    .iter()
                    .position(|group| group.roots_of.contains(&q) && *group.node == **node)?;
                build(memo, &building, q, g, &scope_of, &mut built)
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
        .collect::<Option<_>>()?;
    // A group's own node carries the frontend's guarantee (none); what it
    // guarantees depends on the implementations chosen below it, so every
    // node's guarantee is composed again, as Pass 1 composition does.
    recompose_guarantees(&roots).ok()
}

/// Group `g` built in `scope` (a query, or the scope of groups no query
/// root reaches): its building candidate's node over its children's built
/// nodes, each child built in `scope_of(scope, child)`; memoized in
/// `built`.
fn build(
    memo: &Memo,
    building: &[Option<&ImplementationCandidate>],
    scope: usize,
    g: usize,
    scope_of: &dyn Fn(usize, usize) -> usize,
    built: &mut Vec<HashMap<usize, Rc<OperatorNode>>>,
) -> Option<Rc<OperatorNode>> {
    if let Some(node) = built[scope].get(&g) {
        return Some(Rc::clone(node));
    }
    let candidate = building[g]?;
    let mut replaced: HashMap<*const OperatorNode, Rc<OperatorNode>> = HashMap::new();
    for child in &candidate.children {
        let c = child.0 as usize;
        let node = build(memo, building, scope_of(scope, c), c, scope_of, built)?;
        replaced.insert(Rc::as_ptr(&memo.groups[c].node), node);
    }
    // A node of its own in each scope, even a leaf's (a scan), so that two
    // queries share it only through the identical-expression rule.
    let node = Rc::new(OperatorNode::clone(&substituted(
        &candidate.node,
        &replaced,
        &mut HashMap::new(),
    )));
    for implemented in &candidate.implements {
        built[scope].insert(implemented.0 as usize, Rc::clone(&node));
    }
    Some(node)
}

/// Mark `g` and every group below it through the groups' children.
fn mark_below(memo: &Memo, g: usize, below: &mut [bool]) {
    if below[g] {
        return;
    }
    below[g] = true;
    for child in &memo.groups[g].children {
        mark_below(memo, child.0 as usize, below);
    }
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

/// The shared builds `plan` resized: each shared-build reader's build that
/// differs from the build of the memo's candidate it was chosen as
/// (`choice`).
fn resized_builds(
    memo: &Memo,
    choice: &GroupChoice,
    plan: &[Option<ImplementationCandidate>],
) -> Vec<Rc<OperatorNode>> {
    choice
        .iter()
        .zip(plan)
        .enumerate()
        .filter_map(|(g, (c, candidate))| {
            let candidate = candidate.as_ref()?;
            let original = &memo.candidates[g][(*c)?];
            let build = super::capability::shared_build_of(candidate)?;
            let own = super::capability::shared_build_of(original)?;
            (candidate.origin == super::CandidateOrigin::SharedBuild && *build != *own)
                .then_some(build)
        })
        .collect()
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
    let started = std::time::Instant::now();
    let memo = memo(per_computation, entries);
    let memo_stats = stats(&memo, started.elapsed().as_secs_f64() * 1e3);
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
        // Scans are widened as the identical-expression rule widens them,
        // every metric's at once ([`identical_expression_variants`]).
        if chosen
            .iter()
            .any(|(_, c)| c.origin == super::CandidateOrigin::WidenedScans)
        {
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
            // Shared builds are one build (the summary-capability rule),
            // then identical nodes shared or not, as the exhaustive search
            // lists them.
            let roots = super::capability::share_equal_builds(
                &roots,
                &resized_builds(&memo, choice, &plan),
            );
            for roots in identical_expression_variants(roots) {
                number += 1;
                let id = format!("L{number}");
                let Ok(implemented) = optimize_physical_asap(
                    &id,
                    &roots,
                    entries,
                    data,
                    models.deployment_capabilities,
                ) else {
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
    }
    let (mut selection, selected) = selector.finish()?;
    selection.search.memo = Some(memo_stats);
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

    /// The 0.9 quantile of `m[5m]` within 1% and within 2% error, each
    /// query reading its shared-build twin: at the larger threshold both
    /// builds are resized to one `k`, and the composed plan, its identical
    /// nodes shared, has one KLL build, although the two queries' windows
    /// are two groups. At the smaller threshold the stricter reader may not
    /// read the build, so it is the only plan.
    #[test]
    fn two_kll_readers_at_a_threshold_compose_to_one_build() {
        use super::super::CandidateOrigin;
        let roots: Vec<(usize, QueryRoot)> = [0.01, 0.02]
            .iter()
            .map(|&e| {
                QueryRoot::Operator(lower_promql(
                    "quantile_over_time(0.9, m[5m])",
                    AccuracyTarget::max_error(e),
                ))
            })
            .enumerate()
            .collect();
        let per_computation = generate_per_computation_candidates(roots).unwrap();
        let mut memo = groups::memo_of(&per_computation);
        super::super::capability::add_shared_builds(&mut memo);
        let is_kll = |c: &ImplementationCandidate| {
            OperatorNode::reachable(&c.node).iter().any(|n| {
                matches!(n.asap(), Some(asap_types::ir::ASAPOp::SummaryAgg { family, .. })
                    if format!("{family:?}").contains("Kll"))
            })
        };
        // Each query's root group reads its KLL twin; every other group
        // its one candidate.
        let choice: GroupChoice = memo
            .groups
            .iter()
            .enumerate()
            .map(|(g, group)| {
                if group.roots_of.is_empty() {
                    return Some(0);
                }
                memo.candidates[g]
                    .iter()
                    .position(|c| c.origin == CandidateOrigin::SharedBuild && is_kll(c))
            })
            .collect();
        assert!(choice.iter().all(Option::is_some));
        let plans = at_thresholds(&memo, &choice);
        assert_eq!(plans.len(), 1);
        let roots = composed_roots(&memo, &per_computation, &plans[0]).unwrap();
        let roots = super::super::capability::share_equal_builds(
            &roots,
            &resized_builds(&memo, &choice, &plans[0]),
        );
        let builds: std::collections::HashSet<*const OperatorNode> = roots
            .iter()
            .flat_map(|root| match root {
                QueryRoot::Operator(node) => OperatorNode::reachable(node),
                QueryRoot::Scalar(_) => Vec::new(),
            })
            .filter(|n| matches!(n.asap(), Some(asap_types::ir::ASAPOp::SummaryAgg { .. })))
            .map(|n| Rc::as_ptr(&n))
            .collect();
        assert_eq!(builds.len(), 1);
    }

    /// A quantile's memo, Pass 1 only, has three groups (scan, window,
    /// quantile) and five candidates (the quantile's exact, KLL and
    /// DDSketch); with one candidate made a pane candidate and coupling
    /// sites added, the stats count candidates by origin and distinct sites
    /// by kind, a site two candidates share once.
    #[test]
    fn stats_count_candidates_by_origin_and_sites_by_kind() {
        use super::super::{CandidateOrigin, CouplingSite};
        let roots = vec![(
            0usize,
            QueryRoot::Operator(lower_promql(
                "quantile_over_time(0.9, m[5m])",
                AccuracyTarget::max_error(0.01),
            )),
        )];
        let mut memo = groups::memo_of(&generate_per_computation_candidates(roots).unwrap());
        let shared = CouplingSite {
            key: 1,
            kind: CouplingKind::SharedBuildSize,
        };
        let chain = CouplingSite {
            key: 2,
            kind: CouplingKind::PaneChain,
        };
        memo.candidates[2][1].coupling_sites = vec![shared];
        memo.candidates[2][2].coupling_sites = vec![shared, chain];
        memo.candidates[2][2].origin = CandidateOrigin::Panes;
        let stats = stats(&memo, 1.5);
        assert_eq!(
            stats,
            MemoStats {
                groups: 3,
                shared_groups: 0,
                candidates: 5,
                candidates_by_origin: [("Pass1".to_string(), 4), ("Panes".to_string(), 1)].into(),
                coupling_sites_by_kind: [
                    ("SharedBuildSize".to_string(), 1),
                    ("PaneChain".to_string(), 1)
                ]
                .into(),
                build_ms: 1.5,
            }
        );
    }

    /// The reference search reports the memo it built.
    #[test]
    fn the_brute_force_selection_reports_its_memo() {
        let query = "quantile_over_time(0.9, m[5m])";
        let target = AccuracyTarget::max_error(0.01);
        let roots = vec![(
            0usize,
            QueryRoot::Operator(lower_promql(query, target.clone())),
        )];
        let per_computation = generate_per_computation_candidates(roots).unwrap();
        let entries = vec![QueryWorkloadEntry {
            query: asap_types::workload::Query(query.into()),
            requirements: QueryRequirements {
                accuracy: target,
                max_latency_ms: None,
            },
            predictability: Predictability::Predictable { known_at: None },
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                RepetitionInterval(60_000),
            )),
            range_evaluation: None,
        }];
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
        let (selection, _, _) = brute_force(
            &per_computation,
            &entries,
            &data,
            crate::PlanningModels::builtin(),
        )
        .unwrap();
        let reported = selection.search.memo.clone().expect("the memo's stats");
        let built = memo(&per_computation, &entries);
        assert_eq!(reported.groups, built.groups.len());
        assert_eq!(
            reported.candidates,
            built.candidates.iter().map(Vec::len).sum::<usize>()
        );
        assert!(reported.build_ms >= 0.0);
        let json = serde_json::to_value(&selection.search).unwrap();
        assert_eq!(json["memo"]["groups"], reported.groups);
    }

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
        eprintln!("{:?}", stats(&memo, 0.0));
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
