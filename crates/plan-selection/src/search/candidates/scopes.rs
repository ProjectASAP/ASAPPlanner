//! Unit tests of Stage 2's execution-scope expansion
//! (`asap_physical_asap_optimization::catalog`) that select with the solver.
use crate::search::candidates::dag::PhysicalCandidatesWithCosts;
use asap_physical_asap_optimization::catalog::test_support::expand_execution_scopes_before;
use asap_physical_asap_optimization::catalog::ConfigurationError;

/// Stage 2's execution-scope expansion of `source`'s candidates (after
/// [`PhysicalCandidatesWithCosts::sync_physical_for_tests`]), with no
/// deadline. Each copy costs what its candidate did: candidate `i` of
/// `source` must run its own operator node `i`, as the test constructors make
/// it, which then names its index in `source`.
#[cfg(test)]
pub(in crate::search) fn expand_execution_scopes(
    mut source: PhysicalCandidatesWithCosts,
    original_uses: &[usize],
    output_contexts: &[usize],
) -> Result<PhysicalCandidatesWithCosts, ConfigurationError> {
    source.sync_physical_for_tests();
    let expanded =
        expand_execution_scopes_before(source.physical, original_uses, output_contexts, None)?;
    let mut configured = PhysicalCandidatesWithCosts::new(expanded);
    for (candidate, physical) in configured
        .dag
        .candidates
        .iter_mut()
        .zip(&configured.physical.candidates)
    {
        candidate.local_cost = source.dag.candidates[physical.operator_node.0].local_cost;
    }
    Ok(configured)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::milp::model::select_candidates;
    use asap_types::ir::properties::ExecutionTiming;
    use asap_types::physical::Materialization;

    use crate::search::candidates::dag::{
        DagOfPhysicalCandidatesWithCosts, InputProducerCandidates, PhysicalCandidateWithCost,
        QueryOutputCandidates,
    };
    use asap_types::stages::{PhysicalCandidateId, ReaderConstraint};

    fn terminal_reader_inventory(separate_producers: bool) -> PhysicalCandidatesWithCosts {
        use asap_logical_asap_optimization::test_support::{cms_summary, estimate_over};
        use asap_types::ir::properties::AccuracyGuarantee;
        use std::rc::Rc;
        let state = Rc::new(cms_summary("count(m)"));
        let reader = estimate_over(state, AccuracyGuarantee::exact("scope fixture"));
        let mut graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![
                PhysicalCandidateWithCost {
                    local_cost: 2.0,
                    producer_candidates_per_input: vec![],
                },
                PhysicalCandidateWithCost {
                    local_cost: 3.0,
                    producer_candidates_per_input: vec![],
                },
                PhysicalCandidateWithCost {
                    local_cost: 1.0,
                    producer_candidates_per_input: vec![InputProducerCandidates {
                        candidates: vec![PhysicalCandidateId(0), PhysicalCandidateId(1)].into(),
                    }],
                },
            ],
            query_roots: vec![
                QueryOutputCandidates {
                    candidates: vec![PhysicalCandidateId(2)]
                };
                2
            ],
            ..Default::default()
        };
        if separate_producers {
            for output in 0..2 {
                graph
                    .reader_constraints
                    .push(ReaderConstraint::AllowedOutputs {
                        candidate: PhysicalCandidateId(output),
                        outputs: vec![output],
                    });
            }
        }
        let placeholder = crate::search::candidates::dag::placeholder_node_for_tests;
        PhysicalCandidatesWithCosts::from_dag_for_tests(
            graph,
            [placeholder(), placeholder(), reader]
                .into_iter()
                .map(|node| {
                    (
                        node,
                        (ExecutionTiming::QueryTime, Materialization::NotMaterialized),
                    )
                })
                .collect(),
        )
    }

    // Fix the producer catalog independently of planning budgets and progressive
    // pane domains. Root-only readers need one optional binding per output;
    // producers stay constant and producer connections grow exactly linearly.
    // Output-to-copy assignment choices are intentionally N*N, not claimed linear.
    #[test]
    fn repeated_outputs_have_constant_producers_and_linear_reader_connections() {
        for output_count in [2, 4, 8] {
            let mut source = terminal_reader_inventory(false);
            source.dag.query_roots = vec![
                QueryOutputCandidates {
                    candidates: vec![PhysicalCandidateId(2)]
                };
                output_count
            ];
            let expanded =
                expand_execution_scopes(source, &[1, 1, 1], &vec![0; output_count]).unwrap();
            let graph = &expanded.dag;
            let producer_count = expanded
                .physical
                .candidates
                .iter()
                .filter(|configuration| configuration.operator_node.0 < 2)
                .count();
            let terminal_count = expanded
                .physical
                .candidates
                .iter()
                .filter(|configuration| configuration.operator_node.0 == 2)
                .count();
            let producer_connections: usize = graph
                .candidates
                .iter()
                .flat_map(|candidate| &candidate.producer_candidates_per_input)
                .map(|input| input.candidates.len())
                .sum();
            assert_eq!(producer_count, 2);
            assert_eq!(terminal_count, output_count);
            assert_eq!(graph.candidates.len(), 2 + output_count);
            assert_eq!(producer_connections, 2 * output_count);
            assert_eq!(
                graph
                    .query_roots
                    .iter()
                    .map(|output| output.candidates.len())
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
        assert!(select_candidates(&source.dag).unwrap().is_none());
        let expanded = expand_execution_scopes(source, &[1, 1, 1], &[0, 0]).unwrap();
        let selected = select_candidates(&expanded.dag)
            .unwrap()
            .expect("each root reference can bind a separate reader");
        assert_ne!(
            selected.query_roots[0].candidate,
            selected.query_roots[1].candidate
        );
        let suppliers = selected
            .connections
            .iter()
            .map(|edge| {
                expanded.physical.candidates[edge.producer.0]
                    .operator_node
                    .0
            })
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(suppliers, std::collections::HashSet::from([0, 1]));
        assert_eq!(selected.modeled_cost, 7.0);

        // Capacity is optional: without conflicting eligibility, both root
        // references may still consume one reader and pay its cost once.
        let shared =
            expand_execution_scopes(terminal_reader_inventory(false), &[1, 1, 1], &[0, 0]).unwrap();
        let selected = select_candidates(&shared.dag).unwrap().unwrap();
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
        source.push_for_tests(
            crate::search::candidates::dag::placeholder_node_for_tests(),
            ExecutionTiming::QueryTime,
            Materialization::NotMaterialized,
            1.0,
            vec![InputProducerCandidates {
                candidates: vec![PhysicalCandidateId(2)].into(),
            }],
        );
        for output in &mut source.dag.query_roots {
            output.candidates = vec![PhysicalCandidateId(3)];
        }
        let expanded = expand_execution_scopes(source, &[1; 4], &[0, 0]).unwrap();
        assert_eq!(
            expanded
                .physical
                .candidates
                .iter()
                .filter(|configuration| configuration.operator_node.0 == 2)
                .count(),
            1
        );
    }

    // The unknown-phase query reads both states; the two known-phase queries
    // share different copies. Another independent state remains shared by all.
    #[test]
    fn overlapping_readers_can_separate_one_state_and_share_another() {
        let id = PhysicalCandidateId;
        let mut graph = DagOfPhysicalCandidatesWithCosts::default();
        for (index, cost) in [3.0, 5.0, 1.0, 1.0].into_iter().enumerate() {
            graph.candidates.push(PhysicalCandidateWithCost {
                local_cost: cost,
                producer_candidates_per_input: if index < 2 {
                    Vec::new()
                } else {
                    vec![
                        InputProducerCandidates {
                            candidates: vec![id(0)].into(),
                        },
                        InputProducerCandidates {
                            candidates: vec![id(1)].into(),
                        },
                    ]
                },
            });
        }
        graph.query_roots = [2, 3, 2, 3]
            .into_iter()
            .map(|candidate| QueryOutputCandidates {
                candidates: vec![id(candidate)],
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
        let source = PhysicalCandidatesWithCosts::from_dag_for_tests(
            graph,
            (0..4)
                .map(|_| {
                    (
                        crate::search::candidates::dag::placeholder_node_for_tests(),
                        (ExecutionTiming::QueryTime, Materialization::NotMaterialized),
                    )
                })
                .collect(),
        );
        let expanded = expand_execution_scopes(source, &[2, 2, 2, 2], &[0, 1, 2, 3]).unwrap();
        // Copy expansion must preserve every slot's complete ordered producer
        // mapping even though repeated copies share the backing allocation.
        for (id, configuration) in expanded.physical.candidates.iter().enumerate() {
            let original = &original_graph.candidates[configuration.operator_node.0];
            let copied = &expanded.dag.candidates[id];
            assert_eq!(
                original.producer_candidates_per_input.len(),
                copied.producer_candidates_per_input.len()
            );
            for (input, copied_input) in original
                .producer_candidates_per_input
                .iter()
                .zip(&copied.producer_candidates_per_input)
            {
                let expected: Vec<_> = input
                    .candidates
                    .iter()
                    .flat_map(|producer| {
                        expanded.physical.candidates.iter().enumerate().filter_map(
                            move |(id, configuration)| {
                                (configuration.operator_node.0 == producer.0)
                                    .then_some(PhysicalCandidateId(id))
                            },
                        )
                    })
                    .collect();
                assert_eq!(copied_input.candidates.as_slice(), expected.as_slice());
            }
        }
        let metrics = asap_types::stages::producer_storage_metrics(&expanded.physical);
        assert!(
            metrics.stored_producer_ids < metrics.logical_connections,
            "{metrics:?}"
        );
        let selected = select_candidates(&expanded.dag).unwrap().unwrap();
        assert_eq!(selected.modeled_cost, 13.0);
        let uses = |source| {
            selected
                .selected_candidates
                .iter()
                .filter(|id| expanded.physical.candidates[id.0].operator_node.0 == source)
                .count()
        };
        assert_eq!(uses(0), 2);
        assert_eq!(uses(1), 1);
        assert_eq!(uses(2), 1);
        assert_eq!(uses(3), 1);
        for constraint in &expanded.dag.reader_constraints {
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
