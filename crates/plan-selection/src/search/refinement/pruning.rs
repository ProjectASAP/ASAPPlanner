//! Remove provably unusable configurations before constructing solver variables.
//! No costs or dominance assumptions are involved: finite input realizability,
//! potential output reach and necessary reader counts are sufficient proofs.
use crate::search::candidates::configurations::CandidateConfigurationInventory;
use crate::search::candidates::graph::{
    CandidateDependencyGraph, PhysicalCandidateId, ReaderConstraint,
};
use crate::search::candidates::suppliers::{SupplierCandidates, SupplierSetInterner};
use std::collections::HashMap;

#[derive(Debug, Default)]
pub(in crate::search) struct PruningMetrics {
    pub removed_candidates: usize,
    pub removed_connections: usize,
}

pub(in crate::search) fn prune(
    configured: &mut CandidateConfigurationInventory,
) -> Result<PruningMetrics, String> {
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

fn usable_candidates(graph: &CandidateDependencyGraph) -> Vec<bool> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::candidates::configurations::CandidateConfiguration;
    use crate::search::candidates::graph::{
        PhysicalCandidate, ProducerRequirement, QueryRootCandidates, ResourceBudget,
    };
    use crate::search::candidates::inventory::CandidateInventory;
    use crate::search::milp::model::select_candidates;
    use asap_logical_asap_optimization::memo::MemoGroupId;
    use asap_types::ir::properties::ExecutionTiming;
    use asap_types::physical::Materialization;

    fn candidate(cost: f64, inputs: &[&[usize]]) -> PhysicalCandidate {
        PhysicalCandidate {
            group: MemoGroupId(0),
            local_cost: cost,
            producer_requirements: inputs
                .iter()
                .map(|ids| ProducerRequirement {
                    compatible_producers: ids.iter().map(|id| PhysicalCandidateId(*id)).collect(),
                })
                .collect(),
        }
    }
    fn configured(graph: CandidateDependencyGraph) -> CandidateConfigurationInventory {
        let count = graph.candidates.len();
        CandidateConfigurationInventory {
            inventory: CandidateInventory {
                graph,
                templates: vec![None; count],
                group_candidates: vec![(0..count).map(PhysicalCandidateId).collect()],
            },
            configurations: (0..count)
                .map(|id| CandidateConfiguration {
                    source_candidate: PhysicalCandidateId(id),
                    timing: ExecutionTiming::QueryTime,
                    materialization: Materialization::NotMaterialized,
                })
                .collect(),
        }
    }

    // Removing an unrelated candidate remaps IDs once per shared supplier
    // list; the subsequent solve still sees independent consumer requirements.
    #[test]
    fn pruning_retains_shared_supplier_storage_after_remapping() {
        let graph = CandidateDependencyGraph {
            candidates: vec![
                candidate(9.0, &[]),
                candidate(1.0, &[]),
                candidate(1.0, &[&[1]]),
                candidate(1.0, &[&[1]]),
            ],
            query_roots: vec![
                QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(2)],
                },
                QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(3)],
                },
            ],
            ..Default::default()
        };
        let original = graph.clone();
        let mut inventory = configured(graph);
        assert_eq!(prune(&mut inventory).unwrap().removed_candidates, 1);
        let graph = &inventory.inventory.graph;
        assert_eq!(
            graph.candidates[1].producer_requirements[0]
                .compatible_producers
                .as_slice(),
            &[PhysicalCandidateId(0)]
        );
        assert_eq!(
            crate::search::candidates::suppliers::supplier_storage_metrics(graph),
            crate::search::candidates::suppliers::SupplierStorageMetrics {
                logical_connections: 2,
                unique_supplier_sets: 1,
                stored_supplier_ids: 1
            }
        );
        assert_eq!(
            select_candidates(graph).unwrap().unwrap().modeled_cost,
            select_candidates(&original).unwrap().unwrap().modeled_cost
        );
    }

    // Dead dependency cycles and unused nodes disappear; a cycle with a finite
    // escape remains available. Cost and resource accounting survive remapping.
    #[test]
    fn pruning_preserves_optimum_and_resource_indices() {
        let graph = CandidateDependencyGraph {
            candidates: vec![
                candidate(9.0, &[&[1]]),
                candidate(9.0, &[&[0]]),
                candidate(2.0, &[]),
                candidate(1.0, &[&[2, 4]]),
                candidate(1.0, &[&[3]]),
                candidate(1.0, &[]),
            ],
            query_roots: vec![QueryRootCandidates {
                compatible_candidates: vec![PhysicalCandidateId(0), PhysicalCandidateId(3)],
            }],
            resource_budgets: vec![ResourceBudget {
                name: "state".into(),
                capacity: 5.0,
                candidate_usage: vec![9.0, 9.0, 2.0, 3.0, 1.0, 9.0],
            }],
            ..Default::default()
        };
        let before = select_candidates(&graph).unwrap().unwrap();
        let mut inventory = configured(graph);
        let metrics = prune(&mut inventory).unwrap();
        assert_eq!(metrics.removed_candidates, 3);
        assert_eq!(
            inventory.inventory.graph.resource_budgets[0].candidate_usage,
            vec![2.0, 3.0, 1.0]
        );
        assert_eq!(
            inventory.configurations[0].source_candidate,
            PhysicalCandidateId(2)
        );
        let after = select_candidates(&inventory.inventory.graph)
            .unwrap()
            .unwrap();
        assert_eq!(before.modeled_cost, after.modeled_cost);
        assert_eq!(after.modeled_cost, 3.0);
    }

    // Multiple output positions of one query do not satisfy two-reader sharing.
    #[test]
    fn required_reader_pruning_propagates_without_losing_fallback() {
        let mut graph = CandidateDependencyGraph {
            candidates: vec![
                candidate(1.0, &[]),
                candidate(2.0, &[]),
                candidate(1.0, &[&[0]]),
            ],
            query_roots: vec![
                QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(1), PhysicalCandidateId(2)]
                };
                2
            ],
            reader_constraints: vec![ReaderConstraint::MinimumQueryReaders {
                candidate: PhysicalCandidateId(0),
                query_outputs: vec![vec![0, 1]],
                minimum: 2,
            }],
            ..Default::default()
        };
        assert_eq!(usable_candidates(&graph), vec![false, true, false]);
        graph.reader_constraints = vec![ReaderConstraint::MinimumQueryReaders {
            candidate: PhysicalCandidateId(0),
            query_outputs: vec![vec![0], vec![1]],
            minimum: 2,
        }];
        assert_eq!(usable_candidates(&graph), vec![true, true, true]);
        graph
            .reader_constraints
            .push(ReaderConstraint::AllowedOutputs {
                candidate: PhysicalCandidateId(0),
                outputs: vec![0],
            });
        assert_eq!(usable_candidates(&graph), vec![false, true, false]);
    }

    // Missing inputs preserve an uncovered root as infeasible, not an empty workload.
    #[test]
    fn impossible_root_is_not_silently_removed() {
        let mut inventory = configured(CandidateDependencyGraph {
            candidates: vec![candidate(1.0, &[&[]])],
            query_roots: vec![QueryRootCandidates {
                compatible_candidates: vec![PhysicalCandidateId(0)],
            }],
            ..Default::default()
        });
        prune(&mut inventory).unwrap();
        assert_eq!(inventory.inventory.graph.query_roots.len(), 1);
        assert!(select_candidates(&inventory.inventory.graph)
            .unwrap()
            .is_none());
    }

    // Activation implications do not create execution cycles; an unreachable
    // required copy does make the dependent choice unusable.
    #[test]
    fn activation_requirements_preserve_selection_semantics() {
        let mut graph = CandidateDependencyGraph {
            candidates: vec![candidate(1.0, &[]), candidate(2.0, &[])],
            query_roots: vec![
                QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(0)],
                },
                QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(1)],
                },
            ],
            reader_constraints: vec![
                ReaderConstraint::ActivationRequires {
                    candidate: PhysicalCandidateId(0),
                    required_candidate: PhysicalCandidateId(1),
                },
                ReaderConstraint::ActivationRequires {
                    candidate: PhysicalCandidateId(1),
                    required_candidate: PhysicalCandidateId(0),
                },
            ],
            ..Default::default()
        };
        assert_eq!(usable_candidates(&graph), vec![true, true]);
        graph.query_roots.pop();
        assert_eq!(usable_candidates(&graph), vec![false, false]);
    }
}
