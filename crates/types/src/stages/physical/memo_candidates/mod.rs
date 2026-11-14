//! Stage 2's output: each logical candidate's physical alternatives
//! $\mathcal C$, one per valid $p^{\mathrm{phys}} = (\tau, \ell)$, and
//! the physical edges $\mathcal E$ with $\alpha^{\log}$ and
//! $\alpha^{\mathrm{phys}}$. Stage 3 selects among them;
//! Stage 2 prices nothing.

use std::collections::HashMap;

use crate::ir::{Operator, QueryRoot};
use crate::stages::{
    LogicalASAPMemoCandidates, MemoGroupId, PhysicalCandidate, PhysicalCandidateId, PhysicalEdge,
    PhysicalEdgeId, PrunedCandidate,
};

/// Stage 2's output, which Stage 3 selects from.
#[derive(Debug, Clone)]
pub struct PhysicalASAPMemoCandidates {
    /// Pass 2's output: the memo, its candidates' $e_v$, $p^{\log}_v$,
    /// $r^{\log}_{c,i}$, and the logical edges.
    pub logical_candidates: LogicalASAPMemoCandidates,
    /// $\mathcal C$, indexed by [`PhysicalCandidateId`].
    pub physical_candidates: Vec<PhysicalCandidate>,
    /// The alternatives Stage 2 pruned, in pruning order, each as the
    /// [`PhysicalCandidate`] it would have been: candidates are numbered
    /// after pruning, so a pruned one has no [`PhysicalCandidateId`].
    pub pruned_candidates: Vec<PrunedCandidate<PhysicalCandidate>>,
    /// $\mathcal E$, grouped by (consumer, input); indexed by
    /// [`PhysicalEdgeId`].
    edges: Vec<PhysicalEdge>,
    /// $P_{c,i}$ after $\alpha$: the admissible edges into each (consumer, input).
    admissible_edges_into_input: HashMap<(PhysicalCandidateId, usize), Vec<PhysicalEdgeId>>,
    /// The admissible edges out of each candidate, by [`PhysicalCandidateId`].
    admissible_edges_out_of_candidate: Vec<Vec<PhysicalEdgeId>>,
}

impl PhysicalASAPMemoCandidates {
    /// Stage 2's output over `physical_candidates` and `edges` (grouped by
    /// (consumer, input)), with the admissible-edge indexes built once.
    /// Query roots are read from `logical_candidates`, not copied.
    pub fn new(
        logical_candidates: LogicalASAPMemoCandidates,
        physical_candidates: Vec<PhysicalCandidate>,
        edges: Vec<PhysicalEdge>,
        pruned_candidates: Vec<PrunedCandidate<PhysicalCandidate>>,
    ) -> Self {
        let mut admissible_edges_into_input: HashMap<_, Vec<PhysicalEdgeId>> = HashMap::new();
        let mut admissible_edges_out_of_candidate = vec![Vec::new(); physical_candidates.len()];
        for (id, edge) in edges.iter().enumerate() {
            if edge.is_logically_and_physically_admissible() {
                admissible_edges_into_input
                    .entry((edge.consumer_candidate_id, edge.input_index_of_consumer))
                    .or_default()
                    .push(PhysicalEdgeId(id));
                admissible_edges_out_of_candidate[edge.producer_candidate_id.0]
                    .push(PhysicalEdgeId(id));
            }
        }
        Self {
            logical_candidates,
            physical_candidates,
            pruned_candidates,
            edges,
            admissible_edges_into_input,
            admissible_edges_out_of_candidate,
        }
    }

    /// $\mathcal E$: every edge, in (consumer, input) order.
    pub fn edges(&self) -> impl Iterator<Item = (PhysicalEdgeId, PhysicalEdge)> + '_ {
        self.edges
            .iter()
            .enumerate()
            .map(|(id, edge)| (PhysicalEdgeId(id), *edge))
    }

    /// Edge `id` of $\mathcal E$.
    pub fn edge(&self, id: PhysicalEdgeId) -> &PhysicalEdge {
        &self.edges[id.0]
    }

    /// The operator of candidate `v`'s logical candidate.
    pub fn operator(&self, v: PhysicalCandidateId) -> &Operator<MemoGroupId> {
        &self
            .logical_candidates
            .memo
            .candidate(self.physical_candidates[v.0].logical_candidate_id)
            .expression
            .operator
    }

    /// The number of inputs of candidate `v`: its logical candidate's
    /// operator's children (e.g. 2 for a binary operator).
    pub fn input_count(&self, v: PhysicalCandidateId) -> usize {
        self.operator(v).children().len()
    }

    /// $P^{\mathrm{adm}}_{c,i}$: the admissible edges into input
    /// `input_index_of_consumer` of `consumer`
    /// ($\alpha^{\log} = \alpha^{\mathrm{phys}} = 1$).
    pub fn admissible_edges_into(
        &self,
        consumer: PhysicalCandidateId,
        input_index_of_consumer: usize,
    ) -> &[PhysicalEdgeId] {
        self.admissible_edges_into_input
            .get(&(consumer, input_index_of_consumer))
            .map_or(&[], Vec::as_slice)
    }

    /// The admissible edges out of `producer`: every consumer that may share it.
    pub fn admissible_edges_out_of(&self, producer: PhysicalCandidateId) -> &[PhysicalEdgeId] {
        &self.admissible_edges_out_of_candidate[producer.0]
    }
}

/// The output references of `query_roots`: each operator reference, in
/// order (a scalar root's in `map_operator_refs` order), with the index of
/// the query whose root it is.
pub fn output_references<C: Clone>(query_roots: &[QueryRoot<C>]) -> Vec<(usize, C)> {
    let mut references = Vec::new();
    for (query, root) in query_roots.iter().enumerate() {
        match root {
            QueryRoot::Operator(reference) => references.push((query, reference.clone())),
            QueryRoot::Scalar(expression) => {
                expression.map_operator_refs(&mut |reference| {
                    references.push((query, reference.clone()))
                });
            }
        }
    }
    references
}

#[cfg(test)]
mod tests;
