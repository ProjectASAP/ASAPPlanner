# `Concat` and `unique_keys`: defer the discriminator override (issue #228)

## Context

[`QueryExpr::Concat`](../../crates/types/src/pre_asap/query_expr.rs) (the
n-ary exact `UNION ALL` node, renamed from `Merge` in #226) always drops
`unique_keys` on its output — `merge_drops_the_branches_unique_keys` and
`merge_and_setop_agree_on_unique_keys` pin this down. Issue #228 asks whether
a `Concat`'s *constructor* should be able to assert a compound unique key
(`(discriminator_col, inner_key)`) when it can prove branch disjointness via
an explicit discriminator column (e.g. PromQL `histogram_quantiles`'s
per-branch φ, or a Postgres-style `GROUPING()` id for `ROLLUP`/`CUBE`/
`GROUPING SETS`), and lists two options:

1. **Producer-supplied override**: let the code building a `Concat` assert
   its own `unique_keys` when it can prove branch disjointness.
2. **Don't preserve it in the IR at all**: a consumer that needs the
   guarantee re-establishes it with an explicit `Dedup`.

Per the issue's own instructions, the decisive question is empirical: does
any current call site actually pay for a redundant `Dedup`-equivalent today
that a discriminator-based unique key would let it drop?

## What was checked

Both current `Concat`-constructing call sites, and every consumer of
`Schema::unique_keys` in the tree:

- **PromQL `histogram_quantiles`** —
  [`walk_histogram_quantiles`](../../crates/frontend-promql/src/promql.rs).
  Each branch is `PromqlRelabel { dst: label, value: Literal(φᵢ), child: Aggregate{…} }`
  — the discriminator (φ, formatted the way `open_metrics_float` renders it)
  really is a distinct literal per branch, so the "prove disjointness via a
  tagged discriminator" premise the issue describes does hold structurally
  here. The function returns `Unresolved::Concat { children: branches }`
  directly — **no `Dedup` or dedup-equivalent node follows it**, in this
  function or in any caller (`walk_call` returns its result unmodified).
- **SQL `ROLLUP`/`CUBE`/`GROUPING SETS`** —
  [`lower_grouping_sets`](../../crates/frontend-sql/src/sql/mod.rs). Each
  level's branch is `Project { …, child: Aggregate{…} }`, reinstating omitted
  keys as typed `NULL`s, and the levels are `Concat`ed. The function returns
  `Unresolved::Concat { children: branches }` directly — **no `Dedup` follows
  it here either.** Notably, this lowering *already discards* DataFusion's
  `__grouping_id` discriminator column on purpose (see the comment at the
  bottom of `lower_grouping_sets`'s doc comment): "it only exists to tell a
  subtotal's `NULL` apart from a data `NULL`, which is observable solely
  through `GROUPING(col)` — an aggregate this front end rejects." So today
  there is no discriminator column even available to ride along at this call
  site; wiring one in would mean *first* deciding to stop rejecting
  `GROUPING()` and surfacing `__grouping_id` as real IR — a separate, larger
  change outside #228's scope, not a small addition to this lowering.
- **Every other `QueryExpr::Concat { … }` construction site** in the repo is
  a test/tooling AST match (`promql_lowering.rs`, `promql_conformance.rs`,
  `sql_lowering.rs`, `dag_export.rs`, `variant_coverage.rs`, netflow/synthetic
  test fixtures) — none of them builds a fresh `Concat` with a `Dedup` on top
  that this feature could remove.
- **Every consumer of `Schema::unique_keys`** in the tree, to check for a
  cost beyond "a literal `Dedup` node": `pre_asap::cse::share_common_subtrees`
  (gates CSE producer-sharing on `Schema::has_unique_key()`) and
  `asap_aware_mapping::rollup::is_legal_rollup_source` (gates rollup-source
  legality the same way, on an *`Aggregate`'s* own output schema). Neither
  case is exercised by a `histogram_quantiles` or `ROLLUP`/`CUBE`/
  `GROUPING SETS` `Concat` in any current test, workload, or call site: no
  test constructs a workload with two structurally-identical
  `histogram_quantiles`/grouping-set queries for CSE to attempt to share, and
  no call site aggregates further on top of a `Concat`'s output in a way that
  would ask `is_legal_rollup_source` about it.
- No canonicalization/optimization pass in the repo removes a `Dedup` (or
  anything else) on the strength of a child's `unique_keys` — there is no
  such rewrite rule today, in `canonicalize.rs` or elsewhere — so even a
  perfectly-preserved `unique_keys` on these `Concat` nodes would not, by
  itself, delete any node from any plan that exists today.

## Decision: defer (Option 2 for now — no code change)

Neither `histogram_quantiles` nor `ROLLUP`/`CUBE`/`GROUPING SETS` lowering
emits a `Dedup`, or anything playing that role, after its `Concat` today.
There is nothing redundant in the tree for a discriminator-based override to
remove. Building the producer-supplied-override machinery (Option 1) now
would be pure speculative surface area — new trust-boundary code
(`output_schema()` accepting a caller-asserted key that must actually be
sound, forever) with no current call site to exercise or justify it, exactly
what the issue's own "before implementing either" section says to avoid.

This defers, it does not foreclose. If a future change adds a real consumer
— e.g. a canonicalization rule that drops a provably-redundant `Dedup` based
on a child's `unique_keys`, or a workload-level CSE scenario that actually
puts two identical `histogram_quantiles`/grouping-set `Concat`s in front of
`share_common_subtrees` — Option 1 (producer-supplied override, additive,
`output_schema()`'s default unchanged for ordinary callers) is still the
right shape for it then, for the reasons #228 already gives: it's strictly
additive, and the alternative of changing `Concat`'s default is unsound (a
key unique within one branch is not unique across the union unless the
branches are provably disjoint, and nothing about matching schemas or
matching per-branch keys establishes that on its own).

For SQL grouping sets specifically, note that the natural discriminator
(`__grouping_id`) is not just unwired but actively discarded on purpose today
because `GROUPING()` itself is rejected — so revisiting this decision for SQL
means revisiting that rejection first, which is its own design question, not
a corollary of #228.

## No code change

This document is the entire change for #228: an investigation write-up, with
no implementation. `merge_drops_the_branches_unique_keys` and
`merge_and_setop_agree_on_unique_keys` continue to describe `Concat`'s only
behavior — dropping `unique_keys` unconditionally — unchanged.
