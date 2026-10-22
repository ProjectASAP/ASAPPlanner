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
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use asap_logical_asap_optimization::accuracy::estimators::size_for_all_readers;
use asap_types::ir::operator::Reduction;
use asap_types::ir::schema::sketch::{GroupingStrategy, SketchParams};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, OperatorNode};

use super::{CandidateOrigin, CouplingKind, CouplingSite, GroupId, ImplementationCandidate, Memo};

/// One size a shared build may take: the per-parameter maximum
/// ([`size_for_all_readers`]) of the requirements of the readers it admits.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SizeThreshold {
    /// The build's size parameters; `None` for an exact accumulator, which
    /// has no size and admits every reader.
    pub params: Option<SketchParams>,
}

/// Add the summary-capability transformation's candidates to `memo`: for
/// every set of two or more reader groups whose Pass 1 summary candidates
/// build one family over one input group, a
/// [`CandidateOrigin::SharedBuild`] copy of each such candidate with that
/// set's [`CouplingKind::SharedBuildSize`] site added.
pub(crate) fn add_shared_builds(memo: &mut Memo) {
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
pub(crate) fn size_thresholds(memo: &Memo, site: &CouplingSite) -> Vec<SizeThreshold> {
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
pub(crate) fn may_read(threshold: &SizeThreshold, reader: &ImplementationCandidate) -> bool {
    match (&threshold.params, reader_build(reader)) {
        (_, None) => false,
        (None, Some((_, params))) => params.is_none(),
        (Some(threshold), Some((_, Some(params)))) => admits(threshold, &params),
        (Some(_), Some((_, None))) => false,
    }
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
/// grouping, or the exact accumulator), the input group, the reduction,
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
    let input_group = candidate.children.iter().find(|&&GroupId(g)| {
        let node = &memo.groups[g as usize].node;
        Rc::ptr_eq(node, child) || **node == **child
    })?;
    let family_key = match family {
        FieldDataType::Sketch(kind, strategy) => format!("{:?}/{strategy:?}", kind.algorithm()),
        exact => format!("{exact:?}"),
    };
    let mut hasher = DefaultHasher::new();
    (
        family_key,
        input_group,
        format!("{reduction:?}/{grouping:?}/{filter:?}/{input:?}"),
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
    use crate::pass::subexpr::MemoGroup;
    use asap_logical_asap_optimization::test_support::{metric_scan, time_range};
    use asap_types::ir::scalar::ColumnRef;
    use asap_types::ir::schema::sketch::{SketchAlgorithm, SketchKind, SketchStatistic};
    use asap_types::ir::schema::{Field, Schema, SummaryUpdate};
    use asap_types::ir::Operator;
    use std::time::Duration;

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
        let group = |node: &Rc<OperatorNode>, children: Vec<GroupId>| super::super::MemoGroup {
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
            memo.groups.push(super::super::MemoGroup {
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
        use crate::pass::subexpr::groups::memo_of;
        use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
        use asap_logical_asap_optimization::test_support::lower_promql;
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
}
