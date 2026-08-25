# CSE sharing: rule-based vs. cost-based framework (issue #237)

## Context

[`asap_types::pre_asap::cse::share_common_subtrees`](../../crates/types/src/pre_asap/cse.rs)
(issue #223 stages 1-2, PR #235) already *detects* every structurally-identical,
legally-shareable (`Schema::unique_keys`-gated) subtree and shares it
**unconditionally** — there is no cost gate on top of legality. This document
decides the framework for stage 4, "wire workload-level CSE credit into
`CostModel`" — turning "these two subtrees are the same computation" into
"and it's actually worth maintaining one shared summary for them."

## The two textbook framings (as posed in #237)

| Framework | Mechanism | CSE policy |
|---|---|---|
| Volcano/Cascades (SQL Server, Snowflake, Calcite) | cost-based: explores a plan space via DP + memo | share iff a real cost comparison (materialize/maintain vs. recompute-per-site) favors it |
| System R (classic) | heuristic: fixed rules over basic statistics | share whenever a fixed rule says to (e.g. "referenced more than once"), no per-case comparison |

## Decision: cost-based (Volcano/Cascades), implemented for real

This lands as an actual cost comparison, not a documented-but-unimplemented
shape. [`CostModel::cse_share_decision`](../../crates/asap-aware-mapping/src/cost_model.rs)
compares two real, overridable cost estimates for every CSE candidate with
two or more consumers:

- `cse_recompute_cost(candidate) * candidate.consumer_count` — the total cost
  of recomputing the subtree independently at every use site.
- `cse_shared_maintenance_cost(candidate)` — the cost of keeping one shared
  summary alive and continuously updated for the workload's lifetime.

Share iff the shared-maintenance cost is no greater than the total recompute
cost. This is a genuine Volcano/Cascades-style decision: a real, per-candidate
cost comparison, not a fixed "always share when legal" rule.

Why cost-based and not pure System R: a shared summary here is not a free win
the way sharing a relational scan is in a textbook OLTP optimizer — it is a
sketch/accumulator that (per this crate's stated purpose: *workload*-level
planning, not single-query) is typically kept **continuously updated** as new
data arrives, for as long as the workload runs, regardless of how often it's
actually read. A structurally-shareable subtree that is cheap to recompute on
demand, or rarely queried, can cost more to keep alive as a standing shared
summary than to just recompute independently at each of its (few, or cheap)
use sites. A blanket "always share" rule cannot express that trade-off; a
cost comparison does, without needing a separately hardcoded cheap-threshold
carve-out — a cheap-to-recompute candidate naturally loses the comparison on
its own.

This decision does not need search infrastructure of its own. Issue #252's
MEMO-based search engine (`PlanSpace`/`MemoGroup` in `replacement.rs`) already
enumerates and ranks the larger, workload-wide candidate space. The choice
between sharing and recomputing one already-detected CSE candidate is binary,
so `PlanSpace::cost_sorted` reuses one direct
`CostModel::cse_share_decision` comparison per group. This preserves the
policy described here—compare costs rather than applying a fixed rule—inside
the larger search engine. `search_workload_with`'s
target-discovery pass still computes the true `consumer_count` for each
candidate via a whole-workload traversal before any ranking happens — the
decision is made from full knowledge of the workload's sharing structure, the
same way a real cost-based optimizer would.

## Layering constraint

`share_common_subtrees` lives in `asap-types::pre_asap` — a lower layer that
`asap-aware-mapping` (which owns `CostModel`) depends on, never the reverse.
Detection therefore cannot consult cost even if it wanted to. This is why
stage 1/2's detection stays unconditional (correctly, as a legality-only
gate) and the cost-aware decision is applied downstream, in
`asap-aware-mapping`, after detection rather than fused into it.

## Where it hooks in

[`PlanSpace::cost_sorted`](../../crates/asap-aware-mapping/src/replacement.rs)
is where this hooks in today. `search_workload_with` computes each shared
subtree's true `consumer_count` across the whole workload up front (the same
role `implement_workload_with`'s pre-pass used to play, before that function
was retired along with `bind.rs` — this crate no longer commits to one
physically-materialized answer at all; picking and building one final
`SummaryNode` per shared subtree is a downstream deployment's job, not this
crate's). For a `MemoGroup` whose candidates are a
[`SharedSubtreeStrategy`](../../crates/asap-aware-mapping/src/replacement.rs)
share-vs-recompute pair, `cost_sorted`'s ranking step (`rank_group`/
`cse_preference`) asks `CostModel::cse_share_decision` once per group — using
one representative bound `SummaryNode` built just for that comparison, not
cached anywhere — and sorts the pair so the preferred candidate (`Share` or
`RecomputeIndependently`) comes first. Both candidates are still returned;
ranking never drops one: a `CostModel` orders and parameterizes candidates; it
does not prune them.

## Defaults

`cse_recompute_cost`'s default is a structural-size proxy: `cse::dag_node_count`,
the number of *unique* nodes in the subtree's DAG (deduplicated by `Rc`
pointer identity), not a raw serialization length. This distinction matters
here specifically — a `CseCandidate`'s subtree is, by definition, something
CSE already found sharing in, so it's generally a DAG, not a tree; a naive
tree-shaped size measure (a full `serde_json` serialization, or a recursive
walk with no identity tracking) would re-count any descendant the subtree
already shares internally once per parent that reaches it, over-stating the
real cost of holding or recomputing it once. `cse_shared_maintenance_cost`'s default
is a small per-`SummaryFamilyType` weight table (exact accumulators cheapest,
sketches/samples/wavelets/stat-models progressively more expensive to keep
continuously updated) scaled to the same order of magnitude as typical
subtree sizes. Both are documented as coarse heuristic proxies — a real
deployment with actual memory/update-cost/query-frequency knowledge overrides
either or both, same as `size_params` already lets a deployment override
`asap-plan`'s built-in sizing formulas without forking anything else.

## Scope

This decision, and `cse_share_decision`'s wiring into `PlanSpace::cost_sorted`
(originally into `implement_workload_with`, before `bind.rs` was retired —
see above), close out #223's stage 4 and #212's original "add CSE" tracking
issue. Stage 3 (`dag_export::structural_hash` unification) landed separately
in PR #244.
