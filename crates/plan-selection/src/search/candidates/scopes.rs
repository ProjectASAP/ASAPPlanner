//! Today's engine's execution-scope copies (#1095 §2.0, "Stage 2 emits no
//! copies"): the engine's MILP runs each candidate at most once, so before it
//! solves, Stage 3 copies a candidate once per execution scope that may need
//! its own run (was Stage 2's `catalog/scopes.rs`), and decodes each selected
//! copy into a node of the [`DagOfSelectedCandidates`] over Stage 2's
//! candidates. Copies are interchangeable, not assigned to disjoint reader
//! sets: one query may consume several copies while different subsets share
//! other inputs. Activation-order constraints remove permutations of unused
//! copies only. Deleted with the engine.
use std::collections::{HashMap, HashSet};

use asap_types::ir::properties::ExecutionTiming;
use asap_types::physical::Materialization;
use asap_types::stages::{
    output_references, DagOfSelectedCandidates, PhysicalASAPMemoCandidates, PhysicalCandidateId,
    ProducerCandidates, ProducerSetInterner,
};
use asap_types::workload::QueryWorkloadEntry;

use crate::search::candidates::dag::{PhysicalCandidatesWithCosts, ReaderConstraint};
use crate::search::candidates::graph::SelectedCandidateGraph;

/// Stage 2's candidates with the execution-scope copies the engine selects
/// over.
#[derive(Debug, Clone)]
pub(crate) struct ExpandedCandidates {
    /// The copies, each a candidate of its own, with Stage 2's reader
    /// constraints on every copy and the copies' activation order in the
    /// DAG's reader constraints.
    pub configured: PhysicalCandidatesWithCosts,
    /// Indexed by copy: the Stage 2 candidate it runs.
    pub candidate_of_copy: Vec<PhysicalCandidateId>,
}

impl ExpandedCandidates {
    /// `selected` (over the copies) as a DAG over Stage 2's candidates: one
    /// node per selected copy, in `selected_candidates` order.
    pub(crate) fn decode(&self, selected: &SelectedCandidateGraph) -> DagOfSelectedCandidates {
        let dag = selected.to_selected_dag();
        DagOfSelectedCandidates {
            candidate_of_node: dag
                .candidate_of_node
                .iter()
                .map(|copy| self.candidate_of_copy[copy.0])
                .collect(),
            edges: dag.edges,
            node_of_output_reference: dag.node_of_output_reference,
        }
    }
}

/// `physical`'s candidates with their execution-scope copies, sized as
/// Stage 2 sized them: per candidate, the larger of the distinct output
/// contexts reaching it (an output reference's workload entry and root
/// operator nodes; identical outputs share one) and the distinct
/// (consumer memo group, input) uses of its operator node, or one copy when
/// at most one context reaches it or it is a shared ingestion-time leaf;
/// a terminal unary summary reader gets at least one copy per output
/// reference reaching it.
pub(crate) fn expand_execution_scopes(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    deadline: Option<std::time::Instant>,
) -> Result<ExpandedCandidates, String> {
    let node_count = physical.operator_nodes_of_candidates_for_memo_groups.len();
    let output_queries: Vec<usize> = output_references(&physical.query_roots)
        .into_iter()
        .map(|(query, _)| query)
        .collect();
    let mut context_ids = HashMap::new();
    let mut output_contexts = Vec::new();
    for (output, &query) in output_queries.iter().enumerate() {
        let mut root_nodes: Vec<usize> = physical
            .query_output_candidates(output)
            .map(|id| physical.candidates[id.0].operator_node.0)
            .collect();
        root_nodes.sort_unstable();
        root_nodes.dedup();
        let entry = entries
            .get(query)
            .ok_or("execution-scope output contexts are incomplete")?;
        let key = serde_json::to_string(&(entry, root_nodes)).map_err(|e| e.to_string())?;
        let next = context_ids.len();
        output_contexts.push(*context_ids.entry(key).or_insert(next));
    }
    let mut uses = vec![HashSet::new(); node_count];
    for candidate in &physical.candidates {
        let group = physical.operator_nodes_of_candidates_for_memo_groups
            [candidate.operator_node.0]
            .group();
        for (input, producers) in candidate.producer_candidates_per_input.iter().enumerate() {
            for producer in producers {
                uses[physical.candidates[producer.0].operator_node.0].insert((group, input));
            }
        }
    }
    let mut root_uses = vec![HashSet::new(); node_count];
    for (output, &context) in output_contexts.iter().enumerate() {
        for id in physical.query_output_candidates(output) {
            root_uses[physical.candidates[id.0].operator_node.0].insert(context);
        }
    }
    let original_uses: Vec<usize> = uses
        .iter()
        .zip(&root_uses)
        .map(|(uses, roots)| uses.len() + roots.len())
        .collect();
    expand_with_uses(physical.clone(), &original_uses, &output_contexts, deadline)
}

/// [`expand_execution_scopes`] with the uses of each operator node
/// (`original_uses`) and each output reference's context given.
pub(crate) fn expand_with_uses(
    source: PhysicalASAPMemoCandidates,
    original_uses: &[usize],
    output_contexts: &[usize],
    deadline: Option<std::time::Instant>,
) -> Result<ExpandedCandidates, String> {
    let past = || deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline);
    let timeout = || "execution-scope expansion exceeded the deadline".to_string();
    let count = source.candidates.len();
    let query_output_candidates: Vec<Vec<_>> = (0..source.output_reference_count())
        .map(|output| source.query_output_candidates(output).collect())
        .collect();
    if output_contexts.len() != query_output_candidates.len() {
        return Err("execution-scope output contexts are incomplete".into());
    }
    let mut internally_consumed = vec![false; count];
    for input in source
        .candidates
        .iter()
        .flat_map(|candidate| &candidate.producer_candidates_per_input)
    {
        for producer in input {
            internally_consumed[producer.0] = true;
        }
    }
    let mut output_uses = vec![HashSet::new(); count];
    let mut output_positions = vec![0usize; count];
    for (output, root) in query_output_candidates.iter().enumerate() {
        if past() {
            return Err(timeout());
        }
        let mut seen = vec![false; count];
        let mut pending = root.clone();
        while let Some(id) = pending.pop() {
            if std::mem::replace(&mut seen[id.0], true) {
                continue;
            }
            output_uses[id.0].insert(output_contexts[output]);
            output_positions[id.0] += 1;
            pending.extend(
                source.candidates[id.0]
                    .producer_candidates_per_input
                    .iter()
                    .flat_map(|input| input.iter().copied()),
            );
        }
    }
    let mut candidates = Vec::new();
    let mut candidate_of_copy = Vec::new();
    let mut activation = Vec::new();
    let mut copies = vec![Vec::new(); count];
    for (id, candidate) in source.candidates.iter().enumerate() {
        if past() {
            return Err(timeout());
        }
        let uses = *original_uses
            .get(candidate.operator_node.0)
            .ok_or("execution-scope source occurrence count is incomplete")?;
        // Keep the original occurrence capacity, including repeated input slots.
        // This is a finite catalog bound, not arbitrary unfolded-DAG enumeration.
        let conflicting_readers = source.reader_constraints.iter().any(|constraint| {
            matches!(constraint,
            asap_types::stages::ReaderConstraint::IncompatibleOutputs { candidate, .. } if candidate.0 == id)
        });
        let shared_ingestion_leaf = candidate.producer_candidates_per_input.is_empty()
            && candidate.timing == ExecutionTiming::IngestionTime
            && candidate.materialization == Materialization::NotMaterialized
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
        let terminal_unary_reader =
            !internally_consumed[id] && candidate.producer_candidates_per_input.len() == 1 && {
                let node = source.physical_node(PhysicalCandidateId(id));
                matches!(
                    node.asap(),
                    Some(
                        asap_types::ir::ASAPOp::SummaryEstimate { .. }
                            | asap_types::ir::ASAPOp::FinalizeExactAccumulator { .. }
                    )
                ) && node.children().len() == 1
            };
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
            let copy = PhysicalCandidateId(candidates.len());
            if let Some(&previous) = copies[id].last() {
                activation.push(ReaderConstraint::ActivationRequires {
                    candidate: copy,
                    required_candidate: previous,
                });
            }
            copies[id].push(copy);
            candidates.push(candidate.clone());
            candidate_of_copy.push(PhysicalCandidateId(id));
        }
    }
    let mut producer_sets = ProducerSetInterner::default();
    let mut input_cache: HashMap<ProducerCandidates, ProducerCandidates> = HashMap::new();
    for (id, candidate) in source.candidates.iter().enumerate() {
        if past() {
            return Err(timeout());
        }
        let mut inputs = Vec::new();
        for input in &candidate.producer_candidates_per_input {
            let compatible_producers = if let Some(producer_candidates) = input_cache.get(input) {
                producer_candidates.clone()
            } else {
                let compatible: Vec<_> = input
                    .iter()
                    .flat_map(|producer| copies[producer.0].iter().copied())
                    .collect();
                let producer_candidates = producer_sets.intern(compatible);
                input_cache.insert(input.clone(), producer_candidates.clone());
                producer_candidates
            };
            inputs.push(compatible_producers);
        }
        // Consumer copies retain distinct input slots and MILP edges; only
        // their immutable compatible-producer storage is shared.
        for &copy in &copies[id] {
            candidates[copy.0].producer_candidates_per_input = inputs.clone();
        }
    }
    let mut reader_constraints = Vec::new();
    for constraint in &source.reader_constraints {
        for &copy in &copies[constraint.candidate().0] {
            let mut constraint = constraint.clone();
            *constraint.candidate_mut() = copy;
            reader_constraints.push(constraint);
        }
    }
    let mut expanded = PhysicalASAPMemoCandidates::new(
        source.operator_nodes_of_candidates_for_memo_groups,
        candidates,
        source.query_roots,
        reader_constraints,
    );
    expanded.pruned_candidates = source.pruned_candidates;
    let mut configured = PhysicalCandidatesWithCosts::new(expanded);
    configured.dag.reader_constraints.extend(activation);
    Ok(ExpandedCandidates {
        configured,
        candidate_of_copy,
    })
}

/// [`expand_with_uses`] of `source`'s candidates (after
/// [`PhysicalCandidatesWithCosts::sync_physical_for_tests`]). Each copy
/// costs what its candidate did.
#[cfg(test)]
pub(in crate::search) fn expand_execution_scopes_for_tests(
    mut source: PhysicalCandidatesWithCosts,
    original_uses: &[usize],
    output_contexts: &[usize],
) -> Result<PhysicalCandidatesWithCosts, String> {
    source.sync_physical_for_tests();
    let expanded = expand_with_uses(source.physical, original_uses, output_contexts, None)?;
    let mut configured = expanded.configured;
    for (candidate, original) in configured
        .dag
        .candidates
        .iter_mut()
        .zip(&expanded.candidate_of_copy)
    {
        candidate.local_cost = source.dag.candidates[original.0].local_cost;
    }
    Ok(configured)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::milp::model::select_candidates;
    use asap_types::ir::properties::ExecutionTiming;
    use asap_types::physical::Materialization;

    use crate::search::candidates::dag::ReaderConstraint;
    use crate::search::candidates::dag::{
        DagOfPhysicalCandidatesWithCosts, InputProducerCandidates, PhysicalCandidateWithCost,
        QueryOutputCandidates,
    };
    use asap_types::stages::PhysicalCandidateId;

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
                expand_execution_scopes_for_tests(source, &[1, 1, 1], &vec![0; output_count])
                    .unwrap();
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
        let expanded = expand_execution_scopes_for_tests(source, &[1, 1, 1], &[0, 0]).unwrap();
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
        let shared = expand_execution_scopes_for_tests(
            terminal_reader_inventory(false),
            &[1, 1, 1],
            &[0, 0],
        )
        .unwrap();
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
        let expanded = expand_execution_scopes_for_tests(source, &[1; 4], &[0, 0]).unwrap();
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
        let expanded =
            expand_execution_scopes_for_tests(source, &[2, 2, 2, 2], &[0, 1, 2, 3]).unwrap();
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

    // A selection over the copies decodes into a DAG over Stage 2's
    // candidates that Stage 2's validation accepts: the state read by the
    // two incompatible outputs runs as two nodes of one candidate.
    #[test]
    fn selected_copies_decode_into_nodes_of_stage2_candidates() {
        let id = PhysicalCandidateId;
        let input = |producer| InputProducerCandidates {
            candidates: vec![id(producer)].into(),
        };
        let reader = |cost| PhysicalCandidateWithCost {
            local_cost: cost,
            producer_candidates_per_input: vec![input(0), input(1)],
        };
        let leaf = |cost| PhysicalCandidateWithCost {
            local_cost: cost,
            producer_candidates_per_input: Vec::new(),
        };
        let graph = DagOfPhysicalCandidatesWithCosts {
            candidates: vec![leaf(3.0), leaf(5.0), reader(1.0), reader(1.0)],
            query_roots: [2, 3, 2, 3]
                .into_iter()
                .map(|candidate| QueryOutputCandidates {
                    candidates: vec![id(candidate)],
                })
                .collect(),
            reader_constraints: vec![ReaderConstraint::IncompatibleOutputs {
                candidate: id(0),
                left: 2,
                right: 3,
            }],
            ..Default::default()
        };
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
        let mut expanded =
            expand_with_uses(source.physical.clone(), &[2, 2, 2, 2], &[0, 1, 2, 3], None).unwrap();
        for (candidate, original) in expanded
            .configured
            .dag
            .candidates
            .iter_mut()
            .zip(&expanded.candidate_of_copy)
        {
            candidate.local_cost = source.dag.candidates[original.0].local_cost;
        }
        let selected = select_candidates(&expanded.configured.dag)
            .unwrap()
            .unwrap();
        let dag = expanded.decode(&selected);
        dag.validate(&source.physical).unwrap();
        assert_eq!(
            dag.candidate_of_node
                .iter()
                .filter(|candidate| **candidate == id(0))
                .count(),
            2
        );
        assert_eq!(dag.node_of_output_reference.len(), 4);
        // One node of candidate 0 read by both incompatible outputs is
        // rejected per node.
        let mut shared = dag.clone();
        let nodes_of_0: Vec<usize> = (0..shared.candidate_of_node.len())
            .filter(|&node| shared.candidate_of_node[node] == id(0))
            .collect();
        let (kept, removed) = (nodes_of_0[0], nodes_of_0[1]);
        let renumber = |node: usize| match node {
            node if node == removed => kept - usize::from(kept > removed),
            node => node - usize::from(node > removed),
        };
        shared.candidate_of_node.remove(removed);
        for edge in &mut shared.edges {
            edge.producer.0 = renumber(edge.producer.0);
            edge.consumer.0 = renumber(edge.consumer.0);
        }
        for node in &mut shared.node_of_output_reference {
            node.0 = renumber(node.0);
        }
        assert_eq!(
            shared.validate(&source.physical),
            Err("decoded selection violates selected-reader compatibility".to_string())
        );
    }
}
