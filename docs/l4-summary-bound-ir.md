# L4 — summary-bound IR

The point where a plan commits to *what* answers each intent that L3's
canonical form deliberately leaves open. "Summary" is the umbrella term
for whatever answers an intent without re-scanning raw samples —
concretely, either an approximate sketch (e.g. a quantile sketch, a
cardinality sketch, a frequency/heavy-hitter sketch) or an exact
accumulator (e.g. sum, count, min/max, rate, increase). New primitive
classes are meant to slot in as additional summary kinds, not as a new
IR or a new translation layer.

This layer has two distinct halves, deliberately kept separate:
**planning-time** binding (deciding, symbolically, which summary
answers which intent) and **serving-time** execution (actually
resolving that decision against whatever is materialized right now).
They're covered in turn below.

## Planning-time: binding

A decision, made once per query shape, symbolically: given an intent
and its accuracy requirement, which summary family and parameters
answer it — with no reference to what's actually stored anywhere yet.

### The shape of a summary-bound plan

A summary-bound plan mirrors the canonical intent tree but replaces
each summarizable node with a decision:

- **Unbound / logical** — nothing rewrote this node (e.g. a filter or a
  projection); it stays as ordinary logical computation over its
  child's output.
- **Summary aggregation** — a summary family and its parameters, chosen
  for the intent and its accuracy target, over a designated column and
  grouping keys.
- **Summary-aware join** — a join realized via a summary technique
  (e.g. a cardinality-aware join sketch), rather than a full
  materialized join.
- **Summary subtraction / deletion** — algebraic operations on an
  already-built summary, only valid for summary kinds that support
  them.
- **Summary readout** — reading a value out of an already-built
  summary; the inverse of building one.
- **Summary merge** — combining multiple built summaries into one; only
  valid when every input agrees on family and parameters.

A summary-bound plan is a DAG, not just a tree — a shared
sub-computation can appear as more than one reference to the same bound
node, mirroring the sharing already present in the canonical form (see
[`l3-intent-algebra.md`](./l3-intent-algebra.md)). Binding only fires
where an intent is recognizable in the tree; anything underneath an
unrewritten (logical) parent stays logical too — extending binding to
rewrite through a logical parent is a known open design question, not
attempted by the base design.

Each bound node's output schema extends the canonical schema with one
new possibility: a field whose type *is* a built summary (identified by
its family and parameters). Two summary-typed fields are only
compatible if their family and parameters match exactly — which is what
lets subtraction, deletion, and merge reject a mismatched combination
as a planning-time error rather than a runtime one.

## Serving-time: execution

A lookup, done on every query: resolve each leaf of an already-bound
plan against whatever summaries are actually materialized right now.
Reality can diverge from what planning assumed — data might be missing,
several instances of the same summary might need merging, instances
might disagree on parameters — so serving needs its own error cases,
distinct from anything planning-time binding can raise.

### Split of responsibility

The *structural* rules are shared and deployment-independent: which
nestings of a summary-bound plan are valid, and what must agree for a
merge to be legal. Storage, summary math, and readout are entirely
deployment-supplied — this layer defines the interface a deployment
implements, not an implementation of summaries themselves. Concretely,
a deployment supplies: how to find candidate materialized instances for
a bound node, how to fetch one instance's state, how to merge several
instances' states, how to read a value out of a state, and how to
evaluate an unrewritten logical node directly. A shared execution
routine walks the bound tree and calls into these deployment-supplied
operations, so every deployment gets the same walk and merge logic for
free.

Finding candidates for a summary-aggregation node requires seeing that
node's entire input sub-tree, not just a bare name — a summary
aggregation node carries no source identity of its own; that identity
lives further down, inside a scan. Walking down to find it is
deployment-specific knowledge (how a real store names and indexes
summarized data); the shared execution routine doesn't need to
interpret it, only pass the sub-tree along.

### Nested composition

A bound plan nests to arbitrary depth by construction, so execution is
plain recursion with no special-casing for depth. Two composition rules
matter:

- **Only a node that produces summary state (a summary aggregation, or
  another merge) can be a merge's child.** A readout or a logical node
  has already collapsed to a plain value; merging values isn't the
  operation a merge models.
- **A merge of merges preserves the family/parameter agreement
  transitively** — the agreed family and parameters propagate upward
  through every nesting level, so a nested merge is checked the same
  way a flat one is.

Summary-aggregation-over-summary-aggregation is itself valid and
expected — e.g. a two-stage precompute pipeline where one tier streams
a per-group reduction and another tier builds a richer summary over
that stream is a real, intended shape.

**Deliberately left open:** whether a summary aggregation can validly
sit above a *readout* — "build a new summary from another summary's
already-computed value, at query time." Every design considered so far
only builds summaries incrementally from raw samples, at ingest time;
there's no established answer for building one from a
query-time-derived scalar. A deployment that hits this shape should
treat it as unsupported rather than assume either answer.

### What's out of scope here

Placement — *which machine* runs a piece of the plan — is L5's concern
entirely (see [`l5-physical-plan.md`](./l5-physical-plan.md)). This
layer is only about what "answer a query against already-materialized
summaries" means, on whichever machine ends up doing it. Some
operations (summary-aware join, subtraction, deletion) have no defined
execution behavior yet, because no binding rule produces them yet in
the base design — a deployment should add that behavior only once it
has a real need for the shape, rather than speculatively guessing the
right one now.

## Choosing a summary for an intent

Which summary family (if any) answers a given intent is itself a
pluggable decision: a default choice exists per intent, but a
deployment can supply its own ranking to prefer an alternative family
where one exists (e.g. an alternative quantile sketch, an alternative
cardinality sketch). This ranking is the one general-purpose extension
point in this layer — a deployment customizes *which* summary is
chosen, without touching how binding or execution works structurally.

Not every intent has a summary counterpart. An intent whose accurate
computation requires richer partial state than a single summary value
can capture, or whose result is fundamentally non-mergeable across
partitions, has no summary realization by design — it always stays as
ordinary logical computation over raw data.

## Interface

The planning-time IR:

```rust
pub struct L4Node {
    pub expr: SummaryExpr,
    pub schema: L4Schema,
}

pub enum SummaryExpr {
    Logical(Box<QueryExpr>),
    SummaryAgg {
        child: Rc<L4Node>,
        summary: SummaryKind,
        params: SummaryParams,
        col: ColumnRef,
        reduction: Reduction,
    },
    SummaryJoin { outer: Rc<L4Node>, inner: Rc<L4Node>, key: ColumnRef, summary: SummaryKind, params: SummaryParams },
    SummarySubtract { left: Rc<L4Node>, right: Rc<L4Node> },
    SummaryDelete { summary_input: Rc<L4Node>, key: ColumnRef },
    SummaryEstimate { summary_input: Rc<L4Node>, query: SketchQuery },
    SummaryMerge { children: Vec<Rc<L4Node>> },
}
```

The `summary`/`summary_input` field names match this doc's own "summary"
umbrella term (see the top of this doc) directly. An earlier revision of
this type used `sketch`/`sketch_input`, predating that umbrella term —
renamed in #170, landed together with the `Implementation` variant merge
described below.

Per-node validity constraints — each is a summary-catalog fact (see
"Summary catalog" above) checked at planning time, before this shape ever
reaches serving-time execution:

| Node | `summary` field | Constraint |
|---|---|---|
| `Logical` | — | none — wraps an arbitrary unrewritten `QueryExpr` subtree |
| `SummaryAgg` | own | none beyond `col`'s intent already requiring an accuracy target compatible with `summary`; this is the leaf every other row's constraints are checked *against* |
| `SummaryJoin` | own | only emitted by a join-specific `Bind*OnJoin` rule — none exist yet in the base design, so this variant has no live producer |
| `SummarySubtract` | read from `left`/`right` | `left`/`right` agree on `(kind, params)`; that kind's catalog entry sets `subtractable` |
| `SummaryDelete` | read from `summary_input` | that kind's catalog entry sets `deletable` |
| `SummaryEstimate` | read from `summary_input` | `query`'s `SketchQuery` variant is one that kind actually supports (not a single boolean flag — closer to that kind's whole reason for existing) |
| `SummaryMerge` | read from `children`, which must all agree | `children` non-empty; every child produces summary *state*, never a value (see diagram below); that shared kind's catalog entry sets `mergeable` |

The trickiest of these to hold in your head is `SummaryMerge`'s "children
must produce state, not a value" rule — everything here either produces
partial summary **state** (still mergeable, not yet read out) or a final
**value** (already collapsed, e.g. by a readout). Only a state-producing
node may feed a merge:

```mermaid
flowchart LR
    subgraph State["produce summary state"]
        SA["SummaryAgg"]
        SM["SummaryMerge"]
    end
    subgraph Value["produce a plain value"]
        SE["SummaryEstimate"]
        LG["Logical"]
    end
    SA -->|"valid merge child"| SM
    SM -->|"valid merge child (nested)"| SM
    SA --> SE
    SM --> SE
    SE -. "✗ already a value" .-> SM
    LG -. "✗ already a value" .-> SM
```

Binding — turning a canonical `QueryExpr` into an `L4Node` tree:

```rust
pub fn implement_tree(expr: &QueryExpr) -> Result<Rc<L4Node>, ImplementError>;
pub fn implement_tree_with(expr: &QueryExpr, cost_model: &dyn CostModel) -> Result<Rc<L4Node>, ImplementError>;
```

**"Implementation" is the answer to one question: how is this one
intent actually realized?** It's the return type of the per-intent
binding decision (`implementation_for`) — for a given `AggIntent`,
exactly one of: build an approximate sketch (bounded error, sized to
the accuracy target), build an exact accumulator (zero error, still
mergeable — see "Choosing a summary for an intent" above), or don't
summarize at all and evaluate the intent directly over raw data
(`PassThrough`). It's the same three-way choice this doc's "shape of a
summary-bound plan" section already describes in prose; this is that
decision's concrete type:

```rust
pub enum Implementation {
    Summary { kind: SummaryKind, params: SummaryParams },
    PassThrough,
}

pub fn implementation_for(intent: &AggIntent) -> Implementation;
```

An earlier revision of this type split `Summary` into two variants,
`Sketch{kind,params}` and `ExactAccumulator{kind,params}`, carrying
identical shapes — the split existed only because binding needed to
know, right here, whether the chosen `kind` requires a `SummaryEstimate`
readout afterward (approximate) or *is* the answer once built (exact —
no estimate step). #170 collapsed them into the single `Summary`
variant above, recovering that same fact from `SummaryKind::is_exact()`
instead of from the variant tag.

The one pluggable extension point at this layer — a deployment supplies
its own `CostModel` to override which summary family is chosen and how
its parameters are sized, without touching the binding pass itself.
Every method past the first has a sensible default, so a deployment that
only wants to reorder candidates doesn't have to implement the rest:

```rust
pub trait CostModel {
    fn rank_candidates(&self, intent: &AggIntent, candidates: &[SummaryKind]) -> Vec<SummaryKind>;

    fn size_params(&self, kind: SummaryKind, intent: &AggIntent, eps: f64, delta: f64) -> SummaryParams {
        /* default: this crate's built-in sizing formulas */
    }
    fn realize_extension(&self, ext_kind: &str, payload: &serde_json::Value) -> Implementation {
        /* default: PassThrough */
    }
    fn readout_extension(&self, ext_kind: &str, payload: &serde_json::Value, col: &ColumnRef) -> SketchQuery {
        /* default: panics -- only reachable if realize_extension was overridden without this */
    }
}
```

Serving-time — the interface a deployment implements to actually answer
a query against an already-bound `L4Node` tree:

```rust
pub trait SummaryExecutor {
    type Handle: Clone;
    type State;
    type Value;
    type Error;
    type GroupKey: Clone + Ord + Default;

    fn find_candidates(
        &self,
        summary: &SummaryKind,
        params: &SummaryParams,
        col: &ColumnRef,
        reduction: &Reduction,
        child: &L4Node,
    ) -> Result<Vec<(Self::GroupKey, Self::Handle)>, Self::Error>;

    fn fetch_state(&self, handle: &Self::Handle) -> Result<Self::State, Self::Error>;
    fn merge_states(&self, states: Vec<Self::State>) -> Result<Self::State, Self::Error>;
    fn readout(&self, state: &Self::State, query: &SketchQuery) -> Result<Self::Value, Self::Error>;
    fn logical(&self, expr: &QueryExpr) -> Result<Self::Value, Self::Error>;
}

pub fn execute<E: SummaryExecutor>(node: &L4Node, exec: &E) -> Result<ExecOutcome<E>, ExecError<E::Error>>;
```

`find_candidates`'s `reduction` parameter is exactly the `Reduction`
this doc's design section already explains — an implementer must branch
on `Reduce` vs. `PerEntity` there, not guess from an empty key list.

### Design proposal: an accuracy algebra for composed summaries

What does it mean to build a summary over the *output* of another
summary, rather than over raw data? The design below — one guarantee, one
composition theorem, and three small interfaces built on it — answers
that question once, in the abstract. The section after it,
["Which issue this solves"](#which-issue-this-solves), maps each piece
back to [#171](https://github.com/ProjectASAP/ASAPController/issues/171),
[#172](https://github.com/ProjectASAP/ASAPController/issues/172), and
[#173](https://github.com/ProjectASAP/ASAPController/issues/173).

#### The guarantee every summary kind already makes

Every `SummaryKind` implicitly satisfies a guarantee of this shape, for
its build map `β` (raw values → state), readout `ρ` (state → value), and
the true aggregate function `φ` it approximates (`Sum`, `Count`,
`Quantile_q`, `Cardinality`, `TopK_k`, …) over a multiset `X`:

```
Pr[ |ρ(β(X)) − φ(X)| > ε·φ(X) ] ≤ δ                      (G)
```

with `(ε, δ) = (0, 0)` for the exact accumulators (`Sum`/`Count`/`MinMax`/
`Rate`/`Increase`) — precisely what `SummaryKind::is_exact()` records.

This is already two distinct shapes in the current catalog, not one, and
the type has to say which: (G) as written bounds a single output value
against a *norm* on the underlying value vector, and that norm isn't
uniform across kinds — CMS bounds per-item error against the *L1* norm of
the whole frequency vector (`‖X‖₁`); `CountSketch`/`CountSketchWithHeap`
bound it against the *L2* norm of the residual vector instead
(`boundary.rs`'s own sizing comment already flags this as an unresolved
refinement) — a tighter but not directly comparable quantity on skewed
data; KLL/HLL/DDSketch/Theta/Kmv have no underlying vector norm at all, a
plain pointwise bound. And separately, a structured kind like a QTree-style
range tree ([#173](https://github.com/ProjectASAP/ASAPController/issues/173))
doesn't give a probabilistic `(ε, δ)` bound at all — it returns a
*deterministic* interval whose half-width shrinks with tree capacity.
`Accuracy` has to represent both shapes:

```rust
pub enum ErrorNorm {
    /// Per-item error relative to Σ|x| over the value vector — CMS.
    L1,
    /// Per-item error relative to the L2 norm of the (residual) value
    /// vector — Count-Sketch. Tighter than L1 on skewed data, but not
    /// directly convertible to it without an explicit, workload-dependent
    /// constant.
    L2,
    /// A single-value guarantee with no underlying vector norm — KLL rank
    /// error, HLL cardinality error, DDSketch, Theta, Kmv.
    Pointwise,
}

pub enum Accuracy {
    /// (G)'s probabilistic bound, against a specific norm.
    Probabilistic { epsilon: f64, delta: f64, norm: ErrorNorm },
    /// A deterministic interval half-width — e.g. a QTree node's bound —
    /// with no failure probability to speak of.
    Bounded { radius: f64 },
}

impl Accuracy {
    pub const EXACT: Accuracy = Accuracy::Probabilistic { epsilon: 0.0, delta: 0.0, norm: ErrorNorm::Pointwise };
}

impl SummaryKind {
    /// The `Accuracy` this kind, at these params, actually satisfies for
    /// (G) — the inverse of `boundary::default_size_params`'s sizing
    /// formulas (e.g. Kll: ε ≈ 2/k; Hll: ε ≈ 1.04/√(2^p); Cms: ε ≈ e/width,
    /// δ ≈ e^(−depth)). `Accuracy::EXACT` for every kind where
    /// `is_exact()` holds.
    pub fn implied_accuracy(&self, params: &SummaryParams) -> Accuracy { .. }
}
```

This is the same abstraction Algebird's `Approximate[T]` monoid packages —
a confidence interval that composes algebraically *within one chain*. The
open question is what happens at the boundary where a second instance of
(G) is layered on top of the *output* of a first, instead of being built
once over raw `X`.

#### The composition theorem

Let a child summary produce `ṽ = ρ_child(β_child(X))` with `Accuracy A_c`
for `φ_child`. Let an outer summary be built over `ṽ` as if it were exact
input, with `Accuracy A_o` for `φ_outer`, where `φ_outer` is `L`-Lipschitz
with respect to `A_c`'s own representation (its `norm`, if `Probabilistic`;
plain magnitude, if `Bounded`) — call that pairing a `Sensitivity`:

```rust
/// `φ_outer`'s sensitivity, valid only against inputs whose error is
/// already stated in `from`. `None`/mismatched `from` means "no proven
/// bound for this combination" — composition must refuse, not guess.
pub struct Sensitivity {
    pub lipschitz: f64,
    pub from: ErrorNorm,   // ignored when composing two `Bounded` accuracies
}

impl Accuracy {
    pub fn compose(outer: Accuracy, child: Accuracy, sensitivity: Sensitivity) -> Option<Accuracy> {
        match (outer, child) {
            (
                Accuracy::Probabilistic { epsilon: e_o, delta: d_o, norm },
                Accuracy::Probabilistic { epsilon: e_c, delta: d_c, norm: child_norm },
            ) if child_norm == sensitivity.from => Some(Accuracy::Probabilistic {
                epsilon: e_o + sensitivity.lipschitz * e_c,
                delta: d_o + d_c,
                norm,
            }),
            (Accuracy::Bounded { radius: r_o }, Accuracy::Bounded { radius: r_c }) => {
                Some(Accuracy::Bounded { radius: r_o + sensitivity.lipschitz * r_c })
            }
            _ => None,   // norm mismatch, or a Probabilistic/Bounded pairing
        }
    }
}
```

For two `Probabilistic` accuracies, by the triangle inequality on the two
error terms and a union bound on their two failure events:

```
Pr[ |ρ_outer(β_outer(ṽ)) − φ_outer(φ_child(X))| > (ε_o + L·ε_c)·φ_outer(φ_child(X)) ] ≤ δ_o + δ_c     (G∘)
```

For two `Bounded` accuracies the same triangle-inequality shape holds on
the interval radius directly, with no failure probability to union-bound:
`radius_total = radius_outer + L·radius_child`.

**Why `compose` refuses instead of defaulting to `L = 1`.** Linear
aggregates (`Sum`/`Count`/`Cms`-frequency) are `(L = 1, from:
Pointwise-or-L1)`-sensitive — the clean, dimension-free case most
compositions land in. Rank-based aggregates (`Quantile`/`TopK`) are
`(L = 1, from: Pointwise)`-sensitive only under the standard assumption
that a relative perturbation of each element doesn't move its rank by more
than a proportional amount — an assumption, not a proof. Neither
`Sensitivity` is valid against an `ErrorNorm::L2` child: composing over a
`CountSketch`/`CountSketchWithHeap` child needs its own, separately
justified `Sensitivity { from: L2, .. }` — for a linear outer this is a
real, derivable (if `√n`-dependent, by Cauchy–Schwarz) bound; for a
rank-based outer, sensitivity to an L2-bounded noise vector is
data-dependent (it depends on the local density of values near the query
rank, not just the aggregate norm bound), so no fixed `L` exists for it at
all — a deployment enabling that composition is asserting a
workload-specific bound, not one this crate can derive.

#### Interface: letting a summary's input be another summary's output

One new `ColumnRef` variant carries an `Accuracy` alongside the reference,
so every consumer of `col` can see what it's actually built on without
re-deriving it:

```rust
pub enum ColumnRef {
    Named(String),
    Qualified { table: String, name: String },
    SampleValue,
    FromSummary { nodes: Vec<Rc<L4Node>>, accuracy: Accuracy },   // NEW
}
```

`nodes` is plural: a single already-realized child is the common case
(`nodes.len() == 1`), but nothing about `compose` or the guarantee above
is specific to one child — summing `(G∘)`'s per-child term over several
children is the same union bound, just applied more than once (`ε_total =
ε_outer + Σᵢ Lᵢ·εᵢ`, `δ_total = δ_outer + Σᵢ δᵢ`).

Constructing `FromSummary` is legal in exactly two cases:

- `accuracy == Accuracy::EXACT` — always allowed, unconditionally.
- Any non-`EXACT` `accuracy` — allowed only when the deployment's
  `CostModel` opts in:

  ```rust
  pub trait CostModel {
      /// Does this deployment accept building `outer` over input already
      /// carrying `child_kind`'s own approximation error? Default `false`
      /// — refuse rather than silently double-approximate.
      fn accepts_nested_approx(&self, outer: &AggIntent, child_kind: &SummaryKind) -> bool {
          false
      }

      /// Size `kind`'s params so the composed guarantee holds against
      /// `target` (what the query asked for), given the input's own
      /// resolved `Accuracy` when it's a `FromSummary` column. Solves
      /// `target == Accuracy::compose(result, child_accuracy, sensitivity)`
      /// for `result` — e.g. for two `Probabilistic` accuracies,
      /// `result.epsilon = target.epsilon − L·child_accuracy.epsilon`,
      /// `result.delta = target.delta − child_accuracy.delta` — rejecting
      /// if either goes non-positive (no achievable outer accuracy leaves
      /// enough of the target budget for the child's contribution).
      fn size_params(
          &self,
          kind: SummaryKind,
          intent: &AggIntent,
          target: Accuracy,
          child_accuracy: Option<Accuracy>,   // None over raw/exact input
      ) -> SummaryParams { .. }
  }
  ```

  When `accepts_nested_approx` returns `false` (the default), binding
  falls back to `Implementation::PassThrough` instead of constructing an
  unsound `FromSummary` column — the same paired-method shape
  `realize_extension`/`readout_extension` already uses one layer up.

#### Interface: folding over a child, as a monoid homomorphism

Not every composition is an accuracy question. Folding an *exact* function
(`Min`/`Max`/`Avg`/`StdDev`/`Variance`) over a child's already-produced
output is exact by construction — the open question there is which of
those fold operators are valid to apply directly to a child's
already-*collapsed* value, versus which need the child's pre-readout
state. This is exactly Algebird's `Averaged`/moment-monoid distinction:
`Avg`/`Variance`/`StdDev` are not associative on the scalar output (the
average of two averages isn't the true average under unequal group sizes)
— the monoid lives on the sufficient statistic (`Σv`; `Σv, n`; `Σv, Σv²,
n`), not on the folded scalar itself.

```rust
pub enum FoldOp {
    Min,
    Max,
    Avg,
    Variance { population: bool },
    StdDev { population: bool },
}

pub enum FoldInput {
    /// (ℝ, min, +∞) / (ℝ, max, −∞) are commutative monoids on the value
    /// itself — the child's already-read-out scalar estimate suffices.
    Value,
    /// No monoid exists on the scalar average/variance alone — folding
    /// needs the child's pre-readout sufficient statistic instead.
    SufficientStatistic,
}

impl FoldOp {
    pub fn required_input(&self) -> FoldInput {
        match self {
            FoldOp::Min | FoldOp::Max => FoldInput::Value,
            FoldOp::Avg | FoldOp::Variance { .. } | FoldOp::StdDev { .. } => {
                FoldInput::SufficientStatistic
            }
        }
    }
}

SummaryExpr::SummaryFold {
    children: Vec<Rc<L4Node>>,   // per FoldOp::required_input: already-read-out
                                  // values, or pre-readout sufficient-statistic state
    fold: FoldOp,
    by: Vec<ColumnRef>,          // this node's own grouping — may be coarser than any child's
},
```

Because `fold` is always exact, `SummaryFold` carries no `Accuracy` of its
own — it's transparent to whatever accuracy its children already have.

### Which issue this solves

The design above is issue-agnostic on purpose; here is what each piece
answers.

**[#171](https://github.com/ProjectASAP/ASAPController/issues/171) —
composed exact ↔ summary aggregation, either nesting order.**

- *Direction 1* (an outer exact fold, e.g. `max by (zone)
  (quantile_over_time(0.99, m[5m]))`, over an inner independently-realized
  summary) is answered by `SummaryFold`/`FoldOp`. `Min`/`Max` consume the
  child's scalar estimate directly; `Avg`/`StdDev`/`Variance` consume the
  child's pre-readout sufficient-statistic state instead, per
  `required_input`. No new accumulator kind is needed — today's code path
  (`accumulator(intent, SummaryKind::MinMax, ..)`, which requires a real,
  independently-materialized `MinMax` instance) is what made this
  direction look like it needed one.
- *Direction 2* (an outer summary, e.g. `TopK`, over an inner exact
  accumulator like `Rate`) is the `FromSummary` interface at
  `accuracy = Accuracy::EXACT`. `compose`'s `Probabilistic` arm with
  `A_c = Accuracy::EXACT` returns `A_o` unchanged — composing over an
  exact child is free, so this case is allowed unconditionally, with no
  `CostModel` opt-in required.

**[#172](https://github.com/ProjectASAP/ASAPController/issues/172) —
nested approximate-over-approximate composition has no error-propagation
model.** This is the `FromSummary` interface at a non-`EXACT` `Accuracy`:
`compose`'s `Probabilistic` arm with `A_c.epsilon > 0` (or `A_c.delta >
0`) is the formula this issue asked for — `ε_total = ε_outer + L·ε_child`,
`δ_total = δ_outer + δ_child` — gated by `CostModel::accepts_nested_approx`
(default `false`, answering the issue's own open question: refuse to bind
by default rather than silently double-approximate) and sized by
`size_params` receiving `child_accuracy` so the *combined* bound, not just
the outer layer's, is checked against the query's actual target.

**[#173](https://github.com/ProjectASAP/ASAPController/issues/173) —
multi-dimensional/structured summaries (QTree, Hydra).** Two things fall
out of the design without extra machinery:

- Hydra's "sketch of sketches" is `FromSummary`/`SummaryFold` with
  `nodes`/`children` containing more than one entry — the union bound in
  `compose`'s per-child term already generalizes to a sum over children,
  so a cross-dimension combiner is the existing interface applied to a
  vector instead of a single node, not a new mechanism.
- QTree-style range trees are the `Accuracy::Bounded` variant — a
  deterministic interval radius rather than a probabilistic `(ε, δ)` pair.
  `compose`'s `Bounded` arm gives the same triangle-inequality composition
  without a failure probability to union-bound, so a range-tree kind
  slots into `FromSummary`/`SummaryFold` the same way a probabilistic
  sketch does; only `compose` and `size_params` need to know which variant
  they're holding.
