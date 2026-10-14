//! Stage 1 (logical ASAP-aware optimization), Pass 2: ASAP-aware
//! common-subexpression elimination, as defined in
//! `docs/design_docs/proposals/planner-layering.md` (Algorithm 2).
//!
//! Only the identical-expression rule is implemented. A *sharing group* of a
//! whole-workload candidate is a set of two or more of its nodes with
//! identical input and computation semantics that are legal to share: the
//! `ir::cse` interning merges only ASAP nodes and nodes with a unique key.
//! [`identical_expression_variants`] returns one candidate per non-empty
//! subset of the groups (Algorithm 2, line 31): each group in the subset
//! becomes one node serving every consumer, and every other group stays
//! separate. The caller keeps the independent candidate beside them, so plan
//! selection compares all of them; nothing is capped.
//!
//! Sharing a node shares the sub-DAG below it. A subset that leaves out a
//! group whose nodes all lie below the nodes of a group it shares gives the
//! same candidate as the subset with that group; such a candidate is
//! returned once, under the subset of the groups it actually shares.
//!
//! The summary-capability and window-composition rules are not implemented.

use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use std::rc::Rc;

use asap_types::ir::cse::share_common_sub_dags;
use asap_types::ir::{OperatorNode, QueryRoot};

/// A Pass 2 candidate: the candidate's roots with the sharing groups
/// `shared_groups` (indices into [`sharing_groups`]) shared.
#[derive(Debug, Clone)]
pub struct IdenticalExpressionVariant {
    pub shared_groups: Vec<usize>,
    pub roots: Vec<QueryRoot>,
}

/// One sharing group: the identical nodes, in the order a children-first
/// walk of the roots meets them.
#[derive(Debug, Clone)]
pub struct SharingGroup {
    pub nodes: Vec<Rc<OperatorNode>>,
}

/// The sharing groups of the candidate with query roots `roots`, children
/// before parents.
pub fn sharing_groups(roots: &[QueryRoot]) -> Vec<SharingGroup> {
    let operators: Vec<Rc<OperatorNode>> = roots.iter().flat_map(root_operators).cloned().collect();
    let interned = share_common_sub_dags(operators.iter().cloned().enumerate().collect());
    // Each node's interned representative: interning rebuilds every node
    // over its children in order, so the two DAGs are walked side by side.
    let mut representative: HashMap<*const OperatorNode, *const OperatorNode> = HashMap::new();
    let mut order = Vec::new();
    for (original, (_, merged)) in operators.iter().zip(&interned) {
        pair(original, merged, &mut representative, &mut order);
    }
    let mut members: HashMap<*const OperatorNode, Vec<Rc<OperatorNode>>> = HashMap::new();
    let mut first_seen = Vec::new();
    for node in order {
        let key = representative[&Rc::as_ptr(&node)];
        let entry = members.entry(key).or_default();
        if entry.is_empty() {
            first_seen.push(key);
        }
        entry.push(node);
    }
    first_seen
        .into_iter()
        .filter_map(|key| {
            let nodes = members.remove(&key)?;
            (nodes.len() >= 2).then_some(SharingGroup { nodes })
        })
        .collect()
}

/// Record `original`'s representative `merged` and recurse into the
/// children, then list `original` (children first, each node once).
fn pair(
    original: &Rc<OperatorNode>,
    merged: &Rc<OperatorNode>,
    representative: &mut HashMap<*const OperatorNode, *const OperatorNode>,
    order: &mut Vec<Rc<OperatorNode>>,
) {
    if representative.contains_key(&Rc::as_ptr(original)) {
        return;
    }
    representative.insert(Rc::as_ptr(original), Rc::as_ptr(merged));
    for (child, merged_child) in original.children().into_iter().zip(merged.children()) {
        pair(child, merged_child, representative, order);
    }
    order.push(Rc::clone(original));
}

/// Every Pass 2 candidate of the identical-expression rule for the
/// candidate with query roots `roots`: one per non-empty subset of its
/// [`sharing_groups`] that gives a distinct candidate, in binary-counter
/// order of the subsets (the first group varying fastest). Empty when
/// nothing is identical.
pub fn identical_expression_variants(roots: &[QueryRoot]) -> Vec<IdenticalExpressionVariant> {
    let groups = sharing_groups(roots);
    let group_of: HashMap<*const OperatorNode, usize> = groups
        .iter()
        .enumerate()
        .flat_map(|(index, group)| group.nodes.iter().map(move |n| (Rc::as_ptr(n), index)))
        .collect();
    let mut variants = Vec::new();
    let mut subset = vec![false; groups.len()];
    while next_subset(&mut subset) {
        let mut sharing = Sharing {
            subset: &subset,
            group_of: &group_of,
            rebuilt: HashMap::new(),
            shared: HashMap::new(),
        };
        let shared_roots: Vec<QueryRoot> = roots.iter().map(|r| sharing.root(r)).collect();
        // The groups this candidate actually shares: every node of the group
        // that is still read became one node. A node below a shared node
        // other than the one kept is no longer read.
        let merged: Vec<bool> = groups
            .iter()
            .map(|group| {
                let mut nodes = group.nodes.iter().filter_map(|node| {
                    let (_, rebuilt) = sharing.rebuilt.get(&Rc::as_ptr(node))?;
                    Some(Rc::as_ptr(rebuilt))
                });
                let first = nodes.next();
                nodes.all(|node| Some(node) == first)
            })
            .collect();
        if merged == subset {
            variants.push(IdenticalExpressionVariant {
                shared_groups: (0..groups.len()).filter(|&g| subset[g]).collect(),
                roots: shared_roots,
            });
        }
    }
    variants
}

/// Binary increment of `subset`, the first entry the lowest bit; `false`
/// once every subset has been visited.
fn next_subset(subset: &mut [bool]) -> bool {
    for bit in subset.iter_mut() {
        *bit = !*bit;
        if *bit {
            return true;
        }
    }
    false
}

/// One application of the rule: the groups in `subset` shared.
struct Sharing<'a> {
    subset: &'a [bool],
    group_of: &'a HashMap<*const OperatorNode, usize>,
    /// Original node → (original, rebuilt). The original is kept alive so
    /// its address cannot be reused while it is a key.
    rebuilt: HashMap<*const OperatorNode, (Rc<OperatorNode>, Rc<OperatorNode>)>,
    /// Group → the one node every member of a shared group becomes.
    shared: HashMap<usize, Rc<OperatorNode>>,
}

impl Sharing<'_> {
    fn root(&mut self, root: &QueryRoot) -> QueryRoot {
        match root {
            QueryRoot::Operator(node) => QueryRoot::Operator(self.node(node)),
            QueryRoot::Scalar(expr) => {
                QueryRoot::Scalar(expr.map_operator_refs(&mut |node| self.node(node)))
            }
        }
    }

    fn node(&mut self, node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        if let Some((_, rebuilt)) = self.rebuilt.get(&Rc::as_ptr(node)) {
            return Rc::clone(rebuilt);
        }
        let group = self
            .group_of
            .get(&Rc::as_ptr(node))
            .copied()
            .filter(|&g| self.subset[g]);
        let rebuilt = match group.and_then(|g| self.shared.get(&g)) {
            Some(shared) => Rc::clone(shared),
            None => {
                let rebuilt = self.over_rebuilt_children(node);
                if let Some(g) = group {
                    self.shared.insert(g, Rc::clone(&rebuilt));
                }
                rebuilt
            }
        };
        self.rebuilt
            .insert(Rc::as_ptr(node), (Rc::clone(node), Rc::clone(&rebuilt)));
        rebuilt
    }

    /// `node` over its rebuilt children; the same `Rc` when none changed.
    /// Every child is replaced by an identical node, so `node`'s schema,
    /// guarantee, timing and materialization stay valid.
    fn over_rebuilt_children(&mut self, node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        let mut changed = false;
        let operator = node.operator.map_children(|child| {
            let rebuilt = self.node(child);
            changed |= !Rc::ptr_eq(&rebuilt, child);
            rebuilt
        });
        if !changed {
            return Rc::clone(node);
        }
        Rc::new(
            OperatorNode::with_schema(operator, node.schema.clone())
                .with_guarantee(node.guarantee.clone())
                .with_timing(node.timing)
                .with_materialization(node.materialization),
        )
    }
}

fn root_operators(root: &QueryRoot) -> Vec<&Rc<OperatorNode>> {
    match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    }
}

/// Distinct nodes (by identity) reachable from `roots`.
#[cfg(test)]
fn distinct_nodes(roots: &[QueryRoot]) -> usize {
    roots
        .iter()
        .flat_map(root_operators)
        .flat_map(OperatorNode::reachable)
        .map(|node| Rc::as_ptr(&node))
        .collect::<HashSet<_>>()
        .len()
}

/// Nodes reachable from both `a` and `b`.
#[cfg(test)]
fn common_nodes(a: &QueryRoot, b: &QueryRoot) -> HashSet<*const OperatorNode> {
    let reach = |root: &QueryRoot| -> HashSet<*const OperatorNode> {
        root_operators(root)
            .into_iter()
            .flat_map(OperatorNode::reachable)
            .map(|node| Rc::as_ptr(&node))
            .collect()
    };
    reach(a).intersection(&reach(b)).copied().collect()
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

    fn kinds(group: &SharingGroup) -> &'static str {
        group.nodes[0].operator.kind_name()
    }

    /// Two queries over the same range selector have two sharing groups,
    /// the scans and the time ranges. Sharing the time range shares the
    /// scan below it, so there are two variants: the scan alone shared, and
    /// both shared. Each query's value is unchanged.
    #[test]
    fn identical_range_selector_gives_one_variant_per_distinct_subset() {
        let roots = roots(&[
            "sum by (job) (rate(m[1m]))",
            "topk by (job) (10, sum_over_time(m[1m]))",
        ]);
        let groups = sharing_groups(&roots);
        assert_eq!(
            groups.iter().map(kinds).collect::<Vec<_>>(),
            ["Scan", "TimeRange"]
        );
        let variants = identical_expression_variants(&roots);
        assert_eq!(
            variants
                .iter()
                .map(|v| v.shared_groups.clone())
                .collect::<Vec<_>>(),
            [vec![0], vec![0, 1]]
        );
        for variant in &variants {
            assert_eq!(variant.roots, roots, "values are unchanged");
            assert!(distinct_nodes(&variant.roots) < distinct_nodes(&roots));
        }
        assert_eq!(
            common_nodes(&variants[0].roots[0], &variants[0].roots[1]).len(),
            1
        );
        assert_eq!(
            common_nodes(&variants[1].roots[0], &variants[1].roots[1]).len(),
            2
        );
    }

    /// Two pairs of queries over different metrics: each pair has a scan
    /// group and an instant-selector group, with two distinct ways to share
    /// (the scan, or the scan and the selector) or none, so 3 × 3 − 1 = 8
    /// Pass 2 candidates; each pair is shared alone or with the other.
    #[test]
    fn independent_groups_give_every_non_empty_subset() {
        let roots = roots(&["sum(a)", "max(a)", "sum(b)", "max(b)"]);
        assert_eq!(
            sharing_groups(&roots).iter().map(kinds).collect::<Vec<_>>(),
            ["Scan", "TimeRange", "Scan", "TimeRange"]
        );
        let variants = identical_expression_variants(&roots);
        assert_eq!(variants.len(), 8);
        let shares = |v: &IdenticalExpressionVariant| {
            (
                !common_nodes(&v.roots[0], &v.roots[1]).is_empty(),
                !common_nodes(&v.roots[2], &v.roots[3]).is_empty(),
            )
        };
        assert_eq!(
            variants.iter().map(shares).collect::<HashSet<_>>(),
            HashSet::from([(true, false), (false, true), (true, true)])
        );
        assert!(variants.iter().all(|v| v.roots == roots));
    }

    /// Queries with nothing in common have no Pass 2 candidate.
    #[test]
    fn nothing_identical_gives_no_variant() {
        assert!(
            identical_expression_variants(&roots(&["sum(rate(a[1m]))", "sum(rate(b[1m]))"]))
                .is_empty()
        );
    }
}
