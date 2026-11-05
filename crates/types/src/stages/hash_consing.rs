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
type NodeToStructuralHashCache = HashMap<*const OperatorNode, (Rc<OperatorNode>, u64)>;

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
fn structural_hash(node: &OperatorNode, cache: &mut NodeToStructuralHashCache) -> u64 {
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

/// [`structural_hash`] of `node`, reading `cache` but not adding to it: the
/// nodes hashed only for a lookup are memoized in `looked_up`, which lives
/// no longer than the borrowed `node` that keeps them alive.
fn looked_up_structural_hash(
    node: &OperatorNode,
    cache: &NodeToStructuralHashCache,
    looked_up: &mut HashMap<*const OperatorNode, u64>,
) -> u64 {
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
        let pointer = Rc::as_ptr(child);
        let hash = match cache.get(&pointer) {
            Some((_, hash)) => *hash,
            None => match looked_up.get(&pointer) {
                Some(hash) => *hash,
                None => {
                    let hash = looked_up_structural_hash(child, cache, looked_up);
                    looked_up.insert(pointer, hash);
                    hash
                }
            },
        };
        hash.hash(&mut hasher);
    }
    hasher.finish()
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
fn shareable_when_equal(node: &OperatorNode) -> bool {
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
    /// The added nodes' hashes (and their descendants'), persisted for the
    /// table's whole lifetime so hashing is `O(1)` per node beyond its
    /// children; a lookup ([`Self::find`]) adds nothing, so it keeps no node
    /// alive.
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
    pub fn find(&self, node: &Rc<OperatorNode>) -> Option<V> {
        if !shareable_when_equal(node) {
            return None;
        }
        let hash = match self.structural_hashes.get(&Rc::as_ptr(node)) {
            Some((_, hash)) => *hash,
            None => looked_up_structural_hash(node, &self.structural_hashes, &mut HashMap::new()),
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::operator::agg_intent::AggIntent;
    use crate::ir::operator::operator_properties::{BinaryOpKind, Reduction, Source};
    use crate::ir::properties::AccuracyTarget;
    use crate::ir::scalar::expr_ir::CompareOpKind;
    use crate::ir::schema::{DataType, Field, Schema};
    use crate::ir::{BinaryOperator, ScalarExpr};

    fn node(op: NonASAPOp) -> Rc<OperatorNode> {
        OperatorNode::new_shared(Operator::NonASAP(op)).unwrap()
    }

    /// `[ts, service, value, latency]`, no unique key.
    fn scan() -> Rc<OperatorNode> {
        node(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("service", DataType::Utf8, false),
                    Field::plain("value", DataType::Float64, false),
                    Field::plain("latency", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        })
    }

    fn quantile_agg(by: Vec<usize>, col: Option<usize>, q: f64) -> Rc<OperatorNode> {
        node(NonASAPOp::Aggregate {
            reduction: Reduction::by(by),
            measures: vec![AggIntent::Quantile {
                col,
                q,
                accuracy: AccuracyTarget::exact(),
            }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: scan(),
        })
    }

    fn compare(lhs: Rc<OperatorNode>, rhs: Rc<OperatorNode>) -> Rc<OperatorNode> {
        node(NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                checked_relative_division: false,
                checked_finite_division: false,
                kind: BinaryOpKind::Compare(CompareOpKind::Eq),
                vector_match: None,
            },
            return_bool: false,
            lhs,
            rhs,
        })
    }

    /// `vector(scalar(sum by (service) (m)))`.
    fn scalar_of_vector() -> Rc<OperatorNode> {
        let sum_up = node(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![1]),
            measures: vec![AggIntent::Sum { col: Some(2) }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: scan(),
        });
        node(NonASAPOp::PromqlVectorFromScalar(
            ScalarExpr::PromqlScalarFromVector(sum_up),
        ))
    }

    #[test]
    fn structural_hash_sees_through_a_scalar_reference() {
        // Two equal bridges must hash equal whether or not their referenced
        // vector is the same Rc: the reference contributes the vector's
        // memoized hash, not its identity.
        let a = scalar_of_vector();
        let b = scalar_of_vector();
        let mut cache = HashMap::new();
        assert_eq!(
            structural_hash(&a, &mut cache),
            structural_hash(&b, &mut cache)
        );
        assert_eq!(
            cache.len(),
            4,
            "aggregate + scan cached once per root: {cache:?}"
        );
        let other = node(NonASAPOp::PromqlVectorFromScalar(
            ScalarExpr::PromqlScalarFromVector(quantile_agg(vec![1], Some(2), 0.5)),
        ));
        assert_ne!(
            structural_hash(&a, &mut cache),
            structural_hash(&other, &mut cache)
        );
    }

    #[test]
    fn structural_hash_is_stable_across_cache_states() {
        let agg = quantile_agg(vec![1], Some(2), 0.5);
        let mut cold = HashMap::new();
        let mut warm = HashMap::new();
        structural_hash(&scan(), &mut warm);
        assert_eq!(
            structural_hash(&agg, &mut cold),
            structural_hash(&agg, &mut warm),
            "hash must be independent of unrelated cache state"
        );
    }

    #[test]
    fn structural_hash_of_an_internally_shared_dag_matches_the_unshared_equivalent() {
        let agg = quantile_agg(vec![1], Some(2), 0.5);
        let shared_root = compare(Rc::clone(&agg), Rc::clone(&agg));
        let unshared_root = compare(
            quantile_agg(vec![1], Some(2), 0.5),
            quantile_agg(vec![1], Some(2), 0.5),
        );
        assert_eq!(
            structural_hash(&shared_root, &mut HashMap::new()),
            structural_hash(&unshared_root, &mut HashMap::new()),
        );
    }

    #[test]
    fn structural_hash_memoizes_a_shared_descendant_exactly_once() {
        let agg = quantile_agg(vec![1], Some(2), 0.5);
        let root = compare(Rc::clone(&agg), Rc::clone(&agg));
        let mut cache = HashMap::new();
        structural_hash(&root, &mut cache);
        assert_eq!(
            cache.len(),
            2,
            "one entry per unique node in the shared branch (Aggregate + Scan): {cache:?}"
        );
    }
}
