//! Bounded execution-scope copies preserve independent producer bindings.
//! Copies are interchangeable, not assigned to disjoint reader sets: one query
//! may consume multiple copies while different subsets share other inputs.
//! Activation-prefix constraints remove permutations of unused copies only.
use crate::search::candidates::configurations::{
    CandidateConfigurationInventory, ConfigurationError,
};
use crate::search::candidates::graph::{
    CandidateDependencyGraph, PhysicalCandidateId, ProducerRequirement, QueryRootCandidates,
    ReaderConstraint,
};
use crate::search::candidates::inventory::CandidateInventory;

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
        let configuration = source.configurations[id];
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
            result.configurations.push(configuration);
            result
                .inventory
                .templates
                .push(source.inventory.templates[id].clone());
            result.inventory.graph.candidates.push(candidate.clone());
        }
    }
    for (id, candidate) in graph.candidates.iter().enumerate() {
        super::check_deadline(deadline)?;
        for &copy in &copies[id] {
            super::check_deadline(deadline)?;
            let mut requirements = Vec::new();
            for input in &candidate.producer_requirements {
                let mut compatible_producers = Vec::new();
                for producer in &input.compatible_producers {
                    super::check_deadline(deadline)?;
                    compatible_producers.extend(copies[producer.0].iter().copied());
                }
                requirements.push(ProducerRequirement {
                    compatible_producers,
                });
            }
            result.inventory.graph.candidates[copy.0].producer_requirements = requirements;
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
    use asap_logical_asap_optimization::memo::MemoGroupId;
    use asap_types::ir::properties::ExecutionTiming;
    use asap_types::physical::Materialization;

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
                            compatible_producers: vec![id(0)],
                        },
                        ProducerRequirement {
                            compatible_producers: vec![id(1)],
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
        let source = CandidateConfigurationInventory {
            inventory: CandidateInventory {
                graph,
                templates: vec![None; 4],
                group_candidates: (0..4).map(|i| vec![id(i)]).collect(),
            },
            configurations: (0..4)
                .map(|i| CandidateConfiguration {
                    source_candidate: id(i),
                    materialization: Materialization::NotMaterialized,
                    timing: ExecutionTiming::QueryTime,
                })
                .collect(),
        };
        let expanded = expand_execution_scopes(source, &[2, 2, 2, 2], &[0, 1, 2, 3]).unwrap();
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
