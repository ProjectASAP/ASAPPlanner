//! Selections over Stage 2's physical candidates, and their structural
//! validation.
use super::producers::ProducerCandidates;
use std::collections::HashSet;

/// Index into [`PhysicalASAPMemoCandidates::candidates`](super::PhysicalASAPMemoCandidates::candidates).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PhysicalCandidateId(pub usize);

/// Legality conditioned on selected downstream outputs, not potential consumers.
/// References index output references; multiple references of one query stay distinct.
#[derive(Debug, Clone)]
pub enum ReaderConstraint {
    /// Symmetry only: interchangeable execution-scope copies activate in order.
    /// Both copies still need genuine consumers; this does not create a DAG edge.
    ActivationRequires {
        candidate: PhysicalCandidateId,
        required_candidate: PhysicalCandidateId,
    },
    /// Each bucket is one query's output references; together they partition
    /// every output exactly once. Empty buckets represent literal-only queries.
    MinimumQueryReaders {
        candidate: PhysicalCandidateId,
        query_outputs: Vec<Vec<usize>>,
        minimum: usize,
    },
    /// A non-summary query materialization must be a root or have an
    /// immediate selected consumer reached by at most one query (Stage 2).
    HighestSharedQueryMaterialization {
        candidate: PhysicalCandidateId,
        query_outputs: Vec<Vec<usize>>,
    },
    AllowedOutputs {
        candidate: PhysicalCandidateId,
        outputs: Vec<usize>,
    },
    IncompatibleOutputs {
        candidate: PhysicalCandidateId,
        left: usize,
        right: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProducerConsumerConnection {
    pub producer: PhysicalCandidateId,
    pub consumer: PhysicalCandidateId,
    /// Index in the consumer's inputs (`producer_candidates_per_input`).
    pub requirement: usize,
}

/// One selection over Stage 2's physical candidates: what Stage 3 selects
/// and the facade builds through [`AssignmentRealizer::realize`](crate::stages::AssignmentRealizer::realize)
/// (#1047; was `SelectedCandidateGraph`, without its `modeled_cost`: Stage 3
/// checks cost itself).
#[derive(Debug, Clone, PartialEq)]
pub struct SelectedCandidates {
    pub candidates: Vec<PhysicalCandidateId>,
    pub connections: Vec<ProducerConsumerConnection>,
    /// The candidate producing each output reference (index over the group
    /// references of the query roots, [`output_references`](super::output_references)),
    /// one per output reference.
    pub query_output_candidates: Vec<PhysicalCandidateId>,
}

/// Check a decoded selection's structure against the connections it may use,
/// instead of trusting rounded solver values: every selected candidate exists
/// once, each output reference has one allowed root, each selected input has
/// exactly one allowed selected producer, the selection is acyclic, reaches
/// only what the roots read, and keeps every reader constraint. Costs and
/// resource budgets are the caller's (Stage 3's) to check.
///
/// `candidate_count` candidates; `input_count(c)` inputs of candidate `c`;
/// `producers_of_input(c, i)` the candidates that may produce its input `i`
/// (`None` past its last input); `query_output_candidates[r]` the candidates
/// that may produce output reference `r`.
pub fn validate_selected_connections<'a>(
    candidate_count: usize,
    input_count: impl Fn(PhysicalCandidateId) -> usize,
    producers_of_input: impl Fn(PhysicalCandidateId, usize) -> Option<&'a ProducerCandidates>,
    query_output_candidates: &[&[PhysicalCandidateId]],
    reader_constraints: &[ReaderConstraint],
    selected: &SelectedCandidates,
) -> Result<(), String> {
    let invalid =
        || "decoded candidate selection violates dependency or root constraints".to_string();
    let active: HashSet<_> = selected.candidates.iter().copied().collect();
    if active.len() != selected.candidates.len()
        || active.iter().any(|c| c.0 >= candidate_count)
        || selected.query_output_candidates.len() != query_output_candidates.len()
    {
        return Err(invalid());
    }
    for (candidate, allowed) in selected
        .query_output_candidates
        .iter()
        .zip(query_output_candidates)
    {
        if !active.contains(candidate) || !allowed.contains(candidate) {
            return Err(invalid());
        }
    }
    let mut producers: Vec<Vec<Option<PhysicalCandidateId>>> = (0..candidate_count)
        .map(|c| vec![None; input_count(PhysicalCandidateId(c))])
        .collect();
    for connection in &selected.connections {
        if !active.contains(&connection.producer) || !active.contains(&connection.consumer) {
            return Err(invalid());
        }
        let Some(producer_candidates) =
            producers_of_input(connection.consumer, connection.requirement)
        else {
            return Err(invalid());
        };
        let target = &mut producers[connection.consumer.0][connection.requirement];
        if target.is_some() || !producer_candidates.contains(&connection.producer) {
            return Err(invalid());
        }
        *target = Some(connection.producer);
    }
    if active
        .iter()
        .any(|c| producers[c.0].iter().any(Option::is_none))
    {
        return Err(invalid());
    }
    // Kahn's algorithm checks cycles without recursion on a potentially deep DAG.
    let mut outstanding: Vec<_> = producers.iter().map(Vec::len).collect();
    let mut consumers = vec![Vec::new(); candidate_count];
    for connection in &selected.connections {
        consumers[connection.producer.0].push(connection.consumer.0);
    }
    let mut ready: Vec<_> = active
        .iter()
        .filter(|c| outstanding[c.0] == 0)
        .map(|c| c.0)
        .collect();
    let mut visited = 0;
    while let Some(producer) = ready.pop() {
        visited += 1;
        for &consumer in &consumers[producer] {
            outstanding[consumer] -= 1;
            if outstanding[consumer] == 0 {
                ready.push(consumer);
            }
        }
    }
    if visited != active.len() {
        return Err(invalid());
    }
    let mut reachable = HashSet::new();
    let mut pending: Vec<_> = selected.query_output_candidates.clone();
    while let Some(candidate) = pending.pop() {
        if reachable.insert(candidate) {
            pending.extend(producers[candidate.0].iter().flatten().copied());
        }
    }
    // Reuse one traversal per output across all reader-compatibility checks.
    let mut output_reach = Vec::new();
    if !reader_constraints.is_empty() {
        output_reach = vec![HashSet::new(); query_output_candidates.len()];
        for (output, &root) in selected.query_output_candidates.iter().enumerate() {
            let reached = &mut output_reach[output];
            let mut pending = vec![root];
            while let Some(candidate) = pending.pop() {
                if reached.insert(candidate) {
                    pending.extend(producers[candidate.0].iter().flatten().copied());
                }
            }
        }
    }
    let output_reaches =
        |output: usize, target: PhysicalCandidateId| output_reach[output].contains(&target);
    for requirement in reader_constraints {
        let violated = match requirement {
            ReaderConstraint::ActivationRequires {
                candidate,
                required_candidate,
            } => active.contains(candidate) && !active.contains(required_candidate),
            ReaderConstraint::MinimumQueryReaders {
                candidate,
                query_outputs,
                minimum,
            } => {
                active.contains(candidate)
                    && query_outputs
                        .iter()
                        .filter(|outputs| {
                            outputs
                                .iter()
                                .any(|&output| output_reaches(output, *candidate))
                        })
                        .count()
                        < *minimum
            }
            ReaderConstraint::HighestSharedQueryMaterialization {
                candidate,
                query_outputs,
            } => {
                active.contains(candidate)
                    && !selected.query_output_candidates.contains(candidate)
                    && !selected.connections.iter().any(|edge| {
                        edge.producer == *candidate
                            && query_outputs
                                .iter()
                                .filter(|outputs| {
                                    outputs
                                        .iter()
                                        .any(|output| output_reaches(*output, edge.consumer))
                                })
                                .count()
                                <= 1
                    })
            }
            ReaderConstraint::AllowedOutputs { candidate, outputs } => (0..query_output_candidates
                .len())
                .any(|output| !outputs.contains(&output) && output_reaches(output, *candidate)),
            ReaderConstraint::IncompatibleOutputs {
                candidate,
                left,
                right,
            } => output_reaches(*left, *candidate) && output_reaches(*right, *candidate),
        };
        if violated {
            return Err("decoded selection violates selected-reader compatibility".into());
        }
    }
    if reachable != active {
        return Err(invalid());
    }
    Ok(())
}
