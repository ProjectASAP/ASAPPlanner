# ASAP-Aware Mapping contracts

This reference defines the current public concepts and interface contracts used
by ASAP-aware mapping. Read the [architecture](asap-aware-mapping-architecture.md)
first; use the [extension guide](extend-asap-aware-mapping.md) when changing one.

## Interfaces and definitions

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

There are currently three forms:

```rust
pub enum Replacement {
    Summary(Rc<SummaryNode>),
    Rewrite(Rc<QueryExpr>),
    ExactComposition(ExactComposition),
}
```

Use `Replacement::Summary` when the alternative is a constructed post-ASAP summary plan.

Use `Replacement::Rewrite` when the alternative is still a logical pre-ASAP `QueryExpr`.

Use `Replacement::ExactComposition` when an exact operation refers to a child
target whose implementation must remain undecided. Selection coordinates the
parent/child pair; materialization constructs and validates the composed DAG.
See [exact_composition.rs](../../crates/asap-aware-mapping/src/exact_composition.rs).

Examples:

```text
Quantile(...)
    -> KLL SummaryNode
```

is a `Summary`; KLL (Karnin–Lang–Liberty) is a quantile-sketch algorithm.

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
    pub strategy: &'static str,
    pub provenance: ReplacementProvenance,
    pub rationale: String,
}
```

The `rationale` is for debugging, reporting, and explaining planner choices. It is human-readable text, not a machine-readable protocol.

Every candidate should carry a useful rationale. Search records the strategy
name, and selection uses typed provenance instead of inferring candidate roles
from the rationale or pointer identity.

---

### `ReplacementStrategy`

The main extension point for adding a new optimization or replacement source.

```rust
// Required methods; the trait also provides default name and propose methods.
pub trait ReplacementStrategy {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool;

    fn replacements(
        &self,
        target: &TargetSubDAG<'_>,
    ) -> Vec<ReplacementSubDAG>;
}
```

The two required methods have intentionally different responsibilities.
`name` supplies the diagnostic strategy name. `propose` returns accepted
candidates and structured accuracy rejections; its default wraps `replacements`.
Search calls `propose`, so rejected candidates remain available for explanation.

`matches` answers:

> Does this strategy have anything to offer for this target?

`replacements` answers:

> What are all semantically valid alternatives for this target?

`replacements` must be **exhaustive and not cost-filtered**. When its output has
a preferred order, that ordering must come from the supplied `CostModel`; the
strategy must still return every supported legal candidate. Required accuracy,
schema and capability checks can reject an otherwise applicable algorithm;
exhaustiveness is not a promise of all theoretically possible plans.

---

### `Implementation`

One valid realization of an `AggIntent`, the pre-ASAP description of what an
aggregation must compute without committing to a physical summary algorithm.
An implementation may be an approximate sketch, an exact mergeable
accumulator, or a pass-through that keeps the original operation instead of
building a summary. `implementations_for_with` enumerates these concrete
realizations; `SketchAlgorithmStrategy::replacements()` constructs each one as
a `ReplacementSubDAG`. It returns all
candidates in preferred order without selecting a winner. At workload scale,
`search_workload`/`search_workload_with` preserve all supported legal alternatives
across every `TargetSubDAG`. Optional planner APIs coordinate compatible semantic selections; physical
commitment and placement remain downstream deployment decisions.

This guide uses the Cascades/Volcano terminology:

- An **implementation rule** maps a logical operation to a candidate physical
  realization. For example, a quantile `AggIntent` may have KLL and DDSketch
  `Implementation` values.
- A **transformation rule** maps a logical operation to another logical
  operation. In this crate, that kind of candidate is represented by
  `Replacement::Rewrite`.
- A **replacement candidate** packages either kind of result as a
  `ReplacementSubDAG` for search. `PlanSpace` stores and ranks these candidates.
- **Physical commitment and placement** happen downstream. An `Implementation`
  therefore does not mean that the planner has committed the workload to that
  choice.

The concrete flow is:

```text
AggIntent
  -> implementations_for_with(): enumerate Implementation values
  -> SketchAlgorithmStrategy: construct ReplacementSubDAG candidates
  -> PlanSpace: store and rank candidates
  -> downstream deployment: select and place a final choice
```

`ReplacementStrategy` enumerates supported legal candidates. The caller can
consume ranked groups or use coordinated selection and semantic materialization;
see [code architecture §3](asap-aware-mapping-architecture.md#3-how-the-current-pieces-fit-together).

---

### `CostModel`

`CostModel` covers every deployment-specific numeric or configuration decision—not only which candidate is cheapest. For example, sketch sizing trades memory and update cost for accuracy, so it belongs here too.

The crate cannot hardcode real deployment costs: `asap-aware-mapping` uses `asap-types` and pinned `asap_sketchlib` mapping
bounds, but does not execute workloads or own deployment measurements. Most hooks therefore provide the crate's built-in static behavior as a default. Override only the decisions your deployment needs to change.

| Hook | Use it to | Default? |
|---|---|---|
| `rank_candidates` | Order valid sketch algorithms | No |
| `size_params` | Convert an accuracy target into sketch parameters | Yes |
| `realize_extension` | Map a custom intent to an implementation | Yes |
| `readout_extension` | Query a custom extension summary | Panics until paired with a custom realization |
| `cse_recompute_cost` | Estimate independent recomputation | Yes |
| `cse_shared_maintenance_cost` | Estimate shared maintenance | Yes |
| `cse_share_decision` | Choose sharing or recomputation | Yes |
| `estimate_cost` | Attach a comparable numeric cost to a replacement | Returns `NaN` (IEEE “not a number”); `DefaultCostModel` provides real values |

- **`rank_candidates`** — order the sketch candidates for one `AggIntent`, best first. This is the only required hook.

  ```rust
  fn rank_candidates(&self, intent: &AggIntent, candidates: &[SketchAlgorithm]) -> Vec<SketchAlgorithm>;
  ```

- **`size_params`** — choose parameters, such as sketch capacity, for an already-selected `SketchAlgorithm` and accuracy target `(eps, delta)`, where `eps` is the tolerated error and `delta` is the tolerated probability of exceeding that error. It is separate from ranking so a deployment can customize sizing without changing algorithm preference. The trait provides a default implementation.

  ```rust
  fn size_params(&self, kind: SketchAlgorithm, intent: &AggIntent, eps: f64, delta: f64) -> SketchParams;
  ```

- **`realize_extension`** — map a deployment-defined `AggIntent::Extension` to a post-ASAP `Implementation`. The default is `Implementation::PassThrough`.

  Use `AggIntent::Extension { ext_kind, payload }` for intent shapes that only your deployment needs. Core treats both fields as opaque. For example, a deployment can tag an approximate-frequency intent with `ext_kind: "frequency"` and recognize it in `realize_extension`:

  ```rust
  fn realize_extension(&self, ext_kind: &str, _payload: &serde_json::Value) -> Implementation {
      if ext_kind == "frequency" {
          Implementation::Sketch(SketchKind::new(
              SketchAlgorithm::CountSketch,
              SketchParams::CountSketch { width: 1024, depth: 5 },
          ))
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

### `PlanSpace` / `TargetSubDAGCandidates` / `RankedTargetSubDAGCandidates` — the whole-workload view

`ReplacementStrategy` answers "what are the candidates for this one target?" `PlanSpace` answers the same question for every target in a whole workload at once, without materializing `2^N` fully-copied plans for `N` independently-choosable sites.

```rust
// replacement.rs

// One TargetSubDAGCandidates per distinct TargetSubDAG in the whole workload —
// never a flat list of fully-materialized plans.
pub struct TargetSubDAGCandidates {
    pub target: Rc<QueryExpr>,
    pub consumer_count: usize,
    pub candidates: Vec<ReplacementSubDAG>,  // accepted alternatives, unranked
    pub rejected: Vec<RejectedCandidate>,    // failed accuracy checks
}

pub struct RankedTargetSubDAGCandidates<'a> {
    pub target: &'a Rc<QueryExpr>,
    pub consumer_count: usize,
    pub candidates: Vec<&'a ReplacementSubDAG>,  // same candidates, ranked
    pub costs: Vec<f64>,                         // costs[i] <-> candidates[i]
}
```

`search_workload(roots)` runs the shared-subtree pass once, discovers every target across every root's whole DAG (not just root-level sharing — a `SharedSubtreeStrategy` candidate three levels under an unshared `Filter` is exactly as real a site as a shared whole root), and asks every registered strategy to a fixpoint. Two logically different candidates at two different targets are never copied into two separate plans — they're two entries in two different `TargetSubDAGCandidates`s, sharing every other node in the workload by construction.

`PlanSpace::cost_sorted(cost_model)` is the one ranking step: for each group, it dispatches by candidate shape — a same-shape `Rewrite` pair (a `SharedSubtreeStrategy` share/recompute choice) goes through `CostModel::cse_share_decision`; a same-shape run of `Summary` candidates realizing sketches (a `SketchAlgorithmStrategy` choice) goes through `CostModel::rank_candidates`; and a mixed group is ordered by each candidate's `CostModel::estimate_cost`. Every candidate gets a numeric cost aligned index-for-index in `costs`. Count in, count out—nothing is dropped to produce a ranking. Legality checks
may already have removed proposals before this boundary. In particular,
`search_workload_with_targets` checks explicit per-root targets. Use
`global_selection` for coordinated sharing/composition choices and
`GlobalSelection::assemble_selected_dag` for the resulting semantic DAG; neither deploys it.

---

### Family, category, algorithm, and parameters

Sketches separate their query category from the concrete algorithm and its parameters:

| Level | Type | Example |
| --- | --- | --- |
| **family** | `SummaryFamilyType` | `Sketch`, `Sample`, `Wavelet`, `StatModel`, `ExactAggregate` |
| **category** | `SketchCategory` | `Quantile`, `Cardinality`, `Frequency`, `TopK` |
| **algorithm** | `SketchAlgorithm` | `Kll` / `DDSketch` (both quantile); `Hll` (HyperLogLog) / `Theta` / `Kmv` (K-Minimum Values), all cardinality |
| **committed choice** | `SketchKind` | one validated category + algorithm + parameter combination |

A `SketchKind` is a validated committed choice. Its public constructor,
`SketchKind::new(algorithm, params)`, verifies that the parameter variant belongs
to the selected algorithm and classifies the pair into its category. The public
`.category()`, `.algorithm()`, and `.params()` accessors expose the committed
values without permitting an invalid combination.

Where this matters in practice: `CostModel::rank_candidates`, `CostModel::size_params`, and `SketchAlgorithmStrategy::replacements` operate at the **algorithm** level. `summary_candidates(intent)` returns a list of `SketchAlgorithm`s (`[Kll, DDSketch]` for a `Quantile` intent), never a bare `SketchKind` with nothing chosen underneath it. `SketchKind` appears after an algorithm has been selected and sized—on `Implementation::Sketch(SketchKind)` and `SummaryFamilyType::Sketch(SketchKind)`.

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

For example, a deployment may have a DDSketch for `latency` while a request
was planned with KLL. A shared quantile query category is insufficient to prove
substitutability: rank-error and relative-value guarantees differ. The deployment
must check the required guarantee, parameters, grouping, coverage and storage
compatibility before reusing state. `Matcher::is_satisfied_by(required, available)` lets the deployment make that decision: `required` is what the query needs, and `available` is what the inventory already contains.

The crate provides no default `Matcher` implementation because the answer depends on deployment-specific inventory and storage rules. If you need one, implement the complete trait for your deployment.

---

## 2. Replacement explanations (`explanation.rs`)

`explanation::explain_replacements`/`explain_replacements_with` answer a different question than everything above: not "what could this target become" (`ReplacementStrategy::replacements`) but "why does the replacement already discovered for this target exist, and where." It is a **reporting view over `PlanSpace`**, not a second search or a second rule engine — this crate's *explanation of a replacement*, not an applicability classifier deciding admissibility from scratch.

### The rule

> A `TargetSubDAG` is worth explaining exactly when its `PlanSpace` candidate list contains something beyond the trivial, no-op realization.

Concretely, `explanation.rs` reports three candidate kinds from each `TargetSubDAGCandidates`:

- `ExplanationKind::SketchApproximation` — the group's candidates include a `Replacement::Summary` that actually realizes `SummaryFamilyType::Sketch(..)`, i.e. `SketchAlgorithmStrategy` found a real sketch alternative, not just an exact/pass-through candidate.
- `ExplanationKind::CommonSubexpressionReuse` — `consumer_count >= 2` and the group's candidates include `SharedSubtreeStrategy`'s "build once and share" candidate (the `Replacement::Rewrite` whose `Rc` is the group's own `target`).

- `ExplanationKind::ExactComposition` — the group contains an exact operation
  composed with a child target whose implementation remains a coordinated choice.

Each `ReplacementExplanation::reason` is copied verbatim from the matching candidate's own `ReplacementSubDAG::rationale`. Nothing in `explanation.rs` re-explains why a candidate is valid; that explanation already exists exactly once, on the candidate itself.

`ReplacementExplanation` carries both `node_hash` and `target`. A downstream consumer first compares `node_hash` with an exported `DagNode::hash` to narrow the search, then compares the exact target expression with the node's in-process source expression. This preserves the hash's role as a fast filter while making the final association collision-safe; `location` remains human-readable presentation text rather than a machine identifier.

### Why there is no `ExplanationRule` trait

Explanations are derived from candidates already present in `PlanSpace`. A new candidate kind therefore requires an `impl ReplacementStrategy` wired into `default_strategies`/`default_strategies_with`; a second explanation-specific trait would duplicate registration and could drift from the actual search space. Custom callers supply strategies through `explain_replacements_with`, using the same extension point exposed by `search_workload_with`.

### How it derives `location` text

`PlanSpace`/`TargetSubDAGCandidates` track `Rc<QueryExpr>` pointer identity, not human-readable breadcrumbs. `ReplacementExplanation::location` provides prose such as `root "dash_a" > lhs` so reporting consumers can identify the relevant part of the query without interpreting pointer identity. Location derivation does not make replacement or costing decisions.

---
