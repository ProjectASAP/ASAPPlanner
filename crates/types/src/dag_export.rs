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
//! subtrees this module does (via the identical function). The devtools
//! exporter uses that hash to narrow candidates, then compares
//! `ReplacementExplanation::target` with [`DagNode::source_expr`] for a
//! collision-safe match before pushing a [`DagNote`] onto the node.
//! `asap_types` itself never constructs a `DagNote` — see [`DagNode::notes`]
//! for the layering rule this keeps.

use std::collections::HashMap;
use std::rc::Rc;

use serde::Serialize;

use crate::cost::CostAnnotation;
use crate::post_asap::{AccuracyError, ResultGuarantee, SummaryExpr, SummaryNode};
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
    /// Output schema carried by every exported node. Edge renderers use the
    /// child node's schema as the schema flowing along child → consumer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
    /// Child node ids, in the variant's field order (e.g. `Join` is
    /// `[left, right]`).
    pub children: Vec<u32>,
    /// Explicit workload-wide identity assigned by a higher-level exporter.
    /// Viewers use this field to union nodes and must not reconstruct a
    /// structural signature client-side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workload_node_id: Option<u32>,
    /// [`structural_hash`](crate::pre_asap::cse::structural_hash) of the
    /// subtree rooted at this node — the exact same function `cse`'s
    /// `InternTable` uses to bucket CSE candidates, so two nodes hash
    /// equally here iff they would land in the same `InternTable` bucket.
    /// See the module doc for what a hash match here does and doesn't
    /// guarantee.
    ///
    /// `None` for the same reason `source_expr` is `None` — a post-ASAP-
    /// originated node in an [`export_post_asap`] merged graph has no
    /// `QueryExpr` to hash. Omitted from JSON entirely (rather than, say,
    /// serialized as `0`) so a consumer's shared-subtree-by-hash pass can
    /// tell "no hash" apart from a real hash that happens to collide with a
    /// placeholder — `0` is a legal `structural_hash` output, not a safe
    /// sentinel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<u64>,
    /// Exact source expression for in-process annotation matching. It is not
    /// part of the JSON format: callers first narrow by `hash`, then compare
    /// this value structurally to avoid treating a hash collision as node
    /// identity.
    ///
    /// `None` for a node with no corresponding pre-ASAP `QueryExpr` at all —
    /// only possible for a post-ASAP-originated node inside a merged
    /// [`export_post_asap`] graph (a `SummaryAgg`/`SummaryJoin`/… node has no
    /// single `QueryExpr` it corresponds to). Every node [`export`] itself
    /// produces is pre-ASAP by construction and always carries `Some`.
    #[serde(skip)]
    pub source_expr: Option<QueryExpr>,
    /// In-process identity of the source `QueryExpr`. Unlike `source_expr`'s
    /// structural value, this preserves an `Rc` child reached from multiple
    /// parents so post-ASAP flattening can retain true DAG sharing.
    #[serde(skip)]
    source_ptr: Option<usize>,
    /// Arbitrary reporting-layer annotations for this node — e.g. why a
    /// replacement exists here. `asap_types` never populates this itself
    /// (it has no notion of a "replacement" at all — see the module doc's
    /// layering note); a higher layer that does (`asap-aware-mapping`, via
    /// the `dag_export` devtools binary) fills it in after the fact by
    /// matching [`DagNode::hash`] and confirming structural equality. Empty
    /// by default, so every existing [`export`] caller and test is unaffected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<DagNote>,
    /// Explicit workload-level decision that produced or carried this node.
    /// Present only in `post_graph`; consumers must read this rather than
    /// infer strategy provenance from labels, hashes, or graph similarity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<DagDecision>,
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

/// Self-contained explanation of a winning workload-level post-ASAP
/// decision, serialized directly on every node it produced or carried.
#[derive(Debug, Clone, Serialize)]
pub struct DagDecision {
    pub id: u32,
    pub strategy: String,
    pub rationale: String,
    pub rank: usize,
    pub cost: f64,
    /// `replacement_root` for the node replacing the pre-ASAP target;
    /// `replacement_region` for its generated or carried descendants.
    pub role: &'static str,
    /// Structured counterpart of `cost` above — see [`CostAnnotation`]
    /// (issue #286). `None` for the same reason `cost` can be `f64::NAN`:
    /// the plugged-in cost model doesn't estimate a number for this
    /// candidate shape. Additive: every existing reader of `cost` keeps
    /// working unchanged; a reader that wants units, provenance, and an
    /// explicit baseline comparison reads this instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_cost: Option<CostAnnotation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_cost: Option<CostAnnotation>,
    /// `baseline_cost.value - selected_cost.value` under `baseline_cost`'s
    /// own baseline — for a winning `SharedSubtreeStrategy`/`CseShare`
    /// decision this *is* "avoided recomputation for a shared sub-DAG" (one
    /// of `dag_export`'s issue #286 granularity items): the baseline is
    /// exactly the cost of recomputing this subtree independently at every
    /// consumer, so the benefit is exactly what sharing avoided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benefit: Option<CostAnnotation>,
}

/// A cost/benefit annotation attributed to one specific graph edge (`from`
/// -> `to`, in [`DagNode::children`]'s direction) rather than to a node —
/// issue #286's "edge cost only when genuinely attributable to the edge"
/// granularity item. Graph structure alone cannot determine transfer,
/// materialization, or read cost. A higher layer may attach this annotation
/// only when physical evidence attributes cost to this exact edge; this
/// module never derives one from structural node counts.
#[derive(Debug, Clone, Serialize)]
pub struct EdgeCostAnnotation {
    pub from: u32,
    pub to: u32,
    pub cost: CostAnnotation,
}

/// One query's exported graph. `nodes[root as usize]` is the tree's root.
#[derive(Debug, Clone, Serialize)]
pub struct DagGraph {
    pub nodes: Vec<DagNode>,
    pub root: u32,
    /// See [`EdgeCostAnnotation`]. Always empty unless a higher layer
    /// explicitly populated it (same layering rule as [`DagNode::notes`]);
    /// omitted from JSON entirely when empty, so every existing producer of
    /// [`DagGraph`] (every call to [`export`]/[`export_summary`]) is
    /// unaffected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edge_annotations: Vec<EdgeCostAnnotation>,
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
    /// Concrete post-ASAP replacement sites discovered for this query — see
    /// [`TargetReplacement`]. Always empty coming out of anything in this
    /// module (same layering rule as [`DagNode::notes`]: `asap_types` never
    /// runs `asap-aware-mapping`'s search itself); a higher layer populates
    /// this after the fact, e.g. the `dag_export` devtools binary's
    /// `--post-asap` flag. Omitted from the JSON entirely when empty, so
    /// every existing producer/consumer of `NamedGraph` (in particular every
    /// invocation of `dag_export` without `--post-asap`) keeps emitting and
    /// parsing exactly the same shape it always has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replacements: Vec<TargetReplacement>,
    /// One merged "whole query, but post-ASAP" graph — see
    /// [`export_post_asap`] for how a higher layer builds this. Unlike
    /// [`TargetReplacement::before`]/`::after` (small, self-contained
    /// before/after pairs, one per independently-discovered replacement
    /// site), this is a single flattened [`DagGraph`] spanning the whole
    /// query: every node that has no winning replacement renders as an
    /// ordinary pre-ASAP [`DagNode`] (same shape [`export`] itself
    /// produces), and every node that does splices in its winning
    /// candidate's shape instead — a rewritten [`QueryExpr`] subtree, or a
    /// bound `SummaryNode` subtree, rendered inline in the very same node
    /// list. `None` unless a higher layer explicitly built one (e.g. the
    /// `dag_export` devtools binary's `--post-asap` flag); omitted from the
    /// JSON entirely when absent, so every existing producer/consumer of
    /// `NamedGraph` is unaffected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_graph: Option<DagGraph>,
    /// This query's own selected-workload cost/benefit — one of issue
    /// #286's granularity items. Built by summing *this query's own*
    /// `post_graph` decision-node cost annotations, deduplicated by
    /// `decision.id` **within this one query only** (a decision spanning
    /// several nodes in this query's own replacement region is still
    /// counted once here). `None` unless a higher layer built one (same
    /// `--post-asap`-gated pattern as `post_graph`); omitted from JSON when
    /// absent.
    ///
    /// This does **not** dedupe across queries: a target shared by two
    /// queries (e.g. a common `Scan` after workload-wide CSE) is counted
    /// once in *each* query's own `workload_cost` — summing several
    /// `NamedGraph.workload_cost` values by hand double-counts any decision
    /// shared between them. For a cross-query total that dedupes correctly,
    /// use [`WorkloadGraph::workload_cost`] instead, which is built
    /// specifically to cover every query in one pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_cost: Option<crate::cost::WorkloadCostSummary>,
    /// Accuracy-illegal candidates a higher layer's search refused for
    /// targets in this query (issue #172) — see [`TargetRejection`]. Always
    /// empty coming out of this module; omitted from the JSON when empty,
    /// same additive rule as `replacements`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejections: Vec<TargetRejection>,
}

/// A batch of named queries — the shape the viewer's multi-query / compare
/// mode reads (each query starts its own `DagGraph`; shared-subtree
/// highlighting is done by the viewer, matching `DagNode::hash` across
/// queries).
#[derive(Debug, Clone, Serialize)]
pub struct WorkloadGraph {
    pub queries: Vec<NamedGraph>,
    /// The selected multi-query workload's own cost/benefit, deduplicated
    /// across every query in `queries` (not just within one) — the
    /// "Selecting ... multiple queries ... display correct Pre/Post-ASAP
    /// annotations" / "workload totals count shared nodes once" acceptance
    /// criteria for the batch/union case. `None` unless a higher layer
    /// built one; omitted from JSON when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_cost: Option<crate::cost::WorkloadCostSummary>,
}

// ── Post-ASAP replacement export — a second, layering-seam-shaped feature ──
//
// Everything below this point is the post-ASAP counterpart of the pre-ASAP
// flattening above: [`export_summary`] flattens a `SummaryNode` the same way
// [`export`] flattens a `QueryExpr`, and [`TargetReplacement`] is the
// generic, crate-agnostic "one replacement site, before and after" shape a
// higher layer (`asap-aware-mapping`, via the `dag_export` devtools binary's
// `--post-asap` flag) populates after running its own search — the exact
// same layering rule [`DagNode::notes`]'s doc above already states: this
// module never runs `asap_aware_mapping::replacement::search_workload_with`
// itself, never picks a "winning" candidate, and has no opinion on what a
// `ReplacementProvenance` or a cost model even is. It only defines shapes
// concrete and serializable enough for a higher layer to fill in, and for
// `tools/dag-viewer` to render without needing to know anything about
// `asap-aware-mapping`'s own vocabulary.
//
// A single whole-query "post-ASAP tree" isn't attempted here, and isn't
// representable in the current type system either: `SummaryExpr` has no
// variant letting a `SummaryNode` be embedded back inside a plain
// `QueryExpr`'s child slot (`QueryExpr`'s own children are always
// `Rc<QueryExpr>`, never `Rc<SummaryNode>`), so there is no way to splice a
// post-ASAP binding back into its original pre-ASAP tree in place. Inventing
// a bridge type for that is a real `asap_types`/`asap-aware-mapping` IR
// design decision, well beyond what a devtools visualization export should
// decide unilaterally. Instead, each independently-discovered replacement
// target gets its own small, self-contained `before`/`after` pair — the
// target's own pre-ASAP subtree, and either the winning `SummaryNode` or the
// winning rewritten `QueryExpr`, both of which *are* fully representable
// today via [`export`]/[`export_summary`] as-is.

/// One flattened post-ASAP node — the [`SummaryExpr`] analogue of
/// [`DagNode`]. `detail` holds this node's own scalar fields (the summarized
/// column, the summary family, grouping strategy, sketch-query kind, …) —
/// everything except its `SummaryNode` children, which live in `children`
/// instead.
///
/// Unlike [`DagNode`], this carries no `hash`/`source_expr` pair: nothing in
/// this module ever needs to re-identify a particular `SummaryDagNode` the
/// way `DagNode::hash` lets a higher layer re-identify a pre-ASAP node (a
/// `SummaryNode` is always freshly exported for exactly one
/// [`TargetReplacementAfter::Summary`] site, never matched back against a
/// separately-exported graph the way pre-ASAP notes are).
///
/// Several of `SummaryExpr`'s own fields (`SummaryFamilyType`,
/// `GroupingStrategy`, `SketchQuery`) derive neither `Serialize` nor
/// `Deserialize` in `asap_types::post_asap` — they carry no reporting
/// obligation there, since nothing before this module ever needed to
/// serialize a post-ASAP node. Rather than adding `Serialize` impls to
/// `post_asap`'s own core types purely for this devtools-facing export (a
/// change to that module's own public API contract, out of scope for a
/// reporting concern), this module renders those particular fields into
/// `detail` via their `Debug` formatting instead — human-readable, and
/// sufficient for the display purpose `detail` exists for on every other
/// node in this file (see [`DagNode::detail`]'s own doc), at the cost of
/// those particular fields being opaque strings rather than structured JSON
/// on the `SummaryDagNode` side of the export.
#[derive(Debug, Clone, Serialize)]
pub struct SummaryDagNode {
    pub id: u32,
    /// The `SummaryExpr` variant name (e.g. `"SummaryAgg"`).
    pub kind: &'static str,
    /// Short human-readable summary for a node's collapsed on-graph label.
    pub label: String,
    pub detail: serde_json::Value,
    /// Child node ids, in the variant's field order (e.g. `SummaryJoin` is
    /// `[outer, inner]`).
    pub children: Vec<u32>,
    /// The value's machine-readable accuracy guarantee (issue #172) —
    /// [`SummaryNode::guarantee`] serialized structurally (metric, symbolic
    /// bound, failure probability, provenance including any budget
    /// allocation), not as prose. Omitted when the node carries none (raw
    /// summary state, or a family with no error model), so every consumer
    /// predating this field parses the same shape it always has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guarantee: Option<ResultGuarantee>,
}

/// One accuracy-illegal candidate a higher layer's search refused for a
/// target (issue #172) — `asap_aware_mapping::replacement::RejectedCandidate`
/// re-shaped into this crate's own crate-agnostic vocabulary, the same
/// layering rule as [`TargetReplacement`]. Carried on
/// [`NamedGraph::rejections`] so a renderer can explain *why* a target kept
/// its raw/pre-ASAP form, not only what won elsewhere.
#[derive(Debug, Clone, Serialize)]
pub struct TargetRejection {
    /// Id of the [`DagNode`] in this query's own `graph.nodes` the refused
    /// candidate targeted.
    pub target_pre_id: u32,
    /// Which strategy considered the candidate.
    pub strategy: String,
    /// What the candidate would have been.
    pub description: String,
    /// The typed reason it was refused.
    pub error: AccuracyError,
}

/// One post-ASAP `SummaryNode` tree, flattened the same way [`DagGraph`]
/// flattens a pre-ASAP `QueryExpr` tree.
#[derive(Debug, Clone, Serialize)]
pub struct SummaryDagGraph {
    pub nodes: Vec<SummaryDagNode>,
    pub root: u32,
}

/// Flatten a [`SummaryNode`] the same way [`export`] flattens a `QueryExpr`
/// — post-order, one [`SummaryDagNode`] per [`SummaryExpr`] variant, no
/// memoization of repeated `Rc<SummaryNode>` references (a shared
/// sub-expression reachable through two parents is flattened twice, into two
/// separate node entries — the same "this is a flattened tree view, not a
/// pointer-identity-preserving graph" behavior [`build`] already has for
/// `QueryExpr`).
///
/// A `KeepPreAsap(inner)` leaf embeds the *whole* pre-ASAP subtree beneath it
/// as a nested [`DagGraph`] (via [`export(inner)`](export)) inside its own
/// `detail` field (`{"pre_asap_subgraph": <DagGraph>}`) rather than trying to
/// flatten it into this same node list — [`DagNode`] and [`SummaryDagNode`]
/// are different types with different id spaces, so mixing them into one
/// `Vec` isn't type-safe; nesting is. `label` for a `KeepPreAsap` node is
/// `format!("KeepPreAsap({kind})")`, where `kind` is the inner subtree's own
/// top-level `DagNode::kind`.
pub fn export_summary(node: &SummaryNode) -> SummaryDagGraph {
    let mut nodes = Vec::new();
    let root = build_summary(node, &mut nodes);
    SummaryDagGraph { nodes, root }
}

fn push_summary_node(
    nodes: &mut Vec<SummaryDagNode>,
    kind: &'static str,
    label: String,
    detail: serde_json::Value,
    children: Vec<u32>,
    guarantee: Option<ResultGuarantee>,
) -> u32 {
    let id = nodes.len() as u32;
    nodes.push(SummaryDagNode {
        id,
        kind,
        label,
        detail,
        children,
        guarantee,
    });
    id
}

/// A short, human-readable label for a [`crate::post_asap::SummaryFamilyType`]
/// (e.g. `"Sketch(Kll)"`, `"ExactAggregate(Sum)"`) — for
/// [`SummaryDagNode::label`] text on a `SummaryAgg`/`SummaryJoin` node. Not
/// exhaustive prose (mirrors `asap_aware_mapping::replacement::describe_intent`'s
/// own "this is a label, not a decision" stance) — every variant is covered,
/// but via `Debug` for the inner kind rather than hand-written prose per
/// algorithm.
fn family_label(family: &crate::post_asap::SummaryFamilyType) -> String {
    use crate::post_asap::SummaryFamilyType;
    match family {
        SummaryFamilyType::Plain(dtype) => format!("Plain({dtype:?})"),
        SummaryFamilyType::ExactAggregate(kind, _) => format!("ExactAggregate({kind:?})"),
        SummaryFamilyType::Sketch(kind, _grouping) => format!("Sketch({:?})", kind.algorithm()),
        SummaryFamilyType::Sample(kind, _) => format!("Sample({kind:?})"),
        SummaryFamilyType::Wavelet(kind, _) => format!("Wavelet({kind:?})"),
        SummaryFamilyType::StatModel(kind, _) => format!("StatModel({kind:?})"),
    }
}

/// `(kind, label, detail)` for every [`SummaryExpr`] variant *except*
/// [`SummaryExpr::KeepPreAsap`] — that variant has no `SummaryDagNode`/
/// `DagNode` of its own (see [`build_summary`]/[`build_summary_hybrid`], its
/// only two callers, both of which special-case it before ever reaching
/// this function). Factored out so [`build_summary`] (nests a `KeepPreAsap`
/// leaf's pre-ASAP subtree as its own [`SummaryDagGraph`]) and
/// [`build_summary_hybrid`] (splices that same subtree directly into a
/// shared [`DagGraph`] node list — see [`export_post_asap`]) can't drift
/// apart on how every *other* variant's own shape is described, since
/// nothing about that description differs between the two.
macro_rules! define_summary_kind_tags {
    ($($pattern:pat => $tag:literal),+ $(,)?) => {
        #[cfg(test)]
        const SUMMARY_KIND_TAGS: &[&str] = &[$($tag),+];

        fn summary_kind_tag(expr: &SummaryExpr) -> &'static str {
            match expr {
                SummaryExpr::KeepPreAsap(_) => unreachable!(
                    "summary_kind_tag's callers special-case KeepPreAsap"
                ),
                $($pattern => $tag),+
            }
        }
    };
}

define_summary_kind_tags! {
    SummaryExpr::BinaryOp { .. } => "SummaryBinaryOp",
    SummaryExpr::SummaryAgg { .. } => "SummaryAgg",
    SummaryExpr::SummaryJoin { .. } => "SummaryJoin",
    SummaryExpr::SummarySubtract { .. } => "SummarySubtract",
    SummaryExpr::SummaryDelete { .. } => "SummaryDelete",
    SummaryExpr::SummaryEstimate { .. } => "SummaryEstimate",
    SummaryExpr::SummaryMerge { .. } => "SummaryMerge",
}

fn summary_shape(expr: &SummaryExpr) -> (&'static str, String, serde_json::Value) {
    let kind = summary_kind_tag(expr);
    match expr {
        SummaryExpr::KeepPreAsap(_) => {
            unreachable!("summary_shape's callers special-case KeepPreAsap before calling it")
        }
        SummaryExpr::BinaryOp { operator, .. } => {
            let label = format!("BinaryOp({:?})", operator.kind);
            let detail = serde_json::json!({
                "kind": format!("{:?}", operator.kind),
                "vector_match": operator.vector_match,
            });
            (kind, label, detail)
        }
        SummaryExpr::SummaryAgg {
            family,
            input,
            reduction,
            grouping,
            ..
        } => {
            let label = format!("SummaryAgg({})", family_label(family));
            let detail = serde_json::json!({
                "family": format!("{family:?}"),
                "input": input,
                "reduction": reduction,
                "grouping": format!("{grouping:?}"),
            });
            (kind, label, detail)
        }
        SummaryExpr::SummaryJoin { key, family, .. } => {
            let label = format!("SummaryJoin({})", family_label(family));
            let detail = serde_json::json!({
                "key": key,
                "family": format!("{family:?}"),
            });
            (kind, label, detail)
        }
        SummaryExpr::SummarySubtract { .. } => {
            (kind, "SummarySubtract".into(), serde_json::json!({}))
        }
        SummaryExpr::SummaryDelete { key, .. } => {
            let detail = serde_json::json!({ "key": key });
            (kind, "SummaryDelete".into(), detail)
        }
        SummaryExpr::SummaryEstimate { query, .. } => {
            let label = format!("SummaryEstimate({query:?})");
            let detail = serde_json::json!({ "query": format!("{query:?}") });
            (kind, label, detail)
        }
        SummaryExpr::SummaryMerge { children } => {
            let label = format!("SummaryMerge({} children)", children.len());
            (kind, label, serde_json::json!({}))
        }
    }
}

/// `expr`'s own `Rc<SummaryNode>` children, in the variant's field order
/// (e.g. `SummaryJoin` is `[outer, inner]`) — empty for
/// [`SummaryExpr::KeepPreAsap`], which has no `SummaryNode` children at all
/// (only a boxed pre-ASAP `QueryExpr`). Shared by [`build_summary`] and
/// [`build_summary_hybrid`] for the same reason [`summary_shape`] is.
fn summary_children(expr: &SummaryExpr) -> Vec<&Rc<SummaryNode>> {
    match expr {
        SummaryExpr::KeepPreAsap(_) => vec![],
        SummaryExpr::BinaryOp { lhs, rhs, .. } => vec![lhs, rhs],
        SummaryExpr::SummaryAgg { child, .. } => vec![child],
        SummaryExpr::SummaryJoin { outer, inner, .. } => vec![outer, inner],
        SummaryExpr::SummarySubtract { left, right } => vec![left, right],
        SummaryExpr::SummaryDelete { summary_input, .. } => vec![summary_input],
        SummaryExpr::SummaryEstimate { summary_input, .. } => vec![summary_input],
        SummaryExpr::SummaryMerge { children } => children.iter().collect(),
    }
}

/// Recursively flatten `node`, appending [`SummaryDagNode`]s to `nodes` in
/// post-order (children pushed before their parent), and return the pushed
/// root's id. Exhaustive over every [`SummaryExpr`] variant, matching this
/// file's own exhaustive style for `QueryExpr` in [`build`].
fn build_summary(node: &SummaryNode, nodes: &mut Vec<SummaryDagNode>) -> u32 {
    if let SummaryExpr::KeepPreAsap(inner) = &node.expr {
        let pre_asap_subgraph = export(inner);
        let inner_kind = pre_asap_subgraph.nodes[pre_asap_subgraph.root as usize].kind;
        let label = format!("KeepPreAsap({inner_kind})");
        let detail = serde_json::json!({ "pre_asap_subgraph": pre_asap_subgraph });
        return push_summary_node(
            nodes,
            "KeepPreAsap",
            label,
            detail,
            vec![],
            node.guarantee.clone(),
        );
    }
    let children: Vec<u32> = summary_children(&node.expr)
        .into_iter()
        .map(|child| build_summary(child, nodes))
        .collect();
    let (kind, label, detail) = summary_shape(&node.expr);
    push_summary_node(nodes, kind, label, detail, children, node.guarantee.clone())
}

/// One replacement site a higher layer (the `dag_export` binary) found by
/// running `asap_aware_mapping::replacement::search_workload_with` +
/// `PlanSpace::cost_sorted` and picking the best-ranked candidate for one
/// `MemoGroup` — `asap_types` never runs that search itself (same layering
/// rule as [`DagNote`]: this crate defines the shape, a higher crate
/// populates it).
#[derive(Debug, Clone, Serialize)]
pub struct TargetReplacement {
    /// Stable id of this workload-level winning decision. Nodes in
    /// [`NamedGraph::post_graph`] produced by this decision carry the same id,
    /// so renderers can explain a clicked post-ASAP node without guessing by
    /// label, hash, or graph shape.
    pub decision_id: u32,
    /// Id of the [`DagNode`] (in this query's own `graph.nodes`, i.e. the
    /// [`NamedGraph`] this `TargetReplacement` is attached to) this
    /// replacement's `before` subtree is rooted at.
    pub target_pre_id: u32,
    /// Human label for which strategy proposed the winning candidate —
    /// e.g. `"Sketch"` / `"HydraGrouping"` / `"SharedSubtree"` /
    /// `"AvgToSumCountRewrite"` / `"Rollup"`. The higher layer derives this from
    /// `ReplacementProvenance` plus which strategy's shape actually
    /// produced the winning candidate; `asap_types` has no opinion on the
    /// string values here at all — purely a display label.
    pub strategy: String,
    /// The winning candidate's own human-readable rationale (reused
    /// verbatim from `ReplacementSubDAG::rationale` by the higher layer,
    /// not re-derived here).
    pub rationale: String,
    /// This candidate's rank among its `MemoGroup`'s alternatives after
    /// `PlanSpace::cost_sorted` (`0` = best). Exposed so a renderer can show
    /// "this was the best of N candidates" without re-deriving the ranking.
    pub rank: usize,
    /// This candidate's own estimated cost, straight off
    /// `RankedGroup::costs` — `f64::NAN` whenever the plugged-in cost model
    /// doesn't estimate a numeric cost for this candidate shape (see that
    /// field's own doc upstream).
    pub cost: f64,
    /// The target's own pre-ASAP subtree, before replacement — literally
    /// `export(target)` for the `MemoGroup`'s own `target`, reused as-is.
    pub before: DagGraph,
    pub after: TargetReplacementAfter,
    /// Structured baseline/selected/benefit cost annotations for this one
    /// replacement region — issue #286's "replacement-region baseline
    /// cost, selected cost, and benefit" granularity item. Always
    /// consistent with `cost` above: `selected_cost.value == Some(cost)`
    /// whenever `cost` is finite, `None`/`Unavailable` whenever it is
    /// `NaN`. Baseline and selected values require complete, scope-matched
    /// physical evidence; neither is inferred from logical graph structure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_cost: Option<CostAnnotation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_cost: Option<CostAnnotation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benefit: Option<CostAnnotation>,
}

/// What a [`TargetReplacement`] became — either a genuine post-ASAP binding
/// or a still-pre-ASAP-shaped structural rewrite, mirroring
/// `asap_aware_mapping::replacement::Replacement`'s own two variants.
///
/// Serializes as `{"kind": "Summary"|"Rewrite", "graph": {...}}` (serde's
/// adjacently-tagged representation for a `#[serde(tag = "kind", content =
/// "graph")]` enum) — this exact shape is a cross-team contract with
/// `tools/dag-viewer`'s fixture data, so it isn't incidental: changing it
/// needs coordinating with that side, not just a local refactor here.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "graph")]
pub enum TargetReplacementAfter {
    /// A `Replacement::Summary` candidate — a genuine post-ASAP binding.
    Summary(SummaryDagGraph),
    /// A `Replacement::Rewrite` candidate — still pre-ASAP shaped (CSE
    /// share/recompute, `AvgToSumOverCountStrategy`, and `RollupStrategy`
    /// all produce this kind), so this reuses [`DagGraph`]/[`export`] too,
    /// not a new type.
    Rewrite(DagGraph),
}

/// Flatten `expr` into a [`DagGraph`].
pub fn export(expr: &QueryExpr) -> DagGraph {
    let mut nodes = Vec::new();
    // One cache for the whole export — persisted across every `build`/
    // `push_node` call, not reset per node, so `structural_hash` memoizes
    // real work across this pass instead of re-walking an already-hashed
    // shared descendant once per node that references it.
    let mut cache = HashCache::new();
    // No substitution: an ordinary pre-ASAP export never splices anything
    // in — see `build`'s own doc for why it always takes a `find_winner`
    // callback regardless (so `export_post_asap` can share this exact
    // per-variant traversal instead of duplicating it).
    let root = build(expr, &mut nodes, &mut cache, &mut |_| None);
    DagGraph {
        nodes,
        root,
        edge_annotations: Vec::new(),
    }
}

/// What a higher layer found for one specific pre-ASAP node when building a
/// merged post-ASAP graph via [`export_post_asap`] — see that function's own
/// doc for the full design. `asap_types` has no opinion on *how* this is
/// decided (that's `asap_aware_mapping::replacement::search_workload_with` +
/// `PlanSpace::cost_sorted`'s job, a higher layer, exactly the layering rule
/// [`DagNode::notes`] already states); it only defines the shape a decision
/// comes back in.
#[derive(Debug, Clone)]
pub enum PostAsapSubstitution {
    /// This exact node has a winning `Replacement::Rewrite` — keep building
    /// from `.0` instead of the original node. Still pre-ASAP shaped, so
    /// [`build`] renders it via the same ordinary `DagNode` path — see
    /// [`build`]'s own doc for why `.0`'s own top level is rendered without
    /// re-querying `find_winner` on it (its descendants still are).
    Rewrite {
        replacement: Rc<QueryExpr>,
        decision: DagDecision,
    },
    /// This exact node has a winning `Replacement::Summary` — switch to
    /// rendering `.0`'s bound `SummaryNode` shape from here down, via
    /// [`build_summary_hybrid`].
    Summary {
        replacement: Rc<SummaryNode>,
        decision: DagDecision,
    },
}

/// Build one merged "whole query, but post-ASAP" [`DagGraph`] by walking
/// `root`'s ordinary pre-ASAP shape and, at every node, asking `find_winner`
/// whether *that exact node* has a winning replacement — if so, splicing
/// the replacement's own shape in at that position instead, in the very
/// same flattened node list (not a nested sub-graph the way
/// [`TargetReplacement::before`]/`::after` — small, independent, per-site
/// before/after pairs — already do; see this file's "Post-ASAP replacement
/// export" section doc for why *that* design doesn't attempt a single
/// whole-query composite, and why this one can: this is a synthetic
/// id/edge list, the same kind of thing [`DagGraph`] already is for the
/// pre-ASAP side, not a real `QueryExpr`/`SummaryNode` value with a type
/// system to satisfy).
///
/// `find_winner` is the whole layering seam: `asap_types` never runs
/// `asap_aware_mapping::replacement::search_workload_with` or
/// `PlanSpace::cost_sorted` itself, and has no idea what a `MemoGroup` or a
/// `ReplacementProvenance` is — it only asks, for one node at a time, "did a
/// higher layer already decide something for you?" A caller (e.g. the
/// `dag_export` devtools binary) builds this closure once per workload
/// search, over whatever hash/structural-equality lookup it already needs
/// for [`TargetReplacement`] discovery, and passes it in here unchanged.
///
/// `find_winner` is deliberately consulted only once per node, at the
/// moment [`build`] first reaches it — **not** re-consulted on a
/// substitution's own immediate top level (only on that substitution's
/// *descendants*, which get an ordinary fresh call same as any other node).
/// This matters for correctness, not just efficiency:
/// `SharedSubtreeStrategy`'s own "build once and share" candidate is
/// `Replacement::Rewrite(Rc::clone(target))` — literally the *same* value
/// as the target it's a candidate for. Re-querying `find_winner` on that
/// candidate's own top level would find the identical group and its
/// identical winning candidate again, recursing forever. Skipping the
/// re-query at exactly that one level is what makes this termination-safe
/// for every registered strategy, not just the ones that happen not to
/// return the target itself as a candidate.
pub fn export_post_asap(
    root: &QueryExpr,
    find_winner: &mut dyn FnMut(&QueryExpr) -> Option<PostAsapSubstitution>,
) -> DagGraph {
    let mut nodes = Vec::new();
    let mut cache = HashCache::new();
    let root_id = build(root, &mut nodes, &mut cache, find_winner);
    deduplicate_pointer_shared_nodes(nodes, root_id)
}

fn deduplicate_pointer_shared_nodes(nodes: Vec<DagNode>, root: u32) -> DagGraph {
    let mut by_source_ptr = HashMap::<usize, u32>::new();
    let mut old_to_new = vec![0_u32; nodes.len()];
    let mut deduplicated = Vec::with_capacity(nodes.len());
    for mut node in nodes {
        node.children = node
            .children
            .into_iter()
            .map(|child| old_to_new[child as usize])
            .collect();
        if let Some(existing) = node
            .source_ptr
            .and_then(|source_ptr| by_source_ptr.get(&source_ptr).copied())
        {
            old_to_new[node.id as usize] = existing;
            continue;
        }
        let old_id = node.id;
        let new_id = deduplicated.len() as u32;
        node.id = new_id;
        if let Some(source_ptr) = node.source_ptr {
            by_source_ptr.insert(source_ptr, new_id);
        }
        old_to_new[old_id as usize] = new_id;
        deduplicated.push(node);
    }

    DagGraph {
        nodes: deduplicated,
        root: old_to_new[root as usize],
        edge_annotations: Vec::new(),
    }
}

macro_rules! define_query_kind_tags {
    ($($pattern:pat => $tag:literal),+ $(,)?) => {
        #[cfg(test)]
        const QUERY_KIND_TAGS: &[&str] = &[$($tag),+];

        fn kind_tag(expr: &QueryExpr) -> &'static str {
            match expr {
                $($pattern => $tag),+,
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
                | QueryExpr::Case { .. }) => unreachable!(
                    "kind_tag reached a scalar QueryExpr variant directly: {other:?}"
                ),
            }
        }
    };
}

define_query_kind_tags! {
    QueryExpr::Scan { .. } => "Scan",
    QueryExpr::PromqlScalarBridge(_) => "PromqlScalarBridge",
    QueryExpr::EvalTimestamp => "EvalTimestamp",
    QueryExpr::CurrentTimestamp => "CurrentTimestamp",
    QueryExpr::PromqlVectorFromScalar(_) => "PromqlVectorFromScalar",
    QueryExpr::PromqlScalarFromVector(_) => "PromqlScalarFromVector",
    QueryExpr::PromqlRelabel { .. } => "PromqlRelabel",
    QueryExpr::PromqlInfoEnrich { .. } => "PromqlInfoEnrich",
    QueryExpr::PromqlSeriesSample { .. } => "PromqlSeriesSample",
    QueryExpr::Filter { .. } => "Filter",
    QueryExpr::Project { .. } => "Project",
    QueryExpr::Aggregate { .. } => "Aggregate",
    QueryExpr::Dedup { .. } => "Dedup",
    QueryExpr::Concat { .. } => "Concat",
    QueryExpr::Join { .. } => "Join",
    QueryExpr::SetOp { .. } => "SetOp",
    QueryExpr::Sort { .. } => "Sort",
    QueryExpr::Limit { .. } => "Limit",
    QueryExpr::PromqlSubquery { .. } => "PromqlSubquery",
    QueryExpr::TimeRange { .. } => "TimeRange",
    QueryExpr::TimeShift { .. } => "TimeShift",
    QueryExpr::SQLWindowFunc { .. } => "SQLWindowFunc",
    QueryExpr::BinaryOp { .. } => "BinaryOp",
}

/// Push one flattened node for `expr`. `expr` is the *whole* subtree this
/// node represents (not just its own fields) — `hash` is
/// [`structural_hash(expr)`](structural_hash), the identical function and
/// the identical input `InternTable::intern` would hash for this same
/// subtree, so this node's `hash` matches what `cse::share_common_subtrees`
/// would bucket it under. `kind` is [`kind_tag(expr)`](kind_tag), not a
/// caller-supplied argument — see that function's doc for why.
fn push_node(
    nodes: &mut Vec<DagNode>,
    expr: &QueryExpr,
    cache: &mut HashCache,
    label: String,
    detail: serde_json::Value,
    children: Vec<u32>,
) -> u32 {
    let id = nodes.len() as u32;
    let hash = Some(structural_hash(expr, cache));
    nodes.push(DagNode {
        id,
        kind: kind_tag(expr),
        label,
        detail,
        schema: expr
            .output_schema()
            .ok()
            .and_then(|schema| serde_json::to_value(schema).ok()),
        children,
        workload_node_id: None,
        hash,
        source_expr: Some(expr.clone()),
        source_ptr: Some(expr as *const QueryExpr as usize),
        notes: Vec::new(),
        decision: None,
    });
    id
}

/// Push one flattened node with no corresponding pre-ASAP `QueryExpr` at
/// all — a post-ASAP-originated node inside [`export_post_asap`]'s merged
/// graph (a `SummaryAgg`/`SummaryJoin`/… node, via [`build_summary_hybrid`]).
/// `hash`/`source_expr`-based re-identification (see [`DagNode::hash`]'s own
/// doc) has no meaning for a node with no `QueryExpr` behind it, so this
/// pushes a fixed placeholder hash (`0`) and `source_expr: None` rather than
/// inventing a hash over `SummaryExpr` (which, unlike `QueryExpr`, has no
/// [`structural_hash`]-equivalent function at all — see [`SummaryDagNode`]'s
/// own doc on why `SummaryExpr`'s fields don't even derive `Hash`/`PartialEq`
/// consistently enough to build one).
fn push_summary_originated_node(
    nodes: &mut Vec<DagNode>,
    kind: &'static str,
    label: String,
    detail: serde_json::Value,
    children: Vec<u32>,
) -> u32 {
    let id = nodes.len() as u32;
    nodes.push(DagNode {
        id,
        kind,
        label,
        detail,
        schema: None,
        children,
        workload_node_id: None,
        hash: None,
        source_expr: None,
        source_ptr: None,
        notes: Vec::new(),
        decision: None,
    });
    id
}

/// The [`build_summary`]/[`build_summary_hybrid`] counterpart of [`build`]
/// for a bound [`SummaryNode`] reached while building
/// [`export_post_asap`]'s merged graph: appends into the *same* `nodes:
/// Vec<DagNode>` list `build` itself is filling, instead of a separate
/// [`SummaryDagGraph`]. A `KeepPreAsap(inner)` leaf recurses back into
/// [`build`] on `inner` (the general pre-ASAP entry, `find_winner` included)
/// rather than nesting a `{"pre_asap_subgraph": ...}` blob the way
/// [`build_summary`] does — so the merged graph reads as one seamless graph
/// with no dead ends, and so a target reachable underneath a `KeepPreAsap`
/// wrapper (a nested aggregate a strategy independently found a
/// replacement for, say) still gets spliced in correctly.
fn build_summary_hybrid(
    node: &SummaryNode,
    nodes: &mut Vec<DagNode>,
    cache: &mut HashCache,
    find_winner: &mut dyn FnMut(&QueryExpr) -> Option<PostAsapSubstitution>,
) -> u32 {
    if let SummaryExpr::KeepPreAsap(inner) = &node.expr {
        return build(inner, nodes, cache, find_winner);
    }
    let children: Vec<u32> = summary_children(&node.expr)
        .into_iter()
        .map(|child| build_summary_hybrid(child, nodes, cache, find_winner))
        .collect();
    let (kind, label, mut detail) = summary_shape(&node.expr);
    // The merged graph's `DagNode` has no dedicated guarantee field (it is
    // the pre-ASAP node shape); the guarantee rides in `detail` under the
    // same key/shape `SummaryDagNode::guarantee` uses, additively.
    if let Some(guarantee) = &node.guarantee {
        if let (serde_json::Value::Object(map), Ok(value)) =
            (&mut detail, serde_json::to_value(guarantee))
        {
            map.insert("guarantee".into(), value);
        }
    }
    let id = push_summary_originated_node(nodes, kind, label, detail, children);
    nodes[id as usize].schema = Some(summary_schema_json(&node.schema));
    id
}

fn summary_schema_json(schema: &crate::post_asap::SummarySchema) -> serde_json::Value {
    serde_json::json!({
        "fields": schema.fields.iter().map(|field| serde_json::json!({
            "name": field.name,
            "dtype": format!("{:?}", field.dtype),
            "nullable": field.nullable,
        })).collect::<Vec<_>>(),
        "time_index": schema.time_index,
    })
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
///
/// `find_winner` is [`export_post_asap`]'s substitution seam, threaded
/// through every recursive call (including [`export`]'s own, which always
/// passes a closure that returns `None`) so both entry points share this
/// exact traversal instead of maintaining two copies of it. `build` itself
/// only ever calls `find_winner` once, right here at the top, before
/// dispatching into the ordinary per-variant match below — see
/// [`export_post_asap`]'s own doc for why a substitution's own immediate
/// result is rendered via that match directly (recursing into its children
/// through `build` again, so *they* still get a fresh `find_winner` call)
/// rather than by looping back through this check a second time.
fn build(
    expr: &QueryExpr,
    nodes: &mut Vec<DagNode>,
    cache: &mut HashCache,
    find_winner: &mut dyn FnMut(&QueryExpr) -> Option<PostAsapSubstitution>,
) -> u32 {
    match find_winner(expr) {
        Some(PostAsapSubstitution::Rewrite {
            replacement,
            decision,
        }) => {
            let first = nodes.len();
            let root = build_no_recheck(&replacement, nodes, cache, find_winner);
            for node in &mut nodes[first..] {
                if node.decision.is_none() {
                    let mut node_decision = decision.clone();
                    node_decision.role = if node.id == root {
                        "replacement_root"
                    } else {
                        "replacement_region"
                    };
                    node.decision = Some(node_decision);
                }
            }
            return root;
        }
        Some(PostAsapSubstitution::Summary {
            replacement,
            decision,
        }) => {
            let first = nodes.len();
            let root = build_summary_hybrid(&replacement, nodes, cache, find_winner);
            for node in &mut nodes[first..] {
                if node.decision.is_none() {
                    let mut node_decision = decision.clone();
                    node_decision.role = if node.id == root {
                        "replacement_root"
                    } else {
                        "replacement_region"
                    };
                    node.decision = Some(node_decision);
                }
            }
            return root;
        }
        None => {}
    }
    build_no_recheck(expr, nodes, cache, find_winner)
}

/// The actual per-variant match [`build`] dispatches to once it has decided
/// (by consulting `find_winner` exactly once) which `QueryExpr` value to
/// render at this position — either `expr` itself (unchanged), or a winning
/// `Replacement::Rewrite`'s own target. Every recursive call here goes back
/// through [`build`] (not this function), so every child gets its own fresh
/// `find_winner` query.
fn build_no_recheck(
    expr: &QueryExpr,
    nodes: &mut Vec<DagNode>,
    cache: &mut HashCache,
    find_winner: &mut dyn FnMut(&QueryExpr) -> Option<PostAsapSubstitution>,
) -> u32 {
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
            push_node(nodes, expr, cache, label, detail, vec![])
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
                format!("PromqlScalarBridge({inner:?})"),
                detail,
                vec![],
            )
        }
        QueryExpr::EvalTimestamp => push_node(
            nodes,
            expr,
            cache,
            "EvalTimestamp".into(),
            serde_json::json!({}),
            vec![],
        ),
        QueryExpr::CurrentTimestamp => push_node(
            nodes,
            expr,
            cache,
            "CurrentTimestamp".into(),
            serde_json::json!({}),
            vec![],
        ),
        QueryExpr::PromqlVectorFromScalar(child) => {
            let c = build(child, nodes, cache, find_winner);
            push_node(
                nodes,
                expr,
                cache,
                "vector()".into(),
                serde_json::json!({}),
                vec![c],
            )
        }
        QueryExpr::PromqlScalarFromVector(child) => {
            let c = build(child, nodes, cache, find_winner);
            push_node(
                nodes,
                expr,
                cache,
                "scalar()".into(),
                serde_json::json!({}),
                vec![c],
            )
        }
        QueryExpr::PromqlRelabel { dst, value, child } => {
            let c = build(child, nodes, cache, find_winner);
            let detail = serde_json::json!({ "dst": dst, "value": value });
            push_node(
                nodes,
                expr,
                cache,
                format!("PromqlRelabel(dst={dst})"),
                detail,
                vec![c],
            )
        }
        QueryExpr::PromqlInfoEnrich { selector, child } => {
            let c = build(child, nodes, cache, find_winner);
            let detail = serde_json::json!({ "selector": selector });
            push_node(
                nodes,
                expr,
                cache,
                "PromqlInfoEnrich".into(),
                detail,
                vec![c],
            )
        }
        QueryExpr::PromqlSeriesSample { by, kind, child } => {
            let c = build(child, nodes, cache, find_winner);
            let detail = serde_json::json!({ "by": by, "kind": kind });
            push_node(
                nodes,
                expr,
                cache,
                format!("PromqlSeriesSample({kind:?})"),
                detail,
                vec![c],
            )
        }
        QueryExpr::Filter { pred, child } => {
            let c = build(child, nodes, cache, find_winner);
            let detail = serde_json::json!({ "pred": pred });
            push_node(nodes, expr, cache, "Filter".into(), detail, vec![c])
        }
        QueryExpr::Project {
            cols,
            qualifier,
            child,
        } => {
            let c = build(child, nodes, cache, find_winner);
            let detail = serde_json::json!({ "cols": cols, "qualifier": qualifier });
            push_node(
                nodes,
                expr,
                cache,
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
            let c = build(child, nodes, cache, find_winner);
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
                format!("Aggregate({} measures)", measures.len()),
                detail,
                vec![c],
            )
        }
        QueryExpr::Dedup { cols, child } => {
            let c = build(child, nodes, cache, find_winner);
            let detail = serde_json::json!({ "cols": cols });
            push_node(
                nodes,
                expr,
                cache,
                format!("Dedup({} cols)", cols.len()),
                detail,
                vec![c],
            )
        }
        QueryExpr::Concat {
            children,
            discriminator_unique_key,
        } => {
            let ids: Vec<u32> = children
                .iter()
                .map(|c| build(c, nodes, cache, find_winner))
                .collect();
            let label = format!("Concat({} branches)", ids.len());
            let detail =
                serde_json::json!({ "discriminator_unique_key": discriminator_unique_key });
            push_node(nodes, expr, cache, label, detail, ids)
        }
        QueryExpr::Join {
            kind,
            pred,
            left,
            right,
        } => {
            let l = build(left, nodes, cache, find_winner);
            let r = build(right, nodes, cache, find_winner);
            let detail = serde_json::json!({ "kind": kind, "pred": pred });
            push_node(
                nodes,
                expr,
                cache,
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
            let l = build(left, nodes, cache, find_winner);
            let r = build(right, nodes, cache, find_winner);
            let detail = serde_json::json!({ "kind": kind, "all": all });
            push_node(
                nodes,
                expr,
                cache,
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
            let c = build(child, nodes, cache, find_winner);
            let detail = serde_json::json!({ "keys": keys, "partition_by": partition_by });
            push_node(
                nodes,
                expr,
                cache,
                format!("Sort({} keys)", keys.len()),
                detail,
                vec![c],
            )
        }
        QueryExpr::Limit { n, offset, child } => {
            let c = build(child, nodes, cache, find_winner);
            let detail = serde_json::json!({ "n": n, "offset": offset });
            push_node(nodes, expr, cache, format!("Limit({n})"), detail, vec![c])
        }
        QueryExpr::PromqlSubquery {
            range,
            resolution,
            child,
        } => {
            let c = build(child, nodes, cache, find_winner);
            let detail = serde_json::json!({ "range": range, "resolution": resolution });
            push_node(nodes, expr, cache, "PromqlSubquery".into(), detail, vec![c])
        }
        QueryExpr::TimeRange { range, child } => {
            let c = build(child, nodes, cache, find_winner);
            let detail = serde_json::json!({ "range": range });
            push_node(
                nodes,
                expr,
                cache,
                format!("TimeRange({range:?})"),
                detail,
                vec![c],
            )
        }
        QueryExpr::TimeShift { shift, child } => {
            let c = build(child, nodes, cache, find_winner);
            let detail = serde_json::json!({ "shift": shift });
            push_node(nodes, expr, cache, "TimeShift".into(), detail, vec![c])
        }
        QueryExpr::SQLWindowFunc {
            func,
            args,
            partition_by,
            order_by,
            frame,
            output_name,
            child,
        } => {
            let c = build(child, nodes, cache, find_winner);
            let detail = serde_json::json!({
                "func": func,
                "args": args,
                "partition_by": partition_by,
                "order_by": order_by,
                "frame": frame,
                "output_name": output_name,
            });
            push_node(
                nodes,
                expr,
                cache,
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
            let l = build(lhs, nodes, cache, find_winner);
            let r = build(rhs, nodes, cache, find_winner);
            let detail = serde_json::json!({ "op": op.to_string(), "vector_match": vector_match });
            push_node(
                nodes,
                expr,
                cache,
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

    /// `export` itself never populates higher-layer annotations. Empty
    /// annotations must not appear in serialized JSON, so ordinary (non-ASAP)
    /// exports retain their existing shape.
    #[test]
    fn export_omits_empty_higher_layer_annotations() {
        let graph = export(&scan("metrics", value_col()));
        assert!(graph.nodes[0].notes.is_empty());
        assert!(graph.nodes[0].decision.is_none());
        assert!(graph.nodes[0].schema.is_some());
        assert!(graph.edge_annotations.is_empty());
        let json = serde_json::to_string(&graph.nodes[0]).unwrap();
        assert!(
            !json.contains("notes"),
            "empty `notes` must be skipped, not serialized as `[]`: {json}"
        );
        assert!(
            !json.contains("decision"),
            "empty `decision` must be skipped, not serialized as `null`: {json}"
        );
        let graph_json = serde_json::to_string(&graph).unwrap();
        assert!(
            !graph_json.contains("edge_annotations"),
            "empty `edge_annotations` must be skipped, not serialized as `[]`: {graph_json}"
        );
    }

    #[test]
    fn export_post_asap_does_not_invent_costs_for_shared_edges() {
        // Two *distinct* parents (a Dedup and a Limit, each with their own
        // single child slot) share the exact same `Rc` Scan —
        // `export_post_asap` must merge them onto one node id. Sharing alone
        // is not physical cost evidence, so no edge cost may be fabricated.
        let shared_scan = Rc::new(scan("metrics", value_col()));
        let left_branch = QueryExpr::Dedup {
            cols: vec![0],
            child: Rc::clone(&shared_scan),
        };
        let right_branch = QueryExpr::Limit {
            n: 5,
            offset: 0,
            child: Rc::clone(&shared_scan),
        };
        let root = QueryExpr::Concat {
            children: vec![left_branch, right_branch],
            discriminator_unique_key: None,
        };
        let graph = export_post_asap(&root, &mut |_| None);

        assert_eq!(
            graph.nodes.iter().filter(|n| n.kind == "Scan").count(),
            1,
            "the shared Scan must be merged onto one node, not duplicated"
        );
        assert!(graph.edge_annotations.is_empty());
    }

    /// Regression test: a single parent referencing the same shared child
    /// from two of its own operand slots at once (a `Join` whose left and
    /// right sides are the exact same `Rc`, post pointer-dedup) is *one*
    /// downstream consumer, not two — this must not inflate
    /// produce an edge-cost annotation without explicit physical evidence.
    #[test]
    fn a_single_parent_referencing_a_shared_child_twice_is_one_consumer_not_two() {
        let shared_scan = Rc::new(scan("metrics", value_col()));
        let root = QueryExpr::Join {
            kind: crate::pre_asap::query_expr::JoinKind::Inner,
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Boolean(true)))),
            left: Rc::clone(&shared_scan),
            right: Rc::clone(&shared_scan),
        };
        let graph = export_post_asap(&root, &mut |_| None);

        assert_eq!(
            graph.nodes.iter().filter(|n| n.kind == "Scan").count(),
            1,
            "the shared Scan must be merged onto one node, not duplicated"
        );
        assert!(
            graph.edge_annotations.is_empty(),
            "a single parent referencing the same child twice is one consumer, not a genuine \
             multi-consumer share — got: {:?}",
            graph.edge_annotations
        );
    }

    #[test]
    fn export_never_produces_edge_annotations_since_it_never_shares_nodes() {
        // Plain `export` (no `export_post_asap`) never deduplicates by `Rc`
        // pointer identity — even a workload-level shared subtree renders as
        // two independent tree nodes here, so there is nothing to annotate.
        let shared_scan = Rc::new(scan("metrics", value_col()));
        let root = QueryExpr::Join {
            kind: crate::pre_asap::query_expr::JoinKind::Inner,
            pred: Predicate(Rc::new(QueryExpr::Literal(ScalarValue::Boolean(true)))),
            left: Rc::clone(&shared_scan),
            right: Rc::clone(&shared_scan),
        };
        let graph = export(&root);
        assert_eq!(graph.nodes.iter().filter(|n| n.kind == "Scan").count(), 2);
        assert!(graph.edge_annotations.is_empty());
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
        let expr = QueryExpr::concat(vec![
            scan("a", value_col()),
            scan("b", value_col()),
            scan("c", value_col()),
        ]);
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
            Some(structural_hash(&leaf, &mut HashCache::new())),
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
            Some(structural_hash(&root, &mut HashCache::new())),
            "Filter root hash must match cse::structural_hash(&root, &mut HashCache::new())"
        );

        let filter = &graph.nodes[graph.root as usize];
        let agg_node = &graph.nodes[filter.children[0] as usize];
        assert_eq!(
            agg_node.hash,
            Some(structural_hash(&agg, &mut HashCache::new())),
            "the exported Aggregate node's hash must match cse::structural_hash \
             on the Aggregate subtree it represents, not just the root"
        );
    }

    /// Issue #172: a readout's guarantee is exported structurally — metric,
    /// symbolic bound, failure probability, provenance (allocation
    /// included) — and a rejection carries its typed reason.
    #[test]
    fn export_carries_guarantee_allocation_and_rejection_reason() {
        use crate::post_asap::{
            BoundExpr, CompositionOperator, ErrorMetric, GroupingStrategy, GuaranteeSource,
            ProbabilityExpr, SketchAlgorithm, SketchKind, SketchParams, SketchQuery,
            SummaryFamilyType, SummarySchema,
        };
        let leaf = Rc::new(scan("t", vec![Column::new("v", DataType::Float64, false)]));
        let kept = Rc::new(SummaryNode {
            expr: SummaryExpr::KeepPreAsap(Rc::clone(&leaf)),
            schema: SummarySchema {
                fields: vec![],
                time_index: None,
            },
            guarantee: Some(ResultGuarantee::exact("KeepPreAsap")),
        });
        let agg = Rc::new(SummaryNode {
            expr: SummaryExpr::SummaryAgg {
                child: kept,
                family: SummaryFamilyType::Sketch(
                    SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 40 }),
                    GroupingStrategy::default(),
                ),
                input: crate::post_asap::SummaryUpdate::column(
                    crate::pre_asap::expr_ir::ColumnRef::Named("v".into()),
                ),
                reduction: Reduction::by(vec![]),
                grouping: GroupingStrategy::default(),
            },
            schema: SummarySchema {
                fields: vec![],
                time_index: None,
            },
            guarantee: None,
        });
        let guarantee = ResultGuarantee {
            metric: ErrorMetric::Rank,
            bound: BoundExpr::Sum {
                terms: vec![
                    BoundExpr::Constant { value: 0.05 },
                    BoundExpr::Constant { value: 0.05 },
                ],
            },
            failure_probability: ProbabilityExpr::UnionBound {
                terms: vec![ProbabilityExpr::Constant { value: 0.01 }],
            },
            provenance: vec![
                GuaranteeSource::CompositionStep {
                    operator: CompositionOperator::ApproximateAggregate,
                    rule: "additive_union_bound".into(),
                },
                GuaranteeSource::BudgetAllocation {
                    allocator: "EqualSplitAllocator".into(),
                    layer: 0,
                    layer_count: 2,
                    local_target: AccuracyTarget::Epsilon(0.05),
                    end_to_end_target: AccuracyTarget::Epsilon(0.1),
                },
            ],
        };
        let root = SummaryNode {
            expr: SummaryExpr::SummaryEstimate {
                summary_input: agg,
                query: SketchQuery::Quantile { q: 0.99 },
            },
            schema: SummarySchema {
                fields: vec![],
                time_index: None,
            },
            guarantee: Some(guarantee),
        };
        let graph = export_summary(&root);
        let json = serde_json::to_value(&graph).unwrap();
        let root_json = &json["nodes"][graph.root as usize];
        assert_eq!(root_json["guarantee"]["metric"], "rank");
        assert_eq!(root_json["guarantee"]["bound"]["op"], "sum");
        assert_eq!(
            root_json["guarantee"]["failure_probability"]["op"],
            "union_bound"
        );
        let provenance = root_json["guarantee"]["provenance"].as_array().unwrap();
        assert!(provenance
            .iter()
            .any(|s| s["kind"] == "budget_allocation" && s["layer_count"] == 2));
        assert!(provenance.iter().any(|s| s["kind"] == "composition_step"));
        // Raw sketch state carries none; the exact leaf carries zero error.
        let state = &json["nodes"][1];
        assert_eq!(state["kind"], "SummaryAgg");
        assert!(state.get("guarantee").is_none());
        assert_eq!(json["nodes"][0]["guarantee"]["bound"]["op"], "zero");

        let named = NamedGraph {
            name: "q".into(),
            source: None,
            graph: export(&leaf),
            replacements: vec![],
            post_graph: None,
            workload_cost: None,
            rejections: vec![TargetRejection {
                target_pre_id: 0,
                strategy: "SketchAlgorithmStrategy".into(),
                description: "quantile over quantile".into(),
                error: AccuracyError::UnsupportedComposition {
                    operator: CompositionOperator::ApproximateAggregate,
                    input_metrics: vec![ErrorMetric::Rank],
                    local_metric: Some(ErrorMetric::Rank),
                    reason: "no registered rule".into(),
                },
            }],
        };
        let json = serde_json::to_value(&named).unwrap();
        assert_eq!(
            json["rejections"][0]["error"]["kind"],
            "unsupported_composition"
        );
        assert_eq!(json["rejections"][0]["error"]["input_metrics"][0], "rank");
        // Additive: a graph with no rejections omits the key entirely.
        let plain = NamedGraph {
            rejections: vec![],
            ..named
        };
        assert!(serde_json::to_value(&plain)
            .unwrap()
            .get("rejections")
            .is_none());
    }

    fn viewer_kind_categories() -> std::collections::BTreeMap<String, String> {
        const START: &str = "const KIND_CATEGORY_JSON = `";
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tools/dag-viewer/node-style.js"
        ));
        let json = source
            .split_once(START)
            .expect("node-style.js must declare KIND_CATEGORY_JSON")
            .1
            .split_once("`;")
            .expect("KIND_CATEGORY_JSON must be a template literal")
            .0;
        serde_json::from_str(json).expect("KIND_CATEGORY_JSON must be valid JSON")
    }

    #[test]
    fn viewer_categorizes_exactly_the_exported_node_kinds() {
        let expected: std::collections::BTreeSet<_> = QUERY_KIND_TAGS
            .iter()
            .chain(SUMMARY_KIND_TAGS)
            .copied()
            .chain(std::iter::once("KeepPreAsap"))
            .collect();
        assert_eq!(
            expected.len(),
            QUERY_KIND_TAGS.len() + SUMMARY_KIND_TAGS.len() + 1,
            "exported kind tags must be unique"
        );
        let categories = viewer_kind_categories();
        let actual: std::collections::BTreeSet<_> = categories.keys().map(String::as_str).collect();

        assert_eq!(actual, expected);
    }
}
