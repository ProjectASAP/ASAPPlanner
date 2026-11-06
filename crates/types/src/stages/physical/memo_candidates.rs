//! Stage 2's output: each logical candidate's physical alternatives
//! $\mathcal C$, one per valid $p^{\mathrm{phys}} = (\tau, \ell, \sigma)$, and
//! the physical edges $\mathcal E$ with $\alpha^{\log}$ and
//! $\alpha^{\mathrm{phys}}$ (#1200, #1201). Stage 3 selects among them;
//! Stage 2 prices nothing.

use std::collections::HashMap;

use super::graph::PhysicalCandidateId;
use crate::ir::QueryRoot;
use crate::physical::{EvaluationSchedule, PhysicalProperties};
use crate::stages::{
    CandidateForMemoGroupId, LogicalASAPMemoCandidates, Memo, MemoGroupId, PrunedCandidate,
};

/// Index into [`PhysicalASAPMemoCandidates::edges`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PhysicalEdgeId(pub usize);

/// One physical alternative of a logical candidate: what Stage 3 selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalCandidate {
    /// $e_v$ and $p^{\log}_v$.
    pub logical: CandidateForMemoGroupId,
    /// $p^{\mathrm{phys}}_v$.
    pub properties: PhysicalProperties,
}

impl PhysicalCandidate {
    /// Whether this producer delivers on `times` (the concepts doc's
    /// alignment table, section 2). Reads the grid width from the candidate's
    /// logical window layout in `memo`.
    pub fn delivers_on(&self, memo: &Memo, times: &EvaluationSchedule) -> bool {
        let _ = (memo, times);
        todo!("#1207 PR 1: the alignment table of section 2")
    }
}

/// $\alpha^{\mathrm{phys}}_{u,c,i}$: timing and lifetime (the concepts doc's
/// table, section 2), and, for a materialized consumer, that the producer
/// delivers on the consumer's own schedule. Takes candidates rather than
/// bare properties because a tumbling grid's width lives in the logical
/// window layout.
pub fn satisfies_physical(
    memo: &Memo,
    producer: &PhysicalCandidate,
    consumer: &PhysicalCandidate,
) -> bool {
    let _ = (memo, producer, consumer);
    todo!("#1207 PR 1: the timing x lifetime table and the alignment table of section 2")
}

/// $(u, c, i)$ over physical candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalEdge {
    pub producer: PhysicalCandidateId,
    pub consumer: PhysicalCandidateId,
    pub input_index_of_consumer: usize,
    /// $\alpha^{\log}_{u,c,i}$.
    pub satisfies_logical_requirement: bool,
    /// $\alpha^{\mathrm{phys}}_{u,c,i}$.
    pub satisfies_physical_requirement: bool,
}

/// A physical candidate Stage 2 pruned, named by what it would have been:
/// candidates are renumbered after pruning, so a pruned one has no
/// [`PhysicalCandidateId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalCandidateKey {
    pub logical: CandidateForMemoGroupId,
    pub properties: PhysicalProperties,
}

/// Stage 2's output, which Stage 3 selects from.
#[derive(Debug, Clone)]
pub struct PhysicalASAPMemoCandidates {
    /// Pass 2's output: the memo, its candidates' $e_v$, $p^{\log}_v$,
    /// $r^{\log}_{c,i}$, and the logical edges.
    pub logical: LogicalASAPMemoCandidates,
    /// $\mathcal C$, indexed by [`PhysicalCandidateId`].
    pub candidates: Vec<PhysicalCandidate>,
    /// Each query's root over groups, by index in the workload, passed
    /// through from Pass 2.
    pub query_roots: Vec<QueryRoot<MemoGroupId>>,
    /// The candidates Stage 2 pruned, in pruning order.
    pub pruned_candidates: Vec<PrunedCandidate<PhysicalCandidateKey>>,
    /// $\mathcal E$, grouped by (consumer, input); indexed by
    /// [`PhysicalEdgeId`].
    edges: Vec<PhysicalEdge>,
    /// $P_{c,i}$ after $\alpha$: the admissible edges into each (consumer, input).
    admissible_edges_into_input: HashMap<(PhysicalCandidateId, usize), Vec<PhysicalEdgeId>>,
    /// The admissible edges out of each candidate, by [`PhysicalCandidateId`].
    admissible_edges_out_of_candidate: Vec<Vec<PhysicalEdgeId>>,
}

impl PhysicalASAPMemoCandidates {
    /// Stage 2's output over `candidates` and `edges` (grouped by
    /// (consumer, input)), with the admissible-edge indexes built once.
    pub fn new(
        logical: LogicalASAPMemoCandidates,
        candidates: Vec<PhysicalCandidate>,
        query_roots: Vec<QueryRoot<MemoGroupId>>,
        edges: Vec<PhysicalEdge>,
    ) -> Self {
        let _ = (logical, candidates, query_roots, edges);
        todo!("#1207 PR 5: index the admissible edges")
    }

    /// $\mathcal E$: every edge, in (consumer, input) order.
    pub fn edges(&self) -> impl Iterator<Item = (PhysicalEdgeId, PhysicalEdge)> + '_ {
        self.edges
            .iter()
            .enumerate()
            .map(|(id, edge)| (PhysicalEdgeId(id), *edge))
    }

    /// $P^{\mathrm{adm}}_{c,i}$: the admissible edges into input
    /// `input_index_of_consumer` of `consumer`
    /// ($\alpha^{\log} = \alpha^{\mathrm{phys}} = 1$).
    pub fn admissible_edges_into(
        &self,
        consumer: PhysicalCandidateId,
        input_index_of_consumer: usize,
    ) -> &[PhysicalEdgeId] {
        let _ = (
            consumer,
            input_index_of_consumer,
            &self.admissible_edges_into_input,
        );
        todo!("#1207 PR 5")
    }

    /// The admissible edges out of `producer`: every consumer that may share it.
    pub fn admissible_edges_out_of(&self, producer: PhysicalCandidateId) -> &[PhysicalEdgeId] {
        let _ = (producer, &self.admissible_edges_out_of_candidate);
        todo!("#1207 PR 5")
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
