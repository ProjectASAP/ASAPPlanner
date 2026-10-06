//! Structural common-subexpression elimination over the unified operator IR:
//! bottom-up hash-consing of [`OperatorNode`] DAGs across a workload's roots.
//!
//! CSE only runs on already-bound, already-canonicalized plans — structural
//! matching is meaningless before canonicalization has converged
//! semantically-equivalent queries onto one shape. [`share_common_sub_dags`]
//! is the single entry point, run once per workload batch (a batch of one
//! still deduplicates a query's own repeated sub-DAGs, see below).
//!
//! ## Algorithm: classic hash-consing / value-numbering
//!
//! Bottom-up: every child is interned before its parent, so two parents whose
//! children were independently deduplicated down to the same `Rc`s are
//! structurally identical iff their own fields also match, without re-walking
//! the sub-DAGs. "Child" means everything [`OperatorNode::children`] returns:
//! the operator inputs *and* the operator nodes a scalar expression reads
//! (`PromqlScalarFromVector`, `ScalarSubquery`, `Exists`, `InSubquery`), so a
//! vector read by `scalar(v)` in two queries is shared like any other input.
//! The scalar expressions themselves stay opaque data on their owning node.
//!
//! ## Correctness: hash is a filter, `PartialEq` is the decision
//!
//! This is the one non-negotiable rule. A **false positive** here — two
//! sub-DAGs wrongly judged shareable — is a wrong query answer, not a missed
//! optimization: two different queries would read each other's data.
//! [`structural_hash`] (SipHash over a canonical serialization, no
//! collision-freedom guarantee) may only narrow the candidate set within one
//! bucket; the typed equality check on that bucket ([`same_node`]) is what
//! actually decides sharing, every time, no exceptions for "the hash probably
//! didn't collide." Equality is intentionally conservative: it recognizes
//! *exact* structural matches only, never "a stricter-accuracy summary could
//! also answer a looser request" (that subsumption question belongs to the
//! ASAP matcher, not here).
//!
//! ## Legality
//!
//! Structural equality is necessary but not sufficient. A non-ASAP node is
//! only ever *returned* as a match for another when its output has a provable
//! unique key (`Schema::has_unique_key()`): a producer's output can only be
//! shared across consumers when its row identity is stable across reads, so
//! an ungrouped aggregate, a `without(..)` grouping, a `Concat`/`SetOp` that
//! drops its keys, … is always inserted fresh even when it is structurally
//! identical to something already interned. An ASAP node (summary state and
//! its evaluations) has no such gate: equal operator, schema and guarantee make
//! it shareable, exactly as post-ASAP sharing decided before this IR.
//!
//! ## Single-query CSE falls out for free
//!
//! A repeated sub-expression within *one* query (the same grouped aggregate on
//! both `BinaryOp` branches) is deduplicated by the same bottom-up interning —
//! a workload of size one still interns bottom-up within that one DAG.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use super::node::{Operator, OperatorNode};

/// [`structural_hash`]'s memoization cache: an already-hashed node's `Rc`
/// pointer to its hash. A fresh cache is always correct; what matters is
/// letting it persist across every node of one bottom-up pass rather than
/// starting a new one per call. The caller must keep every cached node alive
/// for the cache's lifetime, or a reused address would alias a stale entry.
pub type HashCache = HashMap<*const OperatorNode, u64>;

/// The operator with every child (operator inputs and the operator nodes
/// referenced from its scalar expressions alike) replaced by `()`. What
/// remains is the node's own data: variant tag, scalar expressions,
/// parameters.
fn own_fields(node: &OperatorNode) -> Operator<()> {
    node.operator.map_children(|_| ())
}

/// Coarse structural hash used only to bucket [`InternTable::intern`]'s
/// candidate search — never the sharing decision ([`same_node`] is).
///
/// `OperatorNode` carries `f64`s (`ScalarValue::Float64`, quantile targets,
/// `ResultGuarantee` bounds, …), so it cannot derive `std::hash::Hash`. The
/// hash is SipHash over two parts:
///
/// 1. the canonical JSON of [`own_fields`] plus `result_kind`, `schema`,
///    `guarantee` and `timing` — every field `PartialEq` compares except the
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
pub fn structural_hash(node: &OperatorNode, cache: &mut HashCache) -> u64 {
    fn child_hash(child: &Rc<OperatorNode>, cache: &mut HashCache) -> u64 {
        let ptr = Rc::as_ptr(child);
        if let Some(&h) = cache.get(&ptr) {
            return h;
        }
        let h = structural_hash(child, cache);
        cache.insert(ptr, h);
        h
    }

    let mut hasher = DefaultHasher::new();
    let own = (
        own_fields(node),
        node.result_kind,
        &node.schema,
        &node.guarantee,
        node.timing,
    );
    serde_json::to_string(&own)
        .unwrap_or_default()
        .hash(&mut hasher);
    for child in node.children() {
        child_hash(child, cache).hash(&mut hasher);
    }
    hasher.finish()
}

/// Numeric `PartialEq` alone conflates signed zeros. The serialized check is
/// additional evidence, never a replacement for typed equality (JSON maps
/// non-finite floats to `null`). Used for the guarantee, whose bounds are
/// floats a shared node must preserve bit-for-bit.
fn same_value<T: PartialEq + serde::Serialize>(left: &T, right: &T) -> bool {
    left == right
        && match (serde_json::to_string(left), serde_json::to_string(right)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
}

/// Memo of child-pair comparisons already decided by [`same_node`], keyed by
/// pointer pair. Only interned (table-owned, hence alive) nodes are keys.
type EqMemo = HashMap<(*const OperatorNode, *const OperatorNode), bool>;

/// The sharing decision: typed equality of two nodes.
///
/// `OperatorNode`'s derived `PartialEq` would recurse into children by value
/// even when both sides hold the same `Rc` (`OperatorNode` is not `Eq`, so
/// `Rc` gets no pointer shortcut), expanding a shared diamond once per path.
/// Children are therefore compared by pointer first; only when the pointers
/// differ (an equal child that was not legal to share) are the values
/// compared, memoized per pair so a diamond is still walked once.
fn same_node(left: &OperatorNode, right: &OperatorNode, memo: &mut EqMemo) -> bool {
    let (lc, rc) = (left.children(), right.children());
    if lc.len() != rc.len() {
        return false;
    }
    let children_equal = lc.iter().zip(&rc).all(|(a, b)| {
        if Rc::ptr_eq(a, b) {
            return true;
        }
        let key = (Rc::as_ptr(a), Rc::as_ptr(b));
        if let Some(&eq) = memo.get(&key) {
            return eq;
        }
        let eq = same_node(a, b, memo);
        memo.insert(key, eq);
        eq
    });
    children_equal
        && left.result_kind == right.result_kind
        && left.schema == right.schema
        && left.timing == right.timing
        && same_value(&left.guarantee, &right.guarantee)
        && same_value(&own_fields(left), &own_fields(right))
}

/// Bottom-up hash-consing table: structurally-equal, sharing-legal nodes
/// collapse onto one `Rc`.
///
/// `buckets` is keyed by [`structural_hash`] — a coarse candidate filter
/// only. Every entry within one bucket is a full node kept around for the
/// [`same_node`] comparison that actually decides a match; a hash collision
/// between structurally different nodes just means a harmless linear scan of
/// a few extra candidates.
struct InternTable {
    buckets: HashMap<u64, Vec<Rc<OperatorNode>>>,
    /// Persisted for the table's whole lifetime so hashing is `O(1)` per node
    /// beyond its children; every cached node is owned by `buckets`.
    hash_cache: HashCache,
    eq_memo: EqMemo,
}

impl InternTable {
    fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            hash_cache: HashMap::new(),
            eq_memo: HashMap::new(),
        }
    }

    /// Intern one node whose children are already interned: look it up by
    /// [`structural_hash`], confirm with [`same_node`], and — only when
    /// sharing is legal (module doc, "Legality") — return the existing `Rc`
    /// instead of allocating a new one.
    fn intern(&mut self, node: OperatorNode) -> Rc<OperatorNode> {
        let hash = structural_hash(&node, &mut self.hash_cache);
        // A node that is not legal to share is never *returned* as a match
        // for something else; it still occupies a fresh slot in the bucket
        // (harmless: later scans require legality of the new node too).
        let reusable = node.is_asap() || node.schema.has_unique_key();
        let bucket = self.buckets.entry(hash).or_default();
        if reusable {
            if let Some(existing) = bucket
                .iter()
                .find(|candidate| same_node(candidate, &node, &mut self.eq_memo))
            {
                return Rc::clone(existing);
            }
        }
        let rc = Rc::new(node);
        bucket.push(Rc::clone(&rc));
        rc
    }
}

/// Count of *unique* nodes reachable from `root` (pointer identity,
/// following [`OperatorNode::children`]): the real size of the DAG, not a
/// tree-walk count that re-counts a shared descendant once per parent.
pub fn dag_node_count(root: &Rc<OperatorNode>) -> usize {
    OperatorNode::reachable(root).len()
}

/// Input pointer → (input `Rc`, interned result). The input `Rc` is retained
/// so its address cannot be freed and reused by a fresh allocation while the
/// memo still maps it.
type Visited = HashMap<*const OperatorNode, (Rc<OperatorNode>, Rc<OperatorNode>)>;

/// Intern `node`'s children (recursively), then `node` itself. The rebuilt
/// node keeps `node`'s retained schema, result kind, guarantee and timing:
/// every child is replaced by an equal node, so each derived property stays
/// valid, and the result is `PartialEq`-equal to the input.
fn intern_bottom_up(
    table: &mut InternTable,
    visited: &mut Visited,
    node: &Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    if let Some((_, interned)) = visited.get(&Rc::as_ptr(node)) {
        return Rc::clone(interned);
    }
    let operator = node
        .operator
        .map_children(|child| intern_bottom_up(table, visited, child));
    let rebuilt = OperatorNode {
        operator,
        result_kind: node.result_kind,
        schema: node.schema.clone(),
        guarantee: node.guarantee.clone(),
        timing: node.timing,
    };
    let interned = table.intern(rebuilt);
    visited.insert(Rc::as_ptr(node), (Rc::clone(node), Rc::clone(&interned)));
    interned
}

/// Share structurally-identical, sharing-legal sub-DAGs across a workload's
/// roots (or within one root). Every root's *value* is unchanged
/// (`PartialEq`-equal to its input) — only its internal `Rc` structure may
/// now alias another root's, or another part of its own DAG. A node already
/// reached through two paths is visited once.
///
/// `Id` is caller-chosen — a workload entry's key, an index, a query name.
pub fn share_common_sub_dags<Id>(
    roots: Vec<(Id, Rc<OperatorNode>)>,
) -> Vec<(Id, Rc<OperatorNode>)> {
    let mut table = InternTable::new();
    let mut visited = Visited::new();
    roots
        .into_iter()
        .map(|(id, root)| (id, intern_bottom_up(&mut table, &mut visited, &root)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::asap::ASAPOp;
    use crate::ir::operator_properties::{BinaryOpKind, GroupKeys, Reduction, Source};
    use crate::ir::ScalarExpr;
    use crate::ir::{BinaryOperator, NonASAPOp};
    use crate::post_asap::guarantee::ResultGuarantee;
    use crate::post_asap::sketch::{
        GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryUpdate,
    };
    use crate::pre_asap::agg_intent::AggIntent;
    use crate::pre_asap::expr_ir::{ColumnRef, CompareOpKind};
    use crate::pre_asap::schema::{DataType, Field, FieldDataType, Schema};

    use crate::types::AccuracyTarget;

    fn node(op: NonASAPOp) -> Rc<OperatorNode> {
        OperatorNode::new_shared(crate::ir::Operator::NonASAP(op)).unwrap()
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
                accuracy: AccuracyTarget::Exact,
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

    fn two_roots(a: Rc<OperatorNode>, b: Rc<OperatorNode>) -> (Rc<OperatorNode>, Rc<OperatorNode>) {
        let shared = share_common_sub_dags(vec![("a", a), ("b", b)]);
        let [(_, ra), (_, rb)] = shared.as_slice() else {
            panic!("expected 2 roots");
        };
        (Rc::clone(ra), Rc::clone(rb))
    }

    #[test]
    fn distinct_column_quantiles_do_not_merge() {
        // Grouped (unique key present) so only the differing `col` blocks it.
        let (ra, rb) = two_roots(
            quantile_agg(vec![1], Some(2), 0.5),
            quantile_agg(vec![1], Some(3), 0.5),
        );
        assert!(!Rc::ptr_eq(&ra, &rb));
        assert_ne!(ra, rb);
    }

    #[test]
    fn no_unique_keys_means_no_merge_even_when_structurally_identical() {
        let a = quantile_agg(vec![], Some(2), 0.9);
        let b = quantile_agg(vec![], Some(2), 0.9);
        assert_eq!(a, b, "fixture sanity: structurally equal");
        assert!(
            !a.schema.has_unique_key(),
            "fixture sanity: a global aggregate has no provable unique key"
        );
        let (ra, rb) = two_roots(a, b);
        assert!(
            !Rc::ptr_eq(&ra, &rb),
            "no unique key ⇒ never hoisted, even for an identical structural match"
        );
    }

    #[test]
    fn median_and_explicit_half_percentile_merge() {
        // Two spellings that lower to the identical grouped `Quantile { q: 0.5 }`.
        let (m, p) = two_roots(
            quantile_agg(vec![1], Some(2), 0.5),
            quantile_agg(vec![1], Some(2), 0.5),
        );
        assert!(Rc::ptr_eq(&m, &p));
    }

    #[test]
    fn single_query_shares_its_own_repeated_sub_dag() {
        // One root with the same grouped aggregate on both branches, built as
        // two separately-allocated sub-DAGs (no sharing yet).
        let root = compare(
            quantile_agg(vec![1], Some(2), 0.5),
            quantile_agg(vec![1], Some(2), 0.5),
        );
        let shared = share_common_sub_dags(vec![("q", root)]);
        let [(_, root)] = shared.as_slice() else {
            panic!("expected 1 root");
        };
        let Some(NonASAPOp::BinaryOp { lhs, rhs, .. }) = root.non_asap() else {
            panic!("expected BinaryOp root, got {root:?}");
        };
        assert!(Rc::ptr_eq(lhs, rhs));
    }

    #[test]
    fn shared_root_value_is_unchanged() {
        let a = quantile_agg(vec![1], Some(2), 0.5)
            .as_ref()
            .clone()
            .with_guarantee(Some(ResultGuarantee::exact("fixture")));
        let before = Rc::new(a);
        let (ra, _) = two_roots(Rc::clone(&before), Rc::clone(&before));
        assert_eq!(ra.as_ref(), before.as_ref());
        assert!(
            ra.guarantee.is_some(),
            "retained properties survive the rebuild"
        );
    }

    // ── scalar-referenced sub-DAGs ──────────────────────────────────────

    /// `vector(scalar(sum by (service) (up)))`.
    fn scalar_of_vector() -> Rc<OperatorNode> {
        let sum_up = node(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![1]),
            measures: vec![AggIntent::Sum { col: Some(2) }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: scan(),
        });
        assert!(sum_up.schema.has_unique_key(), "fixture sanity");
        node(NonASAPOp::PromqlVectorFromScalar(
            ScalarExpr::PromqlScalarFromVector(sum_up),
        ))
    }

    fn bridged_vector(root: &Rc<OperatorNode>) -> &Rc<OperatorNode> {
        match root.non_asap() {
            Some(NonASAPOp::PromqlVectorFromScalar(ScalarExpr::PromqlScalarFromVector(v))) => v,
            other => panic!("expected vector(scalar(v)), got {other:?}"),
        }
    }

    #[test]
    fn scalar_referenced_vector_is_shared_across_queries() {
        let (ra, rb) = two_roots(scalar_of_vector(), scalar_of_vector());
        assert!(
            Rc::ptr_eq(bridged_vector(&ra), bridged_vector(&rb)),
            "the vector read by scalar(v) is a child and must be interned"
        );
        assert!(
            !Rc::ptr_eq(&ra, &rb),
            "the scalar bridge itself has no unique key and stays separate"
        );
    }

    #[test]
    fn structural_hash_sees_through_a_scalar_reference() {
        // Two equal bridges must hash equal whether or not their referenced
        // vector is the same Rc — the reference contributes the vector's
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

    // ── ASAP nodes ──────────────────────────────────────────────────────

    fn summary_agg(alpha: f64, guarantee: Option<ResultGuarantee>) -> Rc<OperatorNode> {
        let family = FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::DDSketch, SketchParams::DDSketch { alpha }),
            GroupingStrategy::default(),
        );
        let schema = Schema::lifted(vec![Field::new("state", family.clone(), false)], None);
        assert!(!schema.has_unique_key(), "fixture sanity");
        Rc::new(
            OperatorNode::with_schema(
                Operator::ASAP(ASAPOp::SummaryAgg {
                    child: scan(),
                    family,
                    input: SummaryUpdate::column(ColumnRef::SampleValue),
                    reduction: Reduction::PerEntity,
                    grouping: GroupingStrategy::default(),
                    filter: None,
                }),
                schema,
            )
            .with_guarantee(guarantee),
        )
    }

    #[test]
    fn asap_nodes_share_without_a_unique_key() {
        let exact = || Some(ResultGuarantee::exact("fixture"));
        let (ra, rb) = two_roots(summary_agg(0.01, exact()), summary_agg(0.01, exact()));
        assert!(Rc::ptr_eq(&ra, &rb));
        assert!(ra.guarantee.is_some());
    }

    #[test]
    fn asap_nodes_with_distinct_parameters_or_guarantees_are_not_shared() {
        let exact = || Some(ResultGuarantee::exact("fixture"));
        let (ra, rb) = two_roots(summary_agg(0.01, exact()), summary_agg(0.001, exact()));
        assert!(!Rc::ptr_eq(&ra, &rb), "different sketch parameters");
        let (ra, rb) = two_roots(summary_agg(0.01, exact()), summary_agg(0.01, None));
        assert!(
            !Rc::ptr_eq(&ra, &rb),
            "an unknown guarantee never borrows an exact one"
        );
        assert!(rb.guarantee.is_none());
    }

    #[test]
    fn evaluations_share_their_producer_but_not_each_other() {
        use crate::post_asap::sketch::SketchStatistic;
        let evaluation = |q: f64| {
            Rc::new(OperatorNode::with_schema(
                Operator::ASAP(ASAPOp::SummaryEstimate {
                    summary_input: summary_agg(0.01, None),
                    query: SketchStatistic::Quantile { q },
                }),
                Schema::lifted(
                    vec![Field::plain("quantile", DataType::Float64, false)],
                    None,
                ),
            ))
        };
        let (p95, p99) = two_roots(evaluation(0.95), evaluation(0.99));
        let producer = |n: &Rc<OperatorNode>| Rc::clone(n.children()[0]);
        assert!(!Rc::ptr_eq(&p95, &p99));
        assert!(Rc::ptr_eq(&producer(&p95), &producer(&p99)));
    }

    // ── structural_hash (DAG-aware memoization) ─────────────────────────

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

    // ── dag_node_count ───────────────────────────────────────────────────

    #[test]
    fn dag_node_count_is_the_naive_count_when_nothing_is_shared() {
        assert_eq!(dag_node_count(&scan()), 1);
        assert_eq!(dag_node_count(&quantile_agg(vec![1], Some(2), 0.5)), 2);
        assert_eq!(
            dag_node_count(&scalar_of_vector()),
            3,
            "follows scalar references"
        );
    }

    #[test]
    fn dag_node_count_deduplicates_an_internally_shared_sub_dag() {
        let root = compare(
            quantile_agg(vec![1], Some(2), 0.5),
            quantile_agg(vec![1], Some(2), 0.5),
        );
        assert_eq!(
            dag_node_count(&root),
            5,
            "fixture sanity: nothing shared yet"
        );
        let shared = share_common_sub_dags(vec![("q", root)]);
        let [(_, root)] = shared.as_slice() else {
            panic!("expected 1 root");
        };
        assert_eq!(
            dag_node_count(root),
            3,
            "BinaryOp + one Aggregate + its Scan"
        );
    }

    #[test]
    fn dag_node_count_deduplicates_across_two_workload_roots() {
        let (ra, rb) = two_roots(
            quantile_agg(vec![1], Some(2), 0.5),
            quantile_agg(vec![1], Some(2), 0.5),
        );
        assert!(Rc::ptr_eq(&ra, &rb), "fixture sanity: the two roots merged");
        assert_eq!(dag_node_count(&ra), 2);
        assert_eq!(dag_node_count(&rb), 2);
    }

    #[test]
    fn dedup_gates_sharing_the_same_as_aggregate() {
        // `Dedup { cols }` adds `cols` as a unique key, so two identical
        // `Dedup`s merge even though their keyless `Scan`s could not.
        let dedup = || {
            node(NonASAPOp::Dedup {
                cols: vec![1],
                child: scan(),
            })
        };
        let (ra, rb) = two_roots(dedup(), dedup());
        assert!(Rc::ptr_eq(&ra, &rb));
    }

    #[test]
    fn group_keys_gate_still_prevented_when_partition_by_without_used() {
        let without_agg = || {
            node(NonASAPOp::Aggregate {
                reduction: Reduction::Reduce(GroupKeys::without(vec![0])),
                measures: vec![AggIntent::Count {
                    accuracy: AccuracyTarget::Exact,
                }],
                output_names: vec![],
                filters: vec![],
                having: None,
                child: scan(),
            })
        };
        let a = without_agg();
        assert!(!a.schema.has_unique_key());
        let (ra, rb) = two_roots(a, without_agg());
        assert!(!Rc::ptr_eq(&ra, &rb));
    }

    #[test]
    fn already_shared_nodes_are_visited_once() {
        // A diamond already present in the input stays one node and is not
        // re-interned per path.
        let agg = quantile_agg(vec![1], Some(2), 0.5);
        let root = compare(Rc::clone(&agg), Rc::clone(&agg));
        let shared = share_common_sub_dags(vec![("q", root)]);
        let Some(NonASAPOp::BinaryOp { lhs, rhs, .. }) = shared[0].1.non_asap() else {
            panic!("expected BinaryOp root");
        };
        assert!(Rc::ptr_eq(lhs, rhs));
        assert_eq!(dag_node_count(&shared[0].1), 3);
    }

    // Comparing a shareable node whose equal-but-unshareable children form a
    // deep diamond must not expand the diamond once per path. The timeout is
    // a coarse runaway guard, not a performance SLA.
    #[test]
    fn shared_diamond_does_not_expand_during_comparison() {
        let (done, completion) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            fn keyed_diamond() -> Rc<OperatorNode> {
                // BinaryOp over a keyless scan has no unique key at any level,
                // so none of the 24 levels is shareable; the `Dedup` on top is.
                let mut current = scan();
                for _ in 0..24 {
                    current = compare(Rc::clone(&current), current);
                }
                node(NonASAPOp::Dedup {
                    cols: vec![1],
                    child: current,
                })
            }
            let (ra, rb) = two_roots(keyed_diamond(), keyed_diamond());
            assert!(Rc::ptr_eq(&ra, &rb));
            done.send(()).unwrap();
        });
        completion
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("comparison expanded the shared DAG");
        worker.join().unwrap();
    }

    /// A keyed (hence shareable) projection emitting the literal `value`.
    fn keyed_literal(value: f64) -> Rc<OperatorNode> {
        let keyed = node(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("service", DataType::Utf8, false),
                ],
                0,
                vec![vec![1]],
            ),
        });
        node(NonASAPOp::Project {
            cols: vec![
                crate::ir::ProjectItem {
                    alias: None,
                    expr: ScalarExpr::Column(1),
                },
                crate::ir::ProjectItem {
                    alias: Some("v".into()),
                    expr: ScalarExpr::literal_f64(value),
                },
            ],
            qualifier: None,
            child: keyed,
        })
    }

    /// Sharing preserves IEEE signed zero, and JSON's `null` encoding of
    /// non-finite floats never becomes the equality decision.
    #[test]
    fn signed_zero_and_nonfinite_values_remain_distinct() {
        assert!(
            keyed_literal(0.0).schema.has_unique_key(),
            "fixture is shareable"
        );
        for (a, b) in [
            (0.0, -0.0),
            (-0.0, 0.0),
            (f64::INFINITY, f64::NEG_INFINITY),
            (f64::NAN, f64::NAN),
        ] {
            let (ra, rb) = two_roots(keyed_literal(a), keyed_literal(b));
            assert!(!Rc::ptr_eq(&ra, &rb), "{a} and {b} must not be shared");
        }
        let (ra, rb) = two_roots(keyed_literal(f64::INFINITY), keyed_literal(f64::INFINITY));
        assert!(Rc::ptr_eq(&ra, &rb));
    }
}
