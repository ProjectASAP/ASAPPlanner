//! Logical group identity established before implementation candidate generation.
//!
//! This module depends only on logical IR. It preserves the existing CSE
//! legality rules and owns the original roots so pointer-based lookup remains
//! valid while later stages populate implementation candidates.

/// Logical implementation candidates populated after initial identity construction.
pub mod candidates;
mod groups;
/// Pass 2 summary-capability expansion and consistent producer/reader resizing.
pub mod shared_builds;
/// Concrete summary-state groups for the connection-based planner migration.
pub mod state_groups;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use asap_types::ir::cse::share_common_sub_dags;
use asap_types::ir::{OperatorNode, QueryRoot};

/// A group, by index in [`LogicalMemo::groups`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemoGroupId(pub u32);

/// One distinct logical sub-expression of the workload.
#[derive(Debug, Clone)]
pub struct MemoGroup {
    /// Its logical node as the frontends (and Pass 1 targets) see it; its
    /// children are other groups' nodes.
    pub node: Rc<OperatorNode>,
    /// Its child groups, in the node's child order.
    pub children: Vec<MemoGroupId>,
    /// The groups that read it.
    pub parents: Vec<MemoGroupId>,
    /// The queries whose roots reach it, by index in the workload.
    pub queries: Vec<usize>,
    /// The queries whose root it is.
    pub roots_of: Vec<usize>,
}

/// Initial logical groups, without implementation choices or solver state.
#[derive(Debug, Clone)]
pub struct LogicalMemo {
    pub groups: Vec<MemoGroup>,
    original_roots: Vec<QueryRoot>,
    group_of: GroupOf,
    /// Time spent constructing logical identity, excluding later Pass 1 work.
    pub build_duration: Duration,
}

impl LogicalMemo {
    pub fn from_roots<Id>(roots: &[(Id, QueryRoot)]) -> Self {
        let started = Instant::now();
        let (groups, group_of) = hash_consed(roots);
        Self {
            groups,
            original_roots: roots.iter().map(|(_, root)| root.clone()).collect(),
            group_of,
            build_duration: started.elapsed(),
        }
    }

    /// Group of a node from the original workload, not a rewritten candidate.
    pub fn group_for_node(&self, node: &Rc<OperatorNode>) -> Option<MemoGroupId> {
        self.group_of.get(&Rc::as_ptr(node)).copied()
    }

    /// Check root kind and ordered operator dependencies, not scalar values.
    /// Changing scalar-only output expressions does not change logical groups.
    /// Never compare operator DAGs recursively: shared diamonds and NaN literals
    /// make structural equality inappropriate for pointer-indexed identities.
    pub fn matches_root_dependencies<Id>(&self, roots: &[(Id, QueryRoot)]) -> bool {
        self.original_roots.len() == roots.len()
            && self
                .original_roots
                .iter()
                .zip(roots)
                .all(|(original, (_, root))| {
                    std::mem::discriminant(original) == std::mem::discriminant(root) && {
                        let original_nodes = operator_roots(original);
                        let nodes = operator_roots(root);
                        original_nodes.len() == nodes.len()
                            && original_nodes
                                .iter()
                                .zip(&nodes)
                                .all(|(a, b)| Rc::ptr_eq(a, b))
                    }
                })
    }
}

/// Each original node (of `roots`), by address, to its group.
type GroupOf = HashMap<*const OperatorNode, MemoGroupId>;

/// The groups of `roots`, children before parents, and each original node's
/// group. A scalar query's roots are the operator nodes its expression
/// reads.
fn hash_consed<Id>(roots: &[(Id, QueryRoot)]) -> (Vec<MemoGroup>, GroupOf) {
    let originals: Vec<(usize, Rc<OperatorNode>)> = roots
        .iter()
        .enumerate()
        .flat_map(|(query, (_, root))| {
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
        let roots_of = &mut groups[g.0 as usize].roots_of;
        if !roots_of.contains(query) {
            roots_of.push(*query);
        }
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
    match root {
        QueryRoot::Operator(node) => vec![Rc::clone(node)],
        QueryRoot::Scalar(expr) => expr.operator_refs().into_iter().cloned().collect(),
    }
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
        node: Rc::clone(node),
        children,
        parents: Vec::new(),
        queries: Vec::new(),
        roots_of: Vec::new(),
    });
    id_of.insert(Rc::as_ptr(node), id);
    id
}

/// `g` and every group below it.
fn reach(groups: &[MemoGroup], g: MemoGroupId, reached: &mut HashSet<MemoGroupId>) {
    if reached.insert(g) {
        for &child in &groups[g.0 as usize].children {
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
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::ScalarExpr;

    fn query(text: &str) -> QueryRoot {
        QueryRoot::Operator(lower_promql(text, AccuracyTarget::max_error(0.01)))
    }

    /// Logical interning shares equivalent results without needing Pass 1 choices.
    #[test]
    fn repeated_results_share_groups_but_quantile_levels_do_not() {
        let roots = vec![
            (0, query("sum by (job) (m)")),
            (1, query("sum by (job) (m)")),
            (2, query("quantile_over_time(0.95, m[5m])")),
            (3, query("quantile_over_time(0.99, m[5m])")),
        ];
        let memo = LogicalMemo::from_roots(&roots);
        let root_group = |q| {
            memo.groups
                .iter()
                .position(|g| g.roots_of.contains(&q))
                .unwrap()
        };
        assert_eq!(root_group(0), root_group(1));
        assert_ne!(root_group(2), root_group(3));
        for (index, group) in memo.groups.iter().enumerate() {
            assert!(group
                .children
                .iter()
                .all(|child| (child.0 as usize) < index));
            for child in &group.children {
                assert!(memo.groups[child.0 as usize]
                    .parents
                    .contains(&MemoGroupId(index as u32)));
            }
        }
    }

    /// Scalar references are roots of their query; literals need no invented node.
    #[test]
    fn scalar_and_literal_roots_keep_workload_positions() {
        let QueryRoot::Operator(node) = query("sum by (job) (m)") else {
            unreachable!()
        };
        let roots = vec![
            (10, QueryRoot::Scalar(ScalarExpr::literal_f64(1.0))),
            (
                20,
                QueryRoot::Scalar(ScalarExpr::PromqlScalarFromVector(node.clone())),
            ),
            (30, QueryRoot::Operator(node.clone())),
        ];
        let memo = LogicalMemo::from_roots(&roots);
        let group = &memo.groups[memo.group_for_node(&node).unwrap().0 as usize];
        assert_eq!(group.roots_of, vec![1, 2]);
        assert_eq!(group.queries, vec![1, 2]);
        assert!(memo.matches_root_dependencies(&roots));
        let mut reordered = roots.clone();
        reordered.swap(0, 1);
        assert!(!memo.matches_root_dependencies(&reordered));
        assert!(LogicalMemo::from_roots(&roots[..1]).groups.is_empty());
    }

    /// Non-reflexive floating literals must not invalidate unchanged dependencies.
    #[test]
    fn unchanged_nan_root_matches_by_dependency_identity() {
        let roots = vec![(0, QueryRoot::Scalar(ScalarExpr::literal_f64(f64::NAN)))];
        let memo = LogicalMemo::from_roots(&roots);
        assert!(memo.matches_root_dependencies(&roots));
    }

    /// Dependency checks must not unfold shared operator DAGs once per path.
    #[test]
    fn checking_shared_diamond_uses_root_identity() {
        use asap_types::ir::operator::operator_properties::BinaryOpKind;
        use asap_types::ir::scalar::CompareOpKind;
        use asap_types::ir::{BinaryOperator, NonASAPOp, Operator};
        let QueryRoot::Operator(mut node) = query("m") else {
            unreachable!()
        };
        for _ in 0..24 {
            node = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::BinaryOp {
                operator: BinaryOperator {
                    checked_relative_division: false,
                    checked_finite_division: false,
                    kind: BinaryOpKind::Compare(CompareOpKind::Eq),
                    vector_match: None,
                },
                return_bool: false,
                lhs: node.clone(),
                rhs: node,
            }))
            .unwrap();
        }
        // Construct the identity guard directly so this regression isolates
        // root matching from the independently tested CSE/interner.
        let roots = vec![(0, QueryRoot::Operator(node))];
        let memo = LogicalMemo {
            groups: Vec::new(),
            original_roots: roots.iter().map(|(_, r)| r.clone()).collect(),
            group_of: GroupOf::new(),
            build_duration: Duration::ZERO,
        };
        assert!(memo.matches_root_dependencies(&roots));
    }

    /// Structural equality alone cannot validate pointer-indexed target lookups.
    #[test]
    fn rebuilt_equal_roots_do_not_match_the_original_workload() {
        let roots = vec![(0, query("sum by (job) (m)"))];
        let memo = LogicalMemo::from_roots(&roots);
        let rebuilt = vec![(0, query("sum by (job) (m)"))];
        assert!(!memo.matches_root_dependencies(&rebuilt));
        assert!(memo.matches_root_dependencies(&roots.clone()));
    }
}
