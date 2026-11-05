//! The one node equality that makes two nodes one memo group: Stage 0's
//! hash-consing of the frontends' DAG and the memo's
//! [`find_or_add_group`](super::Memo::find_or_add_group) both use
//! [`HashConsingTable`]. Two nodes are one when they are equal by
//! [`same_node`] (operator, schema, result kind, guarantee, timing,
//! materialization and children) and sharing them is legal
//! ([`shareable_when_equal`]).

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use crate::ir::operator::node::{Operator, OperatorNode};
use crate::ir::NonASAPOp;

/// [`structural_hash`]'s memoization cache: a hashed node's address to the
/// node (kept alive, so its address is not reused) and its hash.
pub type NodeToStructuralHashCache = HashMap<*const OperatorNode, (Rc<OperatorNode>, u64)>;

/// The operator with every child (operator inputs and the operator nodes
/// referenced from its scalar expressions alike) replaced by `()`. What
/// remains is the node's own data: variant tag, scalar expressions,
/// parameters.
fn own_fields(node: &OperatorNode) -> Operator<()> {
    node.operator.map_children(|_| ())
}

/// Coarse structural hash used only to bucket [`HashConsingTable`]'s
/// candidate search — never the sharing decision ([`same_node`] is).
///
/// `OperatorNode` carries `f64`s (`ScalarValue::Float64`, quantile targets,
/// `AccuracyGuarantee` bounds, …), so it cannot derive `std::hash::Hash`. The
/// hash is SipHash over two parts:
///
/// 1. the canonical JSON of [`own_fields`] plus `result_kind`, `schema`,
///    `guarantee`, `timing` and `materialization` — every field `PartialEq` compares except the
///    children. A scalar expression is serialized as data with each operator
///    node it reads replaced by `()`, so a reference to an
///    interned sub-DAG contributes nothing of its own here;
/// 2. for every child in [`OperatorNode::children`] order (operator inputs,
///    then scalar-referenced nodes), the child's own `structural_hash`,
///    memoized in `cache` by `Rc` pointer identity.
///
/// Part 2 is what makes equal sub-DAGs hash equal whether they are reached
/// through an operator input or through a `scalar(v)`, and what keeps the
/// pass linear: a node is generally a DAG, and re-serializing a shared
/// descendant once per parent would cost `O(sub-DAG)` per node instead of
/// `O(1)` beyond the children's already-known hashes. A non-finite `f64`
/// serializes as `null`, merely widening one (still equality-checked) bucket.
pub fn structural_hash(node: &OperatorNode, cache: &mut NodeToStructuralHashCache) -> u64 {
    let mut hasher = DefaultHasher::new();
    let own = (
        own_fields(node),
        node.result_kind,
        &node.schema,
        &node.guarantee,
        node.timing,
        node.materialization,
    );
    serde_json::to_string(&own)
        .unwrap_or_default()
        .hash(&mut hasher);
    for child in node.children() {
        cached_structural_hash(child, cache).hash(&mut hasher);
    }
    hasher.finish()
}

/// [`structural_hash`] of `node`, memoized in `cache`.
fn cached_structural_hash(node: &Rc<OperatorNode>, cache: &mut NodeToStructuralHashCache) -> u64 {
    if let Some((_, hash)) = cache.get(&Rc::as_ptr(node)) {
        return *hash;
    }
    let hash = structural_hash(node, cache);
    cache.insert(Rc::as_ptr(node), (Rc::clone(node), hash));
    hash
}

/// Numeric `PartialEq` alone conflates signed zeros. The serialized check is
/// additional evidence, never a replacement for typed equality (JSON maps
/// non-finite floats to `null`). Used for the guarantee, whose bounds are
/// floats a shared node must preserve bit-for-bit, and for summary families,
/// whose `SketchParams` equality omits the recorded error and probability.
pub fn same_value<T: PartialEq + serde::Serialize>(left: &T, right: &T) -> bool {
    left == right
        && match (serde_json::to_string(left), serde_json::to_string(right)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
}

/// Cache of child-pair comparisons already decided by [`same_node`], keyed by
/// pointer pair; both nodes must stay alive while it is used.
type NodePairToIsSameNodeCache = HashMap<(*const OperatorNode, *const OperatorNode), bool>;

/// The sharing decision: typed equality of two nodes.
///
/// `OperatorNode`'s derived `PartialEq` would recurse into children by value
/// even when both sides hold the same `Rc` (`OperatorNode` is not `Eq`, so
/// `Rc` gets no pointer shortcut), expanding a shared diamond once per path.
/// Children are therefore compared by pointer first; only when the pointers
/// differ (an equal child that was not legal to share) are the values
/// compared, memoized per pair so a diamond is still walked once.
fn same_node(
    left: &OperatorNode,
    right: &OperatorNode,
    cache: &mut NodePairToIsSameNodeCache,
) -> bool {
    let (lc, rc) = (left.children(), right.children());
    if lc.len() != rc.len() {
        return false;
    }
    let children_equal = lc.iter().zip(&rc).all(|(a, b)| {
        if Rc::ptr_eq(a, b) {
            return true;
        }
        let key = (Rc::as_ptr(a), Rc::as_ptr(b));
        if let Some(&eq) = cache.get(&key) {
            return eq;
        }
        let eq = same_node(a, b, cache);
        cache.insert(key, eq);
        eq
    });
    children_equal
        && left.result_kind == right.result_kind
        && left.schema == right.schema
        && left.timing == right.timing
        && left.materialization == right.materialization
        && same_value(&left.guarantee, &right.guarantee)
        && same_value(&own_fields(left), &own_fields(right))
}

/// Whether two equal copies of `node` return the same rows for the same
/// input rows, so they may be one node: an ASAP node, or a non-ASAP node
/// [`NonASAPOp::is_shareable_when_equal`].
pub fn shareable_when_equal(node: &OperatorNode) -> bool {
    node.is_asap()
        || node
            .non_asap()
            .is_some_and(NonASAPOp::is_shareable_when_equal)
}

/// Hash-consing table: each node added with its value (an interned node, a
/// memo group), found again by an equal node legal to share.
///
/// Buckets are keyed by [`structural_hash`] — a coarse candidate filter
/// only; [`same_node`] decides a match, so a hash collision between
/// different nodes only means a longer scan of one bucket.
#[derive(Clone)]
pub struct HashConsingTable<V> {
    buckets: HashMap<u64, Vec<(Rc<OperatorNode>, V)>>,
    /// Persisted for the table's whole lifetime so hashing is `O(1)` per node
    /// beyond its children.
    structural_hashes: NodeToStructuralHashCache,
}

impl<V> Default for HashConsingTable<V> {
    fn default() -> Self {
        Self {
            buckets: HashMap::new(),
            structural_hashes: HashMap::new(),
        }
    }
}

impl<V> std::fmt::Debug for HashConsingTable<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HashConsingTable")
            .field("nodes", &self.buckets.values().map(Vec::len).sum::<usize>())
            .finish_non_exhaustive()
    }
}

impl<V: Clone> HashConsingTable<V> {
    /// The value of a node added before that equals `node` ([`same_node`]),
    /// when sharing `node` is legal ([`shareable_when_equal`]); `None`
    /// otherwise.
    pub fn find(&mut self, node: &Rc<OperatorNode>) -> Option<V> {
        if !shareable_when_equal(node) {
            return None;
        }
        let hash = cached_structural_hash(node, &mut self.structural_hashes);
        let mut same_nodes = NodePairToIsSameNodeCache::new();
        self.buckets
            .get(&hash)?
            .iter()
            .find(|(candidate, _)| same_node(candidate, node, &mut same_nodes))
            .map(|(_, value)| value.clone())
    }

    /// Adds `node` with `value`. A node not legal to share is added too; it
    /// is never found for another node.
    pub fn insert(&mut self, node: Rc<OperatorNode>, value: V) {
        let hash = cached_structural_hash(&node, &mut self.structural_hashes);
        self.buckets.entry(hash).or_default().push((node, value));
    }
}
