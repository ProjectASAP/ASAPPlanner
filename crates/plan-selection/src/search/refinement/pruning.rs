//! Unit tests of Stage 2's structural pruning
//! (`asap_physical_asap_optimization::catalog`) that select with the solver.
use asap_physical_asap_optimization::catalog::prune;
use asap_physical_asap_optimization::catalog::test_support::usable_candidates;
use asap_types::stages::CandidateConfigurationInventory;
use asap_types::stages::{CandidateDependencyGraph, PhysicalCandidateId, ReaderConstraint};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::milp::model::select_candidates;
    use asap_types::ir::properties::ExecutionTiming;
    use asap_types::physical::Materialization;
    use asap_types::stages::CandidateConfiguration;
    use asap_types::stages::CandidateInventory;
    use asap_types::stages::MemoGroupId;
    use asap_types::stages::{
        PhysicalCandidate, ProducerRequirement, QueryRootCandidates, ResourceBudget,
    };

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
                    may_keep_with_sole_reader: Vec::new(),
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
            asap_types::stages::supplier_storage_metrics(graph),
            asap_types::stages::SupplierStorageMetrics {
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
