//! Bounded execution-scope copies preserve independent producer bindings.
//! Copies are interchangeable, not assigned to disjoint reader sets: one query
//! may consume multiple copies while different subsets share other inputs.
//! Activation-prefix constraints remove permutations of unused copies only.
use super::suppliers::{SupplierCandidates, SupplierSetInterner};
use crate::search::candidates::configurations::{
    CandidateConfigurationInventory, ConfigurationError,
};
use crate::search::candidates::graph::{
    CandidateDependencyGraph, PhysicalCandidateId, ProducerRequirement, QueryRootCandidates,
    ReaderConstraint,
};
use crate::search::candidates::inventory::CandidateInventory;
use std::collections::HashMap;

#[cfg(test)]
pub(in crate::search) fn expand_execution_scopes(
    source: CandidateConfigurationInventory,
    original_uses: &[usize],
    output_contexts: &[usize],
) -> Result<CandidateConfigurationInventory, ConfigurationError> {
    expand_execution_scopes_before(source, original_uses, output_contexts, None)
}

pub(in crate::search) fn expand_execution_scopes_before(
    source: CandidateConfigurationInventory,
    original_uses: &[usize],
    output_contexts: &[usize],
    deadline: Option<std::time::Instant>,
) -> Result<CandidateConfigurationInventory, ConfigurationError> {
    super::check_deadline(deadline)?;
    let graph = &source.inventory.graph;
    graph
        .validate()
        .map_err(|e| ConfigurationError::InvalidInventory(e.to_string()))?;
    if !graph.resource_budgets.is_empty() {
        return Err(ConfigurationError::InvalidInventory(
            "execution scopes must be expanded before resource pricing".into(),
        ));
    }
    let count = graph.candidates.len();
    if output_contexts.len() != graph.query_roots.len() {
        return Err(ConfigurationError::InvalidInventory(
            "execution-scope output contexts are incomplete".into(),
        ));
    }
    let mut internally_consumed = vec![false; count];
    for input in graph
        .candidates
        .iter()
        .flat_map(|candidate| &candidate.producer_requirements)
    {
        super::check_deadline(deadline)?;
        for producer in &input.compatible_producers {
            super::check_deadline(deadline)?;
            internally_consumed[producer.0] = true;
        }
    }
    let mut output_uses = vec![std::collections::HashSet::new(); count];
    let mut output_positions = vec![0usize; count];
    for (output, root) in graph.query_roots.iter().enumerate() {
        super::check_deadline(deadline)?;
        let mut seen = vec![false; count];
        let mut pending = root.compatible_candidates.clone();
        while let Some(id) = pending.pop() {
            super::check_deadline(deadline)?;
            if std::mem::replace(&mut seen[id.0], true) {
                continue;
            }
            output_uses[id.0].insert(output_contexts[output]);
            output_positions[id.0] += 1;
            pending.extend(
                graph.candidates[id.0]
                    .producer_requirements
                    .iter()
                    .flat_map(|input| input.compatible_producers.iter().copied()),
            );
        }
    }
    let mut result = CandidateConfigurationInventory {
        inventory: CandidateInventory {
            graph: CandidateDependencyGraph::default(),
            templates: Vec::new(),
            group_candidates: Vec::new(),
        },
        configurations: Vec::new(),
    };
    let mut copies = vec![Vec::new(); count];
    for (id, candidate) in graph.candidates.iter().enumerate() {
        super::check_deadline(deadline)?;
        let configuration = &source.configurations[id];
        let uses = *original_uses
            .get(configuration.source_candidate.0)
            .ok_or_else(|| {
                ConfigurationError::InvalidInventory(
                    "execution-scope source occurrence count is incomplete".into(),
                )
            })?;
        // Keep the original occurrence capacity, including repeated input slots.
        // This is a finite catalog bound, not arbitrary unfolded-DAG enumeration.
        let conflicting_readers = graph.reader_constraints.iter().any(|constraint| {
            matches!(constraint,
            ReaderConstraint::IncompatibleOutputs { candidate, .. } if candidate.0 == id)
        });
        let shared_ingestion_leaf = candidate.producer_requirements.is_empty()
            && configuration.timing == asap_types::ir::properties::ExecutionTiming::IngestionTime
            && configuration.materialization
                == asap_types::physical::Materialization::NotMaterialized
            && !conflicting_readers;
        let contexts = if conflicting_readers {
            output_positions[id]
        } else {
            output_uses[id].len()
        };
        let multiplicity = if contexts <= 1 || shared_ingestion_leaf {
            1
        } else {
            contexts.max(uses)
        };
        let terminal_unary_reader = !internally_consumed[id]
            && candidate.producer_requirements.len() == 1
            && source
                .inventory
                .templates
                .get(id)
                .and_then(Option::as_ref)
                .is_some_and(|node| {
                    matches!(
                        node.asap(),
                        Some(
                            asap_types::ir::ASAPOp::SummaryEstimate { .. }
                                | asap_types::ir::ASAPOp::FinalizeExactAccumulator { .. }
                        )
                    ) && node.children().len() == 1
                });
        // Factored terminal readouts may need a different binding per output
        // reference, including two references in one scalar query. Every active
        // terminal copy must serve a root, so the reference count supplies a
        // finite sufficient capacity. Copies remain optional and can be shared.
        // Internal consumers retain the original occurrence/context rule.
        let multiplicity = if terminal_unary_reader {
            multiplicity.max(output_positions[id])
        } else {
            multiplicity
        };
        for _ in 0..multiplicity {
            let copy = PhysicalCandidateId(result.configurations.len());
            if let Some(&previous) = copies[id].last() {
                result.inventory.graph.reader_constraints.push(
                    ReaderConstraint::ActivationRequires {
                        candidate: copy,
                        required_candidate: previous,
                    },
                );
            }
            copies[id].push(copy);
            result.configurations.push(configuration.clone());
            result
                .inventory
                .templates
                .push(source.inventory.templates[id].clone());
            result.inventory.graph.candidates.push(candidate.clone());
        }
    }
    let mut supplier_sets = SupplierSetInterner::default();
    let mut input_cache: HashMap<SupplierCandidates, SupplierCandidates> = HashMap::new();
    for (id, candidate) in graph.candidates.iter().enumerate() {
        super::check_deadline(deadline)?;
        let mut requirements = Vec::new();
        for input in &candidate.producer_requirements {
            let compatible_producers =
                if let Some(suppliers) = input_cache.get(&input.compatible_producers) {
                    suppliers.clone()
                } else {
                    let mut compatible = Vec::new();
                    for producer in &input.compatible_producers {
                        super::check_deadline(deadline)?;
                        compatible.extend(copies[producer.0].iter().copied());
                    }
                    let suppliers = supplier_sets.intern(compatible);
                    input_cache.insert(input.compatible_producers.clone(), suppliers.clone());
                    suppliers
                };
            requirements.push(ProducerRequirement {
                compatible_producers,
            });
        }
        // Consumer copies retain distinct requirement slots and MILP edges;
        // only their immutable compatible-supplier storage is shared.
        for &copy in &copies[id] {
            super::check_deadline(deadline)?;
            result.inventory.graph.candidates[copy.0].producer_requirements = requirements.clone();
        }
    }
    result.inventory.graph.query_roots = graph
        .query_roots
        .iter()
        .map(|root| QueryRootCandidates {
            compatible_candidates: root
                .compatible_candidates
                .iter()
                .flat_map(|id| copies[id.0].iter().copied())
                .collect(),
        })
        .collect();
    result.inventory.group_candidates = source
        .inventory
        .group_candidates
        .iter()
        .map(|group| {
            group
                .iter()
                .flat_map(|id| copies[id.0].iter().copied())
                .collect()
        })
        .collect();
    for constraint in &graph.reader_constraints {
        super::check_deadline(deadline)?;
        let owner = match constraint {
            ReaderConstraint::ActivationRequires { .. } => {
                return Err(ConfigurationError::InvalidInventory(
                    "execution scopes must only be expanded once".into(),
                ))
            }
            ReaderConstraint::AllowedOutputs { candidate, .. }
            | ReaderConstraint::IncompatibleOutputs { candidate, .. }
            | ReaderConstraint::HighestSharedQueryMaterialization { candidate, .. }
            | ReaderConstraint::MinimumQueryReaders { candidate, .. } => candidate,
        };
        for &candidate in &copies[owner.0] {
            result
                .inventory
                .graph
                .reader_constraints
                .push(match constraint {
                    ReaderConstraint::ActivationRequires { .. } => unreachable!(),
                    ReaderConstraint::AllowedOutputs { outputs, .. } => {
                        ReaderConstraint::AllowedOutputs {
                            candidate,
                            outputs: outputs.clone(),
                        }
                    }
                    ReaderConstraint::IncompatibleOutputs { left, right, .. } => {
                        ReaderConstraint::IncompatibleOutputs {
                            candidate,
                            left: *left,
                            right: *right,
                        }
                    }
                    ReaderConstraint::HighestSharedQueryMaterialization {
                        query_outputs, ..
                    } => ReaderConstraint::HighestSharedQueryMaterialization {
                        candidate,
                        query_outputs: query_outputs.clone(),
                    },
                    ReaderConstraint::MinimumQueryReaders {
                        query_outputs,
                        minimum,
                        ..
                    } => ReaderConstraint::MinimumQueryReaders {
                        candidate,
                        query_outputs: query_outputs.clone(),
                        minimum: *minimum,
                    },
                });
        }
    }
    result
        .inventory
        .graph
        .validate()
        .map_err(|e| ConfigurationError::InvalidInventory(e.to_string()))?;
    super::check_deadline(deadline)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::candidates::configurations::CandidateConfiguration;
    use crate::search::candidates::graph::PhysicalCandidate;
    use crate::search::milp::model::select_candidates;
    use asap_logical_asap_optimization::pass2::memo::MemoGroupId;
    use asap_types::ir::properties::ExecutionTiming;
    use asap_types::physical::Materialization;

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
        let metrics = super::super::suppliers::supplier_storage_metrics(&expanded.inventory.graph);
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
