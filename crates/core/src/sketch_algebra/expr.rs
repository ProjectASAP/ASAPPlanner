use std::rc::Rc;

use super::schema::L4Schema;
use super::sketch::{SketchQuery, SummaryKind, SummaryParams};
use crate::intent_algebra::{ColumnId, ColumnRef, QueryExpr};

// ── L4 DAG node ───────────────────────────────────────────────────────────────

/// A node in the L4 DAG. Mirrors `L3Node`: wraps the expression and its
/// derived output schema so every edge carries a typed schema.
/// `L4Schema` may contain `L4DataType::Sketch` columns; `L3Schema` cannot.
#[derive(Debug, Clone)]
pub struct L4Node {
    pub expr: SummaryExpr,
    /// Output schema of `expr` — the schema of the data flowing on the edge
    /// leading *from* this node to its parent(s).
    pub schema: L4Schema,
}

// ── L4 sketch-bound IR ────────────────────────────────────────────────────────

/// Sketch-bound IR produced by L4 optimizer rules. L4 rules selectively
/// replace logical aggregates and joins in the L3 `QueryExpr` with their
/// sketch-bound counterparts; everything not rewritten passes through as
/// `Logical(Box<QueryExpr>)`.
///
/// Traversing from the root node yields a DAG; shared sub-expressions appear
/// as multiple `Rc` references to the same `L4Node` (L3 fan-in is expressed
/// via `QueryExpr`'s own `LetBinding`/`Ref`).
#[derive(Debug, Clone)]
pub enum SummaryExpr {
    /// Any L3 node that no L4 rule rewrote (e.g. `Filter`, `Project`, `Sort`).
    /// Output schema is the inner L3 node's schema, lifted to `L4Schema`
    /// with all fields as `L4DataType::Primitive`.
    Logical(Box<QueryExpr>),

    /// Sketch aggregation. L4 chose `sketch` + `params` from the catalog
    /// for `AggIntent` under `DeploymentConstraints`.
    /// Output schema: `by` columns (verbatim) + one `Sketch(sketch, params)`
    /// field carrying partial sketch state per group.
    SummaryAgg {
        child: Rc<L4Node>,
        sketch: SummaryKind,
        params: SummaryParams,
        /// The column being summarised (fed into the sketch).
        col: ColumnRef,
        /// GROUP BY keys (positional) carried through to the output schema.
        by: Vec<ColumnId>,
    },

    /// Sketch-aware join (KMV / theta for join-cardinality; join-sample for
    /// sampling). Emitted only when a `Bind*OnJoin` rule fires.
    /// Output schema: one `Sketch(sketch, params)` field read by a downstream
    /// `SummaryEstimate`.
    SummaryJoin {
        outer: Rc<L4Node>,
        inner: Rc<L4Node>,
        key: ColumnRef,
        sketch: SummaryKind,
        params: SummaryParams,
    },

    /// Subtract one sketch from another. Valid only for sketches with a
    /// linear-inverse property (CMS, theta, count-based). Catalog flag
    /// `subtractable` must be true for the sketch family.
    /// Output schema: one `Sketch(s, p)` field (same family + params as inputs).
    SummarySubtract { left: Rc<L4Node>, right: Rc<L4Node> },

    /// Delete a key from a sketch (CMS update with −1, deletable Bloom
    /// filter). Catalog flag `deletable` must be true. Output schema =
    /// input schema unchanged in type (same `Sketch(s, p)` field).
    SummaryDelete {
        sketch_input: Rc<L4Node>,
        key: ColumnRef,
    },

    /// Read out a query result from a built sketch. The `Sketch(…)` field
    /// type does *not* propagate downstream of an estimate — the output
    /// schema is a regular row-shaped schema (Float64 for quantile, Int64
    /// for count/cardinality, `[(key, count)]` for top-k).
    SummaryEstimate {
        sketch_input: Rc<L4Node>,
        query: SketchQuery,
    },

    /// ⊕ — union of sketches across stages / shards. Distinct from L3
    /// `Merge` because sketch union has type constraints: all inputs must
    /// agree on `(sketch, params)` and the catalog flag `mergeable` must
    /// be true. Inserted by the L5 stage allocator on cut edges.
    /// Output schema: one `Sketch(s, p)` field (same family + params as inputs).
    SummaryMerge { children: Vec<Rc<L4Node>> },
}
