//! The Stage 3 part of the candidate graph: structural witnesses of rejected
//! selections, checked against the decoded selected graph.
use asap_types::stages::{
    DagOfSelectedCandidates, PhysicalCandidateId, SelectedEdge, SelectedNodeId,
};

/// One output reference's producing candidate in a decoded selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueryRootAssignment {
    /// The output reference (index over the group references of the query roots).
    pub query_root: usize,
    pub candidate: PhysicalCandidateId,
}

/// One selected input in today's engine, which runs each candidate at most
/// once: `producer`'s output is input `requirement` of `consumer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProducerConsumerConnection {
    pub producer: PhysicalCandidateId,
    pub consumer: PhysicalCandidateId,
    /// Index in the consumer's inputs (`producer_candidates_per_input`).
    pub requirement: usize,
}

/// Today's engine's decoded selection, each candidate run at most once,
/// with the cost the MILP modeled for it, which the engine compares with
/// the cost it validates.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SelectedCandidateGraph {
    pub selected_candidates: Vec<PhysicalCandidateId>,
    pub connections: Vec<ProducerConsumerConnection>,
    pub query_roots: Vec<QueryRootAssignment>,
    pub modeled_cost: f64,
}

impl SelectedCandidateGraph {
    /// The candidate producing each output reference, in output order.
    pub(crate) fn output_candidates(&self) -> Vec<PhysicalCandidateId> {
        let mut roots = self.query_roots.clone();
        roots.sort_by_key(|root| root.query_root);
        roots.into_iter().map(|root| root.candidate).collect()
    }

    /// The node running `candidate` in [`Self::to_selected_dag`]: its
    /// position in `selected_candidates`.
    pub(crate) fn node_of(&self, candidate: PhysicalCandidateId) -> Option<SelectedNodeId> {
        self.selected_candidates
            .iter()
            .position(|selected| *selected == candidate)
            .map(SelectedNodeId)
    }

    /// The selection as Stage 2 realizes it: one node per selected
    /// candidate, in `selected_candidates` order.
    pub(crate) fn to_selected_dag(&self) -> DagOfSelectedCandidates {
        let node_of = |candidate: PhysicalCandidateId| {
            self.node_of(candidate)
                .unwrap_or(SelectedNodeId(usize::MAX))
        };
        DagOfSelectedCandidates {
            candidate_of_node: self.selected_candidates.clone(),
            edges: self
                .connections
                .iter()
                .map(|connection| SelectedEdge {
                    producer: node_of(connection.producer),
                    consumer: node_of(connection.consumer),
                    input: connection.requirement,
                })
                .collect(),
            node_of_output_reference: self.output_candidates().into_iter().map(node_of).collect(),
        }
    }

    #[cfg(test)]
    /// `materializations` (indexed by candidate; `Some` for the selected
    /// ones) per node of [`Self::to_selected_dag`].
    pub(crate) fn materializations_by_node(
        &self,
        materializations: &[Option<asap_types::physical::Materialization>],
    ) -> Vec<asap_types::physical::Materialization> {
        self.selected_candidates
            .iter()
            .map(|candidate| {
                materializations[candidate.0]
                    .unwrap_or(asap_types::physical::Materialization::NotMaterialized)
            })
            .collect()
    }
}

/// A certified size-independent failure of a complete query dependency closure.
/// The evaluator certifies semantics; this module verifies the structural witness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RejectedQuerySubgraph {
    pub query_roots: Vec<QueryRootAssignment>,
    pub connections: Vec<ProducerConsumerConnection>,
}

impl RejectedQuerySubgraph {
    pub fn from_query_outputs(
        selected: &SelectedCandidateGraph,
        outputs: &[usize],
    ) -> Result<Self, String> {
        if outputs.is_empty()
            || outputs
                .iter()
                .enumerate()
                .any(|(i, id)| outputs[..i].contains(id))
        {
            return Err("query cut requires distinct, nonempty output references".into());
        }
        let query_roots = outputs
            .iter()
            .map(|output| {
                selected
                    .query_roots
                    .iter()
                    .find(|root| root.query_root == *output)
                    .copied()
                    .ok_or_else(|| format!("query cut references missing output {output}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut pending: Vec<_> = query_roots.iter().map(|root| root.candidate).collect();
        let mut visited = Vec::new();
        let mut connections = Vec::new();
        while let Some(candidate) = pending.pop() {
            if visited.contains(&candidate) {
                continue;
            }
            visited.push(candidate);
            for edge in selected
                .connections
                .iter()
                .filter(|edge| edge.consumer == candidate)
            {
                connections.push(*edge);
                pending.push(edge.producer);
            }
        }
        Ok(Self {
            query_roots,
            connections,
        })
    }

    pub(crate) fn validate(&self, selected: &SelectedCandidateGraph) -> Result<(), String> {
        let outputs: Vec<_> = self
            .query_roots
            .iter()
            .map(|root| root.query_root)
            .collect();
        let expected = Self::from_query_outputs(selected, &outputs)?;
        if self.query_roots != expected.query_roots
            || self.connections.len() != expected.connections.len()
            || self.connections.iter().enumerate().any(|(i, edge)| {
                self.connections[..i].contains(edge) || !expected.connections.contains(edge)
            })
        {
            return Err("query cut must contain the complete selected upstream closure".into());
        }
        Ok(())
    }
}
