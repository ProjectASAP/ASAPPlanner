//! The strongly connected components of the selectable edges between
//! executions: acyclicity (E) needs an order variable $t$ only inside a
//! component, bounded by its size.

use std::collections::HashMap;

use super::model::SelectionInputs;

/// Each execution's strongly connected component of the selectable edges
/// (those [`crate::cost::ExecutionCosts::per_edge`] costs), computed once
/// per model, by position in `SelectableExecutions::executions`.
pub(crate) struct Components {
    /// Each execution's component number.
    pub(crate) of: Vec<usize>,
    /// The size of each execution's component: 1 for an execution on no
    /// cycle.
    pub(crate) size: Vec<usize>,
}

impl Components {
    pub(crate) fn of(inputs: &SelectionInputs<'_>) -> Self {
        let selectable = inputs.selectable;
        let edges = selectable.edges();
        let costed = |position: &&usize| inputs.costs.per_edge[**position].is_some();
        let of = strongly_connected_components(
            selectable.executions().len(),
            |producer| {
                selectable
                    .edges_out_of(producer)
                    .iter()
                    .filter(costed)
                    .map(|&position| edges[position].consumer)
                    .collect()
            },
            |consumer| {
                (0..selectable.input_count(consumer))
                    .flat_map(|input| selectable.edges_into(consumer, input))
                    .filter(costed)
                    .map(|&position| edges[position].producer)
                    .collect()
            },
        );
        let mut count: HashMap<usize, usize> = HashMap::new();
        for &component in &of {
            *count.entry(component).or_default() += 1;
        }
        let size = of.iter().map(|component| count[component]).collect();
        Self { of, size }
    }
}

/// Each node's strongly connected component of the graph whose arcs out of
/// a node are `forward(node)` and into it `backward(node)` (Kosaraju), as
/// a component number.
fn strongly_connected_components(
    node_count: usize,
    forward: impl Fn(usize) -> Vec<usize>,
    backward: impl Fn(usize) -> Vec<usize>,
) -> Vec<usize> {
    // Nodes by finishing time of a depth-first search over `forward`.
    let mut finished = Vec::with_capacity(node_count);
    let mut visited = vec![false; node_count];
    for start in 0..node_count {
        if std::mem::replace(&mut visited[start], true) {
            continue;
        }
        let mut stack = vec![(start, forward(start), 0)];
        while let Some((node, out, next)) = stack.pop() {
            if let Some(&to) = out.get(next) {
                stack.push((node, out, next + 1));
                if !std::mem::replace(&mut visited[to], true) {
                    stack.push((to, forward(to), 0));
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
            for from in backward(node) {
                if component[from] == usize::MAX {
                    component[from] = number;
                    stack.push(from);
                }
            }
        }
    }
    component
}
