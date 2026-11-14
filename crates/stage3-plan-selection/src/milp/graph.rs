//! The strongly connected components of the selectable edges between
//! candidates: acyclicity (E) needs an order variable $t_v$ only inside a
//! component, bounded by its size.

use std::collections::HashMap;

use super::model::SelectionInputs;

/// Each candidate's strongly connected component of the selectable edges
/// (those [`crate::cost::SelectionCosts::edge_cost_change_per_second`] costs), computed once per model.
pub(crate) struct Components {
    /// Each candidate's component number.
    pub(crate) of: Vec<usize>,
    /// The size of each candidate's component: 1 for a candidate on no
    /// cycle.
    pub(crate) size: Vec<usize>,
}

impl Components {
    pub(crate) fn of(inputs: &SelectionInputs<'_>) -> Self {
        let of = strongly_connected_components(
            inputs.physical.physical_candidates.len(),
            inputs
                .costs
                .edge_cost_change_per_second
                .keys()
                .map(|&edge| {
                    let edge = inputs.physical.edge(edge);
                    (edge.producer_candidate_id.0, edge.consumer_candidate_id.0)
                }),
        );
        let mut count: HashMap<usize, usize> = HashMap::new();
        for &component in &of {
            *count.entry(component).or_default() += 1;
        }
        let size = of.iter().map(|component| count[component]).collect();
        Self { of, size }
    }
}

/// Each node's strongly connected component of the graph with `arcs`
/// (Kosaraju), as a component number.
fn strongly_connected_components(
    node_count: usize,
    arcs: impl Iterator<Item = (usize, usize)>,
) -> Vec<usize> {
    let mut forward = vec![Vec::new(); node_count];
    let mut backward = vec![Vec::new(); node_count];
    for (from, to) in arcs {
        forward[from].push(to);
        backward[to].push(from);
    }
    // Nodes by finishing time of a depth-first search over `forward`.
    let mut finished = Vec::with_capacity(node_count);
    let mut visited = vec![false; node_count];
    for start in 0..node_count {
        if std::mem::replace(&mut visited[start], true) {
            continue;
        }
        let mut stack = vec![(start, 0)];
        while let Some((node, next)) = stack.pop() {
            if let Some(&to) = forward[node].get(next) {
                stack.push((node, next + 1));
                if !std::mem::replace(&mut visited[to], true) {
                    stack.push((to, 0));
                }
            } else {
                finished.push(node);
            }
        }
    }
    let mut component = vec![usize::MAX; node_count];
    for (number, &start) in finished.iter().rev().enumerate() {
        if component[start] != usize::MAX {
            continue;
        }
        component[start] = number;
        let mut stack = vec![start];
        while let Some(node) = stack.pop() {
            for &from in &backward[node] {
                if component[from] == usize::MAX {
                    component[from] = number;
                    stack.push(from);
                }
            }
        }
    }
    component
}
