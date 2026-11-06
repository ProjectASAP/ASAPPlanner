//! Selections over Stage 2's physical candidates, and their structural
//! validation.
use super::memo_candidates::PhysicalASAPMemoCandidates;
use std::collections::HashSet;

/// Index into [`PhysicalASAPMemoCandidates::candidates`](super::PhysicalASAPMemoCandidates::candidates).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PhysicalCandidateId(pub usize);

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
    /// Index in the consumer's inputs: its logical candidate's
    /// [`Operator::children`](crate::ir::operator::Operator::children).
    pub input_index_of_consumer: usize,
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
    /// every node has exactly one edge from a node of a producer on one of
    /// its admissible edges, the graph is acyclic, every node is reached by
    /// some output reference, each output reference comes from a candidate
    /// of its group. Costs and accuracy are Stage 3's to check.
    pub fn validate(&self, physical: &PhysicalASAPMemoCandidates) -> Result<(), String> {
        let memo = &physical.logical.memo;
        let output_groups: Vec<_> = super::output_references(&physical.query_roots)
            .into_iter()
            .map(|(_, group)| group)
            .collect();
        let query_output_candidates: Vec<Vec<PhysicalCandidateId>> = output_groups
            .iter()
            .map(|&group| {
                (0..physical.candidates.len())
                    .map(PhysicalCandidateId)
                    .filter(|id| physical.candidates[id.0].logical.group == group)
                    .collect()
            })
            .collect();
        let query_output_candidates: Vec<&[_]> =
            query_output_candidates.iter().map(Vec::as_slice).collect();
        let edges: Vec<_> = physical.edges().collect();
        self.validate_against(
            physical.candidates.len(),
            |candidate| {
                memo.candidate(physical.candidates[candidate.0].logical)
                    .expression
                    .operator
                    .children()
                    .len()
            },
            |candidate, input_index_of_consumer, producer| {
                physical
                    .admissible_edges_into(candidate, input_index_of_consumer)
                    .iter()
                    .any(|edge| edges[edge.0].1.producer == producer)
            },
            &query_output_candidates,
        )
    }

    /// [`Self::validate`] over a catalog given by its parts:
    /// `candidate_count` candidates; `input_count(c)` inputs of candidate
    /// `c`; `admissible(c, i, u)` whether an admissible edge `(u, c, i)`
    /// exists; `query_output_candidates[r]` the candidates that may produce
    /// output reference `r`.
    fn validate_against(
        &self,
        candidate_count: usize,
        input_count: impl Fn(PhysicalCandidateId) -> usize,
        admissible: impl Fn(PhysicalCandidateId, usize, PhysicalCandidateId) -> bool,
        query_output_candidates: &[&[PhysicalCandidateId]],
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
            let Some(target) = producers[consumer].get_mut(edge.input_index_of_consumer) else {
                return Err(invalid());
            };
            if target.is_some()
                || !admissible(
                    self.candidate_of_node[consumer],
                    edge.input_index_of_consumer,
                    self.candidate_of_node[producer],
                )
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
        Ok(())
    }
}
