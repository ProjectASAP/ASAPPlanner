//! Export the pre-ASAP [`QueryExpr`] tree as a generic node/edge graph, for tools
//! that need to render or diff the IR (the `dag_export` example + the
//! `tools/dag-viewer` viewer — see issue #133) rather than walk it in Rust.
//!
//! `QueryExpr` already derives `Serialize`, but as a Rust-shaped tagged tree
//! (`Rc` children nested inside each variant's own field). This module
//! flattens that into an explicit node list + child-id edges — the shape a
//! generic graph renderer wants — and additionally tags each node with
//! [`structural_hash`](crate::pre_asap::cse::structural_hash), so a caller
//! with several exported queries can spot identical subtrees (a
//! shared `Scan`, a repeated `Aggregate` shape, …) by comparing hashes
//! rather than re-implementing `QueryExpr: PartialEq` structural comparison
//! client-side.
//!
//! This is literally the same hashing
//! [`share_common_subtrees`](crate::pre_asap::cse::share_common_subtrees)
//! uses to bucket candidates in its `InternTable` (issue #223 stage 3) — not
//! a parallel reimplementation. `tools/dag-viewer`'s "shared subtree"
//! highlighting is still a *proxy* for real CSE, though: a hash match here
//! only means two nodes are legal `InternTable` bucket-mates (same coarse
//! hash), the same candidate-narrowing step `structural_hash` performs
//! inside `InternTable::intern` — it does not mean `share_common_subtrees`
//! actually ran on this data and merged them onto one `Rc` (that also
//! requires the `PartialEq` check `InternTable::intern` performs, and the
//! `Schema::has_unique_key` legality gate, neither of which this export
//! step evaluates). See `tools/dag-viewer/README.md` for the up-to-date
//! caveat.
//!
//! ## `DagNode::notes` — a layering seam, not a feature this module implements
//!
//! [`DagNode`] also carries `notes: Vec<`[`DagNote`]`>`, always empty coming
//! out of [`export`]. It exists so a *higher* layer — one that depends on
//! `asap_types`, never the reverse — can annotate an already-exported graph
//! after the fact without this module needing to know anything about that
//! layer's concepts. Concretely: `asap-aware-mapping`'s `explanation` module
//! (issue #257) computes `structural_hash` over the same `QueryExpr`
//! subtrees this module does (via the identical function), so its
//! `ReplacementExplanation::node_hash` matches a [`DagNode::hash`] here
//! one-for-one; `crates/devtools/src/bin/dag_export.rs` is where that
//! matching actually happens, pushing a [`DagNote`] onto each matched node.
//! `asap_types` itself never constructs a `DagNote` — see [`DagNode::notes`]
//! for the layering rule this keeps.

use serde::Serialize;

use crate::pre_asap::cse::{structural_hash, HashCache};
use crate::pre_asap::query_expr::{QueryExpr, Source};

/// One flattened IR node. `detail` holds this node's own scalar fields
/// (predicates, aggregate funcs, schema, sort keys, …) — everything except
/// its children, which live in `children` instead.
#[derive(Debug, Clone, Serialize)]
pub struct DagNode {
    pub id: u32,
    /// The `QueryExpr` variant name (e.g. `"Aggregate"`).
    pub kind: &'static str,
    /// Short human-readable summary for a node's collapsed on-graph label.
    pub label: String,
    pub detail: serde_json::Value,
    /// Child node ids, in the variant's field order (e.g. `Join` is
    /// `[left, right]`).
    pub children: Vec<u32>,
    /// [`structural_hash`](crate::pre_asap::cse::structural_hash) of the
    /// subtree rooted at this node — the exact same function `cse`'s
    /// `InternTable` uses to bucket CSE candidates, so two nodes hash
    /// equally here iff they would land in the same `InternTable` bucket.
    /// See the module doc for what a hash match here does and doesn't
    /// guarantee.
    pub hash: u64,
    /// Arbitrary reporting-layer annotations for this node — e.g. why a
    /// replacement exists here. `asap_types` never populates this itself
    /// (it has no notion of a "replacement" at all — see the module doc's
    /// layering note); a higher layer that does (`asap-aware-mapping`, via
    /// the `dag_export` devtools binary) fills it in after the fact by
    /// matching [`DagNode::hash`]. Empty by default, so every existing
    /// [`export`] caller and test is unaffected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<DagNote>,
}

/// One reporting-layer annotation attached to a [`DagNode`] by a higher
/// layer than `asap_types` — see [`DagNode::notes`]. `asap_types` defines
/// this shape (so the field has a concrete, serializable type) but never
/// constructs one: `asap_types` is a lower crate that `asap-aware-mapping`
/// depends on, never the reverse, so this type is deliberately generic and
/// crate-agnostic rather than naming anything from that higher layer (e.g.
/// its `ExplanationKind`/`ReplacementExplanation`).
#[derive(Debug, Clone, Serialize)]
pub struct DagNote {
    /// A short tag for the kind of annotation this is (e.g. a
    /// `Debug`-formatted `asap_aware_mapping::ExplanationKind`) — opaque to
    /// `asap_types`, meant for a renderer to group or color by.
    pub kind: String,
    /// Human-readable explanation text (e.g. an
    /// `asap_aware_mapping::ReplacementExplanation::reason`).
    pub reason: String,
}

/// One query's exported graph. `nodes[root as usize]` is the tree's root.
#[derive(Debug, Clone, Serialize)]
pub struct DagGraph {
    pub nodes: Vec<DagNode>,
    pub root: u32,
}

/// A single named query within a multi-query export.
#[derive(Debug, Clone, Serialize)]
pub struct NamedGraph {
    pub name: String,
    /// The original query text (SQL or PromQL) this graph was lowered from,
    /// for display alongside the graph — not used by `export` itself, since
    /// that only sees the already-lowered `QueryExpr`. Optional because not
    /// every producer of a `NamedGraph` has the source text on hand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub graph: DagGraph,
}

/// A batch of named queries — the shape the viewer's multi-query / compare
/// mode reads (each query starts its own `DagGraph`; shared-subtree
/// highlighting is done by the viewer, matching `DagNode::hash` across
/// queries).
#[derive(Debug, Clone, Serialize)]
pub struct WorkloadGraph {
    pub queries: Vec<NamedGraph>,
}

/// Flatten `expr` into a [`DagGraph`].
pub fn export(expr: &QueryExpr) -> DagGraph {
    let mut nodes = Vec::new();
    // One cache for the whole export — persisted across every `build`/
    // `push_node` call, not reset per node, so `structural_hash` memoizes
    // real work across this pass instead of re-walking an already-hashed
    // shared descendant once per node that references it.
    let mut cache = HashCache::new();
    let root = build(expr, &mut nodes, &mut cache);
    DagGraph { nodes, root }
}

/// Push one flattened node for `expr`. `expr` is the *whole* subtree this
/// node represents (not just its own fields) — `hash` is
/// [`structural_hash(expr)`](structural_hash), the identical function and
/// the identical input `InternTable::intern` would hash for this same
/// subtree, so this node's `hash` matches what `cse::share_common_subtrees`
/// would bucket it under.
fn push_node(
    nodes: &mut Vec<DagNode>,
    expr: &QueryExpr,
    cache: &mut HashCache,
    kind: &'static str,
    label: String,
    detail: serde_json::Value,
    children: Vec<u32>,
) -> u32 {
    let id = nodes.len() as u32;
    let hash = structural_hash(expr, cache);
    nodes.push(DagNode {
        id,
        kind,
        label,
        detail,
        children,
        hash,
        notes: Vec::new(),
    });
    id
}

fn source_label(source: &Source) -> String {
    match source {
        Source::Table { table_ref } => table_ref.clone(),
        Source::TimeSeries { metric } => metric.clone(),
    }
}

/// Recursively flatten `expr`, appending nodes to `nodes` in post-order
/// (children pushed before their parent), and return the id of the pushed
/// root node. Exhaustive over every **operator** `QueryExpr` variant — a new
/// one fails to compile here until this match is extended, matching the rest
/// of the IR's exhaustive-match style (e.g. `output_schema`). The scalar
/// variants (issue #205) are never passed to `build` directly: every operator
/// arm that carries one (`Filter.pred`, `Project.cols`, `Aggregate.having`, …)
/// serializes it as opaque `detail` JSON via `Predicate`/`ProjectItem`/
/// `AggIntent`'s own `Serialize` impl, same as before the merge — a scalar
/// subtree was never a separate DAG node, so this doesn't change that.
fn build(expr: &QueryExpr, nodes: &mut Vec<DagNode>, cache: &mut HashCache) -> u32 {
    match expr {
        QueryExpr::Scan {
            source,
            predicates,
            schema,
        } => {
            let label = format!("Scan({})", source_label(source));
            let detail = serde_json::json!({
                "source": source,
                "predicates": predicates,
                "schema": schema,
            });
            push_node(nodes, expr, cache, "Scan", label, detail, vec![])
        }
        // The bridged child is a scalar-sub-language node (issue #220), not
        // an operator node `build` can recurse into — serialize it as opaque
        // `detail` JSON, same as every other scalar-typed field
        // (`Filter.pred`, `Project.cols`, …) rather than pushing it as a
        // separate DAG node.
        QueryExpr::PromqlScalarBridge(inner) => {
            let detail = serde_json::json!({ "value": inner });
            push_node(
                nodes,
                expr,
                cache,
                "PromqlScalarBridge",
                format!("PromqlScalarBridge({inner:?})"),
                detail,
                vec![],
            )
        }
        QueryExpr::QueryTimestamp => push_node(
            nodes,
            expr,
            cache,
            "QueryTimestamp",
            "QueryTimestamp".into(),
            serde_json::json!({}),
            vec![],
        ),
        QueryExpr::PromqlVectorFromScalar(child) => {
            let c = build(child, nodes, cache);
            push_node(
                nodes,
                expr,
                cache,
                "PromqlVectorFromScalar",
                "vector()".into(),
                serde_json::json!({}),
                vec![c],
            )
        }
        QueryExpr::PromqlScalarFromVector(child) => {
            let c = build(child, nodes, cache);
            push_node(
                nodes,
                expr,
                cache,
                "PromqlScalarFromVector",
                "scalar()".into(),
                serde_json::json!({}),
                vec![c],
            )
        }
        QueryExpr::PromqlRelabel { dst, value, child } => {
            let c = build(child, nodes, cache);
            let detail = serde_json::json!({ "dst": dst, "value": value });
            push_node(
                nodes,
                expr,
                cache,
                "PromqlRelabel",
                format!("PromqlRelabel(dst={dst})"),
                detail,
                vec![c],
            )
        }
        QueryExpr::PromqlInfoEnrich { selector, child } => {
            let c = build(child, nodes, cache);
            let detail = serde_json::json!({ "selector": selector });
            push_node(
                nodes,
                expr,
                cache,
                "PromqlInfoEnrich",
                "PromqlInfoEnrich".into(),
                detail,
                vec![c],
            )
        }
        QueryExpr::PromqlSeriesSample { by, kind, child } => {
            let c = build(child, nodes, cache);
            let detail = serde_json::json!({ "by": by, "kind": kind });
            push_node(
                nodes,
                expr,
                cache,
                "PromqlSeriesSample",
                format!("PromqlSeriesSample({kind:?})"),
                detail,
                vec![c],
            )
        }
        QueryExpr::Filter { pred, child } => {
            let c = build(child, nodes, cache);
            let detail = serde_json::json!({ "pred": pred });
            push_node(
                nodes,
                expr,
                cache,
                "Filter",
                "Filter".into(),
                detail,
                vec![c],
            )
        }
        QueryExpr::Project {
            cols,
            qualifier,
            child,
        } => {
            let c = build(child, nodes, cache);
            let detail = serde_json::json!({ "cols": cols, "qualifier": qualifier });
            push_node(
                nodes,
                expr,
                cache,
                "Project",
                format!("Project({} cols)", cols.len()),
                detail,
                vec![c],
            )
        }
        QueryExpr::Aggregate {
            reduction,
            measures,
            output_names,
            having,
            child,
        } => {
            let c = build(child, nodes, cache);
            let detail = serde_json::json!({
                "reduction": reduction,
                "measures": measures,
                "output_names": output_names,
                "having": having,
            });
            push_node(
                nodes,
                expr,
                cache,
                "Aggregate",
                format!("Aggregate({} measures)", measures.len()),
                detail,
                vec![c],
            )
        }
        QueryExpr::Dedup { cols, child } => {
            let c = build(child, nodes, cache);
            let detail = serde_json::json!({ "cols": cols });
            push_node(
                nodes,
                expr,
                cache,
                "Dedup",
                format!("Dedup({} cols)", cols.len()),
                detail,
                vec![c],
            )
        }
        QueryExpr::Concat { children } => {
            let ids: Vec<u32> = children.iter().map(|c| build(c, nodes, cache)).collect();
            let label = format!("Concat({} branches)", ids.len());
            push_node(
                nodes,
                expr,
                cache,
                "Concat",
                label,
                serde_json::json!({}),
                ids,
            )
        }
        QueryExpr::Join {
            kind,
            pred,
            left,
            right,
        } => {
            let l = build(left, nodes, cache);
            let r = build(right, nodes, cache);
            let detail = serde_json::json!({ "kind": kind, "pred": pred });
            push_node(
                nodes,
                expr,
                cache,
                "Join",
                format!("Join({kind:?})"),
                detail,
                vec![l, r],
            )
        }
        QueryExpr::SetOp {
            kind,
            all,
            left,
            right,
        } => {
            let l = build(left, nodes, cache);
            let r = build(right, nodes, cache);
            let detail = serde_json::json!({ "kind": kind, "all": all });
            push_node(
                nodes,
                expr,
                cache,
                "SetOp",
                format!("SetOp({kind:?})"),
                detail,
                vec![l, r],
            )
        }
        QueryExpr::Sort {
            keys,
            partition_by,
            child,
        } => {
            let c = build(child, nodes, cache);
            let detail = serde_json::json!({ "keys": keys, "partition_by": partition_by });
            push_node(
                nodes,
                expr,
                cache,
                "Sort",
                format!("Sort({} keys)", keys.len()),
                detail,
                vec![c],
            )
        }
        QueryExpr::Limit { n, offset, child } => {
            let c = build(child, nodes, cache);
            let detail = serde_json::json!({ "n": n, "offset": offset });
            push_node(
                nodes,
                expr,
                cache,
                "Limit",
                format!("Limit({n})"),
                detail,
                vec![c],
            )
        }
        QueryExpr::PromqlSubquery {
            range,
            resolution,
            child,
        } => {
            let c = build(child, nodes, cache);
            let detail = serde_json::json!({ "range": range, "resolution": resolution });
            push_node(
                nodes,
                expr,
                cache,
                "PromqlSubquery",
                "PromqlSubquery".into(),
                detail,
                vec![c],
            )
        }
        QueryExpr::TimeRange { range, child } => {
            let c = build(child, nodes, cache);
            let detail = serde_json::json!({ "range": range });
            push_node(
                nodes,
                expr,
                cache,
                "TimeRange",
                format!("TimeRange({range:?})"),
                detail,
                vec![c],
            )
        }
        QueryExpr::TimeShift { shift, child } => {
            let c = build(child, nodes, cache);
            let detail = serde_json::json!({ "shift": shift });
            push_node(
                nodes,
                expr,
                cache,
                "TimeShift",
                "TimeShift".into(),
                detail,
                vec![c],
            )
        }
        QueryExpr::SQLWindowFunc {
            func,
            args,
            partition_by,
            order_by,
            output_name,
            child,
        } => {
            let c = build(child, nodes, cache);
            let detail = serde_json::json!({
                "func": func,
                "args": args,
                "partition_by": partition_by,
                "order_by": order_by,
                "output_name": output_name,
            });
            push_node(
                nodes,
                expr,
                cache,
                "SQLWindowFunc",
                format!("SQLWindowFunc({func:?})"),
                detail,
                vec![c],
            )
        }
        QueryExpr::BinaryOp {
            op,
            lhs,
            rhs,
            vector_match,
        } => {
            let l = build(lhs, nodes, cache);
            let r = build(rhs, nodes, cache);
            let detail = serde_json::json!({ "op": op.to_string(), "vector_match": vector_match });
            push_node(
                nodes,
                expr,
                cache,
                "BinaryOp",
                format!("BinaryOp({op})"),
                detail,
                vec![l, r],
            )
        }
        other @ (QueryExpr::Column(_)
        | QueryExpr::Literal(_)
        | QueryExpr::Compare { .. }
        | QueryExpr::BoolAnd(_)
        | QueryExpr::BoolOr(_)
        | QueryExpr::Not(_)
        | QueryExpr::IsNull(_)
        | QueryExpr::IsNotNull(_)
        | QueryExpr::Cast { .. }
        | QueryExpr::InList { .. }
        | QueryExpr::FunctionCall { .. }
        | QueryExpr::Arithmetic { .. }
        | QueryExpr::Case { .. }) => {
            unreachable!("dag_export::build reached a scalar QueryExpr variant directly: {other:?}")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;
    use crate::pre_asap::agg_intent::AggIntent;
    use crate::pre_asap::expr_ir::ScalarValue;
    use crate::pre_asap::query_expr::{GroupKeys, Predicate, Reduction};
    use crate::pre_asap::schema::{Column, DataType, Schema};
    use crate::types::AccuracyTarget;

    fn scan(table: &str, columns: Vec<Column>) -> QueryExpr {
        QueryExpr::Scan {
            source: Source::Table {
                table_ref: table.into(),
            },
            predicates: vec![],
            schema: Schema {
                columns,
                time_index: None,
                unique_keys: vec![],
                closed: true,
            },
        }
    }

    fn value_col() -> Vec<Column> {
        vec![Column::new("value", DataType::Float64, false)]
    }

    #[test]
    fn leaf_scan_is_a_single_node() {
        let graph = export(&scan("metrics", value_col()));
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.root, 0);
        assert_eq!(graph.nodes[0].kind, "Scan");
        assert!(graph.nodes[0].children.is_empty());
    }

    /// `export` itself never populates `notes` — that's a higher layer's
    /// job (see the module doc) — and an empty `notes` must not appear in
    /// the serialized JSON at all, so every existing consumer of `export`'s
    /// output (in particular `tools/dag-viewer`, which predates this field)
    /// keeps parsing the same shape it always has.
    #[test]
    fn export_never_populates_notes_and_it_is_omitted_from_json() {
        let graph = export(&scan("metrics", value_col()));
        assert!(graph.nodes[0].notes.is_empty());
        let json = serde_json::to_string(&graph.nodes[0]).unwrap();
        assert!(
            !json.contains("notes"),
            "empty `notes` must be skipped, not serialized as `[]`: {json}"
        );
    }

    #[test]
    fn chain_preserves_shape_and_child_links() {
        let expr = QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Boolean(true)))),
            child: Rc::new(QueryExpr::Aggregate {
                reduction: Reduction::Reduce(GroupKeys::none()),
                measures: vec![AggIntent::Count {
                    accuracy: AccuracyTarget::Exact,
                }],
                output_names: vec![],
                having: None,
                child: Rc::new(scan("metrics", value_col())),
            }),
        };
        let graph = export(&expr);
        assert_eq!(graph.nodes.len(), 3, "Filter -> Aggregate -> Scan");

        let filter = &graph.nodes[graph.root as usize];
        assert_eq!(filter.kind, "Filter");
        assert_eq!(filter.children.len(), 1);

        let agg = &graph.nodes[filter.children[0] as usize];
        assert_eq!(agg.kind, "Aggregate");
        assert_eq!(agg.children.len(), 1);

        let leaf = &graph.nodes[agg.children[0] as usize];
        assert_eq!(leaf.kind, "Scan");
        assert!(leaf.children.is_empty());
    }

    #[test]
    fn merge_keeps_every_branch_as_a_child() {
        let expr = QueryExpr::Concat {
            children: vec![
                scan("a", value_col()),
                scan("b", value_col()),
                scan("c", value_col()),
            ],
        };
        let graph = export(&expr);
        assert_eq!(graph.nodes.len(), 4, "3 branches + the Concat node");
        let merge = &graph.nodes[graph.root as usize];
        assert_eq!(merge.kind, "Concat");
        assert_eq!(merge.children.len(), 3);
    }

    #[test]
    fn identical_subtrees_hash_equal_and_differing_ones_dont() {
        let left = scan("metrics", value_col());
        let right = scan("metrics", value_col());
        let different = scan("other_table", value_col());

        let left_graph = export(&left);
        let right_graph = export(&right);
        let different_graph = export(&different);

        assert_eq!(
            left_graph.nodes[left_graph.root as usize].hash,
            right_graph.nodes[right_graph.root as usize].hash,
            "structurally identical Scans must hash equal"
        );
        assert_ne!(
            left_graph.nodes[left_graph.root as usize].hash,
            different_graph.nodes[different_graph.root as usize].hash,
            "a different table_ref must not collide"
        );
    }

    #[test]
    fn shared_subtree_hash_matches_across_a_larger_tree() {
        // Two roots that each wrap the *same* Scan shape in a different outer
        // node — the exported hash should still flag the shared Scan even
        // though it's embedded at different depths / under different parents.
        let shared_shape = || scan("metrics", value_col());

        let q1 = QueryExpr::Limit {
            n: 10,
            offset: 0,
            child: Rc::new(shared_shape()),
        };
        let q2 = QueryExpr::Dedup {
            cols: vec![0],
            child: Rc::new(shared_shape()),
        };

        let g1 = export(&q1);
        let g2 = export(&q2);

        let scan_hash_in_g1 = g1.nodes.iter().find(|n| n.kind == "Scan").unwrap().hash;
        let scan_hash_in_g2 = g2.nodes.iter().find(|n| n.kind == "Scan").unwrap().hash;
        assert_eq!(scan_hash_in_g1, scan_hash_in_g2);

        // And the two roots themselves (Limit vs Dedup) must not collide.
        assert_ne!(
            g1.nodes[g1.root as usize].hash,
            g2.nodes[g2.root as usize].hash
        );
    }

    // ── Issue #223 stage 3: dag_export's hash literally *is* cse's hash ────

    #[test]
    fn root_hash_matches_cse_structural_hash_for_the_same_node() {
        // Not just "hashes equal for equal inputs" (any two consistent hash
        // functions would do that) — the exported root's `hash` must be the
        // literal `u64` `crate::pre_asap::cse::structural_hash` produces for
        // this exact node, because it's the same function call, not a
        // parallel reimplementation that happens to agree.
        let leaf = scan("metrics", value_col());
        let graph = export(&leaf);
        assert_eq!(
            graph.nodes[graph.root as usize].hash,
            structural_hash(&leaf, &mut HashCache::new()),
            "dag_export's root hash must equal cse::structural_hash(&leaf, &mut HashCache::new()) directly"
        );
    }

    #[test]
    fn every_node_hash_matches_cse_structural_hash_on_its_own_subtree() {
        // A multi-level tree: check the parity holds at every depth, not
        // just the root — each `DagNode::hash` must equal
        // `structural_hash` applied to the actual `QueryExpr` subtree that
        // node represents.
        let agg = QueryExpr::Aggregate {
            reduction: Reduction::Reduce(GroupKeys::none()),
            measures: vec![AggIntent::Count {
                accuracy: AccuracyTarget::Exact,
            }],
            output_names: vec![],
            having: None,
            child: Rc::new(scan("metrics", value_col())),
        };
        let root = QueryExpr::Filter {
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Boolean(true)))),
            child: Rc::new(agg.clone()),
        };

        let graph = export(&root);
        assert_eq!(
            graph.nodes[graph.root as usize].hash,
            structural_hash(&root, &mut HashCache::new()),
            "Filter root hash must match cse::structural_hash(&root, &mut HashCache::new())"
        );

        let filter = &graph.nodes[graph.root as usize];
        let agg_node = &graph.nodes[filter.children[0] as usize];
        assert_eq!(
            agg_node.hash,
            structural_hash(&agg, &mut HashCache::new()),
            "the exported Aggregate node's hash must match cse::structural_hash \
             on the Aggregate subtree it represents, not just the root"
        );
    }
}
