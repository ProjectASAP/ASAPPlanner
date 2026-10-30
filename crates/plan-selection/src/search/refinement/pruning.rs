//! Unit tests of Stage 2's structural pruning
//! (`asap_physical_asap_optimization::catalog`) that select with the solver.
use crate::search::candidates::dag::DagOfPhysicalCandidatesWithCosts;
use crate::search::candidates::dag::PhysicalCandidatesWithCosts;
use asap_physical_asap_optimization::catalog::test_support::usable_candidates;
use asap_types::stages::{PhysicalCandidateId, ReaderConstraint};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::milp::model::select_candidates;
    use asap_types::ir::properties::ExecutionTiming;
    use asap_types::physical::Materialization;

    use crate::search::candidates::dag::{
        InputProducerCandidates, PhysicalCandidateWithCost, QueryOutputCandidates, ResourceBudget,
    };

    fn candidate(cost: f64, inputs: &[&[usize]]) -> PhysicalCandidateWithCost {
        PhysicalCandidateWithCost {
            local_cost: cost,
            producer_candidates_per_input: inputs
                .iter()
                .map(|ids| InputProducerCandidates {
                    candidates: ids.iter().map(|id| PhysicalCandidateId(*id)).collect(),
                })
                .collect(),
        }
    }
    fn configured(graph: DagOfPhysicalCandidatesWithCosts) -> PhysicalCandidatesWithCosts {
        let count = graph.candidates.len();
        PhysicalCandidatesWithCosts::from_dag_for_tests(
            graph,
            (0..count)
                .map(|_| {
                    (
                        crate::search::candidates::dag::placeholder_node_for_tests(),
                        (ExecutionTiming::QueryTime, Materialization::NotMaterialized),
                    )
                })
                .collect(),
        )
    }
    fn usable(graph: &DagOfPhysicalCandidatesWithCosts) -> Vec<bool> {
        usable_candidates(&configured(graph.clone()).physical)
    }

    // Removing an unrelated candidate remaps IDs once per shared producer
    // list; the subsequent solve still sees independent consumer requirements.
    #[test]
    fn pruning_retains_shared_producer_storage_after_remapping() {
        let graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![
                candidate(9.0, &[]),
                candidate(1.0, &[]),
                candidate(1.0, &[&[1]]),
                candidate(1.0, &[&[1]]),
            ],
            query_roots: vec![
                QueryOutputCandidates {
                    candidates: vec![PhysicalCandidateId(2)],
                },
                QueryOutputCandidates {
                    candidates: vec![PhysicalCandidateId(3)],
                },
            ],
            ..Default::default()
        };
        let original = graph.clone();
        let mut inventory = configured(graph);
        assert_eq!(inventory.prune_for_tests().removed_candidates, 1);
        let graph = &inventory.dag;
        assert_eq!(
            graph.candidates[1].producer_candidates_per_input[0]
                .candidates
                .as_slice(),
            &[PhysicalCandidateId(0)]
        );
        assert_eq!(
            asap_types::stages::producer_storage_metrics(&inventory.physical),
            asap_types::stages::ProducerStorageMetrics {
                logical_connections: 2,
                unique_producer_sets: 1,
                stored_producer_ids: 1
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
        let graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![
                candidate(9.0, &[&[1]]),
                candidate(9.0, &[&[0]]),
                candidate(2.0, &[]),
                candidate(1.0, &[&[2, 4]]),
                candidate(1.0, &[&[3]]),
                candidate(1.0, &[]),
            ],
            query_roots: vec![QueryOutputCandidates {
                candidates: vec![PhysicalCandidateId(0), PhysicalCandidateId(3)],
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
        let metrics = inventory.prune_for_tests();
        assert_eq!(metrics.removed_candidates, 3);
        assert_eq!(
            inventory.dag.resource_budgets[0].candidate_usage,
            vec![2.0, 3.0, 1.0]
        );
        assert_eq!(inventory.physical.candidates[0].operator_node.0, 2);
        let after = select_candidates(&inventory.dag).unwrap().unwrap();
        assert_eq!(before.modeled_cost, after.modeled_cost);
        assert_eq!(after.modeled_cost, 3.0);
    }

    // Multiple output positions of one query do not satisfy two-reader sharing.
    #[test]
    fn required_reader_pruning_propagates_without_losing_fallback() {
        let mut graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![
                candidate(1.0, &[]),
                candidate(2.0, &[]),
                candidate(1.0, &[&[0]]),
            ],
            query_roots: vec![
                QueryOutputCandidates {
                    candidates: vec![PhysicalCandidateId(1), PhysicalCandidateId(2)]
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
        assert_eq!(usable(&graph), vec![false, true, false]);
        graph.reader_constraints = vec![ReaderConstraint::MinimumQueryReaders {
            candidate: PhysicalCandidateId(0),
            query_outputs: vec![vec![0], vec![1]],
            minimum: 2,
        }];
        assert_eq!(usable(&graph), vec![true, true, true]);
        graph
            .reader_constraints
            .push(ReaderConstraint::AllowedOutputs {
                candidate: PhysicalCandidateId(0),
                outputs: vec![0],
            });
        assert_eq!(usable(&graph), vec![false, true, false]);
    }

    // Missing inputs preserve an uncovered root as infeasible, not an empty workload.
    #[test]
    fn impossible_root_is_not_silently_removed() {
        let mut inventory = configured(DagOfPhysicalCandidatesWithCosts {
            candidates: vec![candidate(1.0, &[&[]])],
            query_roots: vec![QueryOutputCandidates {
                candidates: vec![PhysicalCandidateId(0)],
            }],
            ..Default::default()
        });
        inventory.prune_for_tests();
        assert_eq!(inventory.dag.query_roots.len(), 1);
        assert!(select_candidates(&inventory.dag).unwrap().is_none());
    }

    // Activation implications do not create execution cycles; an unreachable
    // required copy does make the dependent choice unusable.
    #[test]
    fn activation_requirements_preserve_selection_semantics() {
        let mut graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![candidate(1.0, &[]), candidate(2.0, &[])],
            query_roots: vec![
                QueryOutputCandidates {
                    candidates: vec![PhysicalCandidateId(0)],
                },
                QueryOutputCandidates {
                    candidates: vec![PhysicalCandidateId(1)],
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
        assert_eq!(usable(&graph), vec![true, true]);
        graph.query_roots.pop();
        assert_eq!(usable(&graph), vec![false, false]);
    }
}
