use std::rc::Rc;

use super::schema::SummarySchema;
use super::sketch::{SketchQuery, SummaryKind, SummaryParams};
use crate::pre_asap::{ColumnRef, QueryExpr, Reduction};

// ── Post-ASAP DAG node ───────────────────────────────────────────────────────

/// A node in the post-ASAP DAG: wraps the expression and its derived output
/// schema so every edge carries a typed schema. `SummarySchema` may contain
/// `SummaryDataType::Sketch` columns; the pre-ASAP `Schema` cannot.
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
/// `QueryExpr` with their sketch-bound counterparts; everything not
/// rewritten passes through as `Logical(Box<QueryExpr>)`.
///
/// Traversing from the root node yields a DAG; shared sub-expressions appear
/// as multiple `Rc` references to the same `SummaryNode`.
#[derive(Debug, Clone)]
pub enum SummaryExpr {
    /// Any pre-ASAP node no binding rule rewrote (e.g. `Filter`, `Project`,
    /// `Sort`). Output schema is the inner node's schema, lifted to
    /// `SummarySchema` with all fields as `SummaryDataType::Primitive`.
    Logical(Box<QueryExpr>),

    /// Summary aggregation. Post-ASAP binding chose `summary` + `params`
    /// from the catalog for `AggIntent` under `DeploymentConstraints`.
    /// Output schema: grouping columns (verbatim) + one `Sketch(summary,
    /// params)` field carrying partial summary state per group.
    SummaryAgg {
        child: Rc<SummaryNode>,
        summary: SummaryKind,
        params: SummaryParams,
        /// The column being summarised (fed into the sketch).
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
    },

    /// Sketch-aware join (KMV / theta for join-cardinality; join-sample for
    /// sampling). Emitted only when a `Bind*OnJoin` rule fires.
    /// Output schema: one `Sketch(sketch, params)` field read by a downstream
    /// `SummaryEstimate`.
    SummaryJoin {
        outer: Rc<SummaryNode>,
        inner: Rc<SummaryNode>,
        key: ColumnRef,
        summary: SummaryKind,
        params: SummaryParams,
    },

    /// Subtract one sketch from another. Valid only for sketches with a
    /// linear-inverse property (CMS, theta, count-based). Catalog flag
    /// `subtractable` must be true for the sketch family.
    /// Output schema: one `Sketch(s, p)` field (same family + params as inputs).
    SummarySubtract {
        left: Rc<SummaryNode>,
        right: Rc<SummaryNode>,
    },

    /// Delete a key from a sketch (CMS update with −1, deletable Bloom
    /// filter). Catalog flag `deletable` must be true. Output schema =
    /// input schema unchanged in type (same `Sketch(s, p)` field).
    SummaryDelete {
        summary_input: Rc<SummaryNode>,
        key: ColumnRef,
    },

    /// Read out a query result from a built summary. The `Sketch(…)` field
    /// type does *not* propagate downstream of an estimate — the output
    /// schema is a regular row-shaped schema (Float64 for quantile, Int64
    /// for count/cardinality, `[(key, count)]` for top-k).
    SummaryEstimate {
        summary_input: Rc<SummaryNode>,
        query: SketchQuery,
    },

    /// ⊕ — union of sketches across stages / shards. Distinct from the
    /// pre-ASAP `Merge` because sketch union has type constraints: all
    /// inputs must agree on `(sketch, params)` and the catalog flag
    /// `mergeable` must be true. Inserted by a deployment's own stage
    /// allocator (not modeled in this crate) on cut edges.
    /// Output schema: one `Sketch(s, p)` field (same family + params as inputs).
    SummaryMerge { children: Vec<Rc<SummaryNode>> },
}
