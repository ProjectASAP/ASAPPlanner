//! Stage 1 (logical ASAP-aware optimization), Pass 2: ASAP-aware
//! common-subexpression elimination, as defined in
//! `docs/design_docs/proposals/planner-layering.md` (Algorithm 2).
//!
//! Only the identical-expression rule is implemented: nodes of one
//! whole-workload candidate with identical input and computation semantics
//! become one node serving every consumer. [`share_identical_expressions`]
//! applies it to a composed Pass 1 candidate; the pipeline
//! ([`crate::pass::stage1_to_stage3`]) adds the result as a new candidate and
//! keeps the independent one, so plan selection compares both.
//!
//! Every identical group of the candidate is shared at once (the
//! `ir::cse::share_common_sub_dags` interning, which merges only
//! sharing-legal nodes: ASAP nodes and nodes with a unique key). Algorithm
//! 2's other non-empty subsets of the groups are not enumerated.
//! The summary-capability and window-composition rules are not implemented.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::cse::share_common_sub_dags;
use asap_types::ir::{OperatorNode, QueryRoot};

/// `roots` with every identical sub-DAG merged into one node, across and
/// within roots (including the operators scalar roots read), or `None` when
/// nothing is identical.
pub fn share_identical_expressions(roots: &[QueryRoot]) -> Option<Vec<QueryRoot>> {
    let operators: Vec<Rc<OperatorNode>> =
        roots.iter().flat_map(root_operators).cloned().collect();
    let before = distinct_nodes(&operators);
    let merged: Vec<Rc<OperatorNode>> =
        share_common_sub_dags(operators.iter().cloned().enumerate().collect())
            .into_iter()
            .map(|(_, node)| node)
            .collect();
    if distinct_nodes(&merged) == before {
        return None;
    }
    let merged: HashMap<*const OperatorNode, Rc<OperatorNode>> = operators
        .iter()
        .map(Rc::as_ptr)
        .zip(merged)
        .collect();
    Some(
        roots
            .iter()
            .map(|root| match root {
                QueryRoot::Operator(node) => QueryRoot::Operator(merged[&Rc::as_ptr(node)].clone()),
                QueryRoot::Scalar(expr) => QueryRoot::Scalar(
                    expr.map_operator_refs(&mut |node| merged[&Rc::as_ptr(node)].clone()),
                ),
            })
            .collect(),
    )
}

fn root_operators(root: &QueryRoot) -> Vec<&Rc<OperatorNode>> {
    match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    }
}

/// Distinct nodes (by identity) reachable from `roots`.
fn distinct_nodes<'a>(roots: impl IntoIterator<Item = &'a Rc<OperatorNode>>) -> usize {
    roots
        .into_iter()
        .flat_map(OperatorNode::reachable)
        .map(|node| Rc::as_ptr(&node))
        .collect::<HashSet<_>>()
        .len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower_promql;
    use asap_types::ir::schema_support::with_promql_series_identity;
    use asap_types::types::AccuracyTarget;

    fn roots(queries: &[&str]) -> Vec<QueryRoot> {
        queries
            .iter()
            .map(|q| {
                let root = lower_promql(q, AccuracyTarget::Exact);
                QueryRoot::Operator(with_promql_series_identity(&root).unwrap())
            })
            .collect()
    }

    fn operator(root: &QueryRoot) -> &Rc<OperatorNode> {
        match root {
            QueryRoot::Operator(node) => node,
            QueryRoot::Scalar(_) => panic!("operator root"),
        }
    }

    /// Two queries over the same range selector share it, and only it; each
    /// query's value is unchanged.
    #[test]
    fn identical_range_selector_is_shared() {
        let roots = roots(&[
            "sum by (job) (rate(m[1m]))",
            "topk by (job) (10, sum_over_time(m[1m]))",
        ]);
        let shared = share_identical_expressions(&roots).expect("the selector is shared");
        for (before, after) in roots.iter().zip(&shared) {
            assert_eq!(operator(before), operator(after));
        }
        let reach = |root: &QueryRoot| -> HashSet<*const OperatorNode> {
            OperatorNode::reachable(operator(root))
                .iter()
                .map(Rc::as_ptr)
                .collect()
        };
        let common: Vec<_> = reach(&shared[0])
            .intersection(&reach(&shared[1]))
            .copied()
            .collect();
        assert!(!common.is_empty());
        assert!(distinct_nodes(shared.iter().map(operator)) < distinct_nodes(roots.iter().map(operator)));
    }

    /// Queries with nothing in common have no shared form.
    #[test]
    fn nothing_identical_shares_nothing() {
        assert!(share_identical_expressions(&roots(&["sum(rate(a[1m]))", "sum(rate(b[1m]))"]))
            .is_none());
    }
}
