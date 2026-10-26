//! Complete feasible starts from already validated candidate assignments.
use super::{
    CandidateDependencyGraph, ObservedAssignment, PhysicalCandidateId, SelectedCandidateGraph,
};
use std::collections::HashSet;

pub(in crate::search) struct WarmStart<'a> {
    pub selected: &'a SelectedCandidateGraph,
    pub active: HashSet<PhysicalCandidateId>,
    pub ranks: Vec<usize>,
    pub reached: Vec<HashSet<PhysicalCandidateId>>,
    pub cost: f64,
}

#[derive(PartialEq, Eq)]
struct AssignmentKey {
    candidates: Vec<PhysicalCandidateId>,
    connections: Vec<(usize, usize, usize)>,
    outputs: Vec<(usize, usize)>,
}

impl AssignmentKey {
    fn new(selected: &SelectedCandidateGraph) -> Self {
        let mut candidates = selected.selected_candidates.clone();
        let mut connections: Vec<_> = selected
            .connections
            .iter()
            .map(|edge| (edge.consumer.0, edge.requirement, edge.producer.0))
            .collect();
        let mut outputs: Vec<_> = selected
            .query_roots
            .iter()
            .map(|root| (root.query_root, root.candidate.0))
            .collect();
        candidates.sort_unstable();
        connections.sort_unstable();
        outputs.sort_unstable();
        Self {
            candidates,
            connections,
            outputs,
        }
    }
}

/// Inputs have passed structural validation. A later certified rejection may
/// exclude a formerly feasible observation; never submit that assignment as a
/// start. Cost refinements of the same assignment all apply simultaneously.
pub(in crate::search) fn cheapest_start<'a>(
    graph: &CandidateDependencyGraph,
    observations: &'a [ObservedAssignment],
) -> Option<WarmStart<'a>> {
    // Canonical decision keys avoid repeated quadratic edge-set comparisons
    // as the refinement history grows.
    let keys: Vec<_> = observations
        .iter()
        .map(|observation| AssignmentKey::new(&observation.selected))
        .collect();
    let (observation, cost) = observations
        .iter()
        .enumerate()
        .filter_map(|(index, observation)| {
            observation.exact_cost?;
            let selected = &observation.selected;
            if observations.iter().enumerate().any(|(other_index, other)| {
                (other.exact_cost.is_none() && keys[index] == keys[other_index])
                    || other.rejected_queries.iter().any(|witness| {
                        witness
                            .connections
                            .iter()
                            .all(|edge| selected.connections.contains(edge))
                            && witness
                                .query_roots
                                .iter()
                                .all(|root| selected.query_roots.contains(root))
                    })
            }) {
                return None;
            }
            let local: f64 = selected
                .selected_candidates
                .iter()
                .map(|id| graph.candidates[id.0].local_cost)
                .sum();
            let cost = observations
                .iter()
                .enumerate()
                .filter(|(other_index, _)| keys[index] == keys[*other_index])
                .filter_map(|(_, other)| other.exact_cost)
                .fold(local, f64::max);
            Some((observation, cost))
        })
        .min_by(|(_, left), (_, right)| left.total_cmp(right))?;
    let selected = &observation.selected;
    let active: HashSet<_> = selected.selected_candidates.iter().copied().collect();
    let mut producers = vec![Vec::new(); graph.candidates.len()];
    let mut consumers = vec![Vec::new(); graph.candidates.len()];
    for edge in &selected.connections {
        producers[edge.consumer.0].push(edge.producer);
        consumers[edge.producer.0].push(edge.consumer);
    }
    let mut outstanding: Vec<_> = producers.iter().map(Vec::len).collect();
    let mut ready: Vec<_> = active
        .iter()
        .copied()
        .filter(|id| outstanding[id.0] == 0)
        .collect();
    let mut ranks = vec![0; graph.candidates.len()];
    while let Some(producer) = ready.pop() {
        for &consumer in &consumers[producer.0] {
            ranks[consumer.0] = ranks[consumer.0].max(ranks[producer.0] + 1);
            outstanding[consumer.0] -= 1;
            if outstanding[consumer.0] == 0 {
                ready.push(consumer);
            }
        }
    }
    let mut reached = vec![HashSet::new(); graph.query_roots.len()];
    for root in &selected.query_roots {
        let mut pending = vec![root.candidate];
        while let Some(candidate) = pending.pop() {
            if reached[root.query_root].insert(candidate) {
                pending.extend(producers[candidate.0].iter().copied());
            }
        }
    }
    Some(WarmStart {
        selected,
        active,
        ranks,
        reached,
        cost,
    })
}
