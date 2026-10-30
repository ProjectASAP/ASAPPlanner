//! Unit tests of Stage 2's execution-scope expansion
//! (`asap_physical_asap_optimization::catalog`) that select with the solver.
use asap_physical_asap_optimization::catalog::test_support::expand_execution_scopes_before;
use asap_physical_asap_optimization::catalog::ConfigurationError;
use asap_types::stages::{
    CandidateConfigurationInventory, CandidateDependencyGraph, CandidateInventory,
    PhysicalCandidateId, ProducerRequirement, QueryRootCandidates, ReaderConstraint,
};

#[cfg(test)]
pub(in crate::search) fn expand_execution_scopes(
    source: CandidateConfigurationInventory,
    original_uses: &[usize],
    output_contexts: &[usize],
) -> Result<CandidateConfigurationInventory, ConfigurationError> {
    expand_execution_scopes_before(source, original_uses, output_contexts, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::milp::model::select_candidates;
    use asap_types::ir::properties::ExecutionTiming;
    use asap_types::physical::Materialization;
    use asap_types::stages::CandidateConfiguration;
    use asap_types::stages::MemoGroupId;
    use asap_types::stages::PhysicalCandidate;

    fn terminal_reader_inventory(separate_suppliers: bool) -> CandidateConfigurationInventory {
        use asap_logical_asap_optimization::test_support::{cms_summary, estimate_over};
        use asap_types::ir::properties::AccuracyGuarantee;
        use std::rc::Rc;
        let state = Rc::new(cms_summary("count(m)"));
        let reader = estimate_over(state, AccuracyGuarantee::exact("scope fixture"));
        let mut graph = CandidateDependencyGraph {
            candidates: vec![
                PhysicalCandidate {
                    group: MemoGroupId(0),
                    local_cost: 2.0,
                    producer_requirements: vec![],
                },
                PhysicalCandidate {
                    group: MemoGroupId(0),
                    local_cost: 3.0,
                    producer_requirements: vec![],
                },
                PhysicalCandidate {
                    group: MemoGroupId(1),
                    local_cost: 1.0,
                    producer_requirements: vec![ProducerRequirement {
                        compatible_producers: vec![PhysicalCandidateId(0), PhysicalCandidateId(1)]
                            .into(),
                    }],
                },
            ],
            query_roots: vec![
                QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(2)]
                };
                2
            ],
            ..Default::default()
        };
        if separate_suppliers {
            for output in 0..2 {
                graph
                    .reader_constraints
                    .push(ReaderConstraint::AllowedOutputs {
                        candidate: PhysicalCandidateId(output),
                        outputs: vec![output],
                    });
            }
        }
        CandidateConfigurationInventory {
            inventory: CandidateInventory {
                graph,
                templates: vec![None, None, Some(reader)],
                group_candidates: vec![
                    vec![PhysicalCandidateId(0), PhysicalCandidateId(1)],
                    vec![PhysicalCandidateId(2)],
                ],
            },
            configurations: (0..3)
                .map(|id| CandidateConfiguration {
                    may_keep_with_sole_reader: Vec::new(),
                    source_candidate: PhysicalCandidateId(id),
                    materialization: Materialization::NotMaterialized,
                    timing: ExecutionTiming::QueryTime,
                })
                .collect(),
        }
    }

    // Fix the producer catalog independently of planning budgets and progressive
    // pane domains. Root-only readers need one optional binding per output;
    // producers stay constant and producer connections grow exactly linearly.
    // Output-to-copy assignment choices are intentionally N*N, not claimed linear.
    #[test]
    fn repeated_outputs_have_constant_producers_and_linear_reader_connections() {
        for output_count in [2, 4, 8] {
            let mut source = terminal_reader_inventory(false);
            source.inventory.graph.query_roots = vec![
                QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(2)]
                };
                output_count
            ];
            let expanded =
                expand_execution_scopes(source, &[1, 1, 1], &vec![0; output_count]).unwrap();
            let graph = &expanded.inventory.graph;
            let producer_count = expanded
                .configurations
                .iter()
                .filter(|configuration| configuration.source_candidate.0 < 2)
                .count();
            let terminal_count = expanded
                .configurations
                .iter()
                .filter(|configuration| configuration.source_candidate == PhysicalCandidateId(2))
                .count();
            let producer_connections: usize = graph
                .candidates
                .iter()
                .flat_map(|candidate| &candidate.producer_requirements)
                .map(|input| input.compatible_producers.len())
                .sum();
            assert_eq!(producer_count, 2);
            assert_eq!(terminal_count, output_count);
            assert_eq!(graph.candidates.len(), 2 + output_count);
            assert_eq!(producer_connections, 2 * output_count);
            assert_eq!(
                graph
                    .query_roots
                    .iter()
                    .map(|output| output.compatible_candidates.len())
                    .sum::<usize>(),
                output_count * output_count
            );
            let selected = select_candidates(graph).unwrap().unwrap();
            assert_eq!(selected.query_roots.len(), output_count);
            assert!(selected
                .query_roots
                .iter()
                .all(|output| output.candidate == selected.query_roots[0].candidate));
            assert_eq!(
                selected.modeled_cost, 3.0,
                "additional capacity does not force duplicate work"
            );
        }
    }

    // Terminal-reader factoring must preserve independent bindings for two
    // output references of the same scalar query, not only distinct queries.
    #[test]
    fn terminal_reader_copies_preserve_same_query_independent_bindings() {
        let source = terminal_reader_inventory(true);
        assert!(select_candidates(&source.inventory.graph)
            .unwrap()
            .is_none());
        let expanded = expand_execution_scopes(source, &[1, 1, 1], &[0, 0]).unwrap();
        let selected = select_candidates(&expanded.inventory.graph)
            .unwrap()
            .expect("each root reference can bind a separate reader");
        assert_ne!(
            selected.query_roots[0].candidate,
            selected.query_roots[1].candidate
        );
        let suppliers = selected
            .connections
            .iter()
            .map(|edge| expanded.configurations[edge.producer.0].source_candidate)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            suppliers,
            std::collections::HashSet::from([PhysicalCandidateId(0), PhysicalCandidateId(1)])
        );
        assert_eq!(selected.modeled_cost, 7.0);

        // Capacity is optional: without conflicting eligibility, both root
        // references may still consume one reader and pay its cost once.
        let shared =
            expand_execution_scopes(terminal_reader_inventory(false), &[1, 1, 1], &[0, 0]).unwrap();
        let selected = select_candidates(&shared.inventory.graph).unwrap().unwrap();
        assert_eq!(
            selected.query_roots[0].candidate,
            selected.query_roots[1].candidate
        );
        assert_eq!(selected.modeled_cost, 3.0);
    }

    // The terminal-readout bound does not alter capacity for a reader that
    // supplies another candidate; that requires a separate dependency proof.
    #[test]
    fn terminal_capacity_rule_leaves_internal_readers_unchanged() {
        let mut source = terminal_reader_inventory(false);
        source.inventory.graph.candidates.push(PhysicalCandidate {
            group: MemoGroupId(2),
            local_cost: 1.0,
            producer_requirements: vec![ProducerRequirement {
                compatible_producers: vec![PhysicalCandidateId(2)].into(),
            }],
        });
        source.inventory.templates.push(None);
        source
            .inventory
            .group_candidates
            .push(vec![PhysicalCandidateId(3)]);
        source.configurations.push(CandidateConfiguration {
            may_keep_with_sole_reader: Vec::new(),
            source_candidate: PhysicalCandidateId(3),
            materialization: Materialization::NotMaterialized,
            timing: ExecutionTiming::QueryTime,
        });
        for output in &mut source.inventory.graph.query_roots {
            output.compatible_candidates = vec![PhysicalCandidateId(3)];
        }
        let expanded = expand_execution_scopes(source, &[1; 4], &[0, 0]).unwrap();
        assert_eq!(
            expanded
                .configurations
                .iter()
                .filter(|configuration| configuration.source_candidate == PhysicalCandidateId(2))
                .count(),
            1
        );
    }

    // The unknown-phase query reads both states; the two known-phase queries
    // share different copies. Another independent state remains shared by all.
    #[test]
    fn overlapping_readers_can_separate_one_state_and_share_another() {
        let id = PhysicalCandidateId;
        let mut graph = CandidateDependencyGraph::default();
        for (index, cost) in [3.0, 5.0, 1.0, 1.0].into_iter().enumerate() {
            graph.candidates.push(PhysicalCandidate {
                group: MemoGroupId(index as u32),
                local_cost: cost,
                producer_requirements: if index < 2 {
                    Vec::new()
                } else {
                    vec![
                        ProducerRequirement {
                            compatible_producers: vec![id(0)].into(),
                        },
                        ProducerRequirement {
                            compatible_producers: vec![id(1)].into(),
                        },
                    ]
                },
            });
        }
        graph.query_roots = [2, 3, 2, 3]
            .into_iter()
            .map(|candidate| QueryRootCandidates {
                compatible_candidates: vec![id(candidate)],
            })
            .collect();
        graph
            .reader_constraints
            .push(ReaderConstraint::IncompatibleOutputs {
                candidate: id(0),
                left: 2,
                right: 3,
            });
        assert!(select_candidates(&graph).unwrap().is_none());
        let original_graph = graph.clone();
        let source = CandidateConfigurationInventory {
            inventory: CandidateInventory {
                graph,
                templates: vec![None; 4],
                group_candidates: (0..4).map(|i| vec![id(i)]).collect(),
            },
            configurations: (0..4)
                .map(|i| CandidateConfiguration {
                    may_keep_with_sole_reader: Vec::new(),
                    source_candidate: id(i),
                    materialization: Materialization::NotMaterialized,
                    timing: ExecutionTiming::QueryTime,
                })
                .collect(),
        };
        let expanded = expand_execution_scopes(source, &[2, 2, 2, 2], &[0, 1, 2, 3]).unwrap();
        // Copy expansion must preserve every slot's complete ordered supplier
        // mapping even though repeated copies share the backing allocation.
        for (id, configuration) in expanded.configurations.iter().enumerate() {
            let original = &original_graph.candidates[configuration.source_candidate.0];
            let copied = &expanded.inventory.graph.candidates[id];
            assert_eq!(
                original.producer_requirements.len(),
                copied.producer_requirements.len()
            );
            for (input, copied_input) in original
                .producer_requirements
                .iter()
                .zip(&copied.producer_requirements)
            {
                let expected: Vec<_> = input
                    .compatible_producers
                    .iter()
                    .flat_map(|producer| {
                        expanded.configurations.iter().enumerate().filter_map(
                            move |(id, configuration)| {
                                (configuration.source_candidate == *producer)
                                    .then_some(PhysicalCandidateId(id))
                            },
                        )
                    })
                    .collect();
                assert_eq!(
                    copied_input.compatible_producers.as_slice(),
                    expected.as_slice()
                );
            }
        }
        let metrics = asap_types::stages::supplier_storage_metrics(&expanded.inventory.graph);
        assert!(
            metrics.stored_supplier_ids < metrics.logical_connections,
            "{metrics:?}"
        );
        let selected = select_candidates(&expanded.inventory.graph)
            .unwrap()
            .unwrap();
        assert_eq!(selected.modeled_cost, 13.0);
        let uses = |source| {
            selected
                .selected_candidates
                .iter()
                .filter(|id| {
                    expanded.configurations[id.0].source_candidate == PhysicalCandidateId(source)
                })
                .count()
        };
        assert_eq!(uses(0), 2);
        assert_eq!(uses(1), 1);
        assert_eq!(uses(2), 1);
        assert_eq!(uses(3), 1);
        for constraint in &expanded.inventory.graph.reader_constraints {
            if let ReaderConstraint::ActivationRequires {
                candidate,
                required_candidate,
            } = constraint
            {
                assert!(
                    !selected.selected_candidates.contains(candidate)
                        || selected.selected_candidates.contains(required_candidate)
                );
            }
        }
    }
}
