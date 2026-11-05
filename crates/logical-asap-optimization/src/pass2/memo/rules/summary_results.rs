//! Factor composite implementations at summary readouts. Each readout can then
//! choose a state configuration independently, without enumerating combinations
//! of all private states in the enclosing query.
use super::reuse::{append_producer, append_reader, eligible, replace};
use crate::pass2::memo::state_groups::{
    boundary_ptrs, direct_input_builds, nodes_within, Pass2RuleError,
};
use asap_types::ir::{ASAPOp, OperatorNode};
use asap_types::stages::{Memo, OriginOfCandidateForMemoGroup};
use std::collections::{HashMap, HashSet};
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
                .map(move |candidate| (group, candidate.clone()))
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
        let mut readouts = Vec::new();
        for node in nodes_within(&original.root_node, &boundaries) {
            if !Rc::ptr_eq(&node, &original.root_node)
                && matches!(
                    node.asap(),
                    Some(ASAPOp::SummaryEstimate { .. } | ASAPOp::FinalizeExactAccumulator { .. })
                )
            {
                let private_builds: HashSet<_> = OperatorNode::reachable(&node)
                    .iter()
                    .map(Rc::as_ptr)
                    .filter(|pointer| build_pointers.contains(pointer))
                    .collect();
                if private_builds.len() == 1 {
                    readouts.push(Rc::clone(&node));
                }
            }
        }
        if readouts.is_empty() {
            continue;
        }
        let queries = memo.groups()[owner].queries.clone();
        let mut replacements = HashMap::new();
        for readout in readouts {
            let group = append_producer(
                memo,
                Rc::clone(&readout),
                &queries,
                OriginOfCandidateForMemoGroup::SummaryResult,
                original.accuracy_budget_parts,
            )?;
            // The original reader's allocation was used to generate these
            // parameters; lifting the boundary must not reset that provenance.
            replacements.insert(
                Rc::as_ptr(&readout),
                Rc::clone(&memo.groups()[group.0 as usize].logical_node),
            );
        }
        let reader = replace(&original.root_node, &replacements);
        added += usize::from(append_reader(
            memo,
            owner,
            &original,
            reader,
            OriginOfCandidateForMemoGroup::SummaryResult,
        )?);
    }
    Ok(added)
}
