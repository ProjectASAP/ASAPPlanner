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
this half, and it's the only half that existed before this doc.

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

    fn find_candidates(&self, sketch: &SummaryKind, params: &SummaryParams,
        col: &ColumnRef, by: &[ColumnId], child: &L4Node) -> Result<Vec<Self::Handle>, Self::Error>;
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
