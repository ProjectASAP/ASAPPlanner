//! Selections over Stage 2's physical candidates, and their structural
//! validation.
use super::memo_candidates::PhysicalASAPMemoCandidates;
use super::producers::ProducerCandidates;
use std::collections::{HashMap, HashSet};

/// Index into [`PhysicalASAPMemoCandidates::candidates`](super::PhysicalASAPMemoCandidates::candidates).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PhysicalCandidateId(pub usize);

/// A rule on which output references may read a candidate, applied to each
/// node of a [`DagOfSelectedCandidates`] that runs it: the output references
/// that reach the node are its readers. Output references
/// index the group references of the query roots
/// ([`output_references`](super::output_references)); several references of
/// one query stay distinct.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReaderConstraint {
    /// Each node is read by at least `minimum` queries. Each bucket of
    /// `query_outputs` is one query's output references; together they
    /// partition every output exactly once. Empty buckets represent
    /// literal-only queries.
    MinimumQueryReaders {
        candidate: PhysicalCandidateId,
        query_outputs: Vec<Vec<usize>>,
        minimum: usize,
    },
    /// A non-summary query materialization: each node produces an
    /// output reference, or has an immediate selected consumer reached by
    /// at most one query (`query_outputs` as in `MinimumQueryReaders`).
    HighestSharedQueryMaterialization {
        candidate: PhysicalCandidateId,
        query_outputs: Vec<Vec<usize>>,
    },
    /// Only `outputs` may read a node.
    AllowedOutputs {
        candidate: PhysicalCandidateId,
        outputs: Vec<usize>,
    },
    /// No node is read by both `left` and `right`.
    IncompatibleOutputs {
        candidate: PhysicalCandidateId,
        left: usize,
        right: usize,
    },
}

impl ReaderConstraint {
    /// The candidate it constrains.
    pub fn candidate(&self) -> PhysicalCandidateId {
        match self {
            Self::MinimumQueryReaders { candidate, .. }
            | Self::HighestSharedQueryMaterialization { candidate, .. }
            | Self::AllowedOutputs { candidate, .. }
            | Self::IncompatibleOutputs { candidate, .. } => *candidate,
        }
    }

    /// The candidate it constrains, to renumber.
    pub fn candidate_mut(&mut self) -> &mut PhysicalCandidateId {
        match self {
            Self::MinimumQueryReaders { candidate, .. }
            | Self::HighestSharedQueryMaterialization { candidate, .. }
            | Self::AllowedOutputs { candidate, .. }
            | Self::IncompatibleOutputs { candidate, .. } => candidate,
        }
    }
}

/// Index into [`DagOfSelectedCandidates::candidate_of_node`]: one run of a
/// candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SelectedNodeId(pub usize);

/// One edge of a [`DagOfSelectedCandidates`]: `producer`'s output is input
/// `input` of `consumer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedEdge {
    pub producer: SelectedNodeId,
    pub consumer: SelectedNodeId,
    /// Index in the consumer's inputs (its candidate's
    /// `producer_candidates_per_input`).
    pub input: usize,
}

/// The selected plan as a DAG with one node per run of a candidate: what
/// Stage 3 selects and the facade builds through
/// [`AssignmentRealizer::realize`](crate::stages::AssignmentRealizer::realize).
/// A candidate that runs twice (recomputed by two readers, or two shared
/// instances over different inputs) is two nodes.
#[derive(Debug, Clone, PartialEq)]
pub struct DagOfSelectedCandidates {
    /// Node `i` runs candidate `candidate_of_node[i]`.
    pub candidate_of_node: Vec<PhysicalCandidateId>,
    /// For each input of each node, the node that produces it.
    pub edges: Vec<SelectedEdge>,
    /// The node producing each output reference, one per output reference,
    /// in [`output_references`](super::output_references) order.
    pub node_of_output_reference: Vec<SelectedNodeId>,
}

impl DagOfSelectedCandidates {
    /// Structural check against `physical`, instead of trusting rounded
    /// solver values: every node runs an existing candidate, every input of
    /// every node has exactly one edge from a node of an allowed producer
    /// candidate, the graph is acyclic, every node is read by some output
    /// reference, each output reference comes from one of its
    /// `query_output_candidates`, and every node keeps its candidate's
    /// reader constraints. Costs and resource budgets are Stage 3's to
    /// check.
    pub fn validate(&self, physical: &PhysicalASAPMemoCandidates) -> Result<(), String> {
        let query_output_candidates: Vec<Vec<_>> = (0..physical.output_reference_count())
            .map(|output| physical.query_output_candidates(output).collect())
            .collect();
        let query_output_candidates: Vec<&[_]> =
            query_output_candidates.iter().map(Vec::as_slice).collect();
        self.validate_against(
            physical.candidates.len(),
            |candidate| {
                physical.candidates[candidate.0]
                    .producer_candidates_per_input
                    .len()
            },
            |candidate, input| {
                physical.candidates[candidate.0]
                    .producer_candidates_per_input
                    .get(input)
            },
            &query_output_candidates,
            &physical.reader_constraints,
        )
    }

    /// [`Self::validate`] over a catalog given by its parts:
    /// `candidate_count` candidates; `input_count(c)` inputs of candidate
    /// `c`; `producers_of_input(c, i)` the candidates that may produce its
    /// input `i` (`None` past its last input); `query_output_candidates[r]`
    /// the candidates that may produce output reference `r`.
    fn validate_against<'a>(
        &self,
        candidate_count: usize,
        input_count: impl Fn(PhysicalCandidateId) -> usize,
        producers_of_input: impl Fn(PhysicalCandidateId, usize) -> Option<&'a ProducerCandidates>,
        query_output_candidates: &[&[PhysicalCandidateId]],
        reader_constraints: &[ReaderConstraint],
    ) -> Result<(), String> {
        let invalid =
            || "decoded candidate selection violates dependency or root constraints".to_string();
        let count = self.candidate_of_node.len();
        if self
            .candidate_of_node
            .iter()
            .any(|c| c.0 >= candidate_count)
            || self.node_of_output_reference.len() != query_output_candidates.len()
        {
            return Err(invalid());
        }
        for (node, allowed) in self
            .node_of_output_reference
            .iter()
            .zip(query_output_candidates)
        {
            if node.0 >= count || !allowed.contains(&self.candidate_of_node[node.0]) {
                return Err(invalid());
            }
        }
        // Per node, per input: the node producing it.
        let mut producers: Vec<Vec<Option<usize>>> = self
            .candidate_of_node
            .iter()
            .map(|&candidate| vec![None; input_count(candidate)])
            .collect();
        let mut consumers = vec![Vec::new(); count];
        for edge in &self.edges {
            let (producer, consumer) = (edge.producer.0, edge.consumer.0);
            if producer >= count || consumer >= count {
                return Err(invalid());
            }
            let Some(producer_candidates) =
                producers_of_input(self.candidate_of_node[consumer], edge.input)
            else {
                return Err(invalid());
            };
            let Some(target) = producers[consumer].get_mut(edge.input) else {
                return Err(invalid());
            };
            if target.is_some() || !producer_candidates.contains(&self.candidate_of_node[producer])
            {
                return Err(invalid());
            }
            *target = Some(producer);
            consumers[producer].push(consumer);
        }
        if producers.iter().flatten().any(Option::is_none) {
            return Err(invalid());
        }
        // Kahn's algorithm checks cycles without recursion on a potentially deep DAG.
        let mut outstanding: Vec<_> = producers.iter().map(Vec::len).collect();
        let mut ready: Vec<_> = (0..count).filter(|&i| outstanding[i] == 0).collect();
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
        if visited != count {
            return Err(invalid());
        }
        // The nodes each output reference reaches.
        let output_reach: Vec<HashSet<usize>> = self
            .node_of_output_reference
            .iter()
            .map(|root| {
                let mut reached = HashSet::new();
                let mut pending = vec![root.0];
                while let Some(node) = pending.pop() {
                    if reached.insert(node) {
                        pending.extend(producers[node].iter().flatten().copied());
                    }
                }
                reached
            })
            .collect();
        if (0..count).any(|node| !output_reach.iter().any(|reach| reach.contains(&node))) {
            return Err(invalid());
        }
        let queries_reaching = |node: usize, query_outputs: &[Vec<usize>]| {
            query_outputs
                .iter()
                .filter(|outputs| {
                    outputs
                        .iter()
                        .any(|&output| output_reach[output].contains(&node))
                })
                .count()
        };
        let output_nodes: HashSet<usize> = self
            .node_of_output_reference
            .iter()
            .map(|node| node.0)
            .collect();
        let mut nodes_of_candidate: HashMap<PhysicalCandidateId, Vec<usize>> = HashMap::new();
        for (node, candidate) in self.candidate_of_node.iter().enumerate() {
            nodes_of_candidate.entry(*candidate).or_default().push(node);
        }
        for constraint in reader_constraints {
            let Some(nodes) = nodes_of_candidate.get(&constraint.candidate()) else {
                continue;
            };
            let violated = nodes.iter().any(|&node| match constraint {
                ReaderConstraint::MinimumQueryReaders {
                    query_outputs,
                    minimum,
                    ..
                } => queries_reaching(node, query_outputs) < *minimum,
                ReaderConstraint::HighestSharedQueryMaterialization { query_outputs, .. } => {
                    !output_nodes.contains(&node)
                        && !consumers[node]
                            .iter()
                            .any(|&consumer| queries_reaching(consumer, query_outputs) <= 1)
                }
                ReaderConstraint::AllowedOutputs { outputs, .. } => {
                    (0..output_reach.len()).any(|output| {
                        !outputs.contains(&output) && output_reach[output].contains(&node)
                    })
                }
                ReaderConstraint::IncompatibleOutputs { left, right, .. } => {
                    output_reach[*left].contains(&node) && output_reach[*right].contains(&node)
                }
            });
            if violated {
                return Err("decoded selection violates selected-reader compatibility".into());
            }
        }
        Ok(())
    }
}
