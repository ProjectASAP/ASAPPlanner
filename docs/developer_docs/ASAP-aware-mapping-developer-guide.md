# ASAP-Aware Mapping: Developer Guide

This document explains how to extend ASAP-aware mapping in the current codebase.

It is written for developers who want to:

- add a new `ReplacementStrategy`,
- add or customize a `CostModel`,
- understand how strategies, binding, and costing interact,
- add a new kind of replacement without duplicating existing planner logic,
- and write the tests expected for a new extension.

The focus here is the **current code interfaces and their contracts**. For the higher-level motivation and future search design, see the separate design document, [`docs/design_docs/asap_aware_mapping.md`](../design_docs/asap_aware_mapping.md).

Code samples named `My*` or `Prefer*` (`MyStrategy`, `MyCostModel`, `PreferDDSketch`, …) below are illustrative sketches of a pattern, not code that ships in this crate. Samples that name a real type (`SketchFamilyStrategy`, `SharedSubtreeStrategy`, `bind_with_implementation`, …) are copied verbatim from `replacement.rs`/`bind.rs`/`cost_model.rs`.

---

## Terminology

Two words come up constantly below and are worth pinning down before anything else, since neither is self-explanatory from context alone:

- **`implementation`** (the module `implementation.rs`) — every valid way one `AggIntent` could be realized: as an approximate sketch, an exact mergeable accumulator, or a pass-through (no summary at all). `implementation::implementations_for_with` computes this **one node at a time**, exhaustive and ranked; it doesn't walk anything, and it doesn't pick a favorite — picking is left to its callers.
- **`bind` / "binding"** — this crate's own `bind.rs`. It has no "bind me one tree" entry point of its own: `replacement::SketchFamilyStrategy::replacements()` is the only public way to get bound output for a target, and it always returns *every* candidate; a caller that wants a single executable answer takes the first entry itself (see §3). What `bind.rs` provides is the shared low-level primitive, `bind_with_implementation`, that turns one already-decided candidate into a real `SummaryNode`, plus `implement_workload`/`implement_workload_with`, which drive that same take-the-head selection over a whole workload's roots (see §3's closing note on why those two still keep it internally). This is what "the binding path" means everywhere in this guide: the code that commits to one `Implementation` per node because something has to actually execute. (The word "bind" means other things elsewhere in this crate's downstream consumers — see `lib.rs`'s own "Terminology" section if you need the full picture — but within this guide, "bind"/"binding" always means this.)

So: `implementation` enumerates **every** way one node could become something; `ReplacementStrategy` (this guide's main subject) wraps that list into one `ReplacementSubDAG` per candidate, keeping all of them; a caller that wants one answer takes the first entry itself — see §3 for exactly how these connect.

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

- **`size_params`** — pick concrete parameters (e.g. sketch capacity) for one already-chosen `SketchKind`, given an accuracy target `(eps, delta)`. This is a separate hook from `rank_candidates` specifically so a deployment can override *just* sizing (e.g. an empirically-tuned table, or discrete capacity rungs a downstream catalog requires) without also forking candidate selection — same "one extension point per decision" shape as everything else in this trait. Default: `implementation::default_size_params`, this crate's built-in per-family sizing formulas.

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

- **`cse_recompute_cost`** — estimate the one-time cost of recomputing a CSE candidate's subtree independently at a single consumer. Default: `default_cse_recompute_cost`, a structural-size proxy.

  ```rust
  fn cse_recompute_cost(&self, candidate: &CseCandidate) -> Cost;
  ```

- **`cse_shared_maintenance_cost`** — estimate the cost of maintaining one shared summary continuously for the life of the workload. Default: `default_cse_shared_maintenance_cost`, a per-family weight table.

  ```rust
  fn cse_shared_maintenance_cost(&self, candidate: &CseCandidate) -> Cost;
  ```

  Both hooks return `Cost`, a newtype around `f64` rather than a bare `f64` — a deliberately minimal wrapper today (still one unitless scalar, `Cost(f64)`, comparable via `PartialOrd`/`Add`/`Mul<usize>`), but one that gives a future richer cost type (e.g. separate CPU/memory/network fields) a place to grow into without changing every hook's signature a second time.

- **`cse_share_decision`** — decide whether to share one summary across all of a CSE candidate's consumers, or recompute independently at each. Default: composes the two cost hooks above (share iff shared-maintenance cost ≤ total recompute cost across every consumer) — override the two cost hooks and keep this comparison unless you need a genuinely different policy, not a different cost input.

  ```rust
  fn cse_share_decision(&self, candidate: &CseCandidate) -> ShareDecision;
  ```

A custom cost model does not necessarily need to override every hook. The current tests include a model that overrides only `rank_candidates`, relying on defaults for the rest.

---

## 3. How the current pieces fit together

There is one place this crate decides what an `AggIntent` may become —
[`implementation::implementations_for_with`] — and one place bound output
comes from. There is no separate "binding path" that computes its own
answer and happens to agree with the replacement-strategy path; there is
only the replacement-strategy path:

- **`implementation::implementations_for_with(intent, cost_model)`** enumerates
  every valid `Implementation` for `intent`, exhaustive and ranked
  (most-preferred first via `cost_model`).
- **`replacement::SketchFamilyStrategy::replacements()`** (§2) wraps that list
  directly: every entry becomes its own bound `ReplacementSubDAG`, none
  discarded. This is the *only* public way to get bound output for a
  target — there is no second, single-answer entry point sitting behind it.
- **A caller that wants one executable answer** takes the first
  (`cost_model`-preferred) entry off `replacements()` itself
  (`.into_iter().next()`) and decides what to do if the list comes back
  empty (`bind::logical` is the same conservative pass-through fallback this
  crate's own dispatch would otherwise use).

This is exactly the "alternatives"/"candidates" language in [the design doc](../design_docs/asap_aware_mapping.md): a `ReplacementSubDAG` **is** one candidate; a `TargetSubDAG` with its full `replacements()` list **is** the set of alternatives for one spot in the plan.

**The tradeoff, stated plainly:** because a single-target bind now goes through the same enumeration `SketchFamilyStrategy` uses, it sizes and fully constructs *every* sketch candidate at every sketch-capable node — not just the one a caller keeps — before that caller selects the head. That's strictly more work per bind than a version that only ever computed the preferred candidate, in exchange for there being exactly one place in this crate that decides what an `AggIntent` may become and exactly one place bound output comes from. Recursion into a node's child goes back through the same enumerate-then-select step fresh, so choosing (or forcing) a candidate for one target never leaks into that target's own nested aggregates.

What this is *not*: the design doc's future Cascades/Volcano-style search engine — generate candidates via `ReplacementStrategy` across a *whole plan*, evaluate/select via `CostModel` across whole candidate plans — is a separate, not-yet-built piece of work. What exists today is a **single-node** stand-in for that selection (`cost_model.rank_candidates`, applied one node at a time), not the real thing.

**One exception:** `bind::implement_workload`/`implement_workload_with` still keep the take-the-head step internally, because workload-wide CSE sharing memoizes on `Rc` pointer identity — two workload roots that collapsed onto the same `Rc<QueryExpr>` must resolve to the *same* canonical decision to be shareable at all, so there's no meaningful "N candidates" answer to memoize against. That's the one place inside this crate a single-answer selection still lives; every other caller goes through `SketchFamilyStrategy::replacements()` directly.

### The shared low-level primitive: `bind_with_implementation`

Underneath `SketchFamilyStrategy::replacements()` sits one more function, `bind::bind_with_implementation(expr, implementation, cost_model)` — *given* an already-decided `Implementation` for `expr`'s top intent, bind it into a `SummaryNode` (or fall back to a logical passthrough). It doesn't re-decide how a chosen `Implementation` becomes a `SummaryNode`; `replacements()` just calls it once per candidate:

```text
                    implementations_for_with(intent, cost_model)
                    every valid Implementation, ranked
                                     |
                                     v
                    SketchFamilyStrategy::replacements()
                    keeps every candidate
                                     |
                                     v
                    bind_with_implementation(...)
                    once per candidate
                                     |
                                     v
                    N ReplacementSubDAGs

    a caller wanting one answer: replacements(...).into_iter().next()
```

(`replacements(...)` returns a `Vec<ReplacementSubDAG>` ranked most-preferred-first; `.into_iter().next()` takes just that first entry and drops the rest — the same "keep candidate[0]" step `implement_tree_with` used to do internally, now written out explicitly on the calling side instead of hidden behind a second entry point.)

### Replacement-strategy path

```text
TargetSubDAG
   |
   v
ReplacementStrategy::matches(...)
   |
   v
ReplacementStrategy::replacements(...)
   |
   +---------------------------------------------+
   |                    |                         |
bind_with_implementation(..., candidate A, ...)   ...
   |                    |                         |
   v                    v                         v
candidate A          candidate B              candidate C
```

The important rule is:

> Strategies should reuse existing decision and binding logic where possible instead of reimplementing it.

`SketchFamilyStrategy` follows this literally: it doesn't reimplement any part of binding — it hands `implementations_for_with`'s own list straight to `bind_with_implementation`, once per candidate.

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

The existing `SketchFamilyStrategy` is the model to follow: it reuses `implementation.rs`'s existing candidate list and the existing binder.

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
implementation::implementations_for_with(...)
     |
     v
every ranked Implementation
     |
     v
bind each candidate separately
     |
     v
Vec<ReplacementSubDAG>
```

For an approximate quantile, the current candidate list includes both KLL and DDSketch — the strategy returns both, even though the cost model ranks one ahead of the other. For cases where `implementations_for_with` has only one realization, such as an exact accumulator or pass-through, the strategy returns that single realization. There is no separate per-category dispatch inside the strategy: whatever `implementations_for_with` produces, the strategy binds, one entry at a time.

---

### How each candidate actually gets bound: `bind_with_implementation`

`SketchFamilyStrategy`'s whole `replacements()` body is one loop:

```rust
fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
    let Some(intent) = bindable_intent(target.root) else {
        return Vec::new();
    };
    implementations_for_with(intent, self.cost_model)
        .into_iter()
        .filter_map(|implementation| {
            let rationale = describe_implementation(intent, &implementation);
            let node =
                bind_with_implementation(target.root, implementation, self.cost_model)
                    .ok()?;
            Some(ReplacementSubDAG {
                replacement: Replacement::Summary(node),
                rationale,
            })
        })
        .collect()
}
```

`implementations_for_with` already did the hard part — enumerating and sizing every candidate, ranked. This loop's only job is to hand each one to `bind::bind_with_implementation(expr, implementation, cost_model)` — the shared low-level primitive every caller of this crate bottoms out in, whether it keeps one candidate or all of them (see §3) — and package the result. No per-candidate sizing logic lives here.

Because only `expr`'s own top-level `Implementation` is ever supplied from outside — `bind_with_implementation` recurses into `expr`'s child via a fresh internal selection over that child's own candidates, not a forced one — enumerating a candidate for one target never leaks into that target's own nested aggregates. (An earlier version of this strategy forced ranking via a `CostModel`-wrapping adapter for the whole recursive bind, which had exactly that leak as a latent bug; today's design doesn't have the problem, because nothing below the top node is ever forced.)

This is the pattern to preserve when adding another strategy that needs to enumerate alternatives through an API that normally chooses one: get the exhaustive list from the same enumeration function the single-answer path uses, and hand each entry to the shared low-level binding primitive — never wrap the `CostModel` to trick a ranked-first path into producing what you want.

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
) -> Cost;
```

---

### `cse_shared_maintenance_cost`

Use to estimate the cost of computing and maintaining a shared subtree.

```rust
fn cse_shared_maintenance_cost(
    &self,
    candidate: &CseCandidate,
) -> Cost;
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

## 10. `Matcher` (`implementation.rs`)

There is a third extension point in this crate, much smaller in surface area than `ReplacementStrategy` or `CostModel`, but worth knowing about:

```rust
pub trait Matcher {
    fn is_satisfied_by(&self, required: &Implementation, available: &Implementation) -> bool;
}
```

`Matcher` answers a different question than everything above it: not "how do I *build* a summary for this intent" (that's `implementation`/`bind`/`ReplacementStrategy`), but "is there already a summary sitting around somewhere that answers this without building anything new?" This is the query-optimization-literature "answering queries using views" question, applied to summaries — the same idea as a database reusing an existing materialized view or index instead of recomputing from scratch.

Concrete example: a deployment already has a `DDSketch` built earlier for some other query on `latency`, and a new query needs `Kll` quantiles on `latency`. Can the existing `DDSketch` answer it without building a new `Kll` sketch? A pure sketch-algebra answer says yes — both are quantile sketches, mutually substitutable at the query level. But a deployment with its own storage-layout rules might say no, e.g. if its inventory ties a stored summary to a specific algorithm identity it won't re-interpret. `Matcher::is_satisfied_by(required, available)` is where a deployment plugs in whichever answer is actually true for its own storage layer — `required` is what a query needs (an `Implementation` value that `implementation`/`bind` already computed), `available` is what already exists somewhere in that deployment's inventory.

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

`SketchFamilyStrategy` obtains sketch alternatives through `implementation.rs`'s existing interface:

```rust
summary_candidates(intent)
```

and binds them through the normal binder.

Therefore, when adding a new built-in sketch family, the intended flow is:

```text
1. Teach `implementation.rs` that the sketch is a valid candidate
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

If `implementation.rs` already defines which sketch families satisfy an `AggIntent`, reuse that source.

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
| Add a new built-in sketch candidate | `implementation.rs`'s summary-candidate mapping |
| Prefer one sketch family over another | `CostModel::rank_candidates` |
| Change sketch sizing for an accuracy target | `CostModel::size_params` |
| Add extension-defined implementation behavior | `CostModel::realize_extension` |
| Add extension-defined readout behavior | `CostModel::readout_extension` |
| Change CSE recomputation cost | `CostModel::cse_recompute_cost` |
| Change shared-maintenance cost | `CostModel::cse_shared_maintenance_cost` |
| Change current share/recompute choice | `CostModel::cse_share_decision` |
| Decide whether an available implementation satisfies a required one | `impl Matcher` |
| Produce a normal (ranked-first) bound summary for one target | `SketchFamilyStrategy::replacements(...).into_iter().next()` |
| Bind a whole workload's roots, sharing across CSE-collapsed roots | reuse `bind::implement_workload`/`implement_workload_with` |
| Bind a specific, already-chosen `Implementation` | reuse `bind::bind_with_implementation` |
| Enumerate valid sketch kinds | reuse `implementation::summary_candidates` |
| Build a target with no workload context | `TargetSubDAG::new` |
| Build a target with known sharing context | `TargetSubDAG::with_consumer_count` |
