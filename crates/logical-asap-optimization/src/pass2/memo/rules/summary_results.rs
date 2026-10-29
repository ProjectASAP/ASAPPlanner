//! Factor composite implementations at summary readouts. Each readout can then
//! choose a state configuration independently, without enumerating combinations
//! of all private states in the enclosing query.
use super::reuse::{append_producer, append_reader, eligible, replace};
use crate::pass2::memo::candidates::{CandidateMemo, CandidateOrigin};
use crate::pass2::memo::check_generation_deadline;
use crate::pass2::memo::state_groups::{direct_input_builds, StateGroupExpansionError};
use asap_types::ir::{ASAPOp, OperatorNode};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

pub(in crate::pass2::memo) fn factor_summary_results(
    memo: &mut CandidateMemo,
    deadline: Option<Instant>,
) -> Result<usize, StateGroupExpansionError> {
    check_generation_deadline(deadline)?;
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
        check_generation_deadline(deadline)?;
        let builds = direct_input_builds(memo, &original);
        if builds.len() < 2 {
            continue;
        }
        let build_pointers: HashSet<_> =
            builds.iter().map(|(build, _)| Rc::as_ptr(build)).collect();
        let boundaries: HashSet<_> = original
            .children
            .iter()
            .map(|group| Rc::as_ptr(&memo.groups[group.0 as usize].node))
            .collect();
        let mut pending = vec![Rc::clone(&original.node)];
        let mut seen = HashSet::new();
        let mut readouts = Vec::new();
        while let Some(node) = pending.pop() {
            check_generation_deadline(deadline)?;
            if boundaries.contains(&Rc::as_ptr(&node)) || !seen.insert(Rc::as_ptr(&node)) {
                continue;
            }
            if !Rc::ptr_eq(&node, &original.node)
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
            pending.extend(node.children().into_iter().cloned());
        }
        if readouts.is_empty() {
            continue;
        }
        let queries = memo.groups[owner].queries.clone();
        let mut replacements = HashMap::new();
        for readout in readouts {
            check_generation_deadline(deadline)?;
            let group = append_producer(
                memo,
                Rc::clone(&readout),
                &queries,
                CandidateOrigin::SummaryResult,
                original.budget_share,
            )?;
            // The original reader's allocation was used to generate these
            // parameters; lifting the boundary must not reset that provenance.
            replacements.insert(
                Rc::as_ptr(&readout),
                Rc::clone(&memo.groups[group.0 as usize].node),
            );
        }
        let reader = replace(&original.node, &replacements);
        added += usize::from(append_reader(
            memo,
            owner,
            &original,
            reader,
            CandidateOrigin::SummaryResult,
        )?);
    }
    check_generation_deadline(deadline)?;
    Ok(added)
}
