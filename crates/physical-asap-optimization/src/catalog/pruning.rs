//! Remove provably unusable configurations before constructing solver variables.
//! No costs or dominance assumptions are involved: finite input realizability,
//! potential output reach and necessary reader counts are sufficient proofs.
use asap_types::stages::CandidateConfigurationInventory;
use asap_types::stages::{CandidateDependencyGraph, PhysicalCandidateId, ReaderConstraint};
use asap_types::stages::{SupplierCandidates, SupplierSetInterner};
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct PruningMetrics {
    pub removed_candidates: usize,
    pub removed_connections: usize,
}

pub fn prune(configured: &mut CandidateConfigurationInventory) -> Result<PruningMetrics, String> {
    let graph = &configured.inventory.graph;
    graph.validate().map_err(|error| error.to_string())?;
    let count = graph.candidates.len();
    if configured.configurations.len() != count || configured.inventory.templates.len() != count {
        return Err("candidate pruning requires aligned configurations and templates".into());
    }
    let keep = usable_candidates(graph);
    let mut mapping = vec![None; count];
    let mut next = 0;
    for (id, retained) in keep.iter().enumerate() {
        if *retained {
            mapping[id] = Some(PhysicalCandidateId(next));
            next += 1;
        }
    }
    let connections_before: usize = graph
        .candidates
        .iter()
        .flat_map(|c| &c.producer_requirements)
        .map(|input| input.compatible_producers.len())
        .sum();
    let graph = &mut configured.inventory.graph;
    let mut supplier_sets = SupplierSetInterner::default();
    let mut remapped: HashMap<SupplierCandidates, SupplierCandidates> = HashMap::new();
    graph.candidates = std::mem::take(&mut graph.candidates)
        .into_iter()
        .enumerate()
        .filter_map(|(id, mut candidate)| {
            if !keep[id] {
                return None;
            }
            for input in &mut candidate.producer_requirements {
                input.compatible_producers = remapped
                    .entry(input.compatible_producers.clone())
                    .or_insert_with(|| {
                        supplier_sets.intern(
                            input
                                .compatible_producers
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
    for root in &mut graph.query_roots {
        root.compatible_candidates = root
            .compatible_candidates
            .iter()
            .filter_map(|id| mapping[id.0])
            .collect();
    }
    for budget in &mut graph.resource_budgets {
        budget.candidate_usage = std::mem::take(&mut budget.candidate_usage)
            .into_iter()
            .enumerate()
            .filter_map(|(id, usage)| keep[id].then_some(usage))
            .collect();
    }
    graph.reader_constraints = std::mem::take(&mut graph.reader_constraints)
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
    for group in &mut configured.inventory.group_candidates {
        *group = group.iter().filter_map(|id| mapping[id.0]).collect();
    }
    configured.inventory.templates = std::mem::take(&mut configured.inventory.templates)
        .into_iter()
        .enumerate()
        .filter_map(|(id, template)| keep[id].then_some(template))
        .collect();
    // source_candidate refers to the pre-configuration inventory, not this map.
    configured.configurations = std::mem::take(&mut configured.configurations)
        .into_iter()
        .enumerate()
        .filter_map(|(id, configuration)| keep[id].then_some(configuration))
        .collect();
    graph.validate().map_err(|error| error.to_string())?;
    let connections_after: usize = graph
        .candidates
        .iter()
        .flat_map(|c| &c.producer_requirements)
        .map(|input| input.compatible_producers.len())
        .sum();
    Ok(PruningMetrics {
        removed_candidates: count - next,
        removed_connections: connections_before - connections_after,
    })
}

pub fn usable_candidates(graph: &CandidateDependencyGraph) -> Vec<bool> {
    let count = graph.candidates.len();
    let mut enabled = vec![true; count];
    loop {
        // Least fixed point: a cycle without a finite implementation is not a
        // producer. A cycle with an independent escape candidate remains usable.
        let mut realizable = vec![false; count];
        loop {
            let mut changed = false;
            for (id, candidate) in graph.candidates.iter().enumerate() {
                if enabled[id]
                    && !realizable[id]
                    && candidate
                        .producer_requirements
                        .iter()
                        .all(|input| input.compatible_producers.iter().any(|id| realizable[id.0]))
                {
                    realizable[id] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let mut reached = vec![vec![false; graph.query_roots.len()]; count];
        for (output, root) in graph.query_roots.iter().enumerate() {
            let mut pending = root.compatible_candidates.clone();
            while let Some(id) = pending.pop() {
                if realizable[id.0] && !reached[id.0][output] {
                    reached[id.0][output] = true;
                    pending.extend(
                        graph.candidates[id.0]
                            .producer_requirements
                            .iter()
                            .flat_map(|input| input.compatible_producers.iter().copied()),
                    );
                }
            }
        }
        // Intersect all allowed-reader restrictions before counting potential
        // distinct queries. Potential reach is an upper bound, never a demand.
        for constraint in &graph.reader_constraints {
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
        for constraint in &graph.reader_constraints {
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
