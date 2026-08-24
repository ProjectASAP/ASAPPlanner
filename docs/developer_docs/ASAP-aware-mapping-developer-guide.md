# ASAP-Aware Mapping: Developer Guide

This guide explains how to extend ASAP-aware mapping in the current codebase. Use it when you want to:

- add a new `ReplacementStrategy`,
- add or customize a `CostModel`,
- understand how strategies, binding, and costing interact,
- add a new kind of replacement without duplicating existing planner logic,
- write the tests expected for a new extension.

The focus here is the **current code interfaces and their contracts**. For the higher-level motivation and replacement-plan-search design, see the separate design document, [`docs/design_docs/asap_aware_mapping.md`](../design_docs/asap_aware_mapping.md).

Names such as `MyStrategy`, `MyCostModel`, and `PreferDDSketch` are illustrative; they do not ship with this crate. Samples that use real types—such as `SketchAlgorithmStrategy`, `SharedSubtreeStrategy`, `implementations_for_with`, `PlanSpace`, and `search_workload`—are copied from `replacement.rs`, `explanation.rs`, or `cost_model.rs`.

If you only need to find the right extension point, start with the [extension map](#7-current-extension-map) (Part 3 §7). If you are implementing a strategy, read Part 1 §1 (Mental model), Part 2 §1 (Glossary), and Part 3 §1 (Adding a new `ReplacementStrategy`) first.

---

# Part 1 — Code Architecture

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

## 2. Architecture overview

The diagram below follows one query—or a whole workload—from `TargetSubDAG` discovery through candidate generation, ranking, reporting, and downstream visualization. Section 3 focuses on the replacement-strategy path.

```mermaid
flowchart TB
  classDef input fill:#e8f1ff,stroke:#4b78b8,color:#172b4d
  classDef generate fill:#e7f7ef,stroke:#31835e,color:#173f2d
  classDef store fill:#fff6dd,stroke:#b78922,color:#513d0c
  classDef choose fill:#fcebdc,stroke:#c46a25,color:#572d0c
  classDef report fill:#f2eafe,stroke:#7950b3,color:#34204f

  SQ["One QueryExpr<br/>single-query API or child recursion"]:::input
  WL["Whole workload<br/>Vec&lt;(Id, Rc&lt;QueryExpr&gt;)&gt;"]:::input
  SQ --> NEW["TargetSubDAG::new<br/>consumer_count = 1"]:::generate
  WL --> SEARCH["search_workload_with<br/>CSE + whole-DAG discovery"]:::generate
  NEW --> TARGET[TargetSubDAG]:::generate
  SEARCH --> TARGET

  TARGET --> SKETCH["SketchAlgorithmStrategy<br/>construct every valid SummaryNode"]:::generate
  TARGET --> SHARED["SharedSubtreeStrategy<br/>share vs. recompute"]:::generate
  CM([CostModel]):::choose -. "rank + size" .-> SKETCH
  SKETCH --> CAND["Vec&lt;ReplacementSubDAG&gt;<br/>Summary or Rewrite; nothing pruned"]:::store
  SHARED --> CAND
  CAND --> MEMO["PlanSpace / MemoGroup<br/>all targets, all candidates"]:::store

  MEMO --> SORT[PlanSpace::cost_sorted]:::choose
  SORT --> RANK[rank_candidates]:::choose
  SORT --> CSE[cse_share_decision]:::choose
  SORT --> COST[estimate_cost]:::choose
  RANK --> GROUP["RankedGroup<br/>best-first candidates + aligned costs"]:::choose
  CSE --> GROUP
  COST --> GROUP

  MEMO --> EXPLAIN["explain_replacements<br/>kind + location + rationale + node_hash"]:::report
  EXPLAIN --> EXPORT[dag_export]:::report
  EXPORT --> VIEWER[dag-viewer]:::report
```

---

## 3. How the current pieces fit together

The crate has one source of truth for valid implementations, and deciding and constructing a candidate happen in the same step — not two steps bridged by a separately-named function:

- `SketchAlgorithmStrategy::replacements(target)`, for a bindable `Aggregate`, calls `implementations_for_with(intent, cost_model)` (a `replacement.rs`-private function) to get every valid `Implementation`, ranked by preference and already sized to the target's own accuracy target — then, for each candidate, constructs its bound `SummaryNode` directly (child schema, summarized column, readout, recursion into the child) and returns it as a `ReplacementSubDAG`. It does not discard any candidate.
- A caller that needs one executable answer uses `.into_iter().next()` and handles the empty case. The crate's conservative fallback is `replacement::keep_pre_asap`.

In the [design document](../design_docs/asap_aware_mapping.md), each `ReplacementSubDAG` is a candidate. The complete `replacements()` result is the set of alternatives for one location in the plan.

**Tradeoff:** a single-target bind sizes and constructs every valid sketch candidate before the caller keeps the first one. This costs more than constructing only the preferred candidate, but it means there is exactly one place in the crate that decides what an `AggIntent` may become and exactly one place bound output comes from. Child nodes repeat selection independently (via `replacement::realize_child`), so a choice at one target does not force choices in nested aggregates.

For workload-wide search, `replacement::search_workload`/`search_workload_with` discover every candidate `TargetSubDAG`, run every registered `ReplacementStrategy` against each target to a fixpoint, and deduplicate the results into a `PlanSpace`. Each distinct target has one `MemoGroup` containing every discovered alternative. `PlanSpace::cost_sorted` ranks those candidates with the supplied cost model. This MEMO representation avoids materializing a flat list of `2^N` complete plans while preserving every independent choice.

**This crate does not select or materialize one final answer for a workload.** Callers receive every candidate, ranked with cost, from `search_workload`/`search_workload_with` and make the final choice using deployment information such as placement. The `.into_iter().next()` helpers in `replacement.rs` are narrow implementation details: `realize_child` selects a realization while recursively constructing one candidate, and `realize_one` obtains a representative bound node for a cost comparison. Neither chooses a final workload plan.

### Replacement-strategy path

A `TargetSubDAG` comes from one of two entry points. The chosen path determines whether `consumer_count` is assumed to be one or discovered across a workload, which directly controls strategies such as `SharedSubtreeStrategy`:

```mermaid
flowchart LR
  classDef direct fill:#e8f1ff,stroke:#4b78b8,color:#172b4d
  classDef workload fill:#e7f7ef,stroke:#31835e,color:#173f2d
  classDef common fill:#fff6dd,stroke:#b78922,color:#513d0c

  subgraph D[Direct, single-node path]
    Q["QueryExpr root or recursive child"]:::direct
    Q --> N["TargetSubDAG::new<br/>consumer_count = 1"]:::direct
  end

  subgraph W[Workload discovery path]
    ROOTS["Vec&lt;(Id, Rc&lt;QueryExpr&gt;)&gt;"]:::workload
    ROOTS --> CSE["Run CSE once"]:::workload
    CSE --> WALK["Walk every root's whole DAG"]:::workload
    WALK --> T["TargetSubDAG per node<br/>real consumer_count"]:::workload
  end

  N --> MATCH[ReplacementStrategy::matches]:::common
  T --> MATCH
  MATCH --> REPLACE[ReplacementStrategy::replacements]:::common
  REPLACE --> A[Candidate A]:::common
  REPLACE --> B[Candidate B]:::common
  REPLACE --> C[Candidate C]:::common
```

The left path is what a caller with one query in hand uses directly (see `docs/user-guide/user-guide.md`'s "Step 2"), and what `realize_child` uses internally to bind a candidate's own child. The right path is `search_workload`/`search_workload_with` — it's the only place `consumer_count` is ever discovered rather than assumed, which is why `SharedSubtreeStrategy` only meaningfully matches something reached through it.

The important rule is:

> Strategies should reuse existing decision and binding logic where possible instead of reimplementing it.

`SketchAlgorithmStrategy` follows this literally: `implementations_for_with` is the single source of truth for what an `AggIntent` may become, and every candidate it produces gets constructed the same way.

---

# Part 2 — Interfaces and Definitions

## 1. Glossary

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

### `Implementation`

One valid realization of an `AggIntent`. It may be an approximate sketch, an exact mergeable accumulator, or a pass-through with no summary. `implementations_for_with` (a `replacement.rs`-private function) enumerates the valid implementations for one node and ranks them. It does not walk the plan or select a winner. `SketchAlgorithmStrategy::replacements()` is its only caller: for one target, it constructs every candidate's `SummaryNode` directly and returns all of them — deciding and constructing happen in the same step, not two steps bridged by a separately-named function in another module. A caller that needs one result takes the first candidate. At workload scale, `search_workload`/`search_workload_with` extend the same "never prune" contract across every `TargetSubDAG` in the whole workload (see Part 1 §3, "How the current pieces fit together") — this crate stops at that full candidate space plus cost; it does not itself commit to one final, physically-shared answer per site. That commitment (which candidate to build, where to place it) is a downstream deployment's call, not this crate's.

In short: `ReplacementStrategy` enumerates, packages, and binds every candidate, and the caller selects when it needs a single executable answer. Part 1 §3 shows the complete flow.

---

### `CostModel`

`CostModel` covers every deployment-specific numeric or configuration decision—not only which candidate is cheapest. For example, sketch sizing trades memory and update cost for accuracy, so it belongs here too.

The crate cannot hardcode real deployment costs: `asap-aware-mapping` depends on `asap-types`, not on a runtime or deployment model. Most hooks therefore provide the crate's built-in static behavior as a default. Override only the decisions your deployment needs to change.

| Hook | Use it to | Default? |
|---|---|---|
| `rank_candidates` | Order valid sketch algorithms | No |
| `size_params` | Convert an accuracy target into sketch parameters | Yes |
| `realize_extension` | Map a custom intent to an implementation | Yes |
| `readout_extension` | Query a custom extension summary | Panics until paired with a custom realization |
| `cse_recompute_cost` | Estimate independent recomputation | Yes |
| `cse_shared_maintenance_cost` | Estimate shared maintenance | Yes |
| `cse_share_decision` | Choose sharing or recomputation | Yes |
| `estimate_cost` | Attach a comparable numeric cost to a replacement | Returns `NaN`; `DefaultCostModel` provides real values |

- **`rank_candidates`** — order the sketch candidates for one `AggIntent`, best first. This is the only required hook.

  ```rust
  fn rank_candidates(&self, intent: &AggIntent, candidates: &[SketchAlgorithm]) -> Vec<SketchAlgorithm>;
  ```

- **`size_params`** — choose parameters, such as sketch capacity, for an already-selected `SketchAlgorithm` and accuracy target `(eps, delta)`. It is separate from ranking so a deployment can customize sizing without changing family selection. The default is `replacement::default_size_params`.

  ```rust
  fn size_params(&self, kind: SketchAlgorithm, intent: &AggIntent, eps: f64, delta: f64) -> SketchParams;
  ```

- **`realize_extension`** — map a deployment-defined `AggIntent::Extension` to a post-ASAP `Implementation`. The default is `Implementation::PassThrough`.

  Use `AggIntent::Extension { ext_kind, payload }` for intent shapes that only your deployment needs. Core treats both fields as opaque. For example, a deployment can tag an approximate-frequency intent with `ext_kind: "frequency"` and recognize it in `realize_extension`:

  ```rust
  fn realize_extension(&self, ext_kind: &str, _payload: &serde_json::Value) -> Implementation {
      if ext_kind == "frequency" {
          Implementation::Sketch(SketchKind::Frequency(SketchAlgorithm::CountSketch, /* params */))
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

  Both hooks return `Cost`, currently a unitless `f64` newtype. The wrapper allows the type to grow later—for example, to separate CPU, memory, and network costs—without changing every hook signature.

- **`cse_share_decision`** — choose between one shared summary and independent recomputation at each consumer. By default, it shares when maintenance cost is no greater than total recomputation cost. Override the two cost inputs first; override this decision hook only when you need a different policy.

  ```rust
  fn cse_share_decision(&self, candidate: &CseCandidate) -> ShareDecision;
  ```

- **`estimate_cost`** — attach a comparable numeric cost to an already-constructed replacement. `PlanSpace::cost_sorted` calls it for every candidate and keeps the returned values aligned with the ranked candidates. The trait default returns `f64::NAN` deliberately; override it when a custom model's callers need displayable or otherwise consumable numeric costs. `DefaultCostModel` provides real values derived from its CSE cost hooks.

  ```rust
  fn estimate_cost(
      &self,
      candidate: &ReplacementSubDAG,
      target: &TargetSubDAG<'_>,
  ) -> f64;
  ```

A custom cost model does not necessarily need to override every hook. The current tests include a model that overrides only `rank_candidates`, relying on defaults for the rest. Such a minimal model inherits `estimate_cost`'s `NaN` placeholder; it must also override `estimate_cost` if consumers require numeric costs.

---

### `PlanSpace` / `MemoGroup` / `RankedGroup` — the whole-workload view

`ReplacementStrategy` answers "what are the candidates for this one target?" `PlanSpace` answers the same question for every target in a whole workload at once, without materializing `2^N` fully-copied plans for `N` independently-choosable sites.

```rust
// replacement.rs

// One MemoGroup per distinct TargetSubDAG in the whole workload —
// never a flat list of fully-materialized plans.
pub struct MemoGroup {
    pub target: Rc<QueryExpr>,
    pub consumer_count: usize,
    pub candidates: Vec<ReplacementSubDAG>,  // every alternative, unranked
}

pub struct RankedGroup<'a> {
    pub target: &'a Rc<QueryExpr>,
    pub consumer_count: usize,
    pub candidates: Vec<&'a ReplacementSubDAG>,  // same candidates, ranked
    pub costs: Vec<f64>,                         // costs[i] <-> candidates[i]
}
```

`search_workload(roots)` runs the shared-subtree pass once, discovers every target across every root's whole DAG (not just root-level sharing — a `SharedSubtreeStrategy` candidate three levels under an unshared `Filter` is exactly as real a site as a shared whole root), and asks every registered strategy to a fixpoint. Two logically different candidates at two different targets are never copied into two separate plans — they're two entries in two different `MemoGroup`s, sharing every other node in the workload by construction.

`PlanSpace::cost_sorted(cost_model)` is the one ranking step: for each group, it dispatches by candidate shape — a same-shape `Rewrite` pair (a `SharedSubtreeStrategy` share/recompute choice) goes through `CostModel::cse_share_decision`; a same-shape run of `Summary` candidates realizing sketches (a `SketchAlgorithmStrategy` choice) goes through `CostModel::rank_candidates`; every candidate also gets a real number from `CostModel::estimate_cost`, aligned index-for-index in `costs`. Groups whose candidates don't fit either shape (a lone candidate, or a mix — e.g. one target where both strategies fired at once) keep discovery order for the un-rankable part. Count in, count out — nothing is ever dropped to produce a ranking.

---

### Family, kind, and algorithm

Three levels sit below "summary" in this crate's type vocabulary, and `Sketch` is the only family with all three:

| Level | Type | Example |
| --- | --- | --- |
| **family** | `SummaryFamilyType` | `Sketch`, `Sample`, `Wavelet`, `StatModel`, `ExactAggregate` |
| **kind** | `SketchKind` (only inside `Sketch`) | `Quantile`, `Cardinality`, `Frequency`, `TopK` |
| **algorithm** | `SketchAlgorithm` (nested inside a `SketchKind`) | `Kll` / `DDSketch` (both `Quantile`); `Hll` / `Theta` / `Kmv` (all `Cardinality`) |

A `SketchKind` isn't just a category tag — every value already carries the committed algorithm and its params:

```rust
pub enum SketchKind {
    Quantile(SketchAlgorithm, SketchParams),
    Cardinality(SketchAlgorithm, SketchParams),
    Frequency(SketchAlgorithm, SketchParams),
    TopK(SketchAlgorithm, SketchParams),
}
```

`SketchKind::new(algorithm, params)` is the one place an `(algorithm, params)` pair gets classified into its category — construct through it rather than naming a variant directly, so a new algorithm can't drift out of sync with its category. `.algorithm()`/`.params()` pull the committed pair back out regardless of which category variant it's in.

Where this matters in practice: `CostModel::rank_candidates`/`size_params`, `replacement.rs`'s private `implementations_for_with`, and `SketchAlgorithmStrategy`'s candidate enumeration all operate one level down, at **algorithm** — `summary_candidates(intent)` returns a list of `SketchAlgorithm`s (`[Kll, DDSketch]` for a `Quantile` intent), never a bare `SketchKind` with nothing chosen underneath it. `SketchKind` only shows up once an algorithm has actually been picked and sized — on `Implementation::Sketch(SketchKind)` and `SummaryFamilyType::Sketch(SketchKind)`, both single-field wrappers around the already-committed kind.

`Sample`, `Wavelet`, and `StatModel` each use a flat `(Kind, Params)` pair. `Sketch` needs the additional algorithm level because multiple algorithms can serve the same purpose—for example, KLL and DDSketch both answer quantile queries.

---

### `Matcher`

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

## 2. Replacement explanations (`explanation.rs`)

`explanation::explain_replacements`/`explain_replacements_with` answer a different question than everything above: not "what could this target become" (`ReplacementStrategy::replacements`) but "why does the replacement already discovered for this target exist, and where." It is a **reporting view over `PlanSpace`**, not a second search or a second rule engine — this crate's *explanation of a replacement*, not an applicability classifier deciding admissibility from scratch.

### The rule

> A `TargetSubDAG` is worth explaining exactly when its `PlanSpace` candidate list contains something beyond the trivial, no-op realization.

Concretely, `explanation.rs` reads two shapes off each `MemoGroup`:

- `ExplanationKind::SketchApproximation` — the group's candidates include a `Replacement::Summary` that actually realizes `SummaryFamilyType::Sketch(..)`, i.e. `SketchAlgorithmStrategy` found a real sketch alternative, not just an exact/pass-through candidate.
- `ExplanationKind::CommonSubexpressionReuse` — `consumer_count >= 2` and the group's candidates include `SharedSubtreeStrategy`'s "build once and share" candidate (the `Replacement::Rewrite` whose `Rc` is the group's own `target`).

Each `ReplacementExplanation::reason` is copied verbatim from the matching candidate's own `ReplacementSubDAG::rationale`. Nothing in `explanation.rs` re-explains why a candidate is valid; that explanation already exists exactly once, on the candidate itself.

`ReplacementExplanation` also carries `node_hash: u64` — `asap_types::pre_asap::cse::structural_hash` of the `TargetSubDAG`'s own `target` subtree, the identical function (and identical `Rc<QueryExpr>` input shape) `asap_types::dag_export::DagNode::hash` is computed with. A downstream consumer that independently exported the same `QueryExpr` (e.g. `tools/dag-viewer`'s `dag_export` devtools binary) can match an explanation to the exact `DagNode` it's about by comparing hashes, with no string-matching or path-guessing against `location` required — see `crates/devtools/src/bin/dag_export.rs` for the reference consumer.

### Why there is no `ExplanationRule` trait

Explanations are derived from candidates already present in `PlanSpace`. A new candidate kind therefore requires an `impl ReplacementStrategy` wired into `default_strategies`/`default_strategies_with`; a second explanation-specific trait would duplicate registration and could drift from the actual search space. Custom callers supply strategies through `explain_replacements_with`, using the same extension point exposed by `search_workload_with`.

### How it derives `location` text

`PlanSpace`/`MemoGroup` track `Rc<QueryExpr>` pointer identity, not human-readable breadcrumbs. `explanation.rs` keeps one small, self-contained traversal, `collect_locations`, whose only job is turning "this `Rc`" into prose like `root "dash_a" > lhs` for `ReplacementExplanation::location`. It makes no explanation decision — it runs identically regardless of what any strategy found.

---

# Part 3 — How to Add X, Y, Z

## 1. Adding a new `ReplacementStrategy`

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

### Define the target shape

`matches` should contain the minimum structural and semantic checks needed to determine whether the strategy applies.

For example, `SketchAlgorithmStrategy` only matches the aggregate shape that the existing binder can actually bind:

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

It should answer *whether the strategy applies here* in the plain English sense — not `explanation.rs`'s formal `ReplacementExplanation` concept (issue #257, a separate reporting layer over this trait's own output — see Part 2 §2). `matches` isn't that machinery and doesn't need to produce anything it consumes; it should also not perform ranking or choose a winner.

---

### Enumerate every valid alternative

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

### Choose `Summary` vs. `Rewrite`

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

### Add a rationale

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

### Strategy contract

Every new strategy should follow these rules.

#### Rule 1: `matches == false` should be safe

The existing strategies return an empty vector when `replacements` is called on a target they do not match.

Follow the same convention:

```rust
if !self.matches(target) {
    return Vec::new();
}
```

Do not panic simply because the caller skipped a prior `matches` call.

---

#### Rule 2: enumerate; do not rank

A strategy owns **legality and enumeration**.

A cost model owns **preference and costing**.

This separation is the most important extension rule in this module.

---

#### Rule 3: do not duplicate an existing decision procedure

If another module already knows how to determine whether something is legal or how to bind it, wrap that logic.

Do not create a second implementation of the same semantics inside the strategy.

The existing `SketchAlgorithmStrategy` is the model to follow: it reuses `implementation.rs`'s existing candidate list and the existing binder.

---

#### Rule 4: preserve semantics

Every returned replacement must be semantically valid for the target.

Cost differences do not justify semantic differences.

If a transformation is only valid under additional summary properties, grouping assumptions, or accuracy constraints, check those conditions before returning the candidate.

---

#### Rule 5: a strategy does not need to discover the whole workload

`TargetSubDAG` is passed into the strategy.

The strategy is responsible for deciding what to do with that target, not for walking every workload root to discover targets.

If your transformation requires context not currently represented in `TargetSubDAG`, that is a design question about target metadata or search/discovery — not a reason to hide a second workload traversal inside `replacements`.

---

### Example: current `SketchAlgorithmStrategy`

`SketchAlgorithmStrategy` is the reference implementation for a strategy that produces bound summaries.

Construction:

```rust
let strategy =
    SketchAlgorithmStrategy::default_cost_model();
```

or with a custom cost model:

```rust
let model = MyCostModel; // illustrative
let strategy = SketchAlgorithmStrategy::new(&model);
```

The strategy matches bindable aggregate nodes.

At a high level:

```mermaid
flowchart LR
  A[Target Aggregate] --> B[Extract AggIntent]
  B --> C[implementations_for_with]
  C --> D[Every ranked Implementation]
  D --> E[Construct each SummaryNode separately]
  E --> F["Vec&lt;ReplacementSubDAG&gt;"]
```

For an approximate quantile, the current candidate list includes both KLL and DDSketch — the strategy returns both, even though the cost model ranks one ahead of the other. For cases where `implementations_for_with` has only one realization, such as an exact accumulator or pass-through, the strategy returns that single realization. There is no separate per-category dispatch inside the strategy: whatever `implementations_for_with` produces, the strategy constructs, one entry at a time.

---

#### How each candidate actually gets constructed

`SketchAlgorithmStrategy`'s whole `replacements()` body is one loop:

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

`implementations_for_with` enumerates, sizes, and ranks every candidate. The loop passes each candidate to the private `construct_summary(expr, implementation, cost_model)` helper, which derives the child schema, resolves the summarized column, builds the readout, recurses into the child, and assembles the `SummaryNode`. Candidate sizing remains in `implementations_for_with`.

Only `expr`'s top-level `Implementation` is supplied to `construct_summary`. Child recursion goes through `replacement::realize_child`, which performs a fresh selection over the child's own candidates. A choice for one target therefore does not constrain implementations chosen for nested aggregates.

This is the pattern to preserve when adding another strategy that needs to enumerate alternatives through an API that normally chooses one: get the exhaustive list from the same enumeration function the single-answer path uses, and construct each entry directly — never wrap the `CostModel` to trick a ranked-first path into producing what you want.

---

### Example: current `SharedSubtreeStrategy`

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

### Using a strategy

The basic calling pattern is:

```rust
let target = TargetSubDAG::new(&root);
let strategy =
    SketchAlgorithmStrategy::default_cost_model();

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

### Testing a new strategy

Every new strategy should have focused tests for its contract.

At minimum, test the following.

#### Applicability

A matching target should satisfy:

```rust
assert!(strategy.matches(&target));
```

A non-matching target should satisfy:

```rust
assert!(!strategy.matches(&target));
```

---

#### Safe non-match behavior

Also call `replacements` on a non-matching target:

```rust
assert!(
    strategy.replacements(&target).is_empty()
);
```

This verifies that the strategy does not depend on callers always invoking `matches` first.

---

#### Exhaustive candidate enumeration

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

#### Rationale

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

#### Structural semantics

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

#### Custom cost model behavior

If a strategy accepts a cost model, verify that a custom model changes the intended costing behavior without changing the exhaustive candidate set.

The current sketch strategy does exactly this:

```mermaid
flowchart LR
  MODEL[Custom model prefers DDSketch] --> ORDER["Rank: DDSketch, KLL"]
  ORDER --> RESULT["Strategy returns both<br/>DDSketch + KLL"]
```

That is the expected separation between enumeration and ranking.

---

## 2. Adding or customizing a `CostModel`

### Adding a custom `CostModel`

Use a custom `CostModel` when you want to change preferences or cost assumptions without changing transformation legality.

A minimal model can override only the hook it cares about.

For example (illustrative — `PreferDDSketch` is not a real type in this crate):

```rust
struct PreferDDSketch;

impl CostModel for PreferDDSketch {
    fn rank_candidates(
        &self,
        _intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm> {
        let mut ranked = candidates.to_vec();

        if let Some(pos) =
            ranked.iter().position(
                |k| *k == SketchAlgorithm::DDSketch
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
    SketchAlgorithmStrategy::new(&model);

let replacements =
    strategy.replacements(&target);
```

Important: changing `rank_candidates` changes the preferred ordering, but `SketchAlgorithmStrategy` still enumerates every valid sketch candidate.

A custom cost model should not change which alternatives are semantically legal.

---

### Which `CostModel` hook should I implement?

Use this as a practical guide.

#### `rank_candidates`

Use when you want to change the preference among valid sketch algorithms.

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
    candidates: &[SketchAlgorithm],
) -> Vec<SketchAlgorithm>;
```

The returned vector should rank candidates from most to least preferred.

It must return a permutation of the supplied candidates: every input candidate exactly once, with no additions or removals. Planner call sites enforce this contract and panic if a cost model violates it.

---

#### `size_params`

Use when the sketch algorithm is already known and you want to choose its parameters from an accuracy target.

Signature:

```rust
fn size_params(
    &self,
    kind: SketchAlgorithm,
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

```mermaid
flowchart LR
  ALG[SketchAlgorithm] --> SIZE[CostModel::size_params]
  INTENT[AggIntent] --> SIZE
  ACC[Accuracy target] --> SIZE
  SIZE --> PARAMS[SketchParams]
```

---

#### `realize_extension`

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

#### `readout_extension`

Use when an extension-defined summary also needs custom query/readout behavior.

```rust
fn readout_extension(
    &self,
    ext_kind: &str,
    payload: &serde_json::Value,
    col: &ColumnRef,
) -> SketchQuery;
```

This complements `realize_extension`: realization defines what gets maintained; readout defines how it is queried (see the definition in Part 2 §1's `CostModel` glossary entry if "readout" is unfamiliar).

---

#### `cse_recompute_cost`

Use to estimate the cost of computing a common subtree independently at each consumer.

```rust
fn cse_recompute_cost(
    &self,
    candidate: &CseCandidate,
) -> Cost;
```

---

#### `cse_shared_maintenance_cost`

Use to estimate the cost of computing and maintaining a shared subtree.

```rust
fn cse_shared_maintenance_cost(
    &self,
    candidate: &CseCandidate,
) -> Cost;
```

---

#### `cse_share_decision`

Use when the current binding/planning path needs the final share-vs.-recompute decision.

```rust
fn cse_share_decision(
    &self,
    candidate: &CseCandidate,
) -> ShareDecision;
```

The replacement-strategy layer should still expose both valid alternatives where appropriate. This hook is the cost-sensitive decision point for code paths that need to commit to one answer.

---

#### `estimate_cost`

Use when callers need a comparable numeric cost for each replacement, in addition to relative ordering.

```rust
fn estimate_cost(
    &self,
    candidate: &ReplacementSubDAG,
    target: &TargetSubDAG<'_>,
) -> f64;
```

The default returns `f64::NAN`, making the absence of a numeric model explicit. Override this hook when passing the model to `PlanSpace::cost_sorted` if downstream code displays or otherwise consumes the `costs` values. Prefer to derive the result from the same inputs used by `rank_candidates` and the CSE cost hooks so numeric costs do not disagree with relative ordering.

---

### Testing a new cost model

A cost-model test should focus on the hook being customized.

For ranking:

```rust
let ranked =
    model.rank_candidates(
        &intent,
        &[SketchAlgorithm::Kll,
          SketchAlgorithm::DDSketch],
    );

assert_eq!(
    ranked[0],
    SketchAlgorithm::DDSketch
);
```

Then test integration through a consumer of the cost model.

For example:

```rust
let strategy =
    SketchAlgorithmStrategy::new(&model);

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

## 3. Adding a new sketch algorithm

A new sketch algorithm generally touches more than `ReplacementStrategy`.

The strategy should not maintain its own private list of sketch algorithms.

`SketchAlgorithmStrategy` obtains sketch alternatives through `replacement.rs`'s existing interface:

```rust
summary_candidates(intent)
```

and constructs them through the normal construction path.

Therefore, when adding a new built-in sketch algorithm, the intended flow is:

```mermaid
flowchart LR
  MAP["Add algorithm to summary_candidates<br/>for the relevant AggIntent"]
  MAP --> MODEL["Define CostModel ranking,<br/>sizing, and numeric cost"]
  MODEL --> BUILD["Teach construct_summary<br/>to realize the algorithm"]
  BUILD --> ENUM["SketchAlgorithmStrategy<br/>enumerates it automatically"]
```

This keeps one source of truth for sketch applicability.

Do not special-case the new sketch inside `SketchAlgorithmStrategy` unless the strategy itself needs fundamentally new behavior.

### Verifying a new sketch algorithm

After wiring the new algorithm into `summary_candidates` and giving the cost model a real `rank_candidates`/`size_params` opinion about it, check two things. First, that `SketchAlgorithmStrategy::replacements()` for a matching `TargetSubDAG` actually includes a candidate realizing the new algorithm — extend a test shaped like `replacement.rs`'s own test-module coverage-matrix tests (e.g. `agg_intent_to_summary_kind_coverage_matrix`) to cover the new algorithm's `AggIntent`. Second, that `cost_sorted`/`estimate_cost` produce sane, comparable numbers for the new candidate rather than a `NaN` placeholder or an outlier that swamps every other candidate.

---

## 4. Adding both a strategy and a cost model

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

## 5. Common mistakes

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

If `implementation.rs` already defines which sketch algorithms satisfy an `AggIntent`, reuse that source.

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

## 6. Extension checklist

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
- [ ] Use `rank_candidates` for algorithm preference; return every input candidate exactly once.
- [ ] Use `size_params` for accuracy-to-parameter mapping.
- [ ] Use extension hooks for extension-defined implementations/readouts.
- [ ] Use CSE hooks for recompute-vs.-sharing costs.
- [ ] Override `estimate_cost` if consumers require numeric costs instead of `NaN`.
- [ ] Test the hook directly.
- [ ] Test integration through a consumer such as `SketchAlgorithmStrategy`.
- [ ] Verify that changing cost preferences does not silently remove valid replacement candidates.

---

## 7. Current extension map

Use this table to find the right place for a change.

| I want to... | Primary extension point |
|---|---|
| Add a new logical optimization | new `impl ReplacementStrategy` |
| Add a new replacement for an existing target shape | `ReplacementStrategy::replacements` |
| Change when a strategy applies | `ReplacementStrategy::matches` |
| Add a new built-in sketch candidate | `replacement.rs`'s summary-candidate mapping |
| Prefer one sketch algorithm over another | `CostModel::rank_candidates` |
| Change sketch sizing for an accuracy target | `CostModel::size_params` |
| Add extension-defined implementation behavior | `CostModel::realize_extension` |
| Add extension-defined readout behavior | `CostModel::readout_extension` |
| Change CSE recomputation cost | `CostModel::cse_recompute_cost` |
| Change shared-maintenance cost | `CostModel::cse_shared_maintenance_cost` |
| Change current share/recompute choice | `CostModel::cse_share_decision` |
| Decide whether an available implementation satisfies a required one | `impl Matcher` |
| Produce a normal (ranked-first) bound summary for one target | `SketchAlgorithmStrategy::replacements(...).into_iter().next()` |
| Search a whole workload for every candidate at every `TargetSubDAG` (never pruning) | `replacement::search_workload`/`search_workload_with` |
| Get every candidate ranked best-first, across a whole workload | `PlanSpace::cost_sorted` |
| Get a real numeric cost per candidate, not just a relative rank | `CostModel::estimate_cost` |
| Enumerate valid sketch algorithms | reuse `replacement::summary_candidates` |
| Build a target with no workload context | `TargetSubDAG::new` |
| Build a target with known sharing context | `TargetSubDAG::with_consumer_count` |
| Explain why a replacement exists, where, and why | `explanation::explain_replacements`/`explain_replacements_with` |
| Add a new kind of replacement explanation | new `impl ReplacementStrategy`, wired into `default_strategies`/`default_strategies_with` — not a new explanation-specific trait, see Part 3 §8 |

---

## 8. Using and extending explanation.rs

### Using it

```rust
use asap_aware_mapping::{explain_replacements, ExplanationKind};

let explanations = explain_replacements(vec![("dashboard_p99", query)]);
for explanation in &explanations {
    match explanation.kind {
        ExplanationKind::SketchApproximation => { /* ... */ }
        ExplanationKind::CommonSubexpressionReuse => { /* ... */ }
        _ => { /* ExplanationKind is #[non_exhaustive] */ }
    }
}
```

To plug in a deployment-specific strategy or `CostModel`, use `explain_replacements_with` with a strategy set built the same way `default_strategies_with` builds one — see Part 3 §2 and Part 3 §4.

### Adding a new kind of replacement explanation

There is no separate checklist here: follow [Part 3 §1](#1-adding-a-new-replacementstrategy) to add the new `ReplacementStrategy` and wire it into `default_strategies`/`default_strategies_with`, add an `ExplanationKind` variant, and extend `findings_from_plan_space` to recognize the new candidate shape. If the new strategy's `matches`/`replacements` are already correct and tested, the explanation falls out of the existing `PlanSpace` translation with no new discovery logic required.
