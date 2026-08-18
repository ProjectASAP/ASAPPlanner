//! Export the pre-ASAP [`QueryExpr`] tree as a generic node/edge graph, for tools
//! that need to render or diff the IR (the `dag_export` example + the
//! `tools/dag-viewer` viewer — see issue #133) rather than walk it in Rust.
//!
//! `QueryExpr` already derives `Serialize`, but as a Rust-shaped tagged tree
//! (`Box` children nested inside each variant's own field). This module
//! flattens that into an explicit node list + child-id edges — the shape a
//! generic graph renderer wants — and additionally computes a bottom-up
//! structural hash per node, so a caller with several exported queries can
//! spot identical subtrees (a shared `Scan`, a repeated `Aggregate` shape,
//! …) by comparing hashes rather than re-implementing `QueryExpr: PartialEq`
//! structural comparison client-side.

use serde::Serialize;
use std::hash::{Hash, Hasher};

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
    /// Bottom-up structural hash: two nodes hash equally iff their `kind`,
    /// `detail`, and (recursively) their children's hashes all match.
    pub hash: u64,
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
    let root = build(expr, &mut nodes);
    DagGraph { nodes, root }
}

fn push_node(
    nodes: &mut Vec<DagNode>,
    kind: &'static str,
    label: String,
    detail: serde_json::Value,
    children: Vec<u32>,
) -> u32 {
    let id = nodes.len() as u32;
    let hash = structural_hash(kind, &detail, &children, nodes);
    nodes.push(DagNode {
        id,
        kind,
        label,
        detail,
        children,
        hash,
    });
    id
}

/// `detail.to_string()` is a stable, canonical string: `serde_json::Value`'s
/// default map (no `preserve_order` feature) is a `BTreeMap`, so object keys
/// always serialize in the same sorted order regardless of construction
/// order.
fn structural_hash(
    kind: &str,
    detail: &serde_json::Value,
    children: &[u32],
    nodes: &[DagNode],
) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    kind.hash(&mut hasher);
    detail.to_string().hash(&mut hasher);
    for &c in children {
        nodes[c as usize].hash.hash(&mut hasher);
    }
    hasher.finish()
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
fn build(expr: &QueryExpr, nodes: &mut Vec<DagNode>) -> u32 {
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
            push_node(nodes, "Scan", label, detail, vec![])
        }
        QueryExpr::Scalar(v) => {
            let detail = serde_json::json!({ "value": v });
            push_node(nodes, "Scalar", format!("Scalar({v})"), detail, vec![])
        }
        QueryExpr::EvalTime => push_node(
            nodes,
            "EvalTime",
            "EvalTime".into(),
            serde_json::json!({}),
            vec![],
        ),
        QueryExpr::VectorFromScalar(child) => {
            let c = build(child, nodes);
            push_node(
                nodes,
                "VectorFromScalar",
                "vector()".into(),
                serde_json::json!({}),
                vec![c],
            )
        }
        QueryExpr::ScalarFromVector(child) => {
            let c = build(child, nodes);
            push_node(
                nodes,
                "ScalarFromVector",
                "scalar()".into(),
                serde_json::json!({}),
                vec![c],
            )
        }
        QueryExpr::Relabel { dst, value, child } => {
            let c = build(child, nodes);
            let detail = serde_json::json!({ "dst": dst, "value": value });
            push_node(
                nodes,
                "Relabel",
                format!("Relabel(dst={dst})"),
                detail,
                vec![c],
            )
        }
        QueryExpr::InfoJoin { selector, child } => {
            let c = build(child, nodes);
            let detail = serde_json::json!({ "selector": selector });
            push_node(nodes, "InfoJoin", "InfoJoin".into(), detail, vec![c])
        }
        QueryExpr::Sample { by, kind, child } => {
            let c = build(child, nodes);
            let detail = serde_json::json!({ "by": by, "kind": kind });
            push_node(
                nodes,
                "Sample",
                format!("Sample({kind:?})"),
                detail,
                vec![c],
            )
        }
        QueryExpr::Filter { pred, child } => {
            let c = build(child, nodes);
            let detail = serde_json::json!({ "pred": pred });
            push_node(nodes, "Filter", "Filter".into(), detail, vec![c])
        }
        QueryExpr::Project {
            cols,
            qualifier,
            child,
        } => {
            let c = build(child, nodes);
            let detail = serde_json::json!({ "cols": cols, "qualifier": qualifier });
            push_node(
                nodes,
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
            let c = build(child, nodes);
            let detail = serde_json::json!({
                "reduction": reduction,
                "measures": measures,
                "output_names": output_names,
                "having": having,
            });
            push_node(
                nodes,
                "Aggregate",
                format!("Aggregate({} measures)", measures.len()),
                detail,
                vec![c],
            )
        }
        QueryExpr::Distinct { cols, child } => {
            let c = build(child, nodes);
            let detail = serde_json::json!({ "cols": cols });
            push_node(
                nodes,
                "Distinct",
                format!("Distinct({} cols)", cols.len()),
                detail,
                vec![c],
            )
        }
        QueryExpr::Merge { children } => {
            let ids: Vec<u32> = children.iter().map(|c| build(c, nodes)).collect();
            let label = format!("Merge({} branches)", ids.len());
            push_node(nodes, "Merge", label, serde_json::json!({}), ids)
        }
        QueryExpr::Join {
            kind,
            pred,
            left,
            right,
        } => {
            let l = build(left, nodes);
            let r = build(right, nodes);
            let detail = serde_json::json!({ "kind": kind, "pred": pred });
            push_node(nodes, "Join", format!("Join({kind:?})"), detail, vec![l, r])
        }
        QueryExpr::SetOp {
            kind,
            all,
            left,
            right,
        } => {
            let l = build(left, nodes);
            let r = build(right, nodes);
            let detail = serde_json::json!({ "kind": kind, "all": all });
            push_node(
                nodes,
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
            let c = build(child, nodes);
            let detail = serde_json::json!({ "keys": keys, "partition_by": partition_by });
            push_node(
                nodes,
                "Sort",
                format!("Sort({} keys)", keys.len()),
                detail,
                vec![c],
            )
        }
        QueryExpr::Limit { n, offset, child } => {
            let c = build(child, nodes);
            let detail = serde_json::json!({ "n": n, "offset": offset });
            push_node(nodes, "Limit", format!("Limit({n})"), detail, vec![c])
        }
        QueryExpr::Subquery {
            range,
            resolution,
            child,
        } => {
            let c = build(child, nodes);
            let detail = serde_json::json!({ "range": range, "resolution": resolution });
            push_node(nodes, "Subquery", "Subquery".into(), detail, vec![c])
        }
        QueryExpr::TimeRange { range, child } => {
            let c = build(child, nodes);
            let detail = serde_json::json!({ "range": range });
            push_node(
                nodes,
                "TimeRange",
                format!("TimeRange({range:?})"),
                detail,
                vec![c],
            )
        }
        QueryExpr::TimeShift { shift, child } => {
            let c = build(child, nodes);
            let detail = serde_json::json!({ "shift": shift });
            push_node(nodes, "TimeShift", "TimeShift".into(), detail, vec![c])
        }
        QueryExpr::WindowFunc {
            func,
            args,
            partition_by,
            order_by,
            output_name,
            child,
        } => {
            let c = build(child, nodes);
            let detail = serde_json::json!({
                "func": func,
                "args": args,
                "partition_by": partition_by,
                "order_by": order_by,
                "output_name": output_name,
            });
            push_node(
                nodes,
                "WindowFunc",
                format!("WindowFunc({func:?})"),
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
            let l = build(lhs, nodes);
            let r = build(rhs, nodes);
            let detail = serde_json::json!({ "op": op.to_string(), "vector_match": vector_match });
            push_node(
                nodes,
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
        | QueryExpr::Arith { .. }
        | QueryExpr::Case { .. }) => {
            unreachable!("dag_export::build reached a scalar QueryExpr variant directly: {other:?}")
        }
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn chain_preserves_shape_and_child_links() {
        let expr = QueryExpr::Filter {
            pred: Predicate(Box::new(QueryExpr::Literal(ScalarValue::Boolean(true)))),
            child: Box::new(QueryExpr::Aggregate {
                reduction: Reduction::Reduce(GroupKeys::none()),
                measures: vec![AggIntent::Count {
                    accuracy: AccuracyTarget::Exact,
                }],
                output_names: vec![],
                having: None,
                child: Box::new(scan("metrics", value_col())),
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
        let expr = QueryExpr::Merge {
            children: vec![
                scan("a", value_col()),
                scan("b", value_col()),
                scan("c", value_col()),
            ],
        };
        let graph = export(&expr);
        assert_eq!(graph.nodes.len(), 4, "3 branches + the Merge node");
        let merge = &graph.nodes[graph.root as usize];
        assert_eq!(merge.kind, "Merge");
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
            child: Box::new(shared_shape()),
        };
        let q2 = QueryExpr::Distinct {
            cols: vec![0],
            child: Box::new(shared_shape()),
        };

        let g1 = export(&q1);
        let g2 = export(&q2);

        let scan_hash_in_g1 = g1.nodes.iter().find(|n| n.kind == "Scan").unwrap().hash;
        let scan_hash_in_g2 = g2.nodes.iter().find(|n| n.kind == "Scan").unwrap().hash;
        assert_eq!(scan_hash_in_g1, scan_hash_in_g2);

        // And the two roots themselves (Limit vs Distinct) must not collide.
        assert_ne!(
            g1.nodes[g1.root as usize].hash,
            g2.nodes[g2.root as usize].hash
        );
    }
}
