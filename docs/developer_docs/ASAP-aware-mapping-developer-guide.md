# ASAP-Aware Mapping: Developer Guide

This guide explains how to extend ASAP-aware mapping in the current codebase. Use it when you want to:

- add a new `ReplacementStrategy`,
- add or customize a `CostModel`,
- understand how strategies, binding, and costing interact,
- add a new kind of replacement without duplicating existing planner logic,
- write the tests expected for a new extension.

The focus here is the **current code interfaces and their contracts**. For the higher-level motivation and future search design, see the separate design document, [`docs/design_docs/asap_aware_mapping.md`](../design_docs/asap_aware_mapping.md).

Names such as `MyStrategy`, `MyCostModel`, and `PreferDDSketch` are illustrative; they do not ship with this crate. Samples that use real types—such as `SketchFamilyStrategy`, `SharedSubtreeStrategy`, and `implementations_for_with`—are copied from `replacement.rs`, `bind.rs`, or `cost_model.rs`.

If you only need to find the right extension point, start with the [extension map](#18-current-extension-map). If you are implementing a strategy, read sections 1–5 first.

---

## Terminology

One term is central to this guide:

- **Implementation**: one valid realization of an `AggIntent`. It may be an approximate sketch, an exact mergeable accumulator, or a pass-through with no summary. `implementations_for_with` (a `replacement.rs`-private function) enumerates the valid implementations for one node and ranks them. It does not walk the plan or select a winner. `SketchFamilyStrategy::replacements()` is its only caller: for one target, it constructs every candidate's `SummaryNode` directly and returns all of them — deciding and constructing happen in the same step, not two steps bridged by a separately-named function in another module. A caller that needs one result takes the first candidate. Workload-wide helpers—`implement_workload` and `implement_workload_with`—perform that selection internally so shared `Rc<QueryExpr>` roots receive one consistent decision.

In short: `ReplacementStrategy` enumerates, packages, and binds every candidate, and the caller selects when it needs a single executable answer. Section 3 shows the complete flow.

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

Do not put cost-based pruning into a `ReplacementStrategy`. A strategy must enumerate every valid alternative, even when the default cost model clearly prefers one. See [Rule 2](#rule-2-enumerate-do-not-rank).

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

`consumer_count` counts structural references, not runtime executions. It is the number of places in the workload DAG that point to this exact `Rc<QueryExpr>` node.

For example, consider two top-level queries:

- `sum by (service) (rate(m[5m]))`
- `avg by (service) (rate(m[5m]))`

After `share_common_subtrees` merges their identical `rate(m[5m])` subtrees, both query trees point to the same `Rc`. That node's `consumer_count` is `2`, regardless of how often either query executes.

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

`CostModel` covers every deployment-specific numeric or configuration decision—not only which candidate is cheapest. For example, sketch sizing trades memory and update cost for accuracy, so it belongs here too.

The crate cannot hardcode real deployment costs: `asap-plan` depends on `asap_ir`, not on a runtime or deployment model. Most hooks therefore provide the crate's built-in static behavior as a default. Override only the decisions your deployment needs to change.

| Hook | Use it to | Default? |
|---|---|---|
| `rank_candidates` | Order valid sketch families | No |
| `size_params` | Convert an accuracy target into sketch parameters | Yes |
| `realize_extension` | Map a custom intent to an implementation | Yes |
| `readout_extension` | Query a custom extension summary | Panics until paired with a custom realization |
| `cse_recompute_cost` | Estimate independent recomputation | Yes |
| `cse_shared_maintenance_cost` | Estimate shared maintenance | Yes |
| `cse_share_decision` | Choose sharing or recomputation | Yes |

- **`rank_candidates`** — order the sketch candidates for one `AggIntent`, best first. This is the only required hook.

  ```rust
  fn rank_candidates(&self, intent: &AggIntent, candidates: &[SketchKind]) -> Vec<SketchKind>;
  ```

- **`size_params`** — choose parameters, such as sketch capacity, for an already-selected `SketchKind` and accuracy target `(eps, delta)`. It is separate from ranking so a deployment can customize sizing without changing family selection. The default is `replacement::default_size_params`.

  ```rust
  fn size_params(&self, kind: SketchKind, intent: &AggIntent, eps: f64, delta: f64) -> SketchParams;
  ```

- **`realize_extension`** — map a deployment-defined `AggIntent::Extension` to a post-ASAP `Implementation`. The default is `Implementation::PassThrough`.

  Use `AggIntent::Extension { ext_kind, payload }` for intent shapes that only your deployment needs. Core treats both fields as opaque. For example, a deployment can tag an approximate-frequency intent with `ext_kind: "frequency"` and recognize it in `realize_extension`:

  ```rust
  fn realize_extension(&self, ext_kind: &str, _payload: &serde_json::Value) -> Implementation {
      if ext_kind == "frequency" {
          Implementation::Sketch { kind: SketchKind::CountSketch, params: /* ... */ }
      } else {
          Implementation::PassThrough  // fall back to the default for anything else
      }
  }
  ```

  Return `Implementation::PassThrough` for unrecognized extension kinds. Do not panic.

  ```rust
  fn realize_extension(&self, ext_kind: &str, payload: &serde_json::Value) -> Implementation;
  ```

- **`readout_extension`** — define how queries read an extension summary that `realize_extension` mapped to a `Sketch`. The two hooks are a pair: realization defines what is maintained; readout defines how it is queried. Override both for the same `ext_kind`. The default readout panics to prevent a silent wrong answer.

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

  Both hooks return `Cost`, currently a unitless `f64` newtype. The wrapper allows the type to grow later—for example, to separate CPU, memory, and network cost—without changing every hook signature.

- **`cse_share_decision`** — choose between one shared summary and independent recomputation at each consumer. By default, it shares when maintenance cost is no greater than total recomputation cost. Override the two cost inputs first; override this decision hook only when you need a different policy.

  ```rust
  fn cse_share_decision(&self, candidate: &CseCandidate) -> ShareDecision;
  ```

A custom cost model does not necessarily need to override every hook. The current tests include a model that overrides only `rank_candidates`, relying on defaults for the rest.

---

## 3. How the current pieces fit together

The crate has one source of truth for valid implementations, and deciding and constructing a candidate happen in the same step — not two steps bridged by a separately-named function:

- `SketchFamilyStrategy::replacements(target)`, for a bindable `Aggregate`, calls `implementations_for_with(intent, cost_model)` (a `replacement.rs`-private function) to get every valid `Implementation`, ranked by preference and already sized to the target's own accuracy target — then, for each candidate, constructs its bound `SummaryNode` directly (child schema, summarized column, readout, recursion into the child) and returns it as a `ReplacementSubDAG`. It does not discard any candidate.
- A caller that needs one executable answer uses `.into_iter().next()` and handles the empty case. The crate's conservative fallback is `bind::keep_pre_asap`.

In the [design document](../design_docs/asap_aware_mapping.md), each `ReplacementSubDAG` is a candidate. The complete `replacements()` result is the set of alternatives for one location in the plan.

**Tradeoff:** a single-target bind sizes and constructs every valid sketch candidate before the caller keeps the first one. This costs more than constructing only the preferred candidate, but it means there is exactly one place in the crate that decides what an `AggIntent` may become and exactly one place bound output comes from. Child nodes repeat selection independently (via `bind::select_and_bind`), so a choice at one target does not force choices in nested aggregates.

This is not yet the whole-plan Cascades/Volcano-style search described in the design document. Today, `cost_model.rank_candidates` ranks one node at a time. Whole-plan candidate generation and selection remain future work.

**One exception:** `bind::implement_workload` and `implement_workload_with` select the first candidate internally. Workload-wide CSE memoizes by `Rc` pointer identity, so roots that share an `Rc<QueryExpr>` must use the same canonical decision. Other callers use `SketchFamilyStrategy::replacements()` directly.

### Replacement-strategy path

```text
TargetSubDAG
   |
   v
ReplacementStrategy::matches(...)
   |
   v
ReplacementStrategy::replacements(...)
   | (SketchFamilyStrategy: decide via implementations_for_with,
   |  then construct each candidate's SummaryNode, in one method)
   +---------------------------------------------+
   |                    |                         |
candidate A          candidate B              candidate C
```

The important rule is:

> Strategies should reuse existing decision and binding logic where possible instead of reimplementing it.

`SketchFamilyStrategy` follows this literally: `implementations_for_with` is the single source of truth for what an `AggIntent` may become, and every candidate it produces gets constructed the same way.

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
implementations_for_with(...)
     |
     v
every ranked Implementation
     |
     v
construct each candidate's SummaryNode separately
     |
     v
Vec<ReplacementSubDAG>
```

For an approximate quantile, the current candidate list includes both KLL and DDSketch — the strategy returns both, even though the cost model ranks one ahead of the other. For cases where `implementations_for_with` has only one realization, such as an exact accumulator or pass-through, the strategy returns that single realization. There is no separate per-category dispatch inside the strategy: whatever `implementations_for_with` produces, the strategy constructs, one entry at a time.

---

### How each candidate actually gets constructed

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
                construct_summary(target.root, implementation, self.cost_model)
                    .ok()?;
            Some(ReplacementSubDAG {
                replacement: Replacement::Summary(node),
                rationale,
            })
        })
        .collect()
}
```

`implementations_for_with` already did the hard part — enumerating and sizing every candidate, ranked. This loop's only job is to hand each one to `construct_summary(expr, implementation, cost_model)` — a private helper in the same file, not a separately-named public bridge in a different module — which derives the child schema, resolves the summarized column, builds the readout, recurses into the child, and assembles the `SummaryNode`. No per-candidate sizing logic lives here: sizing already happened inside `implementations_for_with`.

Because only `expr`'s own top-level `Implementation` is ever supplied from outside — `construct_summary` recurses into `expr`'s child via `bind::select_and_bind`, a fresh internal selection over that child's own candidates, not a forced one — enumerating a candidate for one target never leaks into that target's own nested aggregates. (An earlier version of this strategy forced ranking via a `CostModel`-wrapping adapter for the whole recursive bind, which had exactly that leak as a latent bug; today's design doesn't have the problem, because nothing below the top node is ever forced.)

This is the pattern to preserve when adding another strategy that needs to enumerate alternatives through an API that normally chooses one: get the exhaustive list from the same enumeration function the single-answer path uses, and construct each entry directly — never wrap the `CostModel` to trick a ranked-first path into producing what you want.

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

That choice belongs to the cost model. The production binding path calls `CostModel::cse_share_decision` when it encounters a shared subtree. The strategy still returns both alternatives because enumeration and selection are separate steps:

- `consumer_count >= 2` means `share_common_subtrees` has already merged the expression into one shared `Rc`. The shared alternative is therefore an `Rc::clone`; the independent alternative requires a deep clone.
- `cse_share_decision` is used by the production binding path, not by `SharedSubtreeStrategy`.
- The strategy must return both valid alternatives even if the current cost model strongly prefers one. A future whole-plan search may choose differently from today's local comparison.

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

`Matcher` is a smaller, separate extension point:

```rust
pub trait Matcher {
    fn is_satisfied_by(&self, required: &Implementation, available: &Implementation) -> bool;
}
```

`Matcher` does not decide how to build a summary. It asks whether an existing summary can satisfy a required implementation without building anything new. This is similar to a database reusing a materialized view or index.

For example, suppose a deployment already has a `DDSketch` for `latency`, while a new query requests KLL quantiles. Sketch algebra may allow substitution because both answer quantile queries. A deployment's storage rules may forbid it because stored summaries retain a specific algorithm identity. `Matcher::is_satisfied_by(required, available)` lets the deployment make that decision: `required` is what the query needs, and `available` is what the inventory already contains.

The crate provides no default `Matcher` implementation because the answer depends on deployment-specific inventory and storage rules. If you need one, implement the complete trait for your deployment.

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
| Enumerate valid sketch kinds | reuse `replacement::summary_candidates` |
| Build a target with no workload context | `TargetSubDAG::new` |
| Build a target with known sharing context | `TargetSubDAG::with_consumer_count` |
