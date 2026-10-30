//! Bounded execution-scope copies preserve independent producer bindings.
//! Copies are interchangeable, not assigned to disjoint reader sets: one query
//! may consume multiple copies while different subsets share other inputs.
//! Activation-prefix constraints remove permutations of unused copies only.
use super::configurations::ConfigurationError;
use asap_types::stages::{PhysicalASAPMemoCandidates, PhysicalCandidateId, ReaderConstraint};
use asap_types::stages::{SupplierCandidates, SupplierSetInterner};
use std::collections::HashMap;

pub fn expand_execution_scopes_before(
    source: PhysicalASAPMemoCandidates,
    original_uses: &[usize],
    output_contexts: &[usize],
    deadline: Option<std::time::Instant>,
) -> Result<PhysicalASAPMemoCandidates, ConfigurationError> {
    super::check_deadline(deadline)?;
    super::validate_candidates(&source).map_err(ConfigurationError::InvalidInventory)?;
    let count = source.candidates.len();
    let query_output_candidates = super::query_output_candidates(&source);
    if output_contexts.len() != query_output_candidates.len() {
        return Err(ConfigurationError::InvalidInventory(
            "execution-scope output contexts are incomplete".into(),
        ));
    }
    let mut internally_consumed = vec![false; count];
    for input in source
        .candidates
        .iter()
        .flat_map(|candidate| &candidate.supplier_candidates_per_input)
    {
        super::check_deadline(deadline)?;
        for producer in input {
            super::check_deadline(deadline)?;
            internally_consumed[producer.0] = true;
        }
    }
    let mut output_uses = vec![std::collections::HashSet::new(); count];
    let mut output_positions = vec![0usize; count];
    for (output, root) in query_output_candidates.iter().enumerate() {
        super::check_deadline(deadline)?;
        let mut seen = vec![false; count];
        let mut pending = root.clone();
        while let Some(id) = pending.pop() {
            super::check_deadline(deadline)?;
            if std::mem::replace(&mut seen[id.0], true) {
                continue;
            }
            output_uses[id.0].insert(output_contexts[output]);
            output_positions[id.0] += 1;
            pending.extend(
                source.candidates[id.0]
                    .supplier_candidates_per_input
                    .iter()
                    .flat_map(|input| input.iter().copied()),
            );
        }
    }
    let mut candidates = Vec::new();
    let mut reader_constraints = Vec::new();
    let mut copies = vec![Vec::new(); count];
    for (id, candidate) in source.candidates.iter().enumerate() {
        super::check_deadline(deadline)?;
        let uses = *original_uses
            .get(candidate.operator_node.0)
            .ok_or_else(|| {
                ConfigurationError::InvalidInventory(
                    "execution-scope source occurrence count is incomplete".into(),
                )
            })?;
        // Keep the original occurrence capacity, including repeated input slots.
        // This is a finite catalog bound, not arbitrary unfolded-DAG enumeration.
        let conflicting_readers = source.reader_constraints.iter().any(|constraint| {
            matches!(constraint,
            ReaderConstraint::IncompatibleOutputs { candidate, .. } if candidate.0 == id)
        });
        let shared_ingestion_leaf = candidate.supplier_candidates_per_input.is_empty()
            && candidate.timing == asap_types::ir::properties::ExecutionTiming::IngestionTime
            && candidate.materialization == asap_types::physical::Materialization::NotMaterialized
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
            !internally_consumed[id] && candidate.supplier_candidates_per_input.len() == 1 && {
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
                reader_constraints.push(ReaderConstraint::ActivationRequires {
                    candidate: copy,
                    required_candidate: previous,
                });
            }
            copies[id].push(copy);
            candidates.push(candidate.clone());
        }
    }
    let mut supplier_sets = SupplierSetInterner::default();
    let mut input_cache: HashMap<SupplierCandidates, SupplierCandidates> = HashMap::new();
    for (id, candidate) in source.candidates.iter().enumerate() {
        super::check_deadline(deadline)?;
        let mut requirements = Vec::new();
        for input in &candidate.supplier_candidates_per_input {
            let compatible_producers = if let Some(suppliers) = input_cache.get(input) {
                suppliers.clone()
            } else {
                let mut compatible = Vec::new();
                for producer in input {
                    super::check_deadline(deadline)?;
                    compatible.extend(copies[producer.0].iter().copied());
                }
                let suppliers = supplier_sets.intern(compatible);
                input_cache.insert(input.clone(), suppliers.clone());
                suppliers
            };
            requirements.push(compatible_producers);
        }
        // Consumer copies retain distinct requirement slots and MILP edges;
        // only their immutable compatible-supplier storage is shared.
        for &copy in &copies[id] {
            super::check_deadline(deadline)?;
            candidates[copy.0].supplier_candidates_per_input = requirements.clone();
        }
    }
    for constraint in &source.reader_constraints {
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
            reader_constraints.push(match constraint {
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
                ReaderConstraint::HighestSharedQueryMaterialization { query_outputs, .. } => {
                    ReaderConstraint::HighestSharedQueryMaterialization {
                        candidate,
                        query_outputs: query_outputs.clone(),
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
    let result = PhysicalASAPMemoCandidates::new(
        source.operator_nodes_of_candidates_for_memo_groups,
        candidates,
        source.query_roots,
        reader_constraints,
    );
    super::validate_candidates(&result).map_err(ConfigurationError::InvalidInventory)?;
    super::check_deadline(deadline)?;
    Ok(result)
}
