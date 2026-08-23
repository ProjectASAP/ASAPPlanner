# ASAP-Aware Mapping: Developer Guide

This document explains how to extend ASAP-aware mapping in the current codebase.

It is written for developers who want to:

- add a new `ReplacementStrategy`,
- add or customize a `CostModel`,
- understand how strategies, binding, and costing interact,
- add a new kind of replacement without duplicating existing planner logic,
- and write the tests expected for a new extension.

The focus here is the **current code interfaces and their contracts**. For the higher-level motivation and future search design, see the separate design document, [`docs/design_docs/asap_aware_mapping.md`](../design_docs/asap_aware_mapping.md).

Code samples named `My*` or `Prefer*` (`MyStrategy`, `MyCostModel`, `PreferDDSketch`, …) below are illustrative sketches of a pattern, not code that ships in this crate. Samples that name a real type (`SketchFamilyStrategy`, `ForceSketchKind`, `SharedSubtreeStrategy`, …) are copied verbatim from `replacement.rs`/`cost_model.rs`.

---

## 1. Mental model

ASAP-aware mapping has two different jobs that should remain separate:

1. **Generate valid alternatives.**
2. **Choose among alternatives.**

`ReplacementStrategy` is responsible for the first job.

`CostModel` is responsible for the second.

A strategy should answer:

> Does this transformation apply here, and if so, what are all semantically valid replacements?

A cost model should answer:

> Given valid choices, which choices are preferable, and how should they be parameterized?

Do not put cost-based pruning into a `ReplacementStrategy`. A strategy should enumerate valid alternatives even when one of them is obviously more expensive under the default cost model. (This is the single most important rule in this guide — later sections restate it in context rather than re-arguing it; see §5 Rule 2 for the canonical statement.)

---

## 2. Glossary

### `TargetSubDAG`

A pre-ASAP `QueryExpr` node that a strategy may replace.

```rust
pub struct TargetSubDAG<'a> {
    pub root: &'a Rc<QueryExpr>,
    pub consumer_count: usize,
}
```

`root` is the actual `Rc<QueryExpr>` from the workload.

`consumer_count` counts **structural references**, not runtime reads/executions: how many places in the workload's `Rc<QueryExpr>` DAG point at this exact `root` node. Example — two separate top-level queries, `sum by (service) (rate(m[5m]))` and `avg by (service) (rate(m[5m]))`, share an identical `rate(m[5m])` subtree; after CSE (`share_common_subtrees`) collapses that into one `Rc<QueryExpr>`, both queries' trees point at the same node, so its `TargetSubDAG.consumer_count` is `2` — regardless of how many times either query actually executes.

Use:

```rust
let target = TargetSubDAG::new(&root);
```

when you are inspecting a node in isolation.

Use:

```rust
let target = TargetSubDAG::with_consumer_count(&root, count);
```

when the caller already knows the real number of consumers.

`TargetSubDAG::new` assumes one consumer.

---

### `Replacement`

The actual object that substitutes the target.

There are currently two forms:

```rust
pub enum Replacement {
    Summary(Rc<SummaryNode>),
    Rewrite(Rc<QueryExpr>),
}
```

Use `Replacement::Summary` when the alternative is already bound into a post-ASAP summary plan.

Use `Replacement::Rewrite` when the alternative is still a logical pre-ASAP `QueryExpr`.

Examples:

```text
Quantile(...)
    -> KLL SummaryNode
```

is a `Summary`.

```text
compute independently
    vs.
reuse an already shared logical subtree
```

is represented as a `Rewrite`.

---

### `ReplacementSubDAG`

One candidate replacement plus an explanation.

```rust
pub struct ReplacementSubDAG {
    pub replacement: Replacement,
    pub rationale: String,
}
```

The `rationale` is for debugging, reporting, and explaining planner choices. It is human-readable text, not a machine-readable protocol.

Every candidate should carry a useful rationale.

---

### `ReplacementStrategy`

The main extension point for adding a new optimization or replacement source.

```rust
pub trait ReplacementStrategy {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool;

    fn replacements(
        &self,
        target: &TargetSubDAG<'_>,
    ) -> Vec<ReplacementSubDAG>;
}
```

The two methods have intentionally different responsibilities.

`matches` answers:

> Does this strategy have anything to offer for this target?

`replacements` answers:

> What are all semantically valid alternatives for this target?

`replacements` must be **exhaustive, not ranked, and not cost-filtered**.

---

### `CostModel`

`CostModel`'s job is broader than its name suggests — it's the one extension point every deployment-specific numeric/configuration decision plugs into, not just "which candidate is cheapest." Sizing counts as a cost decision for the same reason ranking does: a bigger sketch (more memory, more update cost) buys more accuracy, and how you want to spend that budget is exactly the kind of real-world cost knowledge this crate deliberately doesn't hardcode — see the crate doc's layering note at the top of `cost_model.rs` (`asap-plan` depends only on `asap_ir`, never on a runtime or deployment model, so it can't know real costs itself). Every hook but one has a default body that reproduces this crate's built-in static behavior — override only what you need to change:

- **`rank_candidates`** — order a set of sketch candidates for one `AggIntent`, best choice first. **No default** — this is the one hook every `CostModel` must implement; candidate selection needs a real answer from somewhere.

  ```rust
  fn rank_candidates(&self, intent: &AggIntent, candidates: &[SketchKind]) -> Vec<SketchKind>;
  ```

- **`size_params`** — pick concrete parameters (e.g. sketch capacity) for one already-chosen `SketchKind`, given an accuracy target `(eps, delta)`. This is a separate hook from `rank_candidates` specifically so a deployment can override *just* sizing (e.g. an empirically-tuned table, or discrete capacity rungs a downstream catalog requires) without also forking candidate selection — same "one extension point per decision" shape as everything else in this trait. Default: `boundary::default_size_params`, this crate's built-in per-family sizing formulas.

  ```rust
  fn size_params(&self, kind: SketchKind, intent: &AggIntent, eps: f64, delta: f64) -> SketchParams;
  ```

- **`realize_extension`** — decide what post-ASAP `Implementation` a deployment-defined `AggIntent::Extension` maps to (a shape core has no built-in opinion on). Default: `Implementation::PassThrough`.

  `AggIntent::Extension { ext_kind: String, payload: serde_json::Value }` is the escape hatch for an intent shape only *your* deployment needs — core's `AggIntent` enum deliberately doesn't grow a variant for every capability a single deployment wants (issue #131), so instead a deployment tags its own shape with a `ext_kind` string and puts whatever it needs in `payload`; core treats both opaquely. Example: a deployment wants an approximate-frequency intent core has no variant for. It tags queries with `Extension { ext_kind: "frequency".into(), payload: ... }`, then overrides `realize_extension` to recognize that tag:

  ```rust
  fn realize_extension(&self, ext_kind: &str, _payload: &serde_json::Value) -> Implementation {
      if ext_kind == "frequency" {
          Implementation::Sketch { kind: SketchKind::CountSketch, params: /* ... */ }
      } else {
          Implementation::PassThrough  // fall back to the default for anything else
      }
  }
  ```

  Every `ext_kind` your deployment doesn't recognize should still fall through to `Implementation::PassThrough` (the default), not panic — only the `ext_kind`s you've deliberately implemented get a real realization.

  ```rust
  fn realize_extension(&self, ext_kind: &str, payload: &serde_json::Value) -> Implementation;
  ```

- **`readout_extension`** — build the query-time read for an `Extension` intent this same `CostModel` already realized as `Sketch` via `realize_extension`. "Readout" means the query-time **read** path — how an already-bound summary answers a query — as distinct from `realize_extension`, which decides how the summary is **built and maintained**; the two are a matched pair, so overriding `realize_extension` to return `Sketch` for some `ext_kind` requires overriding this for that same `ext_kind` too. Default: panics (deliberately — a silent wrong answer here is worse than a loud crash) if that pairing was left incomplete.

  ```rust
  fn readout_extension(&self, ext_kind: &str, payload: &serde_json::Value, col: &ColumnRef) -> SketchQuery;
  ```

- **`cse_recompute_cost`** — estimate the one-time cost of recomputing a CSE candidate's subtree independently at a single consumer. Default: `default_cse_recompute_cost`, a structural-size proxy. Returns a bare `f64` today (a unitless scalar compared directly against `cse_shared_maintenance_cost`'s own `f64`) — a richer cost type (e.g. a struct separating CPU/memory/network) would let deployments compare along more than one axis, but is a real signature change across every hook in this trait, not a doc fix; worth its own issue rather than deciding here.

  ```rust
  fn cse_recompute_cost(&self, candidate: &CseCandidate) -> f64;
  ```

- **`cse_shared_maintenance_cost`** — estimate the cost of maintaining one shared summary continuously for the life of the workload. Default: `default_cse_shared_maintenance_cost`, a per-family weight table.

  ```rust
  fn cse_shared_maintenance_cost(&self, candidate: &CseCandidate) -> f64;
  ```

- **`cse_share_decision`** — decide whether to share one summary across all of a CSE candidate's consumers, or recompute independently at each. Default: composes the two cost hooks above (share iff shared-maintenance cost ≤ total recompute cost across every consumer) — override the two cost hooks and keep this comparison unless you need a genuinely different policy, not a different cost input.

  ```rust
  fn cse_share_decision(&self, candidate: &CseCandidate) -> ShareDecision;
  ```

A custom cost model does not necessarily need to override every hook. The current tests include a model that overrides only `rank_candidates`, relying on defaults for the rest.

---

## 3. How the current pieces fit together

There are two related paths in the current code, and it's worth being explicit about how they relate before looking at either one, since neither name says it outright: **the binding path is older and still the one thing that actually runs in production; the replacement-strategy path is new and additive, not a replacement for it** (`replacement.rs`'s own module docs put this as "why this exists alongside `boundary`/`bind`, not instead of them"). Both start from the same input (an `AggIntent`) and both eventually reuse the exact same underlying decision logic (`boundary::implementation_for_with`, `bind::implement_tree_with`) — they differ only in *how much of the answer* they keep:

- The **binding path** is what runs today when a query actually gets bound: it commits to one `Implementation` and discards every alternative a `CostModel` didn't pick, because something has to actually execute.
- The **replacement-strategy path** is what `ReplacementStrategy::replacements()` (§2) produces: instead of committing to one, it *keeps every alternative* the binding path would have thrown away, packaged as `ReplacementSubDAG`s.

This is exactly the "alternatives"/"candidates" language in [the design doc](../design_docs/asap_aware_mapping.md): a `ReplacementSubDAG` **is** one candidate; a `TargetSubDAG` with its full `replacements()` list **is** the set of alternatives for one spot in the plan. The design doc describes a *future* search (not built yet, tracked separately) that would compare whole candidate plans built from these alternatives — the two paths below are what that future search would draw on, not the search itself.

### Binding path

The binding path needs one executable answer.

Conceptually:

```text
AggIntent
   |
   v
boundary::implementation_for_with(...)
   |
   v
one Implementation
   |
   v
bind::implement_tree_with(...)
   |
   v
one bound SummaryNode
```

A `CostModel` is used here because binding must eventually commit to one implementation.

---

### Replacement-strategy path

The strategy path exists to expose alternatives rather than immediately commit to one.

Conceptually:

```text
TargetSubDAG
   |
   v
ReplacementStrategy::matches(...)
   |
   v
ReplacementStrategy::replacements(...)
   |
   +-------------------------------+
   |               |               |
candidate A     candidate B     candidate C
```

The important rule is:

> Strategies should reuse existing decision and binding logic where possible instead of reimplementing it.

For example, `SketchFamilyStrategy` uses the existing boundary and binding machinery to enumerate each valid sketch realization.

---

## 4. Adding a new `ReplacementStrategy`

A new optimization should normally be introduced as a new implementation of `ReplacementStrategy`.

Do not change the trait just because a new optimization is added.

Start with this skeleton (illustrative — `MyStrategy` is not a real type in this crate):

```rust
pub struct MyStrategy;

impl ReplacementStrategy for MyStrategy {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        // Return true only when this transformation is applicable.
        todo!()
    }

    fn replacements(
        &self,
        target: &TargetSubDAG<'_>,
    ) -> Vec<ReplacementSubDAG> {
        if !self.matches(target) {
            return Vec::new();
        }

        // Enumerate every semantically valid replacement.
        todo!()
    }
}
```

There are four decisions to make.

---

### 4.1 Define the target shape

`matches` should contain the minimum structural and semantic checks needed to determine whether the strategy applies.

For example, `SketchFamilyStrategy` only matches the aggregate shape that the existing binder can actually bind:

- the node is an `Aggregate`,
- it has one aggregation intent,
- it does not have `HAVING`.

A strategy that depends on cross-query context may additionally inspect `consumer_count`.

For example:

```rust
fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
    target.consumer_count >= 2
}
```

is enough for the current shared-subtree strategy.

#### Guideline

Keep `matches` cheap and unsurprising.

It should answer *whether the strategy applies here* in the plain English sense — not `applicability.rs`'s formal `ApplicabilityRule`/`ApplicabilityFinding` concept (issue #247, a separate reporting layer covered in the design doc's "Applicability reporting" section). `matches` isn't that machinery and doesn't need to produce anything it consumes; it should also not perform ranking or choose a winner.

---

### 4.2 Enumerate every valid alternative

`replacements` should return every semantically valid candidate for a matched target.

For example, if a quantile can be represented by:

```text
KLL
DDSketch
```

then both should be returned.

Do not write:

```rust
if kll_is_cheaper {
    vec![kll]
} else {
    vec![ddsketch]
}
```

inside a strategy.

That would throw away part of the search space before the cost model can compare complete plans.

Instead:

```rust
vec![kll, ddsketch]
```

and let costing decide later.

---

### 4.3 Choose `Summary` vs. `Rewrite`

Return:

```rust
Replacement::Summary(...)
```

when the candidate is a fully bound post-ASAP summary.

Return:

```rust
Replacement::Rewrite(...)
```

when the candidate is a logical pre-ASAP rewrite.

This distinction matters because a rewrite may enable more transformations later, while a bound summary represents a concrete summary realization.

---

### 4.4 Add a rationale

Every `ReplacementSubDAG` should explain why the candidate exists.

Good:

```rust
ReplacementSubDAG {
    replacement: ...,
    rationale: "derive service-level aggregate by rolling up the \
                mergeable service+region aggregate".into(),
}
```

Less useful:

```rust
rationale: "candidate 2".into()
```

The rationale should help a developer understand a planner trace without reading the strategy implementation.

Do not encode machine-readable state into the string.

---

## 5. Strategy contract

Every new strategy should follow these rules.

### Rule 1: `matches == false` should be safe

The existing strategies return an empty vector when `replacements` is called on a target they do not match.

Follow the same convention:

```rust
if !self.matches(target) {
    return Vec::new();
}
```

Do not panic simply because the caller skipped a prior `matches` call.

---

### Rule 2: enumerate; do not rank

A strategy owns **legality and enumeration**.

A cost model owns **preference and costing**.

This separation is the most important extension rule in this module.

---

### Rule 3: do not duplicate an existing decision procedure

If another module already knows how to determine whether something is legal or how to bind it, wrap that logic.

Do not create a second implementation of the same semantics inside the strategy.

The existing `SketchFamilyStrategy` is the model to follow: it reuses the boundary candidate list and the existing binder.

---

### Rule 4: preserve semantics

Every returned replacement must be semantically valid for the target.

Cost differences do not justify semantic differences.

If a transformation is only valid under additional summary properties, grouping assumptions, or accuracy constraints, check those conditions before returning the candidate.

---

### Rule 5: a strategy does not need to discover the whole workload

`TargetSubDAG` is passed into the strategy.

The strategy is responsible for deciding what to do with that target, not for walking every workload root to discover targets.

If your transformation requires context not currently represented in `TargetSubDAG`, that is a design question about target metadata or search/discovery — not a reason to hide a second workload traversal inside `replacements`.

---

## 6. Example: current `SketchFamilyStrategy`

`SketchFamilyStrategy` is the reference implementation for a strategy that produces bound summaries.

Construction:

```rust
let strategy =
    SketchFamilyStrategy::default_cost_model();
```

or with a custom cost model:

```rust
let model = MyCostModel; // illustrative
let strategy = SketchFamilyStrategy::new(&model);
```

The strategy matches bindable aggregate nodes.

At a high level:

```text
Target Aggregate
     |
     v
extract AggIntent
     |
     v
boundary::implementation_for_with(...)
     |
     +--------------------------+
     |
     | approximate sketch case
     v
boundary::summary_candidates(...)
     |
     v
bind each sketch candidate separately
     |
     v
Vec<ReplacementSubDAG>
```

For an approximate quantile, the current candidate list includes both KLL and DDSketch.

The strategy returns both, even if the cost model ranks one ahead of the other.

For cases where boundary selection has only one realization, such as an exact accumulator or pass-through, the strategy returns that single realization.

---

### Why `ForceSketchKind` exists

The ordinary binding path asks the cost model to rank sketch candidates and then binds the first choice.

That is correct for normal binding, but a replacement strategy needs to bind **each** valid sketch candidate.

`ForceSketchKind` is a small `CostModel` adapter used for that purpose. It's real code — copied verbatim from `replacement.rs`.

The trick: `implement_tree_with` always binds whichever candidate `rank_candidates` puts *first* — there's no way to tell it directly "bind KLL this time." So to bind KLL specifically, wrap the real cost model in a `ForceSketchKind { kind: Kll, inner: real_cost_model }` and pass *that* into `implement_tree_with` instead. `ForceSketchKind::rank_candidates` forces `kind` (here, `Kll`) to the front of whatever `inner` would have ranked, so `implement_tree_with`'s "take the first candidate" logic ends up binding `Kll` — while every other decision (sizing, extension realization, …) still comes from `inner` unchanged, since only `rank_candidates` is overridden. Do this once per candidate (`ForceSketchKind { kind: Kll, .. }`, then separately `ForceSketchKind { kind: DDSketch, .. }`) and each gets bound through the exact same real machinery, one at a time, without touching `implement_tree_with`'s internals at all. It overrides only ranking:

```rust
fn rank_candidates(
    &self,
    intent: &AggIntent,
    candidates: &[SketchKind],
) -> Vec<SketchKind> {
    let mut ranked =
        self.inner.rank_candidates(intent, candidates);

    ranked.retain(|k| *k != self.kind);
    ranked.insert(0, self.kind.clone());
    ranked
}
```

Every other cost-model hook is forwarded to the underlying model.

The result is:

> bind this specific sketch family, but keep using the caller's real cost model for sizing and all other behavior.

This is an important pattern to preserve when adding another strategy that needs to enumerate alternatives through an API that normally chooses one.

---

## 7. Example: current `SharedSubtreeStrategy`

`SharedSubtreeStrategy` is the reference implementation for a logical rewrite strategy.

It applies when:

```rust
target.consumer_count >= 2
```

and returns two alternatives:

```text
1. Build once and share.
2. Build independently for each consumer.
```

The shared candidate reuses the same `Rc<QueryExpr>`:

```rust
Replacement::Rewrite(Rc::clone(target.root))
```

The independent candidate creates a structurally equal but separately allocated node:

```rust
Replacement::Rewrite(
    Rc::new((**target.root).clone())
)
```

This strategy does **not** decide whether sharing is cheaper.

That decision belongs to the cost model — and today, `CostModel::cse_share_decision`'s default body already makes it, via a real cost comparison (§9), every time the *production binding path* (§3) sees a shared subtree. So it's fair to ask: isn't "build independently" (#2) already the exception, not the default — and doesn't that comparison already decide things? It's worth being precise about what "default" means here:

- `target.root` arriving with `consumer_count >= 2` means CSE detection (`share_common_subtrees`) already merged it onto one shared `Rc` *before* this strategy ever runs — so "build once and share" (#1) costs nothing to produce (`Rc::clone` of what's already there); "build independently" (#2) is the one requiring real work (an actual deep clone).
- But `cse_share_decision`'s cost comparison is a *separate, already-existing* mechanism that only the production binding path consults — it isn't something `SharedSubtreeStrategy` calls or defers to. This strategy has to enumerate **both** regardless of which one is cheap to construct or which one that other comparison would currently pick, because per §1's core rule, a strategy is never allowed to pre-decide based on cost — collapsing to just #1 here would be exactly the "cost-based pruning inside a strategy" anti-pattern §5 Rule 2 forbids, even though today's separate binding path already has an answer. The whole reason this module exists (§0) is to keep both alternatives around for a future search to compare *in the context of a full plan* — which might disagree with `cse_share_decision`'s isolated, pairwise-only comparison.

This example is useful when implementing transformations such as:

- roll-up vs. recompute,
- shared grouping vs. per-group instances,
- semantic rewrite vs. original expression.

Each strategy can expose the alternatives without choosing between them.

---

## 8. Adding a custom `CostModel`

Use a custom `CostModel` when you want to change preferences or cost assumptions without changing transformation legality.

A minimal model can override only the hook it cares about.

For example (illustrative — `PreferDDSketch` is not a real type in this crate):

```rust
struct PreferDDSketch;

impl CostModel for PreferDDSketch {
    fn rank_candidates(
        &self,
        _intent: &AggIntent,
        candidates: &[SketchKind],
    ) -> Vec<SketchKind> {
        let mut ranked = candidates.to_vec();

        if let Some(pos) =
            ranked.iter().position(
                |k| *k == SketchKind::DDSketch
            )
        {
            let dd = ranked.remove(pos);
            ranked.insert(0, dd);
        }

        ranked
    }
}
```

Then inject it into code that accepts a `&dyn CostModel`:

```rust
let model = PreferDDSketch;

let strategy =
    SketchFamilyStrategy::new(&model);

let replacements =
    strategy.replacements(&target);
```

Important: changing `rank_candidates` changes the preferred ordering, but `SketchFamilyStrategy` still enumerates every valid sketch candidate.

A custom cost model should not change which alternatives are semantically legal.

---

## 9. Which `CostModel` hook should I implement?

Use this as a practical guide.

### `rank_candidates`

Use when you want to change the preference among valid sketch families.

Example:

```text
KLL vs. DDSketch
HLL vs. Theta vs. KMV
```

Signature:

```rust
fn rank_candidates(
    &self,
    intent: &AggIntent,
    candidates: &[SketchKind],
) -> Vec<SketchKind>;
```

The returned vector should rank candidates from most to least preferred.

It should rank candidates that were supplied to it rather than invent unrelated sketch kinds.

---

### `size_params`

Use when the sketch family is already known and you want to choose its parameters from an accuracy target.

Signature:

```rust
fn size_params(
    &self,
    kind: SketchKind,
    intent: &AggIntent,
    eps: f64,
    delta: f64,
) -> SketchParams;
```

Typical uses include:

- choosing KLL capacity,
- choosing HLL precision,
- selecting sketch-specific error parameters.

Conceptually:

```text
SketchKind + AggIntent + accuracy target
                 |
                 v
            SketchParams
```

---

### `realize_extension`

Use for extension-defined implementation kinds.

```rust
fn realize_extension(
    &self,
    ext_kind: &str,
    payload: &serde_json::Value,
) -> Implementation;
```

This is the hook for turning an extension description into a concrete `Implementation`.

Use it for implementation families that are intentionally outside the built-in enum dispatch.

---

### `readout_extension`

Use when an extension-defined summary also needs custom query/readout behavior.

```rust
fn readout_extension(
    &self,
    ext_kind: &str,
    payload: &serde_json::Value,
    col: &ColumnRef,
) -> SketchQuery;
```

This complements `realize_extension`: realization defines what gets maintained; readout defines how it is queried (see the definition in §2's `CostModel` glossary entry if "readout" is unfamiliar).

---

### `cse_recompute_cost`

Use to estimate the cost of computing a common subtree independently at each consumer.

```rust
fn cse_recompute_cost(
    &self,
    candidate: &CseCandidate,
) -> f64;
```

---

### `cse_shared_maintenance_cost`

Use to estimate the cost of computing and maintaining a shared subtree.

```rust
fn cse_shared_maintenance_cost(
    &self,
    candidate: &CseCandidate,
) -> f64;
```

---

### `cse_share_decision`

Use when the current binding/planning path needs the final share-vs.-recompute decision.

```rust
fn cse_share_decision(
    &self,
    candidate: &CseCandidate,
) -> ShareDecision;
```

The replacement-strategy layer should still expose both valid alternatives where appropriate. This hook is the cost-sensitive decision point for code paths that need to commit to one answer.

---

## 10. `Matcher` (`boundary.rs`)

There is a third extension point in this crate, much smaller in surface area than `ReplacementStrategy` or `CostModel`, but worth knowing about:

```rust
pub trait Matcher {
    fn is_satisfied_by(&self, required: &Implementation, available: &Implementation) -> bool;
}
```

`Matcher` answers a different question than everything above it: not "how do I *build* a summary for this intent" (that's `boundary`/`bind`/`ReplacementStrategy`), but "is there already a summary sitting around somewhere that answers this without building anything new?" This is the query-optimization-literature "answering queries using views" question, applied to summaries — the same idea as a database reusing an existing materialized view or index instead of recomputing from scratch.

Concrete example: a deployment already has a `DDSketch` built earlier for some other query on `latency`, and a new query needs `Kll` quantiles on `latency`. Can the existing `DDSketch` answer it without building a new `Kll` sketch? A pure sketch-algebra answer says yes — both are quantile sketches, mutually substitutable at the query level. But a deployment with its own storage-layout rules might say no, e.g. if its inventory ties a stored summary to a specific algorithm identity it won't re-interpret. `Matcher::is_satisfied_by(required, available)` is where a deployment plugs in whichever answer is actually true for its own storage layer — `required` is what a query needs (an `Implementation` `boundary`/`bind` computed), `available` is what already exists somewhere in that deployment's inventory.

Unlike `CostModel`, this crate ships **no default body and no implementation** for `Matcher` at all — the answer genuinely depends on facts this crate deliberately doesn't settle (see the example above: a pure sketch-algebra answer and a deployment's own storage-layout rules can legitimately disagree, and this crate has no inventory concept of its own to judge between them). If you need this, you're implementing the whole trait for your deployment from scratch; there's no default to lean on and no existing implementation in this crate to copy from.

---

## 11. Adding both a strategy and a cost model

Some features require both.

For example, suppose we add:

```text
roll up a finer group-by
vs.
compute the coarser group-by independently
```

The responsibilities should be divided as follows.

### Strategy

The new strategy determines:

- whether the two group-bys have the required relationship,
- whether the aggregation is mergeable,
- whether roll-up preserves semantics,
- and then returns both valid alternatives.

Conceptually:

```rust
vec![
    rollup_candidate,
    recompute_candidate,
]
```

### Cost model

The cost model determines:

- maintenance cost of the finer-grained summary,
- cost of roll-up,
- cost of independent computation,
- expected query frequency,
- and which complete plan is cheaper.

Do not encode:

```text
"only return roll-up when roll-up is cheaper"
```

inside the strategy.

That turns a cost decision into a legality decision and prevents later global plan search from seeing both options.

---

## 12. Adding a new sketch family

A new sketch family generally touches more than `ReplacementStrategy`.

The strategy should not maintain its own private list of sketch kinds.

`SketchFamilyStrategy` obtains sketch alternatives through the existing boundary interface:

```rust
summary_candidates(intent)
```

and binds them through the normal binder.

Therefore, when adding a new built-in sketch family, the intended flow is:

```text
1. Teach the boundary layer that the sketch is a valid candidate
   for the relevant AggIntent.

2. Teach the cost model how to rank and size it.

3. Ensure the binder can realize the sketch family.

4. SketchFamilyStrategy will then enumerate it through the
   existing candidate/binding path.
```

This keeps one source of truth for sketch applicability.

Do not special-case the new sketch inside `SketchFamilyStrategy` unless the strategy itself needs fundamentally new behavior.

---

## 13. Using a strategy

The basic calling pattern is:

```rust
let target = TargetSubDAG::new(&root);
let strategy =
    SketchFamilyStrategy::default_cost_model();

if strategy.matches(&target) {
    let candidates =
        strategy.replacements(&target);

    for candidate in candidates {
        println!("{}", candidate.rationale);
    }
}
```

A caller may also safely call `replacements` directly and treat an empty vector as "not applicable":

```rust
let candidates =
    strategy.replacements(&target);

if candidates.is_empty() {
    // No candidate from this strategy.
}
```

For strategies that require workload context:

```rust
let target =
    TargetSubDAG::with_consumer_count(
        &root,
        consumer_count,
    );
```

The caller is responsible for providing correct cross-workload metadata.

---

## 14. Testing a new strategy

Every new strategy should have focused tests for its contract.

At minimum, test the following.

### Applicability

A matching target should satisfy:

```rust
assert!(strategy.matches(&target));
```

A non-matching target should satisfy:

```rust
assert!(!strategy.matches(&target));
```

---

### Safe non-match behavior

Also call `replacements` on a non-matching target:

```rust
assert!(
    strategy.replacements(&target).is_empty()
);
```

This verifies that the strategy does not depend on callers always invoking `matches` first.

---

### Exhaustive candidate enumeration

If the target has N valid alternatives:

```rust
let replacements =
    strategy.replacements(&target);

assert_eq!(replacements.len(), N);
```

Check the identities or kinds of all candidates, not just the preferred one.

The current sketch-family tests explicitly verify that:

- quantile returns both KLL and DDSketch,
- cardinality returns HLL, Theta, and KMV.

This is the most important regression test for a strategy.

---

### Rationale

Verify that every candidate has a non-empty rationale:

```rust
assert!(
    replacements
        .iter()
        .all(|r| !r.rationale.is_empty())
);
```

For a strategy whose explanation includes important context, also test that context.

For example, the shared-subtree tests verify that the consumer count appears in the rationale.

---

### Structural semantics

For logical rewrites, test the structural property that distinguishes the alternatives.

For example, the current shared-subtree tests verify:

```rust
Rc::ptr_eq(shared, &q)
```

for the shared candidate, and:

```rust
!Rc::ptr_eq(independent, &q)
```

plus structural equality for the independent candidate.

Do not test only the rationale string; test the actual replacement semantics.

---

### Custom cost model behavior

If a strategy accepts a cost model, verify that a custom model changes the intended costing behavior without changing the exhaustive candidate set.

The current sketch strategy does exactly this:

```text
custom model prefers DDSketch
        |
        v
strategy still returns
KLL + DDSketch
```

That is the expected separation between enumeration and ranking.

---

## 15. Testing a new cost model

A cost-model test should focus on the hook being customized.

For ranking:

```rust
let ranked =
    model.rank_candidates(
        &intent,
        &[SketchKind::Kll,
          SketchKind::DDSketch],
    );

assert_eq!(
    ranked[0],
    SketchKind::DDSketch
);
```

Then test integration through a consumer of the cost model.

For example:

```rust
let strategy =
    SketchFamilyStrategy::new(&model);

let replacements =
    strategy.replacements(&target);
```

The important assertion is usually not that other valid candidates disappeared. They should not.

Instead verify that:

- the model changes ordering or parameters as intended,
- all legal candidates remain available to the replacement layer.

For sizing, test representative accuracy targets and assert the resulting `SketchParams`.

For CSE costing, create a representative `CseCandidate` and test recompute cost, shared-maintenance cost, and the resulting `ShareDecision`.

---

## 16. Common mistakes

### Mistake: choosing the cheapest candidate inside a strategy

Wrong:

```rust
fn replacements(...) -> Vec<ReplacementSubDAG> {
    vec![choose_cheapest_candidate()]
}
```

Right:

```rust
fn replacements(...) -> Vec<ReplacementSubDAG> {
    all_valid_candidates()
}
```

---

### Mistake: maintaining a second sketch-applicability table

If the boundary layer already defines which sketch families satisfy an `AggIntent`, reuse that source.

Otherwise the binder and replacement strategy can silently disagree.

---

### Mistake: reimplementing binding inside a strategy

If the candidate should produce a normal `SummaryNode`, use the existing binding path.

A strategy should steer or wrap that path when necessary, not recreate schema derivation, column resolution, readout construction, or parameter sizing.

---

### Mistake: treating `rationale` as planner state

`rationale` is explanatory text.

If downstream logic needs a fact, represent it in the plan or another typed structure instead of parsing the rationale.

---

### Mistake: hiding workload traversal in a strategy

A `ReplacementStrategy` operates on the `TargetSubDAG` it is given.

Workload-wide target discovery, deduplication, and consumer counting are separate concerns.

---

### Mistake: assuming `Rc` structural equality and identity mean the same thing

For CSE-style decisions, pointer identity can encode actual sharing.

Two `Rc<QueryExpr>` values can be structurally equal but deliberately represent independent computation.

Use the distinction intentionally.

---

## 17. Extension checklist

When adding a new strategy:

- [ ] Define the exact target shape.
- [ ] Implement `ReplacementStrategy::matches`.
- [ ] Implement `ReplacementStrategy::replacements`.
- [ ] Return every semantically valid replacement.
- [ ] Return an empty vector for non-matching targets.
- [ ] Use `Replacement::Summary` for bound post-ASAP output.
- [ ] Use `Replacement::Rewrite` for logical pre-ASAP alternatives.
- [ ] Add a useful rationale to every candidate.
- [ ] Reuse existing legality/binding logic instead of duplicating it.
- [ ] Keep ranking and cost-based pruning out of the strategy.
- [ ] Test positive and negative applicability.
- [ ] Test exhaustive enumeration.
- [ ] Test the actual structural semantics of each replacement.
- [ ] Test behavior with a custom cost model if the strategy uses one.

When adding a new cost model:

- [ ] Override only the hooks whose behavior should change.
- [ ] Keep semantic applicability outside the cost model.
- [ ] Use `rank_candidates` for family preference.
- [ ] Use `size_params` for accuracy-to-parameter mapping.
- [ ] Use extension hooks for extension-defined implementations/readouts.
- [ ] Use CSE hooks for recompute-vs.-sharing costs.
- [ ] Test the hook directly.
- [ ] Test integration through a consumer such as `SketchFamilyStrategy`.
- [ ] Verify that changing cost preferences does not silently remove valid replacement candidates.

---

## 18. Current extension map

Use this table to find the right place for a change.

| I want to... | Primary extension point |
|---|---|
| Add a new logical optimization | new `impl ReplacementStrategy` |
| Add a new replacement for an existing target shape | `ReplacementStrategy::replacements` |
| Change when a strategy applies | `ReplacementStrategy::matches` |
| Add a new built-in sketch candidate | boundary summary-candidate mapping |
| Prefer one sketch family over another | `CostModel::rank_candidates` |
| Change sketch sizing for an accuracy target | `CostModel::size_params` |
| Add extension-defined implementation behavior | `CostModel::realize_extension` |
| Add extension-defined readout behavior | `CostModel::readout_extension` |
| Change CSE recomputation cost | `CostModel::cse_recompute_cost` |
| Change shared-maintenance cost | `CostModel::cse_shared_maintenance_cost` |
| Change current share/recompute choice | `CostModel::cse_share_decision` |
| Decide whether an available implementation satisfies a required one | `impl Matcher` |
| Produce a normal bound summary candidate | reuse `bind::implement_tree_with` |
| Enumerate valid sketch kinds | reuse `boundary::summary_candidates` |
| Build a target with no workload context | `TargetSubDAG::new` |
| Build a target with known sharing context | `TargetSubDAG::with_consumer_count` |

---

## 19. Design rule to remember

If there is one rule to keep in mind when extending this layer, it is:

> **Strategies define the valid search space; cost models express preferences within that search space.**

Keeping those responsibilities separate makes new optimizations composable, keeps one source of truth for semantics, and allows a future whole-plan search engine to compare interactions between choices rather than inheriting irreversible local decisions.
