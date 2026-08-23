# Extension points (dev guide)

`crates/asap-aware-mapping` has three extension points a contributor or a
downstream deployment plugs into: [`ReplacementStrategy`](#replacementstrategy),
[`CostModel`](#costmodel), and [`Matcher`](#matcher-boundaryrs). All three
share the same shape — a small trait, a default or no-default body, and one
job each: `ReplacementStrategy` reports *what's possible*, `CostModel`
decides *what's preferable*, `Matcher` decides *what already satisfies a
requirement*. This doc is the practical "what do I actually write" companion
to [`docs/asap_aware_mapping.md`](asap_aware_mapping.md) (the design
rationale for *why* this vocabulary exists) and each type's own rustdoc (the
authoritative reference for its exact contract). Read those first if you
haven't.

## `ReplacementStrategy`

Defined in [`crates/asap-aware-mapping/src/replacement.rs`](../crates/asap-aware-mapping/src/replacement.rs).
Relevant if you're implementing #253 (semantic rewrite), #254 (roll-up),
#256 (Hydra grouping), #257 (applicability-as-a-view), or any future
optimization that should show up as a candidate in the eventual search
(#252).

### The three types, as a contributor uses them

```rust
pub struct TargetSubDAG<'a> {
    pub root: &'a Rc<QueryExpr>,   // the node you might replace
    pub consumer_count: usize,     // how many workload locations reference it
}

pub enum Replacement {
    Summary(Rc<SummaryNode>),  // "bind it to this post-ASAP summary"
    Rewrite(Rc<QueryExpr>),    // "replace it with this equivalent pre-ASAP tree"
}

pub struct ReplacementSubDAG {
    pub replacement: Replacement,
    pub rationale: String,     // human-readable, for logs/debugging — not parsed
}

pub trait ReplacementStrategy {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool;
    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG>;
}
```

A strategy is stateless business logic over one `TargetSubDAG` at a time. It
does not walk a workload, does not rank, does not decide. Those are, in
order, the future search engine's job (#252), a `CostModel`'s job, and the
search engine's job again.

### The contract, made explicit

- **`matches` must be cheap and side-effect-free.** The future search engine
  (#252) will call it on every `TargetSubDAG` in a workload, against every
  registered strategy, on every iteration of its fixpoint loop. Don't do
  binding work here — that's `replacements`' job, and only after `matches`
  said yes.
- **`replacements` must be exhaustive.** List *every* semantically valid
  candidate, not just the ones you'd guess are good. Pruning is explicitly
  out of scope for a strategy — a `CostModel` (used by the search engine,
  #252) is what narrows candidates down, and it can only compare candidates
  it was actually given. An under-enumerating strategy silently shrinks the
  search space and can hide the globally best plan.
- **`replacements` must not panic when `matches` would have returned
  `false`.** Return an empty `Vec` instead. A caller that skips the
  `matches` check first (or a future search engine that calls
  `replacements` speculatively) should get a safe, merely-uninformative
  answer, not a crash. `SketchFamilyStrategy` and `SharedSubtreeStrategy`
  both follow this; keep doing it.
- **`rationale` is prose for a human, not a machine.** It's meant for a
  debug log or a search engine's explanation of why it considered a
  candidate — write it as you would a code comment explaining a decision,
  not as a structured value another piece of code will parse.
- **Wrap an existing decision procedure; don't invent a new one.** Every
  strategy so far (`SketchFamilyStrategy` wrapping
  `boundary::implementation_for_with`/`summary_candidates`,
  `SharedSubtreeStrategy` wrapping `cse::share_common_subtrees`) reuses
  logic that's already correct and already tested elsewhere in this crate.
  If your strategy needs a decision nothing in the codebase makes yet,
  that decision procedure is its own piece of work — build and test it on
  its own terms first, then wrap it in a thin `ReplacementStrategy`
  `impl`, the same way these two do.

### Worked example: `SharedSubtreeStrategy`

This is the smallest real strategy in the module — a good template to copy
from.

```rust
pub struct SharedSubtreeStrategy;

impl ReplacementStrategy for SharedSubtreeStrategy {
    fn matches(&self, target: &TargetSubDAG<'_>) -> bool {
        target.consumer_count >= 2
    }

    fn replacements(&self, target: &TargetSubDAG<'_>) -> Vec<ReplacementSubDAG> {
        if target.consumer_count < 2 {
            return Vec::new();
        }
        let count = target.consumer_count;
        vec![
            ReplacementSubDAG {
                // The already-interned `Rc` itself: reusing it verbatim *is*
                // "build once and share" — no new node to construct.
                replacement: Replacement::Rewrite(Rc::clone(target.root)),
                rationale: format!(
                    "build once and share: share_common_subtrees already interned this \
                     subtree once and reused it across {count} consumers — one build can \
                     answer all of them instead of computing it {count} times"
                ),
            },
            ReplacementSubDAG {
                // A structurally-identical but freshly-allocated `Rc`: same
                // value (`PartialEq`), deliberately *not* the same pointer,
                // representing "undo the sharing and recompute independently".
                replacement: Replacement::Rewrite(Rc::new((**target.root).clone())),
                rationale: format!(
                    "build independently: undo the sharing share_common_subtrees found and \
                     recompute this subtree separately at each of its {count} consumers — \
                     worth it only when independence outweighs the shared-maintenance cost, \
                     a CostModel's call and not this strategy's"
                ),
            },
        ]
    }
}
```

What to notice, as a pattern to reuse:

1. **`matches` is a one-line predicate** over data `TargetSubDAG` already
   carries (`consumer_count`). It doesn't re-derive anything the caller
   could have told it directly.
2. **The strategy doesn't decide** which of the two candidates is better —
   it returns both, unconditionally, whenever it matches. "Which is
   cheaper" is explicitly deferred to `CostModel::cse_share_decision`
   (today) / the search engine's cost-based ranking (once #252 lands).
3. **Both branches build a `Replacement::Rewrite`**, because sharing-vs-not
   is a structural (pre-ASAP) choice, not a binding decision. Use
   `Replacement::Summary` instead when your strategy's candidates are
   post-ASAP binding choices — see `SketchFamilyStrategy` for that shape
   (it wraps `bind::implement_tree_with` via a `CostModel` adapter,
   `ForceSketchKind`, to steer the existing binding procedure toward each
   candidate `SketchKind` in turn, rather than reimplementing binding).
4. **Guard `replacements` even though `matches` already checked the same
   condition.** The `if target.consumer_count < 2 { return Vec::new() }`
   inside `replacements` looks redundant next to `matches`, but it's what
   keeps `replacements` safe to call on its own — see the contract above.

### Testing a new strategy

Follow the hand-rolled-fixture style `bind.rs`/`boundary.rs`/`cost_model.rs`
already use — build a small `QueryExpr` tree by hand, wrap it in a
`TargetSubDAG`, and assert on `matches`/`replacements` directly. At minimum:

- **A positive case**: a target your strategy matches, asserting the full
  set of `replacements()` it returns (not just its length — check which
  `Replacement` variants and values came back).
- **A negative case**: a target your strategy does *not* match, asserting
  `matches` returns `false` and `replacements` returns an empty `Vec`.
- If your strategy depends on cross-node context (like
  `SharedSubtreeStrategy`'s `consumer_count`), add one test that builds it
  from the real upstream computation instead of a hand-set field — see
  `replacement.rs`'s own test that runs `SharedSubtreeStrategy` over a
  `TargetSubDAG` built from real `share_common_subtrees` output, via a
  small test-only traversal mirroring PR #247's `SharedSubexpressionRule`
  dedup logic. A hand-set `consumer_count` alone can't catch a bug in how
  that value gets computed for real.

### What you are not responsible for

- **Discovering `TargetSubDAG`s across a workload.** Your strategy receives
  one `TargetSubDAG` at a time; walking a whole workload's tree to find
  candidates is the search engine's job (#252) — or, today, ad hoc test
  fixtures / `applicability.rs`'s existing traversal.
- **Ranking or filtering your own candidates.** Return everything valid;
  a `CostModel` picks.
- **Wiring your strategy into anything automatically.** Until #252 lands,
  a new `impl ReplacementStrategy` is just a type other code can construct
  and call directly (e.g. from a test, or from `applicability.rs`) — there
  is no registry or dispatcher yet.

## `CostModel`

Defined in [`crates/asap-aware-mapping/src/cost_model.rs`](../crates/asap-aware-mapping/src/cost_model.rs).
Relevant if you're wiring a deployment (ASAPCollector, ASAPFusion, …) with
real cost knowledge (bandwidth budget, memory footprint, site count,
observed drift, …) into candidate selection, instead of accepting this
crate's built-in static preferences.

### The extension-point discipline

`asap-aware-mapping` deliberately has no cost model *implementation* of its
own beyond [`DefaultCostModel`] — ranking real candidates by real cost needs
deployment knowledge this crate doesn't have and, per the crate's layering
invariant, shouldn't acquire. `CostModel` is the one interface every
deployment plugs its own knowledge into instead of forking `boundary`/`bind`.
Every method has a default body that reproduces today's static behavior
exactly — implement only the hooks you actually need to change:

```rust
pub trait CostModel {
    // No default: candidate selection needs a real answer.
    fn rank_candidates(&self, intent: &AggIntent, candidates: &[SketchKind]) -> Vec<SketchKind>;

    // Defaults to asap-plan's built-in per-family sizing formulas.
    fn size_params(&self, kind: SketchKind, intent: &AggIntent, eps: f64, delta: f64) -> SketchParams { ... }

    // Defaults to Implementation::PassThrough for AggIntent::Extension.
    fn realize_extension(&self, ext_kind: &str, payload: &serde_json::Value) -> Implementation { ... }

    // Panics if realize_extension ever returned Sketch without this being overridden too.
    fn readout_extension(&self, ext_kind: &str, payload: &serde_json::Value, col: &ColumnRef) -> SketchQuery { ... }

    // CSE sharing cost hooks (issue #237) — defaults are structural-size /
    // per-family-weight proxies; override with real cost knowledge.
    fn cse_recompute_cost(&self, candidate: &CseCandidate) -> f64 { ... }
    fn cse_shared_maintenance_cost(&self, candidate: &CseCandidate) -> f64 { ... }
    fn cse_share_decision(&self, candidate: &CseCandidate) -> ShareDecision { ... } // composes the two above
}
```

### The contract, made explicit

- **`rank_candidates` may reorder or drop entries, never invent one.**
  Returning a `SketchKind` that wasn't in `candidates` will panic downstream
  (`boundary::implementation_for_with` has no sizing logic for an unknown
  kind). Returning an empty `Vec` is valid — it means "no candidate is
  acceptable here."
- **Prefer overriding the narrowest hook.** `cse_share_decision`'s default
  body composes `cse_recompute_cost` and `cse_shared_maintenance_cost` into
  a Volcano/Cascades-style comparison (share iff maintaining one shared
  summary costs no more than recomputing it everywhere it's used) — a
  deployment with real cost numbers should override the two cost hooks and
  keep the comparison, not reimplement the comparison itself. Override
  `cse_share_decision` directly only for a genuinely different policy (e.g.
  something other than a cost threshold). See
  [`docs/cse-cost-model-decision.md`](cse-cost-model-decision.md) for the
  full design discussion behind this split.
- **`realize_extension` and `readout_extension` are a matched pair.** If you
  override `realize_extension` to return `Implementation::Sketch` for some
  `ext_kind`, you MUST also override `readout_extension` for that same
  `ext_kind` — the default panics loudly (not silently) the first time that
  intent is actually read out, specifically so a mismatch is caught instead
  of misinterpreting `payload`.
- **Delegate, don't reimplement, when adapting an existing `CostModel`.**
  `SketchFamilyStrategy`'s `ForceSketchKind` (in `replacement.rs`) is the
  reference example: it wraps another `&dyn CostModel`, overrides only
  `rank_candidates` to force one specific kind first, and forwards every
  other method to the wrapped model unchanged — so a deployment's own
  sizing/extension behavior still applies while one method's behavior is
  steered. Reach for this pattern before writing a `CostModel` from scratch
  when you only need to adjust one hook's behavior.

### Testing a new `CostModel`

`cost_model.rs`'s own tests are the reference style: construct a minimal
`CostModel` `impl` inline in the test (only overriding the method(s) under
test), build fixture `QueryExpr`/`SummaryNode` values by hand, and assert
directly on the method's return value — e.g.
`custom_cost_model_can_reorder_candidates`,
`custom_cost_model_can_override_sizing_independently_of_ranking`, and
`cse_share_decision_default_body_composes_the_two_cost_hooks`. For the
default-preserving behavior of any method you don't override, add a test
asserting it matches the crate's built-in default exactly (see
`default_cost_model_preserves_static_order`) — that's the guarantee a
deployment relies on when it only overrides one hook.

## `Matcher` (`boundary.rs`)

Defined in [`crates/asap-aware-mapping/src/boundary.rs`](../crates/asap-aware-mapping/src/boundary.rs).
Much smaller surface than the other two, but shares the same "extension
point instead of a fork" role, so it's worth knowing it exists:

```rust
pub trait Matcher {
    fn is_satisfied_by(&self, required: &Implementation, available: &Implementation) -> bool;
}
```

Unlike `CostModel`, `asap-plan` ships **no implementation and no default
body** for this trait at all — whether an available summary satisfies a
required one (e.g. does an available `DDSketch` satisfy a required `Kll`
request? does a multi-population accumulator satisfy a single-population
query?) depends on facts this crate deliberately doesn't settle: a pure
sketch-algebra answer and a deployment's own storage-layout rules can
legitimately disagree. If you need this, you're implementing the whole
trait for your deployment from scratch — there's no default to lean on and
no existing implementation in this crate to use as a worked example.

## Where this fits

`docs/asap_aware_mapping.md` is the design doc this vocabulary implements
(read it for *why* `ReplacementStrategy` is shaped as "exhaustive, not
decisive," why the eventual search is MEMO-style and iterative, and how a
`CostModel` fits into that search). This guide is the narrower "how do I
add one" doc — for `ReplacementStrategy`, aimed at whoever's picking up
#253/#254/#256/#257 next; for `CostModel`, aimed at whoever's wiring a real
deployment's cost knowledge into candidate selection.
