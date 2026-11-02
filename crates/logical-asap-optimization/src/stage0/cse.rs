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
//! only ever *returned* as a match for another when every execution over the
//! same input rows returns the same output rows
//! ([`NonASAPOp::is_shareable_when_equal`]). Equal nodes are
//! interned children first, so two equal nodes read one input, and one copy
//! returns each reader the rows it would have computed alone. A unique key
//! is not needed: equal per-series quantiles without a provable key are one
//! node. An operator that picks rows by input order alone (an unordered
//! `LIMIT`) is always inserted fresh, because two copies may return
//! different rows.
//! An ASAP node (summary state and its evaluations) has no such gate: equal
//! operator, schema and guarantee make it shareable, exactly as post-ASAP
//! sharing decided before this IR.
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

use asap_types::ir::operator::node::{Operator, OperatorNode};
use asap_types::ir::NonASAPOp;

/// [`structural_hash`]'s memoization cache: an already-hashed node's `Rc`
/// pointer to its hash. A fresh cache is always correct; what matters is
/// letting it persist across every node of one bottom-up pass rather than
/// starting a new one per call. The caller must keep every cached node alive
/// for the cache's lifetime, or a reused address would alias a stale entry.
pub(crate) type NodeToStructuralHashCache = HashMap<*const OperatorNode, u64>;

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
pub(crate) fn structural_hash(node: &OperatorNode, cache: &mut NodeToStructuralHashCache) -> u64 {
    fn child_hash(child: &Rc<OperatorNode>, cache: &mut NodeToStructuralHashCache) -> u64 {
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
        node.materialization,
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

/// Cache of child-pair comparisons already decided by [`same_node`], keyed by
/// pointer pair. Only interned (table-owned, hence alive) nodes are keys.
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
    structural_hash_cache: NodeToStructuralHashCache,
    same_node_cache: NodePairToIsSameNodeCache,
}

impl InternTable {
    fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            structural_hash_cache: HashMap::new(),
            same_node_cache: HashMap::new(),
        }
    }

    /// Intern one node whose children are already interned: look it up by
    /// [`structural_hash`], confirm with [`same_node`], and — only when
    /// sharing is legal (module doc, "Legality") — return the existing `Rc`
    /// instead of allocating a new one.
    fn intern(&mut self, node: OperatorNode) -> Rc<OperatorNode> {
        let hash = structural_hash(&node, &mut self.structural_hash_cache);
        // A node that is not legal to share is never *returned* as a match
        // for something else; it still occupies a fresh slot in the bucket
        // (harmless: later scans require legality of the new node too).
        let reusable = node.is_asap()
            || node
                .non_asap()
                .is_some_and(NonASAPOp::is_shareable_when_equal);
        let bucket = self.buckets.entry(hash).or_default();
        if reusable {
            if let Some(existing) = bucket
                .iter()
                .find(|candidate| same_node(candidate, &node, &mut self.same_node_cache))
            {
                return Rc::clone(existing);
            }
        }
        let rc = Rc::new(node);
        bucket.push(Rc::clone(&rc));
        rc
    }
}

/// Input pointer → (input `Rc`, interned result). The input `Rc` is retained
/// so its address cannot be freed and reused by a fresh allocation while the
/// cache still maps it.
type NodeToInternedNodeCache = HashMap<*const OperatorNode, (Rc<OperatorNode>, Rc<OperatorNode>)>;

/// Intern `node`'s children (recursively), then `node` itself. The rebuilt
/// node keeps `node`'s retained schema, result kind, guarantee, timing and
/// materialization:
/// every child is replaced by an equal node, so each derived property stays
/// valid, and the result is `PartialEq`-equal to the input.
fn intern_bottom_up(
    table: &mut InternTable,
    visited: &mut NodeToInternedNodeCache,
    node: &Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    if let Some((_, interned)) = visited.get(&Rc::as_ptr(node)) {
        return Rc::clone(interned);
    }
    let operator = node
        .operator
        .map_children(|child| intern_bottom_up(table, visited, child));
    let rebuilt = OperatorNode::with_schema(operator, node.schema.clone())
        .with_guarantee(node.guarantee.clone())
        .with_timing(node.timing)
        .with_materialization(node.materialization);
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
    let mut visited = NodeToInternedNodeCache::new();
    roots
        .into_iter()
        .map(|(id, root)| (id, intern_bottom_up(&mut table, &mut visited, &root)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::operator::agg_intent::AggIntent;
    use asap_types::ir::operator::asap::ASAPOp;
    use asap_types::ir::operator::operator_properties::{
        BinaryOpKind, GroupKeys, Reduction, Source,
    };
    use asap_types::ir::properties::guarantee::AccuracyGuarantee;
    use asap_types::ir::scalar::expr_ir::{ColumnRef, CompareOpKind};
    use asap_types::ir::schema::sketch::{
        GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SummaryUpdate,
    };
    use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
    use asap_types::ir::ScalarExpr;
    use asap_types::ir::{BinaryOperator, NonASAPOp};

    use asap_types::ir::properties::AccuracyTarget;

    fn node(op: NonASAPOp) -> Rc<OperatorNode> {
        OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(op)).unwrap()
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

    // Two aggregates that differ only in one measure's `FILTER` predicate
    // compute different values, so structural sharing keeps them apart.
    #[test]
    fn filtered_and_unfiltered_aggregates_do_not_merge() {
        use asap_types::ir::scalar::ScalarValue;
        use asap_types::ir::Predicate;
        let unfiltered = quantile_agg(vec![1], Some(2), 0.5);
        let filtered = node(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![1]),
            measures: vec![AggIntent::Quantile {
                col: Some(2),
                q: 0.5,
                accuracy: AccuracyTarget::exact(),
            }],
            output_names: vec![],
            filters: vec![Some(Predicate(ScalarExpr::Compare {
                left: Box::new(ScalarExpr::Column(3)),
                op: CompareOpKind::Gt,
                right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(1.0))),
                semantics: asap_types::ir::ExprSemantics::Sql,
            }))],
            having: None,
            child: scan(),
        });
        let (ra, rb) = two_roots(unfiltered, filtered);
        assert!(
            !Rc::ptr_eq(&ra, &rb),
            "a filtered measure must not be shared"
        );
        assert_ne!(ra, rb);
    }

    #[test]
    fn identical_global_scalar_aggregates_share_their_single_result() {
        let a = quantile_agg(vec![], Some(2), 0.9);
        let b = quantile_agg(vec![], Some(2), 0.9);
        assert_eq!(a, b, "fixture sanity: structurally equal");
        assert!(
            !a.schema.has_unique_key(),
            "fixture sanity: a global aggregate has no provable unique key"
        );
        let (ra, rb) = two_roots(a, b);
        assert!(
            Rc::ptr_eq(&ra, &rb),
            "a global scalar aggregate has one stable result per evaluation"
        );
    }

    /// A global scalar result can be shared only with the same computation.
    #[test]
    fn distinct_global_quantiles_do_not_merge() {
        let (a, b) = two_roots(
            quantile_agg(vec![], Some(2), 0.5),
            quantile_agg(vec![], Some(2), 0.9),
        );
        assert!(!Rc::ptr_eq(&a, &b));
        let (a, b) = two_roots(
            quantile_agg(vec![], Some(2), 0.5),
            quantile_agg(vec![], Some(3), 0.5),
        );
        assert!(!Rc::ptr_eq(&a, &b));
    }

    /// Two equal scans of a keyless table (SQL rows) are one node: each
    /// reader reads every row the scan outputs.
    #[test]
    fn equal_scans_without_a_unique_key_are_shared() {
        let table_scan = || {
            node(NonASAPOp::Scan {
                source: Source::Table {
                    table_ref: "metrics".into(),
                },
                predicates: vec![],
                schema: Schema::with_time_index(
                    vec![
                        Field::plain("ts", DataType::Timestamp, false),
                        Field::plain("city", DataType::Utf8, true),
                        Field::plain("value", DataType::Float64, true),
                    ],
                    0,
                    vec![],
                ),
            })
        };
        let a = table_scan();
        assert!(!a.schema.has_unique_key(), "fixture sanity: keyless rows");
        let (ra, rb) = two_roots(a, table_scan());
        assert!(Rc::ptr_eq(&ra, &rb));
    }

    /// Grouped quantiles differing only in accuracy keep two roots over one shared scan.
    #[test]
    fn differing_accuracy_is_never_merged_but_the_scan_child_is_shared() {
        let shared_scan = scan();
        let quantile = |epsilon: f64| {
            node(NonASAPOp::Aggregate {
                reduction: Reduction::by(vec![1]),
                measures: vec![AggIntent::Quantile {
                    col: Some(2),
                    q: 0.99,
                    accuracy: AccuracyTarget::max_error(epsilon),
                }],
                output_names: vec![],
                filters: vec![],
                having: None,
                child: Rc::clone(&shared_scan),
            })
        };
        let a = quantile(0.01);
        assert!(a.schema.has_unique_key(), "fixture sanity: grouped");
        let (ra, rb) = two_roots(a, quantile(0.05));
        assert!(!Rc::ptr_eq(&ra, &rb));
        assert_ne!(ra, rb);
        let (
            Some(NonASAPOp::Aggregate { child: child_a, .. }),
            Some(NonASAPOp::Aggregate { child: child_b, .. }),
        ) = (ra.non_asap(), rb.non_asap())
        else {
            panic!("expected Aggregate roots");
        };
        assert!(Rc::ptr_eq(child_a, child_b));
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
            .with_guarantee(Some(AccuracyGuarantee::exact("fixture")));
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
            Rc::ptr_eq(&ra, &rb),
            "the scalar bridge has no unique key but returns the same rows"
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

    fn summary_agg(alpha: f64, guarantee: Option<AccuracyGuarantee>) -> Rc<OperatorNode> {
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
        let exact = || Some(AccuracyGuarantee::exact("fixture"));
        let (ra, rb) = two_roots(summary_agg(0.01, exact()), summary_agg(0.01, exact()));
        assert!(Rc::ptr_eq(&ra, &rb));
        assert!(ra.guarantee.is_some());
    }

    #[test]
    fn asap_nodes_with_distinct_parameters_or_guarantees_are_not_shared() {
        let exact = || Some(AccuracyGuarantee::exact("fixture"));
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
        use asap_types::ir::schema::sketch::SketchStatistic;
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

    /// `DISTINCT` (every column) returns the same rows each time; a `Dedup`
    /// on some columns keeps an unspecified row per key.
    #[test]
    fn dedup_shares_only_on_every_column() {
        let dedup = |cols: Vec<usize>| {
            node(NonASAPOp::Dedup {
                cols,
                child: scan(),
            })
        };
        let (ra, rb) = two_roots(dedup(vec![]), dedup(vec![]));
        assert!(Rc::ptr_eq(&ra, &rb));
        let (ra, rb) = two_roots(dedup(vec![1]), dedup(vec![1]));
        assert!(!Rc::ptr_eq(&ra, &rb));
    }

    /// `LIMIT 10` without an order may return any 10 rows, so two copies may
    /// differ; one shared copy would make both readers see the same rows.
    #[test]
    fn unordered_limits_are_not_shared() {
        let limit = || {
            node(NonASAPOp::Limit {
                n: Some(10),
                offset: 0,
                partition_by: GroupKeys::default(),
                child: scan(),
            })
        };
        let (ra, rb) = two_roots(limit(), limit());
        assert!(!Rc::ptr_eq(&ra, &rb));
    }

    /// A `LIMIT` over a `Sort` with the same partitions keeps the first rows
    /// of a fixed order; over a `Sort` partitioned differently, which rows
    /// come first still depends on input order.
    #[test]
    fn limits_over_a_sort_with_their_partitions_are_shared() {
        let limit = |sort_partitions: GroupKeys| {
            node(NonASAPOp::Limit {
                n: Some(10),
                offset: 0,
                partition_by: GroupKeys::default(),
                child: node(NonASAPOp::Sort {
                    keys: vec![asap_types::ir::scalar::SortKey {
                        expr: ScalarExpr::column(2),
                        ascending: false,
                        nulls_first: false,
                    }],
                    partition_by: sort_partitions,
                    child: scan(),
                }),
            })
        };
        let (ra, rb) = two_roots(limit(GroupKeys::default()), limit(GroupKeys::default()));
        assert!(Rc::ptr_eq(&ra, &rb));
        let (ra, rb) = two_roots(limit(GroupKeys::by(vec![1])), limit(GroupKeys::by(vec![1])));
        assert!(!Rc::ptr_eq(&ra, &rb));
    }

    /// `count without (service)` has no provable unique key, but it computes
    /// the same rows from the same input, so equal copies are one node.
    #[test]
    fn without_grouping_is_shared() {
        let without_agg = || {
            node(NonASAPOp::Aggregate {
                reduction: Reduction::Reduce(GroupKeys::without(vec![0])),
                measures: vec![AggIntent::Count {
                    accuracy: AccuracyTarget::exact(),
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
        assert!(Rc::ptr_eq(&ra, &rb));
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
        assert_eq!(OperatorNode::reachable(&shared[0].1).len(), 3);
    }

    // Comparing a shareable node whose equal-but-unshareable children form a
    // deep diamond must not expand the diamond once per path. The timeout is
    // a coarse runaway guard, not a performance SLA.
    #[test]
    fn shared_diamond_does_not_expand_during_comparison() {
        let (done, completion) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            fn keyed_diamond() -> Rc<OperatorNode> {
                // An unordered `Limit` at each of the 24 levels is never
                // shared, so comparing the `Dedup` on top walks the diamond.
                let mut current = scan();
                for _ in 0..24 {
                    let limited = node(NonASAPOp::Limit {
                        n: Some(10),
                        offset: 0,
                        partition_by: GroupKeys::default(),
                        child: current,
                    });
                    current = compare(Rc::clone(&limited), limited);
                }
                node(NonASAPOp::Dedup {
                    cols: vec![],
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
                asap_types::ir::ProjectItem {
                    alias: None,
                    expr: ScalarExpr::Column(1),
                },
                asap_types::ir::ProjectItem {
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
