//! Export an [`OperatorNode`] DAG as a generic node/edge graph, for tools
//! that need to render or diff the IR (the `dag_export` devtools binary +
//! the `tools/dag-viewer` viewer — see issue #133) rather than walk it in
//! Rust.
//!
//! `OperatorNode` already derives `Serialize`, but as a Rust-shaped tagged
//! tree (`Rc` children nested inside each variant's own field, repeated once
//! per reference). This module flattens that into an explicit node list +
//! child-id edges — one entry per unique node, deduplicated by `Rc` pointer
//! identity, so a shared subtree stays one node with several parents — and
//! additionally tags each node with
//! [`structural_hash`](crate::ir::cse::structural_hash), so a caller with
//! several exported queries can spot identical subtrees (a shared `Scan`, a
//! repeated `Aggregate` shape, …) by comparing hashes rather than
//! re-implementing structural comparison client-side.
//!
//! This is literally the same hashing
//! [`share_common_subtrees`](crate::ir::cse::share_common_subtrees) uses to
//! bucket candidates in its `InternTable` (issue #223 stage 3) — not a
//! parallel reimplementation. `tools/dag-viewer`'s "shared subtree"
//! highlighting is still a *proxy* for real CSE, though: a hash match here
//! only means two nodes are legal `InternTable` bucket-mates (same coarse
//! hash) — it does not mean `share_common_subtrees` actually ran on this
//! data and merged them onto one `Rc` (that also requires the structural
//! equality check `InternTable::intern` performs, and the
//! `Schema::has_unique_key` legality gate, neither of which this export
//! step evaluates). See `tools/dag-viewer/README.md` for the up-to-date
//! caveat.
//!
//! There is one IR before and after ASAP optimization, so there is one
//! exporter: an ordinary operator and an ASAP summary operator are both
//! rendered by the same per-variant [`shape`] match, whichever entry point
//! ([`export`], [`export_summary`], [`export_post_asap`]) reached them.
//!
//! ## Scalar expressions
//!
//! A [`ScalarExpr`] is owned by value by an operator field (`Filter.pred`,
//! `Project.cols`, …) and is rendered into that operator's `detail`, not as
//! a node of its own. The operator nodes a scalar expression reads
//! (`scalar(v)`, `EXISTS (subquery)`, …) *are* nodes of the graph — they are
//! in [`OperatorNode::children`] — so inside `detail` each such reference is
//! rendered as `{"scalar_ref": <child node id>}` rather than inlined.
//!
//! ## `DagNode::notes` — a layering seam, not a feature this module implements
//!
//! [`DagNode`] also carries `notes: Vec<`[`DagNote`]`>`, always empty coming
//! out of [`export`]. It exists so a *higher* layer — one that depends on
//! `asap_types`, never the reverse — can annotate an already-exported graph
//! after the fact without this module needing to know anything about that
//! layer's concepts. Concretely: `asap-aware-mapping`'s `explanation` module
//! (issue #257) computes `structural_hash` over the same nodes this module
//! does (via the identical function). The devtools exporter uses that hash
//! to narrow candidates, then compares its target with
//! [`DagNode::source_node`] for a collision-safe match before pushing a
//! [`DagNote`] onto the node. `asap_types` itself never constructs a
//! `DagNote` — see [`DagNode::notes`] for the layering rule this keeps.

use std::collections::HashMap;
use std::rc::Rc;

use serde::Serialize;

use crate::cost::CostAnnotation;
use crate::ir::cse::{structural_hash, HashCache};
use crate::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, ScalarExpr};
use crate::post_asap::{AccuracyError, ResultGuarantee};
use crate::pre_asap::schema::FieldDataType;
use crate::pre_asap::vocabulary::Source;

/// One flattened IR node. `detail` holds this node's own scalar fields
/// (predicates, aggregate funcs, schema, sort keys, …) — everything except
/// its children, which live in `children` instead.
#[derive(Debug, Clone, Serialize)]
pub struct DagNode {
    pub id: u32,
    /// The operator variant name — [`Operator::kind_name`] (e.g.
    /// `"Aggregate"`, `"SummaryAgg"`).
    pub kind: &'static str,
    /// Short human-readable summary for a node's collapsed on-graph label.
    pub label: String,
    pub detail: serde_json::Value,
    /// Output schema carried by every exported node ([`OperatorNode::schema`]
    /// as JSON). Edge renderers use the child node's schema as the schema
    /// flowing along child → consumer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
    /// Child node ids in [`OperatorNode::children`] order: the operator's
    /// own inputs in field order (e.g. `Join` is `[left, right]`), then the
    /// nodes referenced from its scalar expressions.
    pub children: Vec<u32>,
    /// Explicit workload-wide identity assigned by a higher-level exporter.
    /// Viewers use this field to union nodes and must not reconstruct a
    /// structural signature client-side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workload_node_id: Option<u32>,
    /// [`structural_hash`](crate::ir::cse::structural_hash) of the subtree
    /// rooted at this node — the exact same function `cse`'s `InternTable`
    /// uses to bucket CSE candidates, so two nodes hash equally here iff they
    /// would land in the same `InternTable` bucket. See the module doc for
    /// what a hash match here does and doesn't guarantee. Always `Some`
    /// for a node this module produces; the `Option` is retained for the
    /// JSON shape (`None` is omitted rather than serialized as a sentinel,
    /// since `0` is a legal hash).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<u64>,
    /// The exported node itself, for in-process annotation matching. Not
    /// part of the JSON format: callers first narrow by `hash`, then
    /// compare this value (by pointer or structurally) to avoid treating a
    /// hash collision as node identity. Always `Some` for a node this
    /// module produces.
    #[serde(skip)]
    pub source_node: Option<Rc<OperatorNode>>,
    /// In-process identity of `source_node` (`Rc::as_ptr` as an address):
    /// the key the builder deduplicates on, so a node reached from several
    /// parents is exported once. Not part of the JSON format.
    #[serde(skip)]
    pub source_ptr: Option<usize>,
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

/// One query's exported graph. `nodes[root as usize]` is the DAG's root.
#[derive(Debug, Clone, Serialize)]
pub struct DagGraph {
    pub nodes: Vec<DagNode>,
    pub root: u32,
    /// See [`EdgeCostAnnotation`]. Always empty unless a higher layer
    /// explicitly populated it (same layering rule as [`DagNode::notes`]);
    /// omitted from JSON entirely when empty, so every existing producer of
    /// [`DagGraph`] is unaffected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edge_annotations: Vec<EdgeCostAnnotation>,
}

/// A single named query within a multi-query export.
#[derive(Debug, Clone, Serialize)]
pub struct NamedGraph {
    pub name: String,
    /// The original query text (SQL or PromQL) this graph was lowered from,
    /// for display alongside the graph — not used by `export` itself, since
    /// that only sees the already-lowered DAG. Optional because not every
    /// producer of a `NamedGraph` has the source text on hand.
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
    /// query: every node that has no winning replacement renders as it does
    /// in [`export`], and every node that does splices in its winning
    /// candidate's subtree instead, in the very same node list. `None`
    /// unless a higher layer explicitly built one (e.g. the `dag_export`
    /// devtools binary's `--post-asap` flag); omitted from the JSON entirely
    /// when absent, so every existing producer/consumer of `NamedGraph` is
    /// unaffected.
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

// ── Post-ASAP replacement export — a layering-seam-shaped feature ──────────
//
// [`TargetReplacement`] is the generic, crate-agnostic "one replacement
// site, before and after" shape a higher layer (`asap-aware-mapping`, via
// the `dag_export` devtools binary's `--post-asap` flag) populates after
// running its own search — the exact same layering rule [`DagNode::notes`]'s
// doc above already states: this module never runs
// `asap_aware_mapping::replacement::search_workload_with` itself, never
// picks a "winning" candidate, and has no opinion on what a
// `ReplacementProvenance` or a cost model even is. It only defines shapes
// concrete and serializable enough for a higher layer to fill in, and for
// `tools/dag-viewer` to render without needing to know anything about
// `asap-aware-mapping`'s own vocabulary.

/// One flattened node of a [`SummaryDagGraph`] — the same node as a
/// [`DagNode`], in the shape the summary-maintenance consumers read:
/// snake_case `kind`, the accuracy guarantee as its own field, no
/// hash/annotation seams.
#[derive(Debug, Clone, Serialize)]
pub struct SummaryDagNode {
    pub id: u32,
    /// The operator variant name in snake_case (e.g. `"summary_agg"`,
    /// `"scan"`) — see [`snake_case_kind`].
    pub kind: &'static str,
    /// Short human-readable summary for a node's collapsed on-graph label.
    pub label: String,
    pub detail: serde_json::Value,
    /// [`OperatorNode::schema`] as JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
    /// Child node ids in [`OperatorNode::children`] order.
    pub children: Vec<u32>,
    /// The value's machine-readable accuracy guarantee (issue #172) —
    /// [`OperatorNode::guarantee`] serialized structurally (metric, symbolic
    /// bound, failure probability, provenance including any budget
    /// allocation), not as prose. Omitted when the node carries none (raw
    /// summary state, or a family with no error model), so every consumer
    /// predating this field parses the same shape it always has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guarantee: Option<ResultGuarantee>,
    /// The exported node itself, so a caller annotating the graph can find
    /// a node by `Rc` pointer identity rather than by walk order. Not part
    /// of the JSON format. Always `Some`.
    #[serde(skip)]
    pub source_node: Option<Rc<OperatorNode>>,
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

/// A DAG flattened into [`SummaryDagNode`]s — the same graph [`DagGraph`]
/// holds, in the summary-maintenance consumers' node shape.
#[derive(Debug, Clone, Serialize)]
pub struct SummaryDagGraph {
    pub nodes: Vec<SummaryDagNode>,
    pub root: u32,
}

/// One replacement site a higher layer (the `dag_export` binary) found by
/// running `asap_aware_mapping::replacement::search_workload_with` +
/// `PlanSpace::cost_sorted` and picking the best-ranked candidate for one
/// `TargetSubDAGCandidates` — `asap_types` never runs that search itself (same layering
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
    /// This candidate's rank among its `TargetSubDAGCandidates`'s alternatives after
    /// `PlanSpace::cost_sorted` (`0` = best). Exposed so a renderer can show
    /// "this was the best of N candidates" without re-deriving the ranking.
    pub rank: usize,
    /// This candidate's own estimated cost, straight off
    /// `RankedTargetSubDAGCandidates::costs` — `f64::NAN` whenever the plugged-in cost model
    /// doesn't estimate a numeric cost for this candidate shape (see that
    /// field's own doc upstream).
    pub cost: f64,
    /// The target's own subtree, before replacement — literally
    /// `export(target)` for the `TargetSubDAGCandidates`'s own `target`, reused as-is.
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
/// or a still-relational structural rewrite, mirroring
/// `asap_aware_mapping::replacement::Replacement`'s own two variants. Both
/// carry an ordinary [`DagGraph`]: the unified IR renders a summary subtree
/// and a rewritten relational subtree through the same [`export`].
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
    Summary(DagGraph),
    /// A `Replacement::Rewrite` candidate — still relational (CSE
    /// share/recompute, `AvgToSumOverCountStrategy`, and `RollupStrategy`
    /// all produce this kind).
    Rewrite(DagGraph),
}

/// What a higher layer found for one specific node when building a merged
/// post-ASAP graph via [`export_post_asap`] — see that function's own doc
/// for the full design. `asap_types` has no opinion on *how* this is
/// decided (that's `asap_aware_mapping::replacement::search_workload_with` +
/// `PlanSpace::cost_sorted`'s job, a higher layer, exactly the layering rule
/// [`DagNode::notes`] already states); it only defines the shape a decision
/// comes back in. Both variants render identically (one IR, one builder);
/// they are kept apart so the caller's `Replacement` maps one-to-one.
#[derive(Debug, Clone)]
pub enum PostAsapSubstitution {
    /// This exact node has a winning `Replacement::Rewrite` — keep building
    /// from `replacement` instead of the original node.
    Rewrite {
        replacement: Rc<OperatorNode>,
        decision: DagDecision,
    },
    /// This exact node has a winning `Replacement::Summary` — keep building
    /// from `replacement` (a summary-bound subtree) instead of the original
    /// node.
    Summary {
        replacement: Rc<OperatorNode>,
        decision: DagDecision,
    },
}

/// Flatten the DAG rooted at `root` into a [`DagGraph`]: one [`DagNode`]
/// per unique reachable node, children pushed before their parents.
pub fn export(root: &Rc<OperatorNode>) -> DagGraph {
    let mut no_substitution = |_: &Rc<OperatorNode>| None;
    let mut builder = Builder::new(&mut no_substitution);
    let root = builder.build(root);
    builder.finish(root)
}

/// Flatten the DAG rooted at `node` into a [`SummaryDagGraph`] — the same
/// nodes [`export`] produces, in the [`SummaryDagNode`] shape (snake_case
/// `kind`, `guarantee` as its own field).
pub fn export_summary(node: &Rc<OperatorNode>) -> SummaryDagGraph {
    let graph = export(node);
    let nodes = graph
        .nodes
        .into_iter()
        .map(|node| {
            let source = node
                .source_node
                .expect("every exported node carries its source");
            SummaryDagNode {
                id: node.id,
                kind: snake_case_kind(&source.operator),
                label: node.label,
                detail: node.detail,
                schema: node.schema,
                children: node.children,
                guarantee: source.guarantee.clone(),
                source_node: Some(source),
            }
        })
        .collect();
    SummaryDagGraph {
        nodes,
        root: graph.root,
    }
}

/// Build one merged "whole query, but post-ASAP" [`DagGraph`] by walking
/// `root` and, at every node, asking `find_winner` whether *that exact
/// node* has a winning replacement — if so, splicing the replacement's own
/// subtree in at that position instead, in the very same flattened node
/// list (not a nested sub-graph the way [`TargetReplacement::before`]/
/// `::after` — small, independent, per-site before/after pairs — do).
///
/// `find_winner` is the whole layering seam: `asap_types` never runs
/// `asap_aware_mapping::replacement::search_workload_with` or
/// `PlanSpace::cost_sorted` itself, and has no idea what a `TargetSubDAGCandidates` or a
/// `ReplacementProvenance` is — it only asks, for one node at a time, "did a
/// higher layer already decide something for you?" A caller (e.g. the
/// `dag_export` devtools binary) builds this closure once per workload
/// search, over whatever hash/structural-equality lookup it already needs
/// for [`TargetReplacement`] discovery, and passes it in here unchanged.
///
/// `find_winner` is deliberately consulted only once per node, at the
/// moment the builder first reaches it — **not** re-consulted on a
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
///
/// Every node a substitution introduced carries the substitution's
/// [`DagDecision`] (`role = "replacement_root"` on the spliced-in root,
/// `"replacement_region"` on its newly exported descendants); a descendant
/// that was already exported before the splice (a shared input the
/// replacement reuses) keeps whatever it already had.
pub fn export_post_asap(
    root: &Rc<OperatorNode>,
    find_winner: &mut dyn FnMut(&Rc<OperatorNode>) -> Option<PostAsapSubstitution>,
) -> DagGraph {
    let mut builder = Builder::new(find_winner);
    let root = builder.build(root);
    builder.finish(root)
}

/// The one flattening pass behind every entry point. Nodes are memoized by
/// `Rc` pointer identity: a node reached from several parents (an operator
/// input shared with a scalar reference, say) is exported once.
struct Builder<'a> {
    nodes: Vec<DagNode>,
    /// `Rc::as_ptr` of every node already exported (or substituted) → its id.
    ids: HashMap<*const OperatorNode, u32>,
    /// One cache for the whole export — persisted across every node, not
    /// reset per node, so `structural_hash` memoizes real work across this
    /// pass instead of re-walking an already-hashed shared descendant once
    /// per node that references it.
    cache: HashCache,
    find_winner: &'a mut dyn FnMut(&Rc<OperatorNode>) -> Option<PostAsapSubstitution>,
}

impl<'a> Builder<'a> {
    fn new(
        find_winner: &'a mut dyn FnMut(&Rc<OperatorNode>) -> Option<PostAsapSubstitution>,
    ) -> Self {
        Self {
            nodes: Vec::new(),
            ids: HashMap::new(),
            cache: HashCache::new(),
            find_winner,
        }
    }

    fn finish(self, root: u32) -> DagGraph {
        DagGraph {
            nodes: self.nodes,
            root,
            edge_annotations: Vec::new(),
        }
    }

    /// Export `node` (or, when `find_winner` has a substitution for it, the
    /// substitution's subtree in its place) and return its id.
    fn build(&mut self, node: &Rc<OperatorNode>) -> u32 {
        let ptr = Rc::as_ptr(node);
        if let Some(&id) = self.ids.get(&ptr) {
            return id;
        }
        let (replacement, decision) = match (self.find_winner)(node) {
            None => return self.build_node(node),
            Some(PostAsapSubstitution::Rewrite {
                replacement,
                decision,
            })
            | Some(PostAsapSubstitution::Summary {
                replacement,
                decision,
            }) => (replacement, decision),
        };
        let first = self.nodes.len();
        let root = self.build_node(&replacement);
        for exported in &mut self.nodes[first..] {
            if exported.decision.is_none() {
                let mut node_decision = decision.clone();
                node_decision.role = if exported.id == root {
                    "replacement_root"
                } else {
                    "replacement_region"
                };
                exported.decision = Some(node_decision);
            }
        }
        // The original node now resolves to the substitution: another
        // parent of the same `Rc` reuses the spliced-in subtree.
        self.ids.insert(ptr, root);
        root
    }

    /// Export `node` itself (no substitution check at this level; children
    /// still go through [`Self::build`]) and return its id.
    fn build_node(&mut self, node: &Rc<OperatorNode>) -> u32 {
        let ptr = Rc::as_ptr(node);
        if let Some(&id) = self.ids.get(&ptr) {
            return id;
        }
        let children: Vec<u32> = node.children().into_iter().map(|c| self.build(c)).collect();
        let (label, mut detail) = shape(node, &self.ids);
        if let serde_json::Value::Object(map) = &mut detail {
            if let Some(timing) = node.timing {
                map.insert("timing".into(), serde_json::json!(timing.as_str()));
            }
            if let Some(guarantee) = &node.guarantee {
                if let Ok(value) = serde_json::to_value(guarantee) {
                    map.insert("guarantee".into(), value);
                }
            }
        }
        let hash = structural_hash(node, &mut self.cache);
        self.cache.insert(ptr, hash);
        let id = self.nodes.len() as u32;
        self.nodes.push(DagNode {
            id,
            kind: node.operator.kind_name(),
            label,
            detail,
            schema: serde_json::to_value(&node.schema).ok(),
            children,
            workload_node_id: None,
            hash: Some(hash),
            source_node: Some(Rc::clone(node)),
            source_ptr: Some(ptr as usize),
            notes: Vec::new(),
            decision: None,
        });
        self.ids.insert(ptr, id);
        id
    }
}

/// [`Operator::kind_name`] in snake_case, for [`SummaryDagNode::kind`].
/// Exhaustive so a new operator variant fails to compile here until it is
/// named.
fn snake_case_kind(operator: &Operator) -> &'static str {
    match operator {
        Operator::NonASAP(op) => match op {
            NonASAPOp::Scan { .. } => "scan",
            NonASAPOp::Values { .. } => "values",
            NonASAPOp::Filter { .. } => "filter",
            NonASAPOp::Project { .. } => "project",
            NonASAPOp::Aggregate { .. } => "aggregate",
            NonASAPOp::Join { .. } => "join",
            NonASAPOp::SetOp { .. } => "set_op",
            NonASAPOp::Concat { .. } => "concat",
            NonASAPOp::Dedup { .. } => "dedup",
            NonASAPOp::Sort { .. } => "sort",
            NonASAPOp::Limit { .. } => "limit",
            NonASAPOp::BinaryOp { .. } => "binary_op",
            NonASAPOp::SQLWindowFunc { .. } => "sql_window_func",
            NonASAPOp::TimeRange { .. } => "time_range",
            NonASAPOp::TimeShift { .. } => "time_shift",
            NonASAPOp::PromqlVectorFromScalar(_) => "promql_vector_from_scalar",
            NonASAPOp::PromqlRelabel { .. } => "promql_relabel",
            NonASAPOp::PromqlInfoEnrich { .. } => "promql_info_enrich",
            NonASAPOp::PromqlSeriesSample { .. } => "promql_series_sample",
            NonASAPOp::PromqlSubquery { .. } => "promql_subquery",
            NonASAPOp::ScalarBridge(_) => "scalar_bridge",
        },
        Operator::ASAP(op) => match op {
            ASAPOp::SummaryAgg { .. } => "summary_agg",
            ASAPOp::SummaryEstimate { .. } => "summary_estimate",
            ASAPOp::FinalizeExactAccumulator { .. } => "finalize_exact_accumulator",
            ASAPOp::MaintainPopulation { .. } => "maintain_population",
            ASAPOp::ReadPopulation { .. } => "read_population",
            ASAPOp::SummaryMerge { .. } => "summary_merge",
            ASAPOp::SummarySubtract { .. } => "summary_subtract",
            ASAPOp::SummaryDelete { .. } => "summary_delete",
            ASAPOp::SummaryJoin { .. } => "summary_join",
            ASAPOp::Extension { .. } => "extension",
        },
    }
}

/// A short, human-readable label for a [`FieldDataType`] (e.g.
/// `"Sketch(Kll)"`, `"ExactAggregate(Sum)"`) — for the label text on a
/// `SummaryAgg`/`SummaryJoin` node. Every variant is covered, via `Debug`
/// for the inner kind rather than hand-written prose per algorithm.
fn family_label(family: &FieldDataType) -> String {
    match family {
        FieldDataType::Plain(dtype) => format!("Plain({dtype:?})"),
        FieldDataType::ExactAggregate(kind, _) => format!("ExactAggregate({kind:?})"),
        FieldDataType::Sketch(kind, _grouping) => format!("Sketch({:?})", kind.algorithm()),
        FieldDataType::Sample(kind, _) => format!("Sample({kind:?})"),
        FieldDataType::Wavelet(kind, _) => format!("Wavelet({kind:?})"),
        FieldDataType::StatModel(kind, _) => format!("StatModel({kind:?})"),
    }
}

fn source_label(source: &Source) -> String {
    match source {
        Source::Table { table_ref } => table_ref.clone(),
        Source::TimeSeries { metric } => metric.clone(),
    }
}

/// `(label, detail)` for one node: its own fields, never its children.
/// Exhaustive over every operator variant — a new one fails to compile
/// here until this match is extended, matching the rest of the IR's
/// exhaustive-match style. Scalar expressions are rendered through
/// [`scalar_json`] with `ids` resolving their operator references.
fn shape(
    node: &OperatorNode,
    ids: &HashMap<*const OperatorNode, u32>,
) -> (String, serde_json::Value) {
    let scalar = |expr: &ScalarExpr| scalar_json(expr, ids);
    let scalars =
        |exprs: &[ScalarExpr]| -> Vec<serde_json::Value> { exprs.iter().map(scalar).collect() };
    let predicate = |pred: &crate::ir::Predicate| scalar(&pred.0);
    let sort_keys = |keys: &[crate::ir::SortKey]| -> Vec<serde_json::Value> {
        keys.iter()
            .map(|key| {
                serde_json::json!({
                    "expr": scalar(&key.expr),
                    "ascending": key.ascending,
                    "nulls_first": key.nulls_first,
                })
            })
            .collect()
    };
    match &node.operator {
        Operator::NonASAP(op) => match op {
            NonASAPOp::Scan {
                source,
                predicates,
                schema,
            } => (
                format!("Scan({})", source_label(source)),
                serde_json::json!({
                    "source": source,
                    "predicates": predicates.iter().map(predicate).collect::<Vec<_>>(),
                    "schema": schema,
                }),
            ),
            NonASAPOp::Values { rows, schema } => (
                format!("Values({} rows)", rows.len()),
                serde_json::json!({
                    "rows": rows.iter().map(|row| scalars(row)).collect::<Vec<_>>(),
                    "schema": schema,
                }),
            ),
            NonASAPOp::Filter { pred, .. } => (
                "Filter".into(),
                serde_json::json!({ "pred": predicate(pred) }),
            ),
            NonASAPOp::Project {
                cols, qualifier, ..
            } => (
                format!("Project({} cols)", cols.len()),
                serde_json::json!({
                    "cols": cols.iter().map(|item| serde_json::json!({
                        "alias": item.alias,
                        "expr": scalar(&item.expr),
                    })).collect::<Vec<_>>(),
                    "qualifier": qualifier,
                }),
            ),
            NonASAPOp::Aggregate {
                reduction,
                measures,
                output_names,
                having,
                ..
            } => (
                format!("Aggregate({} measures)", measures.len()),
                serde_json::json!({
                    "reduction": reduction,
                    "measures": measures,
                    "output_names": output_names,
                    "having": having.as_ref().map(predicate),
                }),
            ),
            NonASAPOp::Join { kind, pred, .. } => (
                format!("Join({kind:?})"),
                serde_json::json!({ "kind": kind, "pred": predicate(pred) }),
            ),
            NonASAPOp::SetOp { kind, all, .. } => (
                format!("SetOp({kind:?})"),
                serde_json::json!({ "kind": kind, "all": all }),
            ),
            NonASAPOp::Concat {
                children,
                discriminator_unique_key,
            } => (
                format!("Concat({} branches)", children.len()),
                serde_json::json!({ "discriminator_unique_key": discriminator_unique_key }),
            ),
            NonASAPOp::Dedup { cols, .. } => (
                format!("Dedup({} cols)", cols.len()),
                serde_json::json!({ "cols": cols }),
            ),
            NonASAPOp::Sort {
                keys, partition_by, ..
            } => (
                format!("Sort({} keys)", keys.len()),
                serde_json::json!({ "keys": sort_keys(keys), "partition_by": partition_by }),
            ),
            NonASAPOp::Limit {
                n,
                offset,
                partition_by,
                ..
            } => (
                match n {
                    Some(n) => format!("Limit({n})"),
                    None => format!("Limit(offset {offset})"),
                },
                serde_json::json!({ "n": n, "offset": offset, "partition_by": partition_by }),
            ),
            NonASAPOp::BinaryOp {
                operator,
                return_bool,
                ..
            } => (
                format!("BinaryOp({})", operator.kind),
                serde_json::json!({
                    "op": operator.kind.to_string(),
                    "vector_match": operator.vector_match,
                    "checked_relative_division": operator.checked_relative_division,
                    "checked_finite_division": operator.checked_finite_division,
                    "return_bool": return_bool,
                }),
            ),
            NonASAPOp::SQLWindowFunc {
                func,
                args,
                partition_by,
                order_by,
                frame,
                output_name,
                ..
            } => (
                format!("SQLWindowFunc({func:?})"),
                serde_json::json!({
                    "func": func,
                    "args": scalars(args),
                    "partition_by": partition_by,
                    "order_by": sort_keys(order_by),
                    "frame": frame,
                    "output_name": output_name,
                }),
            ),
            NonASAPOp::TimeRange { range, kind, .. } => (
                format!("TimeRange({kind:?}, {range:?})"),
                serde_json::json!({ "range": range, "kind": kind }),
            ),
            NonASAPOp::TimeShift { shift, .. } => {
                ("TimeShift".into(), serde_json::json!({ "shift": shift }))
            }
            NonASAPOp::PromqlVectorFromScalar(value) => (
                "vector()".into(),
                serde_json::json!({ "value": scalar(value) }),
            ),
            NonASAPOp::PromqlRelabel { dst, value, .. } => (
                format!("PromqlRelabel(dst={dst})"),
                serde_json::json!({ "dst": dst, "value": scalar(value) }),
            ),
            NonASAPOp::PromqlInfoEnrich { selector, .. } => (
                "PromqlInfoEnrich".into(),
                serde_json::json!({ "selector": selector }),
            ),
            NonASAPOp::PromqlSeriesSample { by, kind, .. } => (
                format!("PromqlSeriesSample({kind:?})"),
                serde_json::json!({ "by": by, "kind": kind }),
            ),
            NonASAPOp::PromqlSubquery {
                range, resolution, ..
            } => (
                "PromqlSubquery".into(),
                serde_json::json!({ "range": range, "resolution": resolution }),
            ),
            NonASAPOp::ScalarBridge(value) => (
                format!("ScalarBridge({})", scalar_summary(value)),
                serde_json::json!({ "value": scalar(value) }),
            ),
        },
        Operator::ASAP(op) => match op {
            ASAPOp::SummaryAgg {
                family,
                input,
                reduction,
                grouping,
                ..
            } => (
                format!("SummaryAgg({})", family_label(family)),
                serde_json::json!({
                    "family": format!("{family:?}"),
                    "input": input,
                    "reduction": reduction,
                    "grouping": format!("{grouping:?}"),
                }),
            ),
            ASAPOp::SummaryEstimate { query, .. } => (
                format!("SummaryEstimate({query:?})"),
                serde_json::json!({ "query": format!("{query:?}") }),
            ),
            ASAPOp::FinalizeExactAccumulator { .. } => {
                ("FinalizeExactAccumulator".into(), serde_json::json!({}))
            }
            ASAPOp::MaintainPopulation { population, .. } => (
                format!("MaintainPopulation(max_k={})", population.max_k),
                serde_json::json!({ "population": population }),
            ),
            ASAPOp::ReadPopulation { readout, .. } => (
                format!("ReadPopulation({readout:?})"),
                serde_json::json!({ "readout": readout }),
            ),
            ASAPOp::SummaryMerge { children } => (
                format!("SummaryMerge({} children)", children.len()),
                serde_json::json!({}),
            ),
            ASAPOp::SummarySubtract { .. } => ("SummarySubtract".into(), serde_json::json!({})),
            ASAPOp::SummaryDelete { key, .. } => {
                ("SummaryDelete".into(), serde_json::json!({ "key": key }))
            }
            ASAPOp::SummaryJoin { key, family, .. } => (
                format!("SummaryJoin({})", family_label(family)),
                serde_json::json!({ "key": key, "family": format!("{family:?}") }),
            ),
            ASAPOp::Extension { name, .. } => (
                format!("Extension({name})"),
                serde_json::json!({ "name": name }),
            ),
        },
    }
}

/// A few characters describing a scalar leaf for a `ScalarBridge` label —
/// never the expression's `Debug` form, which would print every referenced
/// operator subtree.
fn scalar_summary(expr: &ScalarExpr) -> String {
    match expr {
        ScalarExpr::Literal(value) => format!("{value:?}"),
        ScalarExpr::Column(id) => format!("col{id}"),
        ScalarExpr::EvalTimestamp => "time()".into(),
        ScalarExpr::CurrentTimestamp => "now()".into(),
        ScalarExpr::PromqlScalarFromVector(_) => "scalar(..)".into(),
        ScalarExpr::ScalarSubquery(_) => "subquery".into(),
        ScalarExpr::Negative { .. } => "-..".into(),
        ScalarExpr::Compare { op, .. } => format!("{op:?}"),
        ScalarExpr::Arithmetic { op, .. } => format!("{op:?}"),
        ScalarExpr::FunctionCall { name, .. } => format!("{name}(..)"),
        ScalarExpr::BoolAnd(_) => "and".into(),
        ScalarExpr::BoolOr(_) => "or".into(),
        ScalarExpr::Not(_) => "not".into(),
        ScalarExpr::IsNull(_) => "is null".into(),
        ScalarExpr::IsNotNull(_) => "is not null".into(),
        ScalarExpr::Cast { to, .. } => format!("cast({to:?})"),
        ScalarExpr::InList { .. } => "in (..)".into(),
        ScalarExpr::InSubquery { .. } => "in (subquery)".into(),
        ScalarExpr::Exists { .. } => "exists".into(),
        ScalarExpr::Case { .. } => "case".into(),
    }
}

/// `{"scalar_ref": <id>}` for an operator node a scalar expression reads.
/// The node is one of the owning operator's children, so it has already
/// been exported by the time its parent's `detail` is built.
fn scalar_ref(
    node: &Rc<OperatorNode>,
    ids: &HashMap<*const OperatorNode, u32>,
) -> serde_json::Value {
    serde_json::json!({ "scalar_ref": ids.get(&Rc::as_ptr(node)).copied() })
}

/// `expr` as JSON in `ScalarExpr`'s own serde shape (externally tagged
/// variants), except that every operator reference is rendered via
/// [`scalar_ref`] instead of inlining the referenced subtree. Exhaustive so
/// a new variant fails to compile here until it is rendered.
fn scalar_json(expr: &ScalarExpr, ids: &HashMap<*const OperatorNode, u32>) -> serde_json::Value {
    let sub = |e: &ScalarExpr| scalar_json(e, ids);
    let list = |es: &[ScalarExpr]| -> Vec<serde_json::Value> { es.iter().map(sub).collect() };
    match expr {
        ScalarExpr::Column(id) => serde_json::json!({ "Column": id }),
        ScalarExpr::Literal(value) => serde_json::json!({ "Literal": value }),
        ScalarExpr::Negative { expr, semantics } => serde_json::json!({
            "Negative": { "expr": sub(expr), "semantics": semantics }
        }),
        ScalarExpr::Compare {
            left,
            op,
            right,
            semantics,
        } => serde_json::json!({
            "Compare": {
                "left": sub(left),
                "op": op,
                "right": sub(right),
                "semantics": semantics,
            }
        }),
        ScalarExpr::BoolAnd(parts) => serde_json::json!({ "BoolAnd": list(parts) }),
        ScalarExpr::BoolOr(parts) => serde_json::json!({ "BoolOr": list(parts) }),
        ScalarExpr::Not(e) => serde_json::json!({ "Not": sub(e) }),
        ScalarExpr::IsNull(e) => serde_json::json!({ "IsNull": sub(e) }),
        ScalarExpr::IsNotNull(e) => serde_json::json!({ "IsNotNull": sub(e) }),
        ScalarExpr::Cast { expr, to, try_cast } => serde_json::json!({
            "Cast": { "expr": sub(expr), "to": to, "try_cast": try_cast }
        }),
        ScalarExpr::InList {
            expr,
            list: items,
            negated,
        } => serde_json::json!({
            "InList": { "expr": sub(expr), "list": list(items), "negated": negated }
        }),
        ScalarExpr::FunctionCall { name, args } => serde_json::json!({
            "FunctionCall": { "name": name, "args": list(args) }
        }),
        ScalarExpr::Arithmetic {
            op,
            left,
            right,
            semantics,
        } => serde_json::json!({
            "Arithmetic": {
                "op": op,
                "left": sub(left),
                "right": sub(right),
                "semantics": semantics,
            }
        }),
        ScalarExpr::Case {
            operand,
            branches,
            else_expr,
        } => serde_json::json!({
            "Case": {
                "operand": operand.as_deref().map(sub),
                "branches": branches
                    .iter()
                    .map(|(when, then)| serde_json::json!([sub(when), sub(then)]))
                    .collect::<Vec<_>>(),
                "else_expr": else_expr.as_deref().map(sub),
            }
        }),
        ScalarExpr::CurrentTimestamp => serde_json::json!("CurrentTimestamp"),
        ScalarExpr::EvalTimestamp => serde_json::json!("EvalTimestamp"),
        ScalarExpr::PromqlScalarFromVector(node) => serde_json::json!({
            "PromqlScalarFromVector": scalar_ref(node, ids)
        }),
        ScalarExpr::ScalarSubquery(node) => serde_json::json!({
            "ScalarSubquery": scalar_ref(node, ids)
        }),
        ScalarExpr::Exists { subquery, negated } => serde_json::json!({
            "Exists": { "subquery": scalar_ref(subquery, ids), "negated": negated }
        }),
        ScalarExpr::InSubquery {
            expr,
            subquery,
            negated,
        } => serde_json::json!({
            "InSubquery": {
                "expr": sub(expr),
                "subquery": scalar_ref(subquery, ids),
                "negated": negated,
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;
    use crate::ir::Predicate;
    use crate::post_asap::{
        BoundExpr, CompositionOperator, ErrorMetric, GroupingStrategy, GuaranteeSource,
        ProbabilityExpr, SketchAlgorithm, SketchKind, SketchParams, SketchQuery, SummaryUpdate,
    };
    use crate::pre_asap::agg_intent::AggIntent;
    use crate::pre_asap::expr_ir::{ColumnRef, ScalarValue};
    use crate::pre_asap::schema::{DataType, Field, Schema};
    use crate::pre_asap::vocabulary::{GroupKeys, JoinKind, Reduction};
    use crate::types::AccuracyTarget;

    fn scan(table: &str, columns: Vec<Field>) -> Rc<OperatorNode> {
        OperatorNode::non_asap_node(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: table.into(),
            },
            predicates: vec![],
            schema: Schema {
                fields: columns,
                time_index: None,
                unique_keys: vec![],
                closed: true,
            },
        })
        .unwrap()
    }

    fn value_col() -> Vec<Field> {
        vec![Field::plain("value", DataType::Float64, false)]
    }

    fn true_pred() -> Predicate {
        Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true)))
    }

    fn count_agg(child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        OperatorNode::non_asap_node(NonASAPOp::Aggregate {
            reduction: Reduction::Reduce(GroupKeys::none()),
            measures: vec![AggIntent::Count {
                accuracy: AccuracyTarget::Exact,
            }],
            output_names: vec![],
            having: None,
            child,
        })
        .unwrap()
    }

    fn join(left: Rc<OperatorNode>, right: Rc<OperatorNode>) -> Rc<OperatorNode> {
        OperatorNode::non_asap_node(NonASAPOp::Join {
            kind: JoinKind::Inner,
            pred: true_pred(),
            left,
            right,
        })
        .unwrap()
    }

    /// A KLL `SummaryAgg` over `leaf`'s `v` column, read out as a quantile.
    fn quantile_readout(
        leaf: Rc<OperatorNode>,
        guarantee: Option<ResultGuarantee>,
    ) -> (Rc<OperatorNode>, Rc<OperatorNode>) {
        let family = FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 40 }),
            GroupingStrategy::default(),
        );
        let agg = OperatorNode::asap_node(
            ASAPOp::SummaryAgg {
                child: leaf,
                family: family.clone(),
                input: SummaryUpdate::column(ColumnRef::Named("v".into())),
                reduction: Reduction::by(vec![]),
                grouping: GroupingStrategy::default(),
            },
            Schema::lifted(vec![Field::new("state", family, false)], None),
            None,
        );
        let readout = OperatorNode::asap_node(
            ASAPOp::SummaryEstimate {
                summary_input: Rc::clone(&agg),
                query: SketchQuery::Quantile { q: 0.99 },
            },
            Schema::lifted(
                vec![Field::plain("quantile", DataType::Float64, false)],
                None,
            ),
            guarantee,
        );
        (agg, readout)
    }

    #[test]
    fn leaf_scan_is_a_single_node() {
        let graph = export(&scan("metrics", value_col()));
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.root, 0);
        assert_eq!(graph.nodes[0].kind, "Scan");
        assert_eq!(graph.nodes[0].label, "Scan(metrics)");
        assert!(graph.nodes[0].children.is_empty());
        assert!(graph.nodes[0].source_node.is_some());
    }

    /// `export` itself never populates higher-layer annotations. Empty
    /// annotations must not appear in serialized JSON, so ordinary exports
    /// retain their existing shape.
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
        assert!(
            !json.contains("source_node"),
            "`source_node` is in-process only: {json}"
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
        let shared_scan = scan("metrics", value_col());
        let left_branch = OperatorNode::non_asap_node(NonASAPOp::Dedup {
            cols: vec![0],
            child: Rc::clone(&shared_scan),
        })
        .unwrap();
        let right_branch = OperatorNode::non_asap_node(NonASAPOp::Limit {
            n: Some(5),
            offset: 0,
            partition_by: GroupKeys::none(),
            child: Rc::clone(&shared_scan),
        })
        .unwrap();
        let root = OperatorNode::non_asap_node(NonASAPOp::Concat {
            children: vec![left_branch, right_branch],
            discriminator_unique_key: None,
        })
        .unwrap();
        let graph = export_post_asap(&root, &mut |_| None);

        assert_eq!(graph.nodes.len(), 4, "Scan, Dedup, Limit, Concat");
        assert_eq!(
            graph.nodes.iter().filter(|n| n.kind == "Scan").count(),
            1,
            "the shared Scan must be merged onto one node, not duplicated"
        );
        assert!(graph.edge_annotations.is_empty());
    }

    /// A single parent referencing the same shared child from two of its
    /// own operand slots at once (a `Join` whose left and right sides are
    /// the exact same `Rc`) is *one* downstream consumer, not two — this
    /// must not produce an edge-cost annotation without explicit physical
    /// evidence.
    #[test]
    fn a_single_parent_referencing_a_shared_child_twice_is_one_consumer_not_two() {
        let shared_scan = scan("metrics", value_col());
        let root = join(Rc::clone(&shared_scan), Rc::clone(&shared_scan));
        let graph = export_post_asap(&root, &mut |_| None);

        assert_eq!(
            graph.nodes.iter().filter(|n| n.kind == "Scan").count(),
            1,
            "the shared Scan must be merged onto one node, not duplicated"
        );
        assert_eq!(graph.nodes[graph.root as usize].children, vec![0, 0]);
        assert!(
            graph.edge_annotations.is_empty(),
            "a single parent referencing the same child twice is one consumer, not a genuine \
             multi-consumer share — got: {:?}",
            graph.edge_annotations
        );
    }

    #[test]
    fn export_merges_pointer_shared_nodes_but_not_equal_copies() {
        // Plain `export` deduplicates by `Rc` pointer identity: the same
        // `Rc` reached twice is one node ...
        let shared_scan = scan("metrics", value_col());
        let graph = export(&join(Rc::clone(&shared_scan), Rc::clone(&shared_scan)));
        assert_eq!(graph.nodes.iter().filter(|n| n.kind == "Scan").count(), 1);
        assert!(graph.edge_annotations.is_empty());

        // ... while two structurally equal but distinct `Rc`s stay two
        // nodes (with equal hashes — that is CSE's job, not the export's).
        let graph = export(&join(
            scan("metrics", value_col()),
            scan("metrics", value_col()),
        ));
        let scans: Vec<_> = graph.nodes.iter().filter(|n| n.kind == "Scan").collect();
        assert_eq!(scans.len(), 2);
        assert_eq!(scans[0].hash, scans[1].hash);
    }

    #[test]
    fn chain_preserves_shape_and_child_links() {
        let root = OperatorNode::non_asap_node(NonASAPOp::Filter {
            pred: true_pred(),
            child: count_agg(scan("metrics", value_col())),
        })
        .unwrap();
        let graph = export(&root);
        assert_eq!(graph.nodes.len(), 3, "Filter -> Aggregate -> Scan");

        let filter = &graph.nodes[graph.root as usize];
        assert_eq!(filter.kind, "Filter");
        assert_eq!(filter.children.len(), 1);

        let agg = &graph.nodes[filter.children[0] as usize];
        assert_eq!(agg.kind, "Aggregate");
        assert_eq!(agg.label, "Aggregate(1 measures)");
        assert_eq!(agg.children.len(), 1);

        let leaf = &graph.nodes[agg.children[0] as usize];
        assert_eq!(leaf.kind, "Scan");
        assert!(leaf.children.is_empty());
    }

    #[test]
    fn merge_keeps_every_branch_as_a_child() {
        let root = OperatorNode::non_asap_node(NonASAPOp::Concat {
            children: vec![
                scan("a", value_col()),
                scan("b", value_col()),
                scan("c", value_col()),
            ],
            discriminator_unique_key: None,
        })
        .unwrap();
        let graph = export(&root);
        assert_eq!(graph.nodes.len(), 4, "3 branches + the Concat node");
        let merge = &graph.nodes[graph.root as usize];
        assert_eq!(merge.kind, "Concat");
        assert_eq!(merge.children.len(), 3);
    }

    /// An operator node read from a scalar expression is a child of the
    /// owning operator (after its operator inputs), and the expression's
    /// `detail` points at it by id instead of inlining it.
    #[test]
    fn scalar_operator_references_are_children_rendered_as_scalar_refs() {
        let subquery = scan("other", value_col());
        let root = OperatorNode::non_asap_node(NonASAPOp::Filter {
            pred: Predicate(ScalarExpr::Exists {
                subquery: Rc::clone(&subquery),
                negated: false,
            }),
            child: scan("metrics", value_col()),
        })
        .unwrap();
        let graph = export(&root);
        assert_eq!(graph.nodes.len(), 3);
        let filter = &graph.nodes[graph.root as usize];
        assert_eq!(
            filter.children.len(),
            2,
            "operator input, then the scalar reference"
        );
        let input = &graph.nodes[filter.children[0] as usize];
        let referenced = &graph.nodes[filter.children[1] as usize];
        assert_eq!(input.label, "Scan(metrics)");
        assert_eq!(referenced.label, "Scan(other)");
        assert_eq!(
            filter.detail["pred"]["Exists"]["subquery"]["scalar_ref"],
            serde_json::json!(referenced.id)
        );
        assert_eq!(filter.detail["pred"]["Exists"]["negated"], false);
        let json = serde_json::to_string(&filter.detail).unwrap();
        assert!(
            !json.contains("other"),
            "the referenced subtree must not be inlined into detail: {json}"
        );
    }

    #[test]
    fn limit_without_n_is_offset_only() {
        let root = OperatorNode::non_asap_node(NonASAPOp::Limit {
            n: None,
            offset: 3,
            partition_by: GroupKeys::none(),
            child: scan("metrics", value_col()),
        })
        .unwrap();
        let graph = export(&root);
        let limit = &graph.nodes[graph.root as usize];
        assert_eq!(limit.label, "Limit(offset 3)");
        assert_eq!(limit.detail["n"], serde_json::Value::Null);
        assert_eq!(limit.detail["offset"], 3);
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
        let q1 = OperatorNode::non_asap_node(NonASAPOp::Limit {
            n: Some(10),
            offset: 0,
            partition_by: GroupKeys::none(),
            child: scan("metrics", value_col()),
        })
        .unwrap();
        let q2 = OperatorNode::non_asap_node(NonASAPOp::Dedup {
            cols: vec![0],
            child: scan("metrics", value_col()),
        })
        .unwrap();

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
        // literal `u64` `crate::ir::cse::structural_hash` produces for this
        // exact node, because it's the same function call, not a parallel
        // reimplementation that happens to agree.
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
        // just the root — each `DagNode::hash` must equal `structural_hash`
        // applied to the actual node it represents.
        let agg = count_agg(scan("metrics", value_col()));
        let root = OperatorNode::non_asap_node(NonASAPOp::Filter {
            pred: true_pred(),
            child: Rc::clone(&agg),
        })
        .unwrap();

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
    /// included) — and a rejection carries its typed reason. A relational
    /// node below a summary is its own node, in the same graph.
    #[test]
    fn export_carries_guarantee_allocation_and_rejection_reason() {
        let leaf = Rc::new(
            OperatorNode::new(Operator::NonASAP(NonASAPOp::Scan {
                source: Source::Table {
                    table_ref: "t".into(),
                },
                predicates: vec![],
                schema: Schema::lifted(vec![Field::plain("v", DataType::Float64, false)], None),
            }))
            .unwrap()
            .with_guarantee(Some(ResultGuarantee::exact("Scan"))),
        );
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
        let (_, root) = quantile_readout(Rc::clone(&leaf), Some(guarantee));
        let graph = export_summary(&root);
        assert_eq!(
            graph.nodes.iter().map(|n| n.kind).collect::<Vec<_>>(),
            ["scan", "summary_agg", "summary_estimate"]
        );
        assert_eq!(graph.nodes[1].label, "SummaryAgg(Sketch(Kll))");
        assert!(graph.nodes[2].label.starts_with("SummaryEstimate(Quantile"));
        assert!(graph.nodes.iter().all(|n| n.schema.is_some()));
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
        assert!(state.get("guarantee").is_none());
        assert_eq!(json["nodes"][0]["guarantee"]["bound"]["op"], "zero");

        // The `DagNode` shape carries the same guarantee inside `detail`.
        let dag = export(&root);
        assert_eq!(
            dag.nodes.iter().map(|n| n.kind).collect::<Vec<_>>(),
            ["Scan", "SummaryAgg", "SummaryEstimate"]
        );
        assert_eq!(dag.nodes[2].detail["guarantee"]["metric"], "rank");
        assert!(dag.nodes[1].detail.get("guarantee").is_none());

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

    /// `export_post_asap` splices a winning summary in place of its target,
    /// tags every node the splice introduced with the decision, and leaves
    /// the rest of the query — including an input the summary reuses that
    /// was already exported — untagged and shared.
    #[test]
    fn export_post_asap_splices_a_summary_substitution_in_place() {
        let leaf = scan("t", vec![Field::plain("v", DataType::Float64, false)]);
        let target = count_agg(Rc::clone(&leaf));
        // `leaf` is exported through the Join's left side before the target
        // (its right side) is reached and substituted.
        let root = join(Rc::clone(&leaf), Rc::clone(&target));
        let (_, readout) = quantile_readout(Rc::clone(&leaf), None);
        let decision = DagDecision {
            id: 7,
            strategy: "Sketch".into(),
            rationale: "quantile via KLL".into(),
            rank: 0,
            cost: 1.0,
            role: "",
            baseline_cost: None,
            selected_cost: None,
            benefit: None,
        };
        let mut calls = Vec::new();
        let graph = export_post_asap(&root, &mut |node| {
            calls.push(node.operator.kind_name());
            Rc::ptr_eq(node, &target).then(|| PostAsapSubstitution::Summary {
                replacement: Rc::clone(&readout),
                decision: decision.clone(),
            })
        });

        let kinds: Vec<_> = graph.nodes.iter().map(|n| n.kind).collect();
        assert_eq!(kinds, ["Scan", "SummaryAgg", "SummaryEstimate", "Join"]);
        assert!(!kinds.contains(&"Aggregate"), "the target itself is gone");
        let join_node = &graph.nodes[graph.root as usize];
        assert_eq!(join_node.children, vec![0, 2]);
        assert!(join_node.decision.is_none());
        let estimate = &graph.nodes[2];
        assert_eq!(estimate.kind, "SummaryEstimate");
        assert_eq!(
            estimate.decision.as_ref().map(|d| (d.id, d.role)),
            Some((7, "replacement_root"))
        );
        let agg = &graph.nodes[estimate.children[0] as usize];
        assert_eq!(
            agg.decision.as_ref().map(|d| (d.id, d.role)),
            Some((7, "replacement_region"))
        );
        let scan_node = &graph.nodes[agg.children[0] as usize];
        assert_eq!(
            scan_node.id, 0,
            "the summary reuses the already-exported input"
        );
        assert!(
            scan_node.decision.is_none(),
            "a node exported before the splice is not tagged by it"
        );
        assert_eq!(
            calls,
            ["Join", "Scan", "Aggregate", "SummaryAgg"],
            "the substitution's own top level (SummaryEstimate) is never re-queried; its \
             descendants are, except the input already exported"
        );
    }

    /// A `SharedSubtreeStrategy`-shaped substitution returns the target
    /// itself as its replacement; the walk must still terminate and render
    /// the target once.
    #[test]
    fn export_post_asap_terminates_when_the_replacement_is_the_target() {
        let target = count_agg(scan("t", value_col()));
        let decision = DagDecision {
            id: 1,
            strategy: "SharedSubtree".into(),
            rationale: "share".into(),
            rank: 0,
            cost: f64::NAN,
            role: "",
            baseline_cost: None,
            selected_cost: None,
            benefit: None,
        };
        let graph = export_post_asap(&target, &mut |node| {
            Rc::ptr_eq(node, &target).then(|| PostAsapSubstitution::Rewrite {
                replacement: Rc::clone(&target),
                decision: decision.clone(),
            })
        });
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.nodes[graph.root as usize].kind, "Aggregate");
        assert_eq!(
            graph.nodes[graph.root as usize]
                .decision
                .as_ref()
                .map(|d| d.role),
            Some("replacement_root")
        );
    }

    /// The snake_case `kind` table is exactly `kind_name` re-cased, for
    /// every variant: a `SummaryDagNode` and a `DagNode` for the same node
    /// never disagree on what it is.
    #[test]
    fn summary_kind_is_the_operator_kind_name_in_snake_case() {
        fn to_snake(name: &str) -> String {
            let mut out = String::new();
            let chars: Vec<char> = name.chars().collect();
            for (i, &c) in chars.iter().enumerate() {
                if c.is_ascii_uppercase() {
                    let prev_lower = i > 0 && !chars[i - 1].is_ascii_uppercase();
                    let next_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
                    if i > 0 && (prev_lower || next_lower) {
                        out.push('_');
                    }
                    out.push(c.to_ascii_lowercase());
                } else {
                    out.push(c);
                }
            }
            out
        }
        let leaf = scan("t", vec![Field::plain("v", DataType::Float64, false)]);
        let (_, readout) = quantile_readout(Rc::clone(&leaf), None);
        let finalize = OperatorNode::asap_node(
            ASAPOp::FinalizeExactAccumulator { child: readout },
            Schema::lifted(vec![], None),
            None,
        );
        let root = OperatorNode::non_asap_node(NonASAPOp::SQLWindowFunc {
            func: crate::pre_asap::vocabulary::WindowFuncKind::RowNumber,
            args: vec![],
            partition_by: GroupKeys::none(),
            order_by: vec![],
            frame: None,
            output_name: "rn".into(),
            child: finalize,
        })
        .unwrap();
        let dag = export(&root);
        let summary = export_summary(&root);
        assert_eq!(dag.nodes.len(), summary.nodes.len());
        for (a, b) in dag.nodes.iter().zip(&summary.nodes) {
            assert_eq!(a.id, b.id);
            assert_eq!(b.kind, to_snake(a.kind), "{}", a.kind);
            assert_eq!(a.children, b.children);
            assert_eq!(a.label, b.label);
        }
        assert_eq!(
            summary.nodes.iter().map(|n| n.kind).collect::<Vec<_>>(),
            [
                "scan",
                "summary_agg",
                "summary_estimate",
                "finalize_exact_accumulator",
                "sql_window_func",
            ]
        );
    }
}
