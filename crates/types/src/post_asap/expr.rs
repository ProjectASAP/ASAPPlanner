use std::rc::Rc;

use super::schema::{SummaryFamilyType, SummarySchema};
use super::sketch::{GroupingStrategy, SketchQuery};
use crate::pre_asap::{ColumnRef, QueryExpr, Reduction};

// ── Post-ASAP DAG node ───────────────────────────────────────────────────────

/// A node in the post-ASAP DAG: wraps the expression and its derived output
/// schema so every edge carries a typed schema. `SummarySchema` may contain
/// summary-state-typed columns (`SummaryFamilyType`'s non-`Plain` variants);
/// the pre-ASAP `Schema` cannot.
#[derive(Debug, Clone)]
pub struct SummaryNode {
    pub expr: SummaryExpr,
    /// Output schema of `expr` — the schema of the data flowing on the edge
    /// leading *from* this node to its parent(s).
    pub schema: SummarySchema,
}

// ── Post-ASAP sketch-bound IR ────────────────────────────────────────────────

/// Sketch-bound IR produced by post-ASAP binding rules. Those rules
/// selectively replace logical aggregates and joins in the pre-ASAP
/// `QueryExpr` with their summary-bound counterparts; everything not
/// rewritten passes through as `KeepPreAsap(Box<QueryExpr>)`.
///
/// Traversing from the root node yields a DAG; shared sub-expressions appear
/// as multiple `Rc` references to the same `SummaryNode`.
#[derive(Debug, Clone)]
pub enum SummaryExpr {
    /// A pre-ASAP subtree kept as-is — no binding rule rewrote it into
    /// post-ASAP form (e.g. `Filter`, `Project`, `Sort`). Output schema is
    /// the inner node's schema, lifted to `SummarySchema` with all fields as
    /// `SummaryFamilyType::Plain`.
    KeepPreAsap(Box<QueryExpr>),

    /// Summary aggregation. Post-ASAP binding chose `family` — which
    /// summary family (exact accumulator, sketch, sample, wavelet, or
    /// statistical model) and its `(kind, params)` — from the catalog for
    /// `AggIntent` under `DeploymentConstraints`.
    /// Output schema: grouping columns (verbatim) + one field carrying
    /// partial summary state per group, typed `family`.
    SummaryAgg {
        child: Rc<SummaryNode>,
        /// Which summary family realizes this aggregation, and that
        /// family's own `(kind, params)`. Never `SummaryFamilyType::Plain`
        /// — this node always produces summary state, not a plain value.
        family: SummaryFamilyType,
        /// The column being summarised (fed into the summary).
        col: ColumnRef,
        /// How this aggregation's output rows relate to `child`'s — the
        /// same [`Reduction`] the pre-ASAP `Aggregate` node it was bound
        /// from carried (issue #165), reused verbatim rather than
        /// flattened to a bare `Vec<ColumnId>`. `Reduction::Reduce(by)`
        /// with an empty `by` is a genuine full reduction (merge every
        /// candidate into one group); `Reduction::PerEntity` has no
        /// grouping concept at all (never merge across entities) — the
        /// two collapsed to the same ambiguous `by: []` before this field
        /// existed (issue #163).
        reduction: Reduction,
        /// How this aggregation's summary state is physically instantiated
        /// across `reduction`'s subpopulations — one independent instance
        /// per `by` key (today's only behavior, and this field's default),
        /// or one shared Hydra-family structure serving all of them (issue
        /// #256). Lives here, next to `reduction`, rather than on `family`
        /// itself: this axis is orthogonal to *which* family/kind answers
        /// the intent, and `reduction` is exactly the field that already
        /// carries the `by` keys this axis's legality depends on (a
        /// `SharedMultiSubpopulation` choice only makes sense when
        /// `reduction` actually has a subpopulation concept — see
        /// `asap_aware_mapping::grouping`'s module docs for the legality
        /// rules). Every existing producer of a `SummaryAgg` sets this to
        /// `GroupingStrategy::PerSubpopulationInstance` (its `Default`),
        /// so no existing behavior changes.
        grouping: GroupingStrategy,
    },

    /// Summary-aware join (KMV / theta for join-cardinality; join-sample for
    /// sampling). Emitted only when a `Bind*OnJoin` rule fires.
    /// Output schema: one field typed `family`, read by a downstream
    /// `SummaryEstimate`.
    SummaryJoin {
        outer: Rc<SummaryNode>,
        inner: Rc<SummaryNode>,
        key: ColumnRef,
        /// Never `SummaryFamilyType::Plain` — see [`SummaryAgg::family`](SummaryExpr::SummaryAgg).
        family: SummaryFamilyType,
    },

    /// Subtract one summary from another. Valid only for families with a
    /// linear-inverse property (CMS, theta, count-based). Catalog flag
    /// `subtractable` must be true for the family.
    /// Output schema: one field (same family + params as inputs).
    SummarySubtract {
        left: Rc<SummaryNode>,
        right: Rc<SummaryNode>,
    },

    /// Delete a key from a summary (CMS update with −1, deletable Bloom
    /// filter). Catalog flag `deletable` must be true. Output schema =
    /// input schema unchanged in type (same field type as input).
    SummaryDelete {
        summary_input: Rc<SummaryNode>,
        key: ColumnRef,
    },

    /// Read out a query result from a built summary. The summary-state field
    /// type does *not* propagate downstream of an estimate — the output
    /// schema is a regular row-shaped schema (Float64 for quantile, Int64
    /// for count/cardinality, `[(key, count)]` for top-k).
    SummaryEstimate {
        summary_input: Rc<SummaryNode>,
        query: SketchQuery,
    },

    /// ⊕ — union of summaries across stages / shards. Distinct from the
    /// pre-ASAP `Concat` because summary union has type constraints: all
    /// inputs must agree on `family` (kind + params) and the catalog flag
    /// `mergeable` must be true. Inserted by a deployment's own stage
    /// allocator (not modeled in this crate) on cut edges.
    /// Output schema: one field (same family + params as inputs).
    SummaryMerge { children: Vec<Rc<SummaryNode>> },
}
