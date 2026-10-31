//! Logical group identity established before implementation candidate generation.
//!
//! This module depends only on logical IR. It preserves the existing CSE
//! legality rules; Pass 1 walks its groups.

/// Logical implementation candidates populated after initial identity construction.
pub mod candidates;
/// The candidate DAGs a memo stands for: choices and their composition.
pub mod enumeration;
pub(crate) mod groups;
pub(crate) use groups::NodeToStructuralIdCache;
pub(crate) mod rules;
/// Shared finite state configuration enumeration.
pub mod shared_configurations;
/// Concrete summary-state groups exposed by sharing rules.
pub mod state_groups;
/// Local pane candidates with selected-reader grid configurations.
pub mod window_candidates;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

use crate::pass2::cse::share_common_sub_dags;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::stages::{CandidateGenerationTimeout, MemoGroup, MemoGroupId};

pub(crate) fn check_generation_deadline(
    deadline: Option<Instant>,
) -> Result<(), CandidateGenerationTimeout> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        Err(CandidateGenerationTimeout)
    } else {
        Ok(())
    }
}

/// Initial logical groups, without implementation choices or solver state.
#[derive(Debug, Clone)]
pub struct DagOfLogicalMemoGroups {
    pub groups: Vec<MemoGroup>,
    /// Each query's root over groups, by index in the workload: its
    /// operator's group, or its scalar expression with every operator
    /// reference replaced by that operator's group.
    pub query_roots: Vec<QueryRoot<MemoGroupId>>,
    group_of: GroupOf,
}

impl DagOfLogicalMemoGroups {
    /// Stage 0: the memo groups of the workload's query roots, query `i`
    /// being `roots[i]`.
    pub fn from_roots(roots: &[QueryRoot]) -> Self {
        let (groups, group_of) = hash_consed(roots);
        let group = |node: &Rc<OperatorNode>| group_of[&Rc::as_ptr(node)];
        let query_roots = roots
            .iter()
            .map(|root| match root {
                QueryRoot::Operator(node) => QueryRoot::Operator(group(node)),
                QueryRoot::Scalar(expr) => {
                    QueryRoot::Scalar(expr.map_operator_refs(&mut |node| group(node)))
                }
            })
            .collect();
        Self {
            groups,
            query_roots,
            group_of,
        }
    }

    /// Group of a node from the original workload, not a rewritten candidate.
    pub fn group_for_node(&self, node: &Rc<OperatorNode>) -> Option<MemoGroupId> {
        self.group_of.get(&Rc::as_ptr(node)).copied()
    }

    /// Each query's root over the groups' logical nodes, in workload order:
    /// a group several queries read is one node.
    pub fn roots_over_groups(&self) -> Vec<QueryRoot> {
        let node = |g: &MemoGroupId| Rc::clone(&self.groups[g.0 as usize].logical_node);
        self.query_roots
            .iter()
            .map(|root| match root {
                QueryRoot::Operator(g) => QueryRoot::Operator(node(g)),
                QueryRoot::Scalar(expr) => {
                    QueryRoot::Scalar(expr.map_operator_refs(&mut |g| node(g)))
                }
            })
            .collect()
    }
}

/// Each original node (of `roots`), by address, to its group.
type GroupOf = HashMap<*const OperatorNode, MemoGroupId>;

/// The groups of `roots`, children before parents, and each original node's
/// group. A scalar query reaches the groups of the operator nodes its
/// expression reads.
fn hash_consed(roots: &[QueryRoot]) -> (Vec<MemoGroup>, GroupOf) {
    let originals: Vec<(usize, Rc<OperatorNode>)> = roots
        .iter()
        .enumerate()
        .flat_map(|(query, root)| {
            operator_roots(root)
                .into_iter()
                .map(move |node| (query, node))
        })
        .collect();
    let interned = share_common_sub_dags(originals.clone());
    // Number the interned nodes children first.
    let mut groups: Vec<MemoGroup> = Vec::new();
    let mut id_of: HashMap<*const OperatorNode, MemoGroupId> = HashMap::new();
    for (_, root) in &interned {
        number(root, &mut groups, &mut id_of);
    }
    for (query, root) in &interned {
        let g = id_of[&Rc::as_ptr(root)];
        let mut reached = HashSet::new();
        reach(&groups, g, &mut reached);
        for reached in reached {
            let queries = &mut groups[reached.0 as usize].queries;
            if !queries.contains(query) {
                queries.push(*query);
            }
        }
    }
    for group in &mut groups {
        group.queries.sort_unstable();
    }
    // Interning rebuilds every node over its children in order, so the two
    // DAGs are walked side by side.
    let mut group_of = GroupOf::new();
    for ((_, original), (_, root)) in originals.iter().zip(&interned) {
        pair(original, root, &id_of, &mut group_of);
    }
    (groups, group_of)
}

/// The operator nodes a query root reads.
fn operator_roots(root: &QueryRoot) -> Vec<Rc<OperatorNode>> {
    root.operator_refs().into_iter().cloned().collect()
}

/// Number `node` (interned) and its descendants, children first.
fn number(
    node: &Rc<OperatorNode>,
    groups: &mut Vec<MemoGroup>,
    id_of: &mut HashMap<*const OperatorNode, MemoGroupId>,
) -> MemoGroupId {
    if let Some(&id) = id_of.get(&Rc::as_ptr(node)) {
        return id;
    }
    let children: Vec<MemoGroupId> = node
        .children()
        .into_iter()
        .map(|child| number(child, groups, id_of))
        .collect();
    let id = MemoGroupId(groups.len() as u32);
    for &child in &children {
        let parents = &mut groups[child.0 as usize].parents;
        if !parents.contains(&id) {
            parents.push(id);
        }
    }
    groups.push(MemoGroup {
        logical_node: Rc::clone(node),
        child_groups: children,
        parents: Vec::new(),
        queries: Vec::new(),
    });
    id_of.insert(Rc::as_ptr(node), id);
    id
}

/// `g` and every group below it.
fn reach(groups: &[MemoGroup], g: MemoGroupId, reached: &mut HashSet<MemoGroupId>) {
    if reached.insert(g) {
        for &child in &groups[g.0 as usize].child_groups {
            reach(groups, child, reached);
        }
    }
}

/// Map `original` and its descendants to the groups of `interned`, its
/// interned copy.
fn pair(
    original: &Rc<OperatorNode>,
    interned: &Rc<OperatorNode>,
    id_of: &HashMap<*const OperatorNode, MemoGroupId>,
    group_of: &mut GroupOf,
) {
    if group_of
        .insert(Rc::as_ptr(original), id_of[&Rc::as_ptr(interned)])
        .is_some()
    {
        return;
    }
    for (child, interned_child) in original.children().into_iter().zip(interned.children()) {
        pair(child, interned_child, id_of, group_of);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lower_promql;
    use asap_types::ir::operator::operator_properties::BinaryOpKind;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
    use asap_types::ir::{NonASAPOp, ScalarExpr};
    use asap_types::stages::queries_rooted_at;

    fn query(text: &str) -> QueryRoot {
        QueryRoot::Operator(lower_promql(text, AccuracyTarget::max_error(0.01)))
    }

    /// Logical interning shares equivalent results without needing Pass 1 choices.
    #[test]
    fn repeated_results_share_groups_but_quantile_levels_do_not() {
        let roots = vec![
            query("sum by (job) (m)"),
            query("sum by (job) (m)"),
            query("quantile_over_time(0.95, m[5m])"),
            query("quantile_over_time(0.99, m[5m])"),
        ];
        let memo = DagOfLogicalMemoGroups::from_roots(&roots);
        let root_group = |q: usize| *memo.query_roots[q].as_operator().unwrap();
        assert_eq!(root_group(0), root_group(1));
        assert_ne!(root_group(2), root_group(3));
        for (index, group) in memo.groups.iter().enumerate() {
            assert!(group
                .child_groups
                .iter()
                .all(|child| (child.0 as usize) < index));
            for child in &group.child_groups {
                assert!(memo.groups[child.0 as usize]
                    .parents
                    .contains(&MemoGroupId(index as u32)));
            }
        }
    }

    /// A per-series quantile has no provable unique key, but it is
    /// deterministic: both operands of `G2 / G2` read the same rows, so the
    /// two equal sub-expressions are one group read twice.
    #[test]
    fn equal_keyless_deterministic_results_share_one_group() {
        let roots = vec![QueryRoot::Operator(lower_promql(
                "avg by (job) (quantile_over_time(0.5, data[5m]) / quantile_over_time(0.5, data[5m]) + quantile_over_time(0.9, data[5m]))",
                AccuracyTarget::max_error(0.01).failure_probability(0.001),
            ))];
        let memo = DagOfLogicalMemoGroups::from_roots(&roots);
        let division = memo
            .groups
            .iter()
            .find(|group| {
                matches!(group.logical_node.non_asap(), Some(NonASAPOp::BinaryOp { operator, .. })
                    if operator.kind == BinaryOpKind::Arithmetic(ArithmeticOpKind::Div))
            })
            .expect("a `G2 / G3` group");
        assert_eq!(
            division.child_groups[0], division.child_groups[1],
            "both operands of the division are one group"
        );
        assert_eq!(memo.groups.len(), 7);
    }

    /// Scalar references are roots of their query; literals need no invented node.
    #[test]
    fn scalar_and_literal_roots_keep_workload_positions() {
        let QueryRoot::Operator(node) = query("sum by (job) (m)") else {
            unreachable!()
        };
        let roots = vec![
            QueryRoot::Scalar(ScalarExpr::literal_f64(1.0)),
            QueryRoot::Scalar(ScalarExpr::PromqlScalarFromVector(node.clone())),
            QueryRoot::Operator(node.clone()),
        ];
        let memo = DagOfLogicalMemoGroups::from_roots(&roots);
        let id = memo.group_for_node(&node).unwrap();
        assert_eq!(queries_rooted_at(&memo.query_roots, id), vec![1, 2]);
        assert_eq!(memo.groups[id.0 as usize].reached_by_queries(), [1, 2]);
        assert!(DagOfLogicalMemoGroups::from_roots(&roots[..1])
            .groups
            .is_empty());
    }
}
