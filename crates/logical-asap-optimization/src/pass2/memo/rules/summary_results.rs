//! Factor composite implementations at summary readouts: a candidate with two
//! or more private builds and a readout of one of them gets a copy as one
//! operator over groups, so each readout is its own group and can choose a
//! state configuration independently, without enumerating combinations of
//! all private states in the enclosing query.
use super::reuse::{add_candidate_over_groups, eligible};
use crate::pass2::memo::state_groups::{
    boundary_ptrs, direct_input_builds, nodes_within, Pass2RuleError,
};
use asap_types::ir::{ASAPOp, OperatorNode};
use asap_types::stages::{Memo, MemoGroupId, OriginOfCandidateForMemoGroup};
use std::collections::HashSet;
use std::rc::Rc;

pub(crate) fn factor_summary_results(memo: &mut Memo) -> Result<usize, Pass2RuleError> {
    let originals: Vec<_> = memo
        .candidates
        .iter()
        .enumerate()
        .flat_map(|(group, candidates)| {
            candidates
                .iter()
                .filter(|candidate| eligible(candidate))
                .map(move |candidate| (MemoGroupId(group as u32), candidate.clone()))
        })
        .collect();
    let mut added = 0;
    for (owner, original) in originals {
        let builds = direct_input_builds(memo, &original);
        if builds.len() < 2 {
            continue;
        }
        let build_pointers: HashSet<_> =
            builds.iter().map(|(build, _)| Rc::as_ptr(build)).collect();
        let boundaries = boundary_ptrs(memo, &original);
        // A readout of exactly one of its private builds.
        let reads_one_build = |node: &Rc<OperatorNode>| {
            !Rc::ptr_eq(node, &original.root_node)
                && matches!(
                    node.asap(),
                    Some(ASAPOp::SummaryEstimate { .. } | ASAPOp::FinalizeExactAccumulator { .. })
                )
                && OperatorNode::reachable(node)
                    .iter()
                    .map(Rc::as_ptr)
                    .filter(|pointer| build_pointers.contains(pointer))
                    .collect::<HashSet<_>>()
                    .len()
                    == 1
        };
        if !nodes_within(&original.root_node, &boundaries)
            .iter()
            .any(reads_one_build)
        {
            continue;
        }
        // Every node beneath the root becomes a group, so each readout is
        // its own group and chooses its state configuration on its own.
        added += usize::from(add_candidate_over_groups(
            memo,
            owner,
            &original,
            Rc::clone(&original.root_node),
            OriginOfCandidateForMemoGroup::SummaryResult,
        )?);
    }
    Ok(added)
}
