//! Remove provably unusable configurations before constructing solver variables.
//! No costs or dominance assumptions are involved: finite input realizability,
//! potential output reach and necessary reader counts are sufficient proofs.
use asap_types::stages::{
    PhysicalASAPMemoCandidates, PhysicalCandidateId, PruningMetrics, ReaderConstraint,
};
use asap_types::stages::{ProducerCandidates, ProducerSetInterner};
use std::collections::HashMap;

/// `configured` without the candidates [`usable_candidates`] rules out, its
/// IDs renumbered densely in their order; the counts removed are recorded in
/// its `pruning_metrics`.
pub fn prune(configured: PhysicalASAPMemoCandidates) -> Result<PhysicalASAPMemoCandidates, String> {
    super::validate_candidates(&configured)?;
    let count = configured.candidates.len();
    let keep = usable_candidates(&configured);
    let mut mapping = vec![None; count];
    let mut next = 0;
    for (id, retained) in keep.iter().enumerate() {
        if *retained {
            mapping[id] = Some(PhysicalCandidateId(next));
            next += 1;
        }
    }
    let connections = |physical: &PhysicalASAPMemoCandidates| -> usize {
        physical
            .candidates
            .iter()
            .flat_map(|c| &c.producer_candidates_per_input)
            .map(|input| input.len())
            .sum()
    };
    let connections_before = connections(&configured);
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
            let id = match &mut constraint {
                ReaderConstraint::AllowedOutputs { candidate, .. }
                | ReaderConstraint::HighestSharedQueryMaterialization { candidate, .. }
                | ReaderConstraint::MinimumQueryReaders { candidate, .. }
                | ReaderConstraint::IncompatibleOutputs { candidate, .. }
                | ReaderConstraint::ActivationRequires { candidate, .. } => candidate,
            };
            *id = mapping[id.0]?;
            if let ReaderConstraint::ActivationRequires {
                required_candidate, ..
            } = &mut constraint
            {
                *required_candidate = mapping[required_candidate.0]?;
            }
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
    super::validate_candidates(&pruned)?;
    pruned.pruning_metrics = PruningMetrics {
        removed_candidates: count - next,
        removed_connections: connections_before - connections(&pruned),
    };
    Ok(pruned)
}

pub fn usable_candidates(physical: &PhysicalASAPMemoCandidates) -> Vec<bool> {
    let count = physical.candidates.len();
    let query_output_candidates = super::query_output_candidates(physical);
    let mut enabled = vec![true; count];
    loop {
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
        let mut retained: Vec<_> = realizable
            .iter()
            .zip(&reached)
            .map(|(realizable, outputs)| *realizable && outputs.iter().any(|value| *value))
            .collect();
        for constraint in &physical.reader_constraints {
            if let ReaderConstraint::ActivationRequires {
                candidate,
                required_candidate,
            } = constraint
            {
                // This is a selection implication, not an execution input.
                // Do not reject legal mutually dependent activation decisions.
                retained[candidate.0] &= enabled[required_candidate.0];
            }
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
                retained[candidate.0] &= queries >= *minimum;
            }
        }
        if retained == enabled {
            return retained;
        }
        enabled = retained;
    }
}
