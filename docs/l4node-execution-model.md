# L4

L4 is the sketch-bound IR — `L4Node`/`SummaryExpr` in `asap-sketch`,
committing *what* answers each intent (a sketch family + params, or an
exact accumulator) that L3 (`QueryExpr`) deliberately leaves open. It
supports two different operations, previously only one of which had a
home in `ASAPController`.

## Planning-time L4

`asap_plan::bind`: `QueryExpr -> L4Node`. A *decision*, made once per query
shape, symbolically — which sketch family and params should answer this
intent, with no reference to what's actually stored anywhere.
`docs/design.md`'s "Sketch binding is already committed by L4" describes
this half at the architecture level; this section has the IR itself.

### The IR

```rust
pub struct L4Node {
    pub expr: SummaryExpr,
    pub schema: L4Schema,
}

pub enum SummaryExpr {
    /// No L4 rule rewrote this L3 node (e.g. `Filter`, `Project`, `Sort`).
    Logical(Box<QueryExpr>),

    /// Sketch aggregation — `sketch`/`params` chosen from the catalog for
    /// the `AggIntent` under an `AccuracyTarget`.
    SummaryAgg {
        child: Rc<L4Node>,
        sketch: SummaryKind,
        params: SummaryParams,
        col: ColumnRef,        // the column being summarised
        reduction: Reduction,  // GROUP BY keys, or "no grouping concept" (#163)
    },

    /// Sketch-aware join (KMV/theta for cardinality, join-sample for
    /// sampling). Emitted only when a `Bind*OnJoin` rule fires.
    SummaryJoin { outer: Rc<L4Node>, inner: Rc<L4Node>, key: ColumnRef, sketch: SummaryKind, params: SummaryParams },

    /// Subtract one sketch from another — requires catalog `subtractable`.
    SummarySubtract { left: Rc<L4Node>, right: Rc<L4Node> },

    /// Delete a key from a sketch — requires catalog `deletable`.
    SummaryDelete { sketch_input: Rc<L4Node>, key: ColumnRef },

    /// Read a value out of a built sketch — inverse of `SummaryAgg`. The
    /// `Sketch(...)` schema type does not propagate past an estimate.
    SummaryEstimate { sketch_input: Rc<L4Node>, query: SketchQuery },

    /// Union of sketches — requires catalog `mergeable`; all children must
    /// agree on `(sketch, params)` (see "Serving-time L4" below for how
    /// this same requirement re-appears, and why, at query time).
    SummaryMerge { children: Vec<Rc<L4Node>> },
}
```

`L4Schema` extends L3's schema with one new field type,
`L4DataType::Sketch(SummaryKind, SummaryParams)` — a schema whose
value-bearing field carries this dtype is a "sketch-state schema." The
`(kind, params)` pair is the type identity: two sketch-state fields are
compatible only if both match exactly, which is what lets `SummarySubtract`/
`SummaryDelete`/`SummaryMerge` reject a family/param mismatch as a
plan-time type error rather than a runtime one. Per-node schema rule
summary: `SummaryAgg` outputs its grouping columns verbatim plus one
`Sketch(sketch, params)` field; `SummaryEstimate` outputs a plain
row-shaped schema (the sketch field doesn't survive it);
`SummarySubtract`/`SummaryMerge` require
every input's `Sketch(s, p)` to already agree, and the catalog capability
flag (`subtractable`/`deletable`/`mergeable`) gates whether the node can
fire at all.

Traversing from the root yields a DAG, not just a tree — shared
sub-expressions appear as multiple `Rc` references to the same `L4Node`
(fan-in is otherwise expressed via L3's own `QueryExpr::LetBinding`/`Ref`).
`asap_plan::bind` only fires on the `Aggregate` spine; anything under an
unrewritten logical parent stays `Logical(...)` unbound — "rewriting
through logical parents" is tracked separately (#6/#33), not attempted
here.

## Serving-time L4

`L4Node -> Value`. A *lookup*, done on every query: resolve each leaf of
an already-decided tree against whatever is actually materialized right
now. Reality can diverge from the plan in ways planning time never sees —
missing data, multiple instances needing a merge, instances that disagree
on params — which is why this half needs its own error cases
(`NoCandidates`, `MergeKindParamsMismatch`), distinct from anything
`asap-plan`'s binder raises. Until now nothing in `ASAPController` defined
this half; every deployment answering queries would otherwise reinvent
the same recursive walk and merge rules independently.
`crates/sketch/src/exec.rs` (`SummaryExecutor` trait + `execute`
function) is the shared answer — the rest of this doc is its design; see
the module docs for the authoritative detail.

### Split of responsibility

`asap-sketch` owns the *structural* rules — which nestings are valid, that
a `SummaryMerge`'s children must agree on `(SummaryKind, SummaryParams)`.
The deployment owns storage, sketch math, and readout — `asap-sketch` has
no KLL/CMS/HLL/DDSketch implementation of its own, so `SummaryExecutor`'s
`State`/`merge_states`/`readout` are entirely deployment-supplied.

```rust
pub trait SummaryExecutor {
    type Handle: Clone;   // e.g. a data plane's sid
    type State;           // decoded sketch/accumulator bytes
    type Value;            // a query answer
    type Error;
    type GroupKey: Clone + Ord + Default;   // e.g. a label-value map

    fn find_candidates(&self, sketch: &SummaryKind, params: &SummaryParams,
        col: &ColumnRef, reduction: &Reduction, child: &L4Node)
        -> Result<Vec<(Self::GroupKey, Self::Handle)>, Self::Error>;
    fn fetch_state(&self, handle: &Self::Handle) -> Result<Self::State, Self::Error>;
    fn merge_states(&self, states: Vec<Self::State>) -> Result<Self::State, Self::Error>;
    fn readout(&self, state: &Self::State, query: &SketchQuery) -> Result<Self::Value, Self::Error>;
    fn logical(&self, expr: &QueryExpr) -> Result<Self::Value, Self::Error>;
}

pub fn execute<E: SummaryExecutor>(node: &L4Node, exec: &E) -> Result<ExecOutcome<E>, ExecError<E::Error>>;
```

`find_candidates` receives the `SummaryAgg`'s whole child subtree (not just
a metric name) because that's the only way to name what to look up —
`SummaryAgg` itself carries no metric/source field; that identity lives
further down, typically inside a `Logical(Window{Scan{..}})` leaf. Walking
down to find it is deployment-specific `QueryExpr`-shape knowledge
(mirrors how a real store's edge-fact extraction already works); `execute`
doesn't interpret it.

**Grouping (`reduction`).** `SummaryAgg` carries `reduction: Reduction` —
the same L3 type (`asap_ir::intent_algebra::Reduction`) the `Aggregate`
node it was bound from carried (issue #165), reused verbatim rather than
flattened to a bare `Vec<ColumnId>`. `execute` has no schema-level
knowledge of what a "group value" looks like for a given deployment —
that's `find_candidates`'s job: it tags every handle it returns with the
`GroupKey` (an opaque, deployment-chosen type) that handle belongs to.
`execute` groups those tagged handles itself before folding
(`fold_states`/`merge_states` only ever combine same-group states), and
every `ExecOutcome` — `State` or `Value` — is a per-group list, never a
single bare value. A `Logical` leaf (which has no grouping concept
either, but for a different reason — it's deferred entirely to the
deployment) is simply a list of one entry under `GroupKey::default()`,
not a different shape — callers don't need to special-case it.

`Reduction` is two variants, not a bare `Vec<ColumnId>` (issue #163):
`Reduce(by)` is a genuine cross-series reduction — an *empty* `by` still
means exactly one group, so `find_candidates` must tag every handle it
returns with one shared `GroupKey` there. `PerEntity` means there is no
grouping concept for this shape at all (a bare per-series range function
like `quantile_over_time(...)`, with no aggregation operator and so no
`by(...)` to begin with) — `find_candidates` must tag every handle with
its own distinct `GroupKey` there, one per entity, and `execute` must
never merge across them. Both used to collapse to the same `by: []` on
`SummaryAgg`, with nothing in the trait able to tell them apart;
`asap-plan::bind` decides which `Reduction` a node gets once, at L3→L4
binding, and carries it onto the L4 node unchanged — the same value that
already decided the node's per-entity-vs-cross-series *schema* — so the
two can't drift out of sync.

`SummaryMerge`'s `(SummaryKind, SummaryParams)` agreement check stays
*global* across every group from every child (it's a planning-time
property of the node, fixed before any group value is known); the actual
state-folding happens *within* each group independently, never across
groups. First landed narrower (a single ungrouped value per tree) in #155;
broadened to a per-`GroupKey` shape in response to #159 once a real
consumer (`data_plane`'s grouped PromQL queries — `sum by (zone)`,
`quantile by (zone)(...)`, the common case, not an edge case) showed the
original shape would have silently merged every group's state together;
`by` was then replaced with `Reduction` in response to #163, once
implementing `find_candidates` for real showed an empty `by` alone
couldn't tell a per-entity shape from an explicit full reduction.

### Nested composition

`L4Node`'s children are `Rc<L4Node>`, so the tree nests to arbitrary depth
by construction — `execute` is plain recursion, no depth special-casing.
Two things aren't obvious from the type alone:

- **Only state-producing nodes can be a `SummaryMerge` child** —
  `SummaryAgg` or another `SummaryMerge`. A `SummaryEstimate` or `Logical`
  child has already collapsed to a value; merging values isn't the
  operation this models. `execute` rejects it (`MergeChildNotState`).
- **Merge-of-merges preserves the `(kind, params)` agreement transitively**
  — `execute` propagates the agreed `(SummaryKind, SummaryParams)` up
  through every level via `ExecOutcome::State`, so nested merges are
  checked the same way a flat one is, without re-deriving anything from
  schema.

`SummaryAgg` nesting itself (`SummaryAgg{child: SummaryAgg{..}}`, e.g.
`quantile(0.9, sum by (job) (m))`) is valid and already binds correctly
today (`asap-plan`'s own `nested_aggregates_bind_per_node` test) — the
child is "the input rows this aggregate is built from," and a two-stage
precompute pipeline (one tier streams per-job sums, another tier builds a
KLL over that stream) is a real shape.

**Deliberately left open**: whether a `SummaryAgg` can validly sit *above*
a `SummaryEstimate` — "build a new summary from another summary's
already-computed readout, at query time." Every materialized-summary store
checked against this design only serves summaries built incrementally at
*ingest* time from raw samples; there's no precedent for building one from
a query-time-derived scalar. A `SummaryExecutor` that hits this shape
should treat it as unsupported rather than assume either answer.

### What's out of scope here

Placement/stage-allocation (L5, `core::physical::executor` in
`docs/design.md`) is unrelated — that's about *which machine* runs a
sub-DAG, decided at plan time. This module is about what "run a sub-DAG
against already-materialized state" means at *query* time, on whichever
machine ends up doing it. `SummaryJoin`/`SummarySubtract`/`SummaryDelete`
have no trait methods yet — no `Bind*` path produces them, so `execute`
just reports them unsupported; add methods when a real consumer needs one
rather than guessing the right shape now.

### Consumer

ASAPQuery-backend's `data_plane` is the first (planned) consumer — see
its own `data_plane/docs/l4node-plan-executor-design.md` for how
`ASAPQueryEngine` implements `SummaryExecutor` (`Handle = sid`,
`find_candidates` via its sid catalog, `merge_states`/`readout` via
`asap_sketchlib`) and the deployment-specific concerns this module doesn't
cover (raw-AST PromQL fallbacks, the `(kind, params)` catalog-drift
question at scale, rollout).

## Supported operators

### `SummaryExpr` operators

| Variant | Planning-time (`asap_plan::bind`) | Serving-time (`exec.rs`) |
|---|---|---|
| `Logical` | produced (the fallback for anything not bound) | supported — delegates to `SummaryExecutor::logical` |
| `SummaryAgg` | produced | supported |
| `SummaryEstimate` | produced | supported |
| `SummaryMerge` | **not produced by anything today** — meant to be inserted by an L5 stage allocator on cut edges (design.md), and no allocator does that yet, in this crate or ASAPQuery-backend's `control_plane` | supported (`execute` fully implements the merge-precondition check and recursion — it just has nothing to run against yet) |
| `SummaryJoin` / `SummarySubtract` / `SummaryDelete` | not produced by any `Bind*` rule | not supported — `execute` returns `ExecError::NotYetSupported` |

### `SummaryKind` (via `boundary::summary_candidates` / `implementation_for`)

All 14 variants are wired into the boundary decision; `DefaultCostModel`
picks the first column below, a custom `CostModel::rank_candidates` can
promote the second:

| `AggIntent` | Default | Also a candidate |
|---|---|---|
| `Quantile` | `Kll` | `DDSketch` |
| `Cardinality` | `Hll` | `Theta`, `Kmv` |
| `Count` (approximate) | `Cms` | `CountSketch` |
| `TopK` | `CmsWithHeap` | `CountSketchWithHeap` |
| `Sum` | `Sum` (exact accumulator) | — |
| `Min` / `Max` | `MinMax` (exact accumulator) | — |
| `Rate` | `Rate` (exact accumulator) | — |
| `Increase` | `Increase` (exact accumulator) | — |
| `Count` (exact) | `Count` (exact accumulator) | — |

`Avg`/`StdDev`/`Variance` and the per-series/range-reducer intents
(`Changes`, `Delta`, `Deriv`, `HistogramQuantile`, …) have no `SummaryKind`
at all — `implementation_for` returns `PassThrough` for them by design
(richer partial state than a single accumulator value, or genuinely
non-mergeable), so they stay `SummaryExpr::Logical`.
