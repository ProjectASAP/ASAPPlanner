//! Source dependencies and query-index mapping shared by planning strategies.

use crate::Rejection;
use asap_types::ir::operator::operator_properties::Source;
use asap_types::ir::{NonASAPOp, Operator, OperatorNode, QueryRoot};
use std::collections::HashMap;
use std::rc::Rc;

/// Queries connected by reads from the same raw source.
pub(crate) fn source_connected_queries(roots: &[QueryRoot]) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..roots.len()).collect();
    fn find(parent: &mut [usize], q: usize) -> usize {
        let mut root = q;
        while parent[root] != root {
            root = parent[root];
        }
        let mut q = q;
        while parent[q] != root {
            let next = parent[q];
            parent[q] = root;
            q = next;
        }
        root
    }
    // The first query read each source, by name (data facts are keyed by
    // name, whatever the source's kind).
    let mut first_reader: HashMap<String, usize> = HashMap::new();
    for (query, root) in roots.iter().enumerate() {
        let starts: Vec<&Rc<OperatorNode>> = match root {
            QueryRoot::Operator(node) => vec![node],
            QueryRoot::Scalar(expr) => expr.operator_refs(),
        };
        for node in starts.into_iter().flat_map(OperatorNode::reachable) {
            let Operator::NonASAP(NonASAPOp::Scan { source, .. }) = &node.operator else {
                continue;
            };
            let name = match source {
                Source::TimeSeries { metric } => metric.clone(),
                Source::Table { table_ref } => table_ref.clone(),
            };
            let other = *first_reader.entry(name).or_insert(query);
            let (a, b) = (find(&mut parent, query), find(&mut parent, other));
            parent[a.max(b)] = a.min(b);
        }
    }
    let mut components: Vec<Vec<usize>> = Vec::new();
    let mut index_of: HashMap<usize, usize> = HashMap::new();
    for query in 0..roots.len() {
        let root = find(&mut parent, query);
        let index = *index_of.entry(root).or_insert_with(|| {
            components.push(Vec::new());
            components.len() - 1
        });
        components[index].push(query);
    }
    components
}

/// Map component-local rejection indices back to workload indices.
pub(crate) fn remap_rejection_query_indices(
    rejection: Rejection,
    component: &[usize],
) -> Rejection {
    let rename = |text: &str| -> String {
        let Some(rest) = text.strip_prefix('q') else {
            return text.to_string();
        };
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        match digits.parse::<usize>() {
            Ok(n) if n >= 1 && n <= component.len() => {
                format!("q{}{}", component[n - 1] + 1, &rest[digits.len()..])
            }
            _ => text.to_string(),
        }
    };
    Rejection {
        id: rename(&rejection.id),
        cause: rejection.cause,
        reason: rename(&rejection.reason),
    }
}
