//! Bounded execution-scope copies preserve independent producer bindings.
//! Copies are interchangeable, not assigned to disjoint reader sets: one query
//! may consume multiple copies while different subsets share other inputs.
//! Activation-prefix constraints remove permutations of unused copies only.
use super::configurations::ConfigurationError;
use asap_types::stages::{
    CandidateConfigurationInventory, CandidateDependencyGraph, CandidateInventory,
    PhysicalCandidateId, ProducerRequirement, QueryRootCandidates, ReaderConstraint,
};
use asap_types::stages::{SupplierCandidates, SupplierSetInterner};
use std::collections::HashMap;

pub fn expand_execution_scopes_before(
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
