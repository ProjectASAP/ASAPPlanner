//! The Stage 3 part of the candidate graph: structural witnesses of rejected
//! selections, checked against the decoded selected graph.
use asap_types::stages::{ProducerConsumerConnection, QueryRootAssignment, SelectedCandidateGraph};

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
