//! The identical-expression and summary-capability transformations of
//! Pass 2 over the memo.
//!
//! **Identical expression.** The memo hash-conses the workload's logical
//! DAG, so identical sub-expressions of several queries are already one
//! group, read by each of their parents: there is nothing to add. Whether
//! the shared group is computed once is the group's single choice of
//! implementation and materialization (Stage 2's `NotMaterialized` or
//! `MatAtQuery` for a group with several parents), which the search makes
//! once for all of its parents. Leaving identical sub-expressions apart is
//! never cheaper under the stages' per-node pricing, since every parent
//! reads the same implementation either way.
//!
//! **Summary capability.** Reader groups whose Pass 1 summary candidates
//! build one family (one sketch algorithm and grouping at any size, or one
//! exact accumulator) over one input group, with the same reduction,
//! grouping, item and weight, can read one build. Each such reader
//! candidate gets a [`CandidateOrigin::SharedBuild`] twin carrying a
//! [`CouplingKind::SharedBuildSize`] site. The twin's node keeps the
//! reader's own size: the shared build's size is the site's threshold,
//! chosen by the search among [`size_thresholds`], and a reader may read it
//! iff [`may_read`].

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use crate::accuracy::estimators::size_for_all_readers;
use crate::asap_aware_cse::with_resized_build;
use crate::per_computation_candidates::PerComputationCandidateError;
use asap_types::ir::operator::Reduction;
use asap_types::ir::schema::sketch::{GroupingStrategy, SketchKind, SketchParams};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::QueryRoot;
use asap_types::ir::{ASAPOp, OperatorNode};

use super::candidates::NodeKeys;
use super::candidates::{
    CandidateMemo as Memo, CandidateOrigin, CouplingKind, CouplingSite, ImplementationCandidate,
};
use super::MemoGroupId as GroupId;

/// One size a shared build may take: the per-parameter maximum
/// ([`size_for_all_readers`]) of the requirements of the readers it admits.
#[derive(Debug, Clone, PartialEq)]
pub struct SizeThreshold {
    /// The build's size parameters; `None` for an exact accumulator, which
    /// has no size and admits every reader.
    pub params: Option<SketchParams>,
}

/// Add the summary-capability transformation's candidates to `memo`: for
/// every set of two or more reader groups whose Pass 1 summary candidates
/// build one family over one input group, a
/// [`CandidateOrigin::SharedBuild`] copy of each such candidate with that
/// set's [`CouplingKind::SharedBuildSize`] site added.
pub fn add_shared_builds(memo: &mut Memo) {
    // Per site key, the (group, candidate index) of each reader candidate.
    let mut readers: HashMap<u64, Vec<(usize, usize)>> = HashMap::new();
    let mut order: Vec<u64> = Vec::new();
    for (group, candidates) in memo.candidates.iter().enumerate() {
        for (index, candidate) in candidates.iter().enumerate() {
            if candidate.origin != CandidateOrigin::Pass1 {
                continue;
            }
            let Some(key) = site_key(memo, candidate) else {
                continue;
            };
            let entry = readers.entry(key).or_default();
            if entry.is_empty() {
                order.push(key);
            }
            entry.push((group, index));
        }
    }
    for key in order {
        let members = &readers[&key];
        let first_group = members[0].0;
        if members.iter().all(|&(group, _)| group == first_group) {
            continue;
        }
        let site = CouplingSite {
            key,
            kind: CouplingKind::SharedBuildSize,
        };
        for &(group, index) in members {
            let mut shared = memo.candidates[group][index].clone();
            shared.origin = CandidateOrigin::SharedBuild;
            shared.coupling_sites.push(site);
            memo.candidates[group].push(shared);
        }
    }
}

/// The sizes the shared build of `site` may take: the per-parameter
/// maximum of every set of its readers' requirements (each reader's alone
/// among them), each once, without a threshold that admits no more
/// readers than a smaller one. Empty for a site no reader carries.
pub fn size_thresholds(memo: &Memo, site: &CouplingSite) -> Vec<SizeThreshold> {
    let requirements: Vec<Option<SketchParams>> = memo
        .candidates
        .iter()
        .flatten()
        .filter(|c| c.origin == CandidateOrigin::SharedBuild && c.coupling_sites.contains(site))
        .filter_map(|c| reader_build(c).map(|(_, params)| params))
        .collect();
    if requirements.iter().any(Option::is_none) {
        // Exact accumulators: one build, no size.
        return vec![SizeThreshold { params: None }];
    }
    let requirements: Vec<SketchParams> = requirements.into_iter().flatten().collect();
    // Every join of a set of requirements, each once.
    let mut joins: Vec<SketchParams> = Vec::new();
    for requirement in &requirements {
        let mut grown: Vec<SketchParams> = joins
            .iter()
            .filter_map(|join| size_for_all_readers(&[join, requirement]))
            .collect();
        grown.push(requirement.clone());
        for join in grown {
            if !joins.iter().any(|known| same_size(known, &join)) {
                joins.push(join);
            }
        }
    }
    // A threshold larger than the join of the readers it admits admits no
    // more readers than that join.
    joins
        .into_iter()
        .filter(|threshold| {
            let admitted: Vec<&SketchParams> = requirements
                .iter()
                .filter(|r| admits(threshold, r))
                .collect();
            size_for_all_readers(&admitted).is_some_and(|join| same_size(&join, threshold))
        })
        .map(|params| SizeThreshold {
            params: Some(params),
        })
        .collect()
}

/// Whether `reader` (a shared-build reader candidate) may read the shared
/// build at `threshold`: its own requirement is no stricter, so the build
/// sized for both is the threshold itself.
fn may_read(threshold: &SizeThreshold, reader: &ImplementationCandidate) -> bool {
    match (&threshold.params, reader_build(reader)) {
        (_, None) => false,
        (None, Some((_, params))) => params.is_none(),
        (Some(threshold), Some((_, Some(params)))) => admits(threshold, &params),
        (Some(_), Some((_, None))) => false,
    }
}

/// `candidate`'s node with its shared build (of `site`) built at
/// `threshold`, every guarantee composed again: the node itself when the
/// build already has that size (or is an exact accumulator, which has
/// none); `None` when `candidate` does not read `site` or may not read the
/// threshold ([`may_read`]).
pub fn resized_at_threshold(
    memo: &Memo,
    candidate: &ImplementationCandidate,
    site: &CouplingSite,
    threshold: &SizeThreshold,
) -> Result<Option<Rc<OperatorNode>>, PerComputationCandidateError> {
    if !candidate.coupling_sites.contains(site)
        || site_key(memo, candidate) != Some(site.key)
        || !may_read(threshold, candidate)
    {
        return Ok(None);
    }
    let (Some((build, Some(own))), Some(size)) = (reader_build(candidate), &threshold.params)
    else {
        return Ok(Some(Rc::clone(&candidate.node)));
    };
    if same_size(&own, size) {
        return Ok(Some(Rc::clone(&candidate.node)));
    }
    let Some(ASAPOp::SummaryAgg {
        family: FieldDataType::Sketch(kind, strategy),
        ..
    }) = build.asap()
    else {
        unreachable!("a reader's build is a sketch when it has a size");
    };
    let family = FieldDataType::Sketch(
        SketchKind::new(kind.algorithm().clone(), size.clone()),
        strategy.clone(),
    );
    let roots = with_resized_build(
        &[QueryRoot::Operator(Rc::clone(&candidate.node))],
        &build,
        family,
    )?;
    let [QueryRoot::Operator(node)] = &roots[..] else {
        unreachable!("an operator root");
    };
    // Composing the guarantees again rebuilds the child groups' nodes too;
    // the candidate reads them as the groups' own nodes.
    let child_nodes: HashSet<*const OperatorNode> = candidate
        .children
        .iter()
        .map(|c| Rc::as_ptr(&memo.groups[c.0 as usize].node))
        .collect();
    Ok(Some(with_child_nodes(
        &candidate.node,
        node,
        &child_nodes,
        &mut HashMap::new(),
    )))
}

/// `resized` (a rewrite of `original` with the same shape) with each node
/// that is a child group's node in `original` put back, and the nodes above
/// rebuilt over them, keeping their own schema and guarantee.
fn with_child_nodes(
    original: &Rc<OperatorNode>,
    resized: &Rc<OperatorNode>,
    child_nodes: &HashSet<*const OperatorNode>,
    done: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Rc<OperatorNode> {
    if child_nodes.contains(&Rc::as_ptr(original)) {
        return Rc::clone(original);
    }
    if let Some(found) = done.get(&Rc::as_ptr(resized)) {
        return Rc::clone(found);
    }
    let pairs: HashMap<*const OperatorNode, Rc<OperatorNode>> = original
        .children()
        .into_iter()
        .zip(resized.children())
        .map(|(o, r)| (Rc::as_ptr(r), with_child_nodes(o, r, child_nodes, done)))
        .collect();
    let mut changed = false;
    let operator = resized.operator.map_children(|child| {
        let new = Rc::clone(&pairs[&Rc::as_ptr(child)]);
        changed |= !Rc::ptr_eq(&new, child);
        new
    });
    let out = match changed {
        false => Rc::clone(resized),
        true => Rc::new(
            OperatorNode::with_schema(operator, resized.schema.clone())
                .with_guarantee(resized.guarantee.clone())
                .with_timing(resized.timing)
                .with_materialization(resized.materialization),
        ),
    };
    done.insert(Rc::as_ptr(resized), Rc::clone(&out));
    out
}

/// The shared build `candidate` reads, if any ([`add_shared_builds`]).
pub fn shared_build_of(candidate: &ImplementationCandidate) -> Option<Rc<OperatorNode>> {
    reader_build(candidate).map(|(build, _)| build)
}

/// `roots` with each set of equal summary builds that one of `resized`
/// equals made one build: the summary-capability rule's builds resized to
/// one size, over equal inputs (two queries' equal windows are two groups,
/// since a window has no unique key, so identical-expression sharing
/// leaves their builds apart). As the rule shares only builds whose sizes
/// are not all equal, equal builds none of which was resized stay apart.
/// Each build is replaced by the first equal one a children-first walk
/// meets, and the nodes above rebuilt.
pub fn share_equal_builds(roots: &[QueryRoot], resized: &[Rc<OperatorNode>]) -> Vec<QueryRoot> {
    // Equal by node key: guarantees, composed again over a plan, may differ
    // from the resized candidate's.
    let mut keys = NodeKeys::default();
    let resized: Vec<u64> = resized.iter().map(|r| keys.key(r)).collect();
    let mut kept: Vec<(u64, Rc<OperatorNode>)> = Vec::new();
    let mut replaced: HashMap<*const OperatorNode, Rc<OperatorNode>> = HashMap::new();
    let mut seen = HashSet::new();
    for node in roots.iter().flat_map(root_nodes) {
        if !seen.insert(Rc::as_ptr(&node))
            || !matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. }))
        {
            continue;
        }
        let key = keys.key(&node);
        if !resized.contains(&key) {
            continue;
        }
        match kept.iter().find(|(k, _)| *k == key).map(|(_, n)| n) {
            Some(first) => {
                replaced.insert(Rc::as_ptr(&node), Rc::clone(first));
            }
            None => kept.push((key, node)),
        }
    }
    if replaced.is_empty() {
        return roots.to_vec();
    }
    let mut done = HashMap::new();
    roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(node) => {
                QueryRoot::Operator(replaced_in(node, &replaced, &mut done))
            }
            QueryRoot::Scalar(expr) => QueryRoot::Scalar(
                expr.map_operator_refs(&mut |node| replaced_in(node, &replaced, &mut done)),
            ),
        })
        .collect()
}

/// Every node of `root`'s operators, each once, children first.
fn root_nodes(root: &QueryRoot) -> Vec<Rc<OperatorNode>> {
    let starts: Vec<Rc<OperatorNode>> = match root {
        QueryRoot::Operator(node) => vec![Rc::clone(node)],
        QueryRoot::Scalar(expr) => expr.operator_refs().into_iter().cloned().collect(),
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    fn walk(
        node: &Rc<OperatorNode>,
        seen: &mut HashSet<*const OperatorNode>,
        out: &mut Vec<Rc<OperatorNode>>,
    ) {
        if !seen.insert(Rc::as_ptr(node)) {
            return;
        }
        for child in node.children() {
            walk(child, seen, out);
        }
        out.push(Rc::clone(node));
    }
    for start in &starts {
        walk(start, &mut seen, &mut out);
    }
    out
}

/// `node` with each node of `replaced` (by address) swapped, the nodes
/// above rebuilt over the swapped ones.
fn replaced_in(
    node: &Rc<OperatorNode>,
    replaced: &HashMap<*const OperatorNode, Rc<OperatorNode>>,
    done: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
) -> Rc<OperatorNode> {
    if let Some(found) = replaced.get(&Rc::as_ptr(node)) {
        return Rc::clone(found);
    }
    if let Some(found) = done.get(&Rc::as_ptr(node)) {
        return Rc::clone(found);
    }
    let mut changed = false;
    let operator = node.operator.map_children(|child| {
        let new = replaced_in(child, replaced, done);
        changed |= !Rc::ptr_eq(&new, child);
        new
    });
    let out = match changed {
        false => Rc::clone(node),
        true => Rc::new(
            OperatorNode::with_schema(operator, node.schema.clone())
                .with_guarantee(node.guarantee.clone())
                .with_timing(node.timing)
                .with_materialization(node.materialization),
        ),
    };
    done.insert(Rc::as_ptr(node), Rc::clone(&out));
    out
}

/// Whether a build at `threshold` serves a reader that requires `required`.
fn admits(threshold: &SketchParams, required: &SketchParams) -> bool {
    size_for_all_readers(&[threshold, required]).is_some_and(|join| same_size(&join, threshold))
}

/// Equal sizes, the recorded failure probabilities and errors included
/// (which `SketchParams` equality ignores).
fn same_size(a: &SketchParams, b: &SketchParams) -> bool {
    a == b && format!("{a:?}") == format!("{b:?}")
}

/// The site key of `candidate`'s summary build, when it is a shareable
/// one over a group of `memo`: a hash of the family (algorithm and
/// grouping, or the exact accumulator), the input group's structure (equal
/// input groups are one input), the reduction,
/// the grouping, the filter, and the item and weight.
fn site_key(memo: &Memo, candidate: &ImplementationCandidate) -> Option<u64> {
    let (build, _) = reader_build(candidate)?;
    let Some(ASAPOp::SummaryAgg {
        child,
        family,
        input,
        reduction,
        grouping,
        filter,
    }) = build.asap()
    else {
        return None;
    };
    // The build reads one of its child groups; equal input groups (two
    // queries' identical windows, which are not one group without a
    // unique key) are one input, as the rule's `same_input_data` decides.
    candidate
        .children
        .iter()
        .find(|&&GroupId(g)| Rc::ptr_eq(&memo.groups[g as usize].node, child))?;
    let input = (NodeKeys::default().key(child), input);
    let family_key = match family {
        FieldDataType::Sketch(kind, strategy) => format!("{:?}/{strategy:?}", kind.algorithm()),
        exact => format!("{exact:?}"),
    };
    let mut hasher = DefaultHasher::new();
    (
        family_key,
        input.0,
        format!("{reduction:?}/{grouping:?}/{filter:?}/{:?}", input.1),
    )
        .hash(&mut hasher);
    Some(hasher.finish())
}

/// `candidate`'s summary build, nearest its root, that the summary
/// capability rule can share, with its size (`None` for an exact
/// accumulator): a per-subpopulation sketch read by an estimate, or a
/// per-series exact accumulator read by its finalization.
fn reader_build(
    candidate: &ImplementationCandidate,
) -> Option<(Rc<OperatorNode>, Option<SketchParams>)> {
    let mut queue = std::collections::VecDeque::from([Rc::clone(&candidate.node)]);
    while let Some(node) = queue.pop_front() {
        let build = match node.asap() {
            Some(ASAPOp::SummaryEstimate { summary_input, .. }) => Some(summary_input),
            Some(ASAPOp::FinalizeExactAccumulator { child }) => Some(child),
            _ => None,
        };
        if let Some(build) = build {
            match build.asap() {
                Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, GroupingStrategy::PerSubpopulationInstance),
                    ..
                }) => return Some((Rc::clone(build), Some(kind.params().clone()))),
                Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::ExactAggregate(..),
                    reduction: Reduction::PerEntity,
                    ..
                }) => return Some((Rc::clone(build), None)),
                _ => {}
            }
        }
        queue.extend(node.children().into_iter().cloned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memo::MemoGroup;
    use crate::test_support::{metric_scan, time_range};
    use asap_types::ir::scalar::ColumnRef;
    use asap_types::ir::schema::sketch::{SketchAlgorithm, SketchKind, SketchStatistic};
    use asap_types::ir::schema::{Field, Schema, SummaryUpdate};
    use asap_types::ir::Operator;
    use std::time::Duration;

    fn memo_of<Id: Clone>(
        inventory: &crate::per_computation_candidates::PerComputationCandidates<Id>,
    ) -> Memo {
        super::super::candidates::try_populate_pass1(
            super::super::LogicalMemo::from_roots(&inventory.roots),
            inventory,
        )
        .expect("generated inventory belongs to its logical memo")
    }

    /// A per-series sketch build of `algorithm` with `params` over `child`.
    fn build(
        algorithm: SketchAlgorithm,
        params: SketchParams,
        child: &Rc<OperatorNode>,
    ) -> Rc<OperatorNode> {
        let family = FieldDataType::Sketch(
            SketchKind::new(algorithm, params),
            GroupingStrategy::PerSubpopulationInstance,
        );
        let schema = Schema::lifted(vec![Field::new("state", family.clone(), false)], None);
        Rc::new(OperatorNode::with_schema(
            Operator::ASAP(ASAPOp::SummaryAgg {
                child: Rc::clone(child),
                family,
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
                grouping: GroupingStrategy::PerSubpopulationInstance,
                filter: None,
            }),
            schema,
        ))
    }

    /// The 0.9 quantile of `build`.
    fn estimate(build: Rc<OperatorNode>) -> Rc<OperatorNode> {
        Rc::new(OperatorNode::with_schema(
            Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input: build,
                query: SketchStatistic::Quantile { q: 0.9 },
            }),
            Schema::lifted(Vec::new(), None),
        ))
    }

    fn kll(k: u32) -> (SketchAlgorithm, SketchParams) {
        (
            SketchAlgorithm::Kll,
            SketchParams::Kll {
                k,
                failure_probability: None,
            },
        )
    }

    fn ddsketch(alpha: f64) -> (SketchAlgorithm, SketchParams) {
        (SketchAlgorithm::DDSketch, SketchParams::DDSketch { alpha })
    }

    /// A memo of a scan (group 0), its 5-minute window (group 1), and one
    /// reader group per entry of `readers` over the window, each with one
    /// Pass 1 candidate: the 0.9 quantile of that sketch over the window.
    fn memo(readers: &[(SketchAlgorithm, SketchParams)]) -> Memo {
        let scan = metric_scan(&[]);
        let window = time_range(Duration::from_secs(300), Rc::clone(&scan));
        let group = |node: &Rc<OperatorNode>, children: Vec<GroupId>| crate::memo::MemoGroup {
            node: Rc::clone(node),
            children,
            parents: Vec::new(),
            queries: Vec::new(),
            roots_of: Vec::new(),
        };
        let mut memo = Memo {
            groups: vec![group(&scan, vec![]), group(&window, vec![GroupId(0)])],
            candidates: vec![Vec::new(), Vec::new()],
        };
        for (q, (algorithm, params)) in readers.iter().enumerate() {
            let node = estimate(build(algorithm.clone(), params.clone(), &window));
            memo.groups.push(crate::memo::MemoGroup {
                roots_of: vec![q],
                queries: vec![q],
                ..group(&node, vec![GroupId(1)])
            });
            memo.candidates.push(vec![ImplementationCandidate {
                implements: vec![GroupId(memo.groups.len() as u32 - 1)],
                node,
                children: vec![GroupId(1)],
                origin: CandidateOrigin::Pass1,
                coupling_sites: Vec::new(),
                materializations: Vec::new(),
                budget_share: 1,
                requires_exact_children: Vec::new(),
                rollup_of: None,
                requires_sites: Vec::new(),
            }]);
        }
        add_shared_builds(&mut memo);
        memo
    }

    /// The shared-build candidates of `memo`, and their sites.
    fn shared(memo: &Memo) -> (Vec<&ImplementationCandidate>, Vec<CouplingSite>) {
        let readers: Vec<&ImplementationCandidate> = memo
            .candidates
            .iter()
            .flatten()
            .filter(|c| c.origin == CandidateOrigin::SharedBuild)
            .collect();
        let mut sites: Vec<CouplingSite> = readers
            .iter()
            .flat_map(|c| c.coupling_sites.iter().copied())
            .collect();
        sites.sort();
        sites.dedup();
        (readers, sites)
    }

    fn k_of(threshold: &SizeThreshold) -> u32 {
        match threshold.params {
            Some(SketchParams::Kll { k, .. }) => k,
            ref other => panic!("not a KLL size: {other:?}"),
        }
    }

    /// Two KLL readers of different `k` over one window: one site, a
    /// shared-build candidate per reader, and one threshold per reader's
    /// `k`.
    #[test]
    fn two_kll_readers_give_one_site_with_two_thresholds() {
        let memo = memo(&[kll(200), kll(400)]);
        let (readers, sites) = shared(&memo);
        assert_eq!(readers.len(), 2);
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].kind, CouplingKind::SharedBuildSize);
        let mut ks: Vec<u32> = size_thresholds(&memo, &sites[0]).iter().map(k_of).collect();
        ks.sort_unstable();
        assert_eq!(ks, vec![200, 400]);
    }

    /// The larger threshold admits both readers; the smaller only the
    /// reader that requires it.
    #[test]
    fn the_larger_threshold_admits_both_readers_the_smaller_one() {
        let memo = memo(&[kll(200), kll(400)]);
        let (readers, sites) = shared(&memo);
        let thresholds = size_thresholds(&memo, &sites[0]);
        let admitted = |k: u32| {
            let threshold = thresholds.iter().find(|t| k_of(t) == k).unwrap();
            readers.iter().filter(|r| may_read(threshold, r)).count()
        };
        assert_eq!(admitted(400), 2);
        assert_eq!(admitted(200), 1);
    }

    /// A DDSketch and a KLL reader over one window are different families:
    /// no site, no shared-build candidate.
    #[test]
    fn ddsketch_and_kll_readers_do_not_share_a_site() {
        let memo = memo(&[ddsketch(0.01), kll(200)]);
        let (readers, sites) = shared(&memo);
        assert!(readers.is_empty());
        assert!(sites.is_empty());
    }

    /// Three DDSketch readers, two of one α: thresholds are the distinct
    /// α's, the strictest admitting all three.
    #[test]
    fn equal_requirements_give_one_threshold() {
        let memo = memo(&[ddsketch(0.02), ddsketch(0.01), ddsketch(0.02)]);
        let (readers, sites) = shared(&memo);
        assert_eq!(readers.len(), 3);
        let thresholds = size_thresholds(&memo, &sites[0]);
        assert_eq!(thresholds.len(), 2);
        let strictest = thresholds
            .iter()
            .find(|t| t.params == Some(SketchParams::DDSketch { alpha: 0.01 }))
            .unwrap();
        assert!(readers.iter().all(|r| may_read(strictest, r)));
    }

    /// Identical expression is group identity: the per-job rate sum of two
    /// queries is one group, reached by both and read by each query's
    /// arithmetic, and the summary-capability transformation adds nothing
    /// for it.
    #[test]
    fn identical_sub_expressions_are_one_group_with_two_parents() {
        use crate::per_computation_candidates::generate_per_computation_candidates;
        use crate::test_support::lower_promql;
        use asap_types::ir::properties::AccuracyTarget;
        use asap_types::ir::QueryRoot;
        let roots = [
            "sum by (job) (rate(m[5m])) * 2",
            "sum by (job) (rate(m[5m])) / 2",
        ]
        .iter()
        .map(|q| QueryRoot::Operator(lower_promql(q, AccuracyTarget::exact())))
        .enumerate()
        .collect();
        let mut memo = memo_of(&generate_per_computation_candidates(roots).unwrap());
        let before: Vec<usize> = memo.candidates.iter().map(Vec::len).collect();
        add_shared_builds(&mut memo);
        let after: Vec<usize> = memo.candidates.iter().map(Vec::len).collect();
        assert_eq!(before, after);
        let sums: Vec<&MemoGroup> = memo
            .groups
            .iter()
            .filter(|g| g.node.operator.kind_name() == "Aggregate" && g.queries == vec![0, 1])
            .collect();
        let sum = sums.last().expect("the shared sum");
        assert_eq!(sum.parents.len(), 2);
        let parents_roots: Vec<Vec<usize>> = sum
            .parents
            .iter()
            .map(|p| memo.groups[p.0 as usize].roots_of.clone())
            .collect();
        assert_eq!(parents_roots, vec![vec![0], vec![1]]);
    }

    /// On a memo built by `memo_of`: the 0.9 quantile of one 5-minute
    /// window within 1% and within 2% error. The scan is one group; the two
    /// windows (no unique key) are a group each but equal, so each query's
    /// KLL candidate gets a shared-build twin with one site, whose
    /// thresholds are the two sizes.
    #[test]
    fn kll_readers_of_equal_windows_share_a_site_on_a_real_memo() {
        use crate::per_computation_candidates::generate_per_computation_candidates;
        use crate::test_support::lower_promql;
        use asap_types::ir::properties::AccuracyTarget;
        use asap_types::ir::QueryRoot;
        let roots = [0.01, 0.02]
            .iter()
            .map(|&e| {
                QueryRoot::Operator(lower_promql(
                    "quantile_over_time(0.9, m[5m])",
                    AccuracyTarget::max_error(e),
                ))
            })
            .enumerate()
            .collect();
        let mut memo = memo_of(&generate_per_computation_candidates(roots).unwrap());
        add_shared_builds(&mut memo);
        let (readers, sites) = shared(&memo);
        let kll_sites: Vec<&CouplingSite> = sites
            .iter()
            .filter(|site| {
                size_thresholds(&memo, site)
                    .iter()
                    .all(|t| matches!(t.params, Some(SketchParams::Kll { .. })))
            })
            .collect();
        assert_eq!(kll_sites.len(), 1);
        let kll_readers: Vec<_> = readers
            .iter()
            .filter(|r| r.coupling_sites.contains(kll_sites[0]))
            .collect();
        assert_eq!(kll_readers.len(), 2);
        assert_ne!(kll_readers[0].implements, kll_readers[1].implements);
        let mut ks: Vec<u32> = size_thresholds(&memo, kll_sites[0])
            .iter()
            .map(k_of)
            .collect();
        ks.sort_unstable();
        assert_eq!(ks.len(), 2);
        assert!(ks[0] < ks[1]);
    }
}
