//! Pre-ASAP structural common-subexpression elimination: bottom-up
//! hash-consing over an already-`resolve_root`'d [`QueryExpr`] tree (issue
//! #212, #222, #223).
//!
//! CSE only runs on an already-bound, already-canonicalized tree —
//! structural matching is meaningless before canonicalization has converged
//! semantically-equivalent queries onto one shape (`docs/pre-asap-ir.md`
//! design principle 3; `median(latency)` and `approx_percentile_cont(latency,
//! 0.5)` already lower to an identical `AggIntent::Quantile` today, per
//! `sql_lowering.rs`'s `median_is_the_same_intent_as_an_explicit_half_percentile`
//! test). [`share_common_subtrees`] is the single entry point, run once per
//! workload batch (or once per query — see "Single-query CSE" below) *after*
//! `resolve_root`, *before* `implement_workload`
//! ([`asap_aware_mapping::implement_workload`]).
//!
//! ## Algorithm: classic hash-consing / value-numbering
//!
//! Bottom-up: every child is interned before its parent, so a parent's
//! candidacy for sharing naturally incorporates whether its own children were
//! themselves shared — two parents whose children were independently
//! deduplicated down to the same `Rc`s are structurally identical iff their
//! own fields also match, without re-walking the subtrees.
//!
//! Only the **relational skeleton** participates — the same set of "operator"
//! children [`canonicalize`](super::canonicalize)'s `children_mut` walks
//! (`Filter`/`Project`/`Aggregate`/`Concat`/`Join`/`BinaryOp`/…). A scalar
//! subexpression reachable only through a wrapper position (`Predicate`,
//! `ProjectItem.expr`, `Aggregate.having`, `SQLWindowFunc.args`, …) stays
//! embedded as opaque data on its owning operator node, compared by
//! `QueryExpr`'s derived `PartialEq` along with the rest of that node's
//! fields, rather than separately hash-consed — the same scope
//! `canonicalize.rs` settled on ("none of the rewrite rules touch a scalar
//! subtree, so there's nothing to gain by recursing into one"). Widening this
//! to scalar positions is future work, not attempted here.
//!
//! ## Correctness: hash is a filter, `PartialEq` is the decision
//!
//! This is the one non-negotiable rule. A **false positive** here — two
//! subtrees wrongly judged shareable — is a wrong query answer, not a missed
//! optimization: two different queries would read each other's data.
//! [`structural_hash`] (`DefaultHasher`/SipHash over a canonical
//! serialization, no collision-freedom guarantee) may only narrow the
//! candidate set within one bucket; [`InternTable::intern`]'s `PartialEq`
//! check on that bucket is what actually decides sharing, every time, no
//! exceptions for "the hash probably didn't collide."
//!
//! This also means the pass is safe by construction against the case #212
//! flagged as a real historical bug (issue #115): `AggIntent::Quantile`
//! carries its input column and its `AccuracyTarget`, both `PartialEq`
//! fields, so `Quantile(x, 0.99, ε=0.01)` and `Quantile(x, 0.99, ε=0.001)` —
//! or `Quantile(x, ..)` vs `Quantile(y, ..)` — are never merged. This is
//! intentionally conservative: it only recognizes *exact* structural
//! matches, not "a stricter-accuracy summary could also answer a looser
//! request." That subsumption question already has a documented,
//! deliberately-unfilled home (`asap_aware_mapping::boundary::Matcher`) —
//! CSE here does not attempt it.
//!
//! ## Legality: gated by `Schema::unique_keys`
//!
//! Structural equality alone is necessary but not sufficient. Per
//! [`Schema::unique_keys`](super::schema::Schema::unique_keys)'s own doc: "a
//! producer's output can only be safely shared across consumers when its row
//! identity is provably stable across reads." A candidate node with no
//! provable unique key (`Schema::has_unique_key()` false, or `output_schema`
//! not even defined for that node, e.g. a `Concat`/`SetOp` branch whose union
//! drops `unique_keys`, or an ungrouped/global `Aggregate`, whose empty `by`
//! also reports no unique key today) is **never** hoisted, even when it is
//! structurally identical to something already interned — it is always
//! inserted fresh, matching the rule the (now-deleted) prior CSE attempt
//! already encoded and the doc comment on `Aggregate`'s `child` field
//! ("`unique_keys` feeds CSE's producer-sharing legality check").
//!
//! ## Single-query CSE falls out for free
//!
//! A repeated sub-expression within *one* query (e.g. the same grouped
//! `Aggregate` referenced twice on two `BinaryOp` branches) is deduplicated
//! by the exact same bottom-up interning — a workload of size one still
//! interns bottom-up within that one tree. No separate mechanism is needed;
//! see the `single_query_shares_its_own_repeated_subtree` test below.
//!
//! ## Landing plan (issue #223)
//!
//! This module is stage 1 of a 4-stage plan. Stage 2
//! ([`asap_aware_mapping::implement_workload`]) is a real caller, wired at
//! the same time so this never becomes unwired dead code again (the original
//! `asap-plan::cse::dedupe_subtrees` was deleted in #192 for exactly that).
//! Stages 3 (`dag_export::structural_hash` unification) and 4 (`CostModel`
//! CSE credit) are deliberately deferred follow-ups, not attempted here.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use super::query_expr::QueryExpr;

/// Bottom-up hash-consing table: structurally-equal, sharing-legal
/// [`QueryExpr`] nodes collapse onto one `Rc`.
///
/// `buckets` is keyed by [`structural_hash`] — a coarse candidate filter
/// only (see the module-level "Correctness" section). Every entry within one
/// bucket is a full node kept around for the `PartialEq` comparison that
/// actually decides a match; a hash collision between structurally different
/// nodes just means a (harmless) linear scan of a few extra candidates.
struct InternTable {
    buckets: HashMap<u64, Vec<Rc<QueryExpr>>>,
}

impl InternTable {
    fn new() -> Self {
        Self {
            buckets: HashMap::new(),
        }
    }

    /// Intern one already-children-rebuilt node: look it up by
    /// [`structural_hash`], confirm with `PartialEq`, and — only when
    /// sharing is legal (see "Legality" above) — return the existing `Rc`
    /// instead of allocating a new one.
    fn intern(&mut self, node: QueryExpr) -> Rc<QueryExpr> {
        let hash = structural_hash(&node);
        // A node with no provable unique key is never *returned* as a match
        // for something else — it may still go on to occupy a fresh slot in
        // the bucket (harmless; it just never gets found by a later
        // `PartialEq` scan that also requires `reusable`).
        let reusable = node
            .output_schema()
            .is_ok_and(|schema| schema.has_unique_key());
        let bucket = self.buckets.entry(hash).or_default();
        if reusable {
            if let Some(existing) = bucket.iter().find(|candidate| candidate.as_ref() == &node) {
                return Rc::clone(existing);
            }
        }
        let rc = Rc::new(node);
        bucket.push(Rc::clone(&rc));
        rc
    }
}

/// Coarse structural hash used only to bucket [`InternTable::intern`]'s
/// candidate search — never the actual sharing decision (`PartialEq` is).
///
/// `QueryExpr` carries `f64`s (`Literal(ScalarValue::Float64)`, `AggIntent::Quantile.q`, …), so it
/// cannot derive `std::hash::Hash`. Serializing to a canonical JSON string
/// and hashing that sidesteps the `f64` problem the same way
/// `dag_export.rs`'s own `structural_hash` does — a deliberately independent
/// implementation for now (this module's stage 1; unifying the two is stage
/// 3 of issue #223's landing plan, not done here). A NaN/infinite `f64`
/// makes JSON serialization fail; falling back to a fixed hash just puts
/// every such node in one (larger, still `PartialEq`-disambiguated) bucket.
fn structural_hash(node: &QueryExpr) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let canonical = serde_json::to_string(node).unwrap_or_default();
    canonical.hash(&mut hasher);
    hasher.finish()
}

/// Recurse into `child`, then intern the result. `Rc::try_unwrap` recovers
/// the owned node without cloning in the overwhelmingly common case — a
/// tree freshly built by a front end / `resolve_root`, not yet shared by any
/// prior CSE pass, where every `Rc` is uniquely owned. Falls back to cloning
/// this node's own fields (its children stay `Rc`s, not deep-copied) only
/// when `child` is already shared — e.g. re-running CSE over a tree that
/// went through a previous `share_common_subtrees` pass; a structural
/// duplicate collapses right back onto `child` itself via `PartialEq`, an
/// already-optimal no-op.
fn intern_child(table: &mut InternTable, child: Rc<QueryExpr>) -> Rc<QueryExpr> {
    match Rc::try_unwrap(child) {
        Ok(owned) => intern_bottom_up(table, owned),
        Err(shared) => intern_bottom_up(table, (*shared).clone()),
    }
}

/// Like [`intern_child`], for a `Concat` branch — stored by value
/// (`Vec<QueryExpr>`, not `Rc<QueryExpr>`), so this position itself can never
/// alias another parent. Interning it anyway still lets any `Rc`-typed
/// descendant of the branch participate in sharing, and registers the
/// branch's own hash/value in the table for a *different* `Concat` elsewhere
/// with a structurally identical branch (which — being in its own `Vec`
/// slot too — still can't literally share the `Rc`, but this keeps the
/// interning behavior uniform and the table's bucket contents consistent).
fn intern_owned(table: &mut InternTable, expr: QueryExpr) -> QueryExpr {
    let rc = intern_bottom_up(table, expr);
    Rc::try_unwrap(rc).unwrap_or_else(|shared| (*shared).clone())
}

/// Bottom-up: rebuild `expr`'s children (recursively interning each), then
/// intern the rebuilt node itself.
fn intern_bottom_up(table: &mut InternTable, expr: QueryExpr) -> Rc<QueryExpr> {
    let rebuilt = rebuild_children(table, expr);
    table.intern(rebuilt)
}

/// Rebuild `expr` with each **operator** child (see the module doc on scope)
/// replaced by its interned `Rc`. Exhaustive over every `QueryExpr` variant,
/// matching `canonicalize.rs`'s `children_mut` exactly in which fields count
/// as an operator child — new variants fail to compile here until this match
/// is extended.
fn rebuild_children(table: &mut InternTable, expr: QueryExpr) -> QueryExpr {
    use QueryExpr::*;
    match expr {
        Scan { .. } | QueryTimestamp => expr,
        PromqlVectorFromScalar(c) => PromqlVectorFromScalar(intern_child(table, c)),
        PromqlScalarFromVector(c) => PromqlScalarFromVector(intern_child(table, c)),
        PromqlRelabel { dst, value, child } => PromqlRelabel {
            dst,
            value,
            child: intern_child(table, child),
        },
        PromqlInfoEnrich { selector, child } => PromqlInfoEnrich {
            selector,
            child: intern_child(table, child),
        },
        PromqlSeriesSample { by, kind, child } => PromqlSeriesSample {
            by,
            kind,
            child: intern_child(table, child),
        },
        Filter { pred, child } => Filter {
            pred,
            child: intern_child(table, child),
        },
        Project {
            cols,
            qualifier,
            child,
        } => Project {
            cols,
            qualifier,
            child: intern_child(table, child),
        },
        Aggregate {
            reduction,
            measures,
            output_names,
            having,
            child,
        } => Aggregate {
            reduction,
            measures,
            output_names,
            having,
            child: intern_child(table, child),
        },
        Dedup { cols, child } => Dedup {
            cols,
            child: intern_child(table, child),
        },
        Concat { children } => Concat {
            children: children
                .into_iter()
                .map(|c| intern_owned(table, c))
                .collect(),
        },
        Join {
            kind,
            pred,
            left,
            right,
        } => Join {
            kind,
            pred,
            left: intern_child(table, left),
            right: intern_child(table, right),
        },
        SetOp {
            kind,
            all,
            left,
            right,
        } => SetOp {
            kind,
            all,
            left: intern_child(table, left),
            right: intern_child(table, right),
        },
        Sort {
            keys,
            partition_by,
            child,
        } => Sort {
            keys,
            partition_by,
            child: intern_child(table, child),
        },
        Limit { n, offset, child } => Limit {
            n,
            offset,
            child: intern_child(table, child),
        },
        PromqlSubquery {
            range,
            resolution,
            child,
        } => PromqlSubquery {
            range,
            resolution,
            child: intern_child(table, child),
        },
        TimeRange { range, child } => TimeRange {
            range,
            child: intern_child(table, child),
        },
        TimeShift { shift, child } => TimeShift {
            shift,
            child: intern_child(table, child),
        },
        SQLWindowFunc {
            func,
            args,
            partition_by,
            order_by,
            output_name,
            child,
        } => SQLWindowFunc {
            func,
            args,
            partition_by,
            order_by,
            output_name,
            child: intern_child(table, child),
        },
        BinaryOp {
            op,
            lhs,
            rhs,
            vector_match,
        } => BinaryOp {
            op,
            lhs: intern_child(table, lhs),
            rhs: intern_child(table, rhs),
            vector_match,
        },
        // `PromqlScalarBridge`'s child is a scalar-sub-language node (issue
        // #220) — same "never descended into" treatment as the scalar
        // variants below; the whole bridge node is still interned as a unit
        // by the `table.intern(rebuilt)` call in `intern_bottom_up`.
        PromqlScalarBridge(_) => expr,
        // Scalar variants (issue #205) — never descended into; see the
        // module doc's "Algorithm" section on scope. Left byte-for-byte
        // unchanged: predicate / project-list / sort-key / window-arg
        // expressions stay embedded as opaque leaf data, compared by the
        // enclosing operator node's derived `PartialEq`.
        Column(_)
        | Literal(_)
        | Compare { .. }
        | BoolAnd(_)
        | BoolOr(_)
        | Not(_)
        | IsNull(_)
        | IsNotNull(_)
        | Cast { .. }
        | InList { .. }
        | FunctionCall { .. }
        | Arithmetic { .. }
        | Case { .. } => expr,
    }
}

/// Share structurally-identical, sharing-legal subtrees across a workload's
/// query roots (or within one query, for `roots.len() == 1` — see the
/// module doc's "Single-query CSE" section). Every root's *value* is
/// unchanged (`PartialEq`-equal to its input) — only its internal `Rc`
/// structure may now alias another root's, or another part of its own tree.
///
/// `roots` must already be bound + canonicalized (post-`resolve_root`).
/// `Id` is caller-chosen — a `QueryWorkload` entry's own key, an index, a
/// query name, whatever identifies one root through the pipeline; this
/// module has no opinion on its shape.
pub fn share_common_subtrees<Id>(roots: Vec<(Id, QueryExpr)>) -> Vec<(Id, Rc<QueryExpr>)> {
    let mut table = InternTable::new();
    roots
        .into_iter()
        .map(|(id, expr)| (id, intern_bottom_up(&mut table, expr)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pre_asap::agg_intent::AggIntent;
    use crate::pre_asap::query_expr::{BinaryOpKind, GroupKeys, Reduction, Source};
    use crate::pre_asap::schema::{Column, DataType, Schema};
    use crate::types::AccuracyTarget;

    /// `[ts, service, value, latency]`.
    fn scan() -> QueryExpr {
        QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("service", DataType::Utf8, false),
                    Column::new("value", DataType::Float64, false),
                    Column::new("latency", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        }
    }

    fn quantile_agg(by: Vec<usize>, col: Option<usize>, q: f64) -> QueryExpr {
        QueryExpr::Aggregate {
            reduction: Reduction::by(by),
            measures: vec![AggIntent::Quantile {
                col,
                q,
                accuracy: AccuracyTarget::Exact,
            }],
            output_names: vec![],
            having: None,
            child: Rc::new(scan()),
        }
    }

    #[test]
    fn distinct_column_quantiles_do_not_merge() {
        // Grouped (unique_keys present) so the legality gate isn't what's
        // blocking the merge — only the differing `col` is.
        let a = quantile_agg(vec![1], Some(2), 0.5);
        let b = quantile_agg(vec![1], Some(3), 0.5);
        let shared = share_common_subtrees(vec![("a", a), ("b", b)]);
        let [(_, ra), (_, rb)] = shared.as_slice() else {
            panic!("expected 2 roots");
        };
        assert!(
            !Rc::ptr_eq(ra, rb),
            "distinct-column Quantiles must not be shared"
        );
        assert_ne!(ra, rb);
    }

    #[test]
    fn no_unique_keys_means_no_merge_even_when_structurally_identical() {
        // Ungrouped (global) aggregate: `by` is empty, so
        // `aggregate_output_schema` reports no unique key today — not
        // hoistable even though `a` and `b` are structurally identical.
        let a = quantile_agg(vec![], Some(2), 0.9);
        let b = quantile_agg(vec![], Some(2), 0.9);
        assert_eq!(a, b, "fixture sanity: the two trees are structurally equal");
        assert!(
            !a.output_schema().unwrap().has_unique_key(),
            "fixture sanity: an ungrouped aggregate has no provable unique key"
        );
        let shared = share_common_subtrees(vec![("a", a), ("b", b)]);
        let [(_, ra), (_, rb)] = shared.as_slice() else {
            panic!("expected 2 roots");
        };
        assert!(
            !Rc::ptr_eq(ra, rb),
            "no unique key ⇒ never hoisted, even for an identical structural match"
        );
    }

    #[test]
    fn median_and_explicit_half_percentile_merge() {
        // Two front-end spellings ("median" and "approx_percentile_cont(.,
        // 0.5)") already lower to the identical `AggIntent::Quantile { q:
        // 0.5, .. }` today (see `sql_lowering.rs`'s
        // `median_is_the_same_intent_as_an_explicit_half_percentile`) — here
        // built directly (grouped, so a unique key is provable) as two
        // independently-constructed but structurally identical trees, the
        // way two different call sites in a workload would produce them.
        let median = quantile_agg(vec![1], Some(2), 0.5);
        let approx_percentile_cont_half = quantile_agg(vec![1], Some(2), 0.5);
        let shared = share_common_subtrees(vec![
            ("median", median),
            ("percentile", approx_percentile_cont_half),
        ]);
        let [(_, m), (_, p)] = shared.as_slice() else {
            panic!("expected 2 roots");
        };
        assert!(
            Rc::ptr_eq(m, p),
            "median and an explicit 0.5 percentile must merge onto one Rc"
        );
    }

    #[test]
    fn single_query_shares_its_own_repeated_subtree() {
        // One query root referencing the same grouped Aggregate on both
        // BinaryOp branches — built as two separately-allocated but
        // structurally identical subtrees (`.clone()` into two distinct
        // `Rc::new` calls), the shape a front end emitting a repeated
        // sub-expression would actually produce (no sharing yet). A
        // workload of size 1 still interns bottom-up within this one tree —
        // no separate single-query mechanism needed.
        let agg = quantile_agg(vec![1], Some(2), 0.5);
        let root = QueryExpr::BinaryOp {
            op: BinaryOpKind::Compare(crate::pre_asap::expr_ir::CompareOpKind::Eq),
            lhs: Rc::new(agg.clone()),
            rhs: Rc::new(agg),
            vector_match: None,
        };
        let shared = share_common_subtrees(vec![("q", root)]);
        let [(_, root)] = shared.as_slice() else {
            panic!("expected 1 root");
        };
        let QueryExpr::BinaryOp { lhs, rhs, .. } = root.as_ref() else {
            panic!("expected BinaryOp root, got {root:?}");
        };
        assert!(
            Rc::ptr_eq(lhs, rhs),
            "the two structurally identical branches must collapse onto one Rc"
        );
    }

    #[test]
    fn dedup_gates_sharing_the_same_as_aggregate() {
        // `Dedup { cols }` adds `cols` as a unique key — so two identical
        // `Dedup` subtrees over a keyed column *do* merge, exercising the
        // legality gate on a non-`Aggregate` node.
        let dedup = |cols: Vec<usize>| QueryExpr::Dedup {
            cols,
            child: Rc::new(scan()),
        };
        let a = dedup(vec![1]);
        let b = dedup(vec![1]);
        let shared = share_common_subtrees(vec![("a", a), ("b", b)]);
        let [(_, ra), (_, rb)] = shared.as_slice() else {
            panic!("expected 2 roots");
        };
        assert!(
            Rc::ptr_eq(ra, rb),
            "Dedup on the same cols has a provable unique key and should merge"
        );
    }

    #[test]
    fn group_keys_gate_still_prevented_when_partition_by_without_used() {
        // Sanity on the module's advertised precedent: a `without(...)`
        // grouping stays open (no unique key) even though `by` is
        // non-empty-shaped structurally, so two identical `without` groups
        // do not merge under the same gate that blocks the ungrouped case.
        let without_agg = || QueryExpr::Aggregate {
            reduction: Reduction::Reduce(GroupKeys::without(vec![0])),
            measures: vec![AggIntent::Count {
                accuracy: AccuracyTarget::Exact,
            }],
            output_names: vec![],
            having: None,
            child: Rc::new(scan()),
        };
        let a = without_agg();
        let b = without_agg();
        assert!(!a.output_schema().unwrap().has_unique_key());
        let shared = share_common_subtrees(vec![("a", a), ("b", b)]);
        let [(_, ra), (_, rb)] = shared.as_slice() else {
            panic!("expected 2 roots");
        };
        assert!(!Rc::ptr_eq(ra, rb));
    }
}
