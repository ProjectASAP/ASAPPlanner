//! Remove provably unusable configurations before constructing solver variables.
//! No costs or dominance assumptions are involved: finite input realizability,
//! potential output reach and necessary reader counts are sufficient proofs.
use asap_types::stages::{
    PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalCandidateKey, PruneReason,
    PrunedCandidate, ReaderConstraint,
};
use asap_types::stages::{ProducerCandidates, ProducerSetInterner};
use std::collections::HashMap;

/// `configured` without the candidates [`prune_reasons`] rules out, its
/// IDs renumbered densely in their order; each removed one is appended to
/// its `pruned_candidates` with the reason.
pub fn prune(configured: PhysicalASAPMemoCandidates) -> Result<PhysicalASAPMemoCandidates, String> {
    super::validate_candidates(&configured)?;
    let count = configured.candidates.len();
    let reasons = prune_reasons(&configured);
    let keep: Vec<bool> = reasons.iter().map(Option::is_none).collect();
    let mut pruned_candidates = configured.pruned_candidates.clone();
    for (candidate, reason) in configured.candidates.iter().zip(&reasons) {
        if let Some(reason) = *reason {
            pruned_candidates.push(PrunedCandidate {
                group: configured.operator_nodes_of_candidates_for_memo_groups
                    [candidate.operator_node.0]
                    .group(),
                candidate: PhysicalCandidateKey {
                    operator_node: candidate.operator_node,
                    timing: candidate.timing,
                    materialization: candidate.materialization,
                },
                reason,
            });
        }
    }
    let mut mapping = vec![None; count];
    let mut next = 0;
    for (id, retained) in keep.iter().enumerate() {
        if *retained {
            mapping[id] = Some(PhysicalCandidateId(next));
            next += 1;
        }
    }
    let mut producer_sets = ProducerSetInterner::default();
    let mut remapped: HashMap<ProducerCandidates, ProducerCandidates> = HashMap::new();
    let candidates = configured
        .candidates
        .into_iter()
        .enumerate()
        .filter_map(|(id, mut candidate)| {
            if !keep[id] {
                return None;
            }
            for input in &mut candidate.producer_candidates_per_input {
                *input = remapped
                    .entry(input.clone())
                    .or_insert_with(|| {
                        producer_sets.intern(
                            input
                                .iter()
                                .filter_map(|id| mapping[id.0])
                                .collect::<Vec<_>>(),
                        )
                    })
                    .clone();
            }
            Some(candidate)
        })
        .collect();
    let reader_constraints = configured
        .reader_constraints
        .into_iter()
        .filter_map(|mut constraint| {
            let id = constraint.candidate_mut();
            *id = mapping[id.0]?;
            Some(constraint)
        })
        .collect();
    // Operator nodes are kept whole: `operator_node` indexes them unchanged.
    let mut pruned = PhysicalASAPMemoCandidates::new(
        configured.operator_nodes_of_candidates_for_memo_groups,
        candidates,
        configured.query_roots,
        reader_constraints,
    );
    pruned.pruned_candidates = pruned_candidates;
    super::validate_candidates(&pruned)?;
    Ok(pruned)
}

/// Which candidates of `physical` [`prune`] keeps.
#[cfg(feature = "test-support")]
pub fn usable_candidates(physical: &PhysicalASAPMemoCandidates) -> Vec<bool> {
    prune_reasons(physical)
        .iter()
        .map(Option::is_none)
        .collect()
}

/// Why each candidate of `physical` cannot be used, `None` for a usable
/// one: the reason of the first round that ruled it out, and within a
/// round the first of: an input with no buildable producer, no query output
/// reached, too few query readers.
fn prune_reasons(physical: &PhysicalASAPMemoCandidates) -> Vec<Option<PruneReason>> {
    let count = physical.candidates.len();
    let query_output_candidates = super::query_output_candidates(physical);
    let mut reasons: Vec<Option<PruneReason>> = vec![None; count];
    loop {
        let enabled: Vec<bool> = reasons.iter().map(Option::is_none).collect();
        // Least fixed point: a cycle without a finite implementation is not a
        // producer. A cycle with an independent escape candidate remains usable.
        let mut realizable = vec![false; count];
        loop {
            let mut changed = false;
            for (id, candidate) in physical.candidates.iter().enumerate() {
                if enabled[id]
                    && !realizable[id]
                    && candidate
                        .producer_candidates_per_input
                        .iter()
                        .all(|input| input.iter().any(|id| realizable[id.0]))
                {
                    realizable[id] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let mut reached = vec![vec![false; query_output_candidates.len()]; count];
        for (output, root) in query_output_candidates.iter().enumerate() {
            let mut pending = root.clone();
            while let Some(id) = pending.pop() {
                if realizable[id.0] && !reached[id.0][output] {
                    reached[id.0][output] = true;
                    pending.extend(
                        physical.candidates[id.0]
                            .producer_candidates_per_input
                            .iter()
                            .flat_map(|input| input.iter().copied()),
                    );
                }
            }
        }
        // Intersect all allowed-reader restrictions before counting potential
        // distinct queries. Potential reach is an upper bound, never a demand.
        for constraint in &physical.reader_constraints {
            if let ReaderConstraint::AllowedOutputs { candidate, outputs } = constraint {
                for (output, value) in reached[candidate.0].iter_mut().enumerate() {
                    *value &= outputs.contains(&output);
                }
            }
        }
        let mut round: Vec<Option<PruneReason>> = realizable
            .iter()
            .zip(&reached)
            .zip(&enabled)
            .map(|((realizable, outputs), enabled)| {
                if !enabled || *realizable && outputs.iter().any(|value| *value) {
                    None
                } else if !realizable {
                    Some(PruneReason::InputHasNoBuildableProducer)
                } else {
                    Some(PruneReason::ReachesNoQueryOutput)
                }
            })
            .collect();
        for constraint in &physical.reader_constraints {
            if let ReaderConstraint::MinimumQueryReaders {
                candidate,
                query_outputs,
                minimum,
            } = constraint
            {
                let queries = query_outputs
                    .iter()
                    .filter(|outputs| outputs.iter().any(|output| reached[candidate.0][*output]))
                    .count();
                if queries < *minimum {
                    round[candidate.0].get_or_insert(PruneReason::TooFewQueryReaders);
                }
            }
        }
        let mut changed = false;
        for (reason, round) in reasons.iter_mut().zip(round) {
            if reason.is_none() && round.is_some() {
                *reason = round;
                changed = true;
            }
        }
        if !changed {
            return reasons;
        }
    }
}
