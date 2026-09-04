# `Concat` and `unique_keys`: the discriminator override (issue #228)

> **Update**: the investigation below found no current call site paying for a
> redundant `Dedup` that this override would remove, and the original version
> of this document recommended deferring Option 1 on that basis. The repo
> owner reviewed that finding and explicitly asked for Option 1 to be built
> anyway — a deliberate "ship the extension point ahead of a proven call-site
> win" call, not a disagreement with the investigation. See "Decision" below
> for what actually shipped and the safety argument for why it's safe to ship
> unused. The investigation section is otherwise unchanged from the original
> write-up, since nothing about it stopped being true.

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

## Decision: build Option 1 anyway, unwired (explicit override of the defer)

The investigation's conclusion stands: neither `histogram_quantiles` nor
`ROLLUP`/`CUBE`/`GROUPING SETS` lowering emits a `Dedup` (or anything playing
that role) after its `Concat` today, so there is nothing redundant in the
tree for a discriminator-based override to remove *right now*. On review,
the decision was made to build the extension point anyway, ahead of a proven
call-site win, rather than wait for one. That is a legitimate call to make
differently from the investigation's own recommendation — "no current
payoff" is a statement about today's call sites, not about whether the
shape is safe to add — and the rest of this section is the safety argument
for why it's fine to ship unused.

### What shipped

`QueryExpr::Concat` gained an opt-in field, `discriminator_unique_key: Option<ConcatDiscriminatorKey<C>>`
(`crates/types/src/pre_asap/query_expr.rs`), plus:

- `ConcatDiscriminatorKey<C>` — a small struct with **private** `discriminator: C` /
  `inner_key: Vec<C>` fields, buildable only via `ConcatDiscriminatorKey::new(discriminator, inner_key)`.
  Privacy is the enforcement mechanism for "the caller must explicitly name a
  discriminator column" (see "Safety argument" below) — from other Rust code,
  there is no path to a non-empty `unique_keys` claim that doesn't go through
  a call site literally writing out which column it's proving is distinct.
  Caveat: this is an API-level guarantee, not a data-level one — the derived
  `Deserialize` impl bypasses `new()` entirely and can build one directly from
  arbitrary field values in untrusted JSON. Not a reachable concern today (see
  "Safety argument" below for why), but stated precisely rather than
  overclaimed.
- `QueryExpr::concat(children)` — the ordinary constructor (`discriminator_unique_key: None`),
  meant to replace the bare `QueryExpr::Concat { children }` struct literal
  everywhere in the tree so a future field addition doesn't force every call
  site to re-litigate this choice.
- `QueryExpr::concat_with_discriminator(children, discriminator, inner_key)` —
  the override constructor.
- `output_schema()`'s `Concat` arm: unchanged default (`unique_keys` cleared
  unconditionally) when `discriminator_unique_key` is `None`; when `Some`,
  adds `(discriminator, inner_key)` as the sole unique key, trusting the
  caller's claim without checking it.
- `resolve.rs`'s `Concat` arm resolves a pre-bind (`ColumnRef`) discriminator
  key into its post-bind (`ColumnId`) equivalent against the first resolved
  branch's own output schema — the same schema `output_schema()` derives the
  merged shape from — so the feature works correctly end-to-end for a future
  caller upstream of `resolve_root`, even though no such caller exists yet.
- Every other match/construction site touching `Concat` across the tree
  (`canonicalize.rs`, `cse.rs`, `binder.rs`, `dag_export.rs`,
  `asap-aware-mapping`'s `replacement.rs`/`explanation.rs`, and every
  test/tooling AST walker) was mechanically updated to bind or ignore the new
  field — most just added `, ..`; the two places that *rebuild* a `Concat`
  node (`cse.rs`'s `rebuild_children`, part of CSE interning) thread
  `discriminator_unique_key` through unchanged rather than dropping it.

### Not wired into any lowering call site (deliberately)

Per the explicit instruction accompanying this decision, `histogram_quantiles`
and `lower_grouping_sets` were **not** changed to call
`concat_with_discriminator` — both still call the plain `concat(children)`
builder, byte-for-byte the same `output_schema()` behavior they had before
this issue. The investigation's own findings are exactly why: `histogram_quantiles`
does have a structurally-available discriminator (φ, via `PromqlRelabel`) but
nothing downstream needs the resulting unique key yet, and SQL's natural
discriminator (`__grouping_id`) is actively discarded today because this
front end rejects `GROUPING()` — wiring that one in is a separate,
larger change (reopening that rejection) outside this issue's scope. Both
remain noted as future work at their call sites (see the comments added
there) and are not attempted here.

### Safety argument: why the default is unaffected

Three independent things hold `discriminator_unique_key: None` as the
observable behavior for every caller that doesn't ask for the override:

1. **Every real construction path defaults to `None`.** `QueryExpr::concat`
   hardcodes it; every call site in the tree (including both real lowering
   call sites) uses `concat`, not `concat_with_discriminator`, so nothing in
   the current tree can produce `Some` at all.
2. **`output_schema()`'s branch on the field is additive.** The `None` arm is
   textually the same clear-and-return the code already did — `s.unique_keys.clear(); ... Ok(s)`
   — with the `Some` branch reached only when the field is populated. This is
   exactly what `merge_drops_the_branches_unique_keys` and
   `merge_and_setop_agree_on_unique_keys` assert, and both pass unchanged.
3. **`Serialize`/`Deserialize` back-compat.** `#[serde(default)]` on the field
   means a pre-#228 serialized `Concat` (missing the field entirely)
   deserializes to `None`, matching its old unconditional-drop behavior. No
   test in the repo constructs raw JSON for a `Concat` node to check this
   directly, but the field shape follows the same `#[serde(default)]`
   convention already used elsewhere on this enum (e.g. `Aggregate.output_names`,
   `Scan.predicates`) for exactly this reason.

### Safety argument: why the discriminator must be caller-proven, not inferred

`ConcatDiscriminatorKey`'s fields are private; the only constructor,
`ConcatDiscriminatorKey::new(discriminator, inner_key)`, takes `discriminator`
as a required, explicitly-named argument — there is no default, no inference
from the branches' schemas, and no way to derive one structurally (e.g. "the
first column all branches disagree on"). This mirrors the same
private-field-plus-smart-constructor shape `GroupKeys` already uses in this
file for its own `by`/`without` invariant. Concretely, this means:

- `output_schema()` never has enough information to fabricate a discriminator
  on its own — it can only read one that a constructor already supplied.
- Nothing prevents a caller from asserting a **wrong** discriminator (one
  that isn't actually distinct per branch), or an `inner_key` that is not
  unique within every branch. The type system enforces "you named the
  columns," not "the compound key is valid." Both obligations are
  documented on `ConcatDiscriminatorKey` itself and have the same shape of
  unverified claim `QueryExpr::Dedup.cols` already carries elsewhere in this
  module (nothing checks a `Dedup`'s `cols` are actually a real key of its
  child either).
- The `no_way_to_fabricate_a_unique_key_without_naming_a_discriminator` test
  (in `query_expr.rs`) checks the two "how would you accidentally get
  `Some`?" shapes concretely: the ordinary `concat` builder, and a bare
  struct literal with `discriminator_unique_key: None` — both still produce
  `unique_keys: []`.

**Caveat, stated precisely (review fix, see "Review fixes" below): this is a
guarantee against *other Rust code*, not against arbitrary data.** Derived
deserialization builds the assertion directly from input values, bypassing
`new()`. Deserialization must therefore be treated exactly like a direct
caller assertion: an external `QueryExpr` boundary must reject this field or
establish both obligations above before using it as uniqueness evidence.
Rejecting unknown fields protects the assertion object from schema drift, but
cannot prove facts about the underlying rows.

### Tests added (`crates/types/src/pre_asap/query_expr.rs`)

- `merge_drops_the_branches_unique_keys` / `merge_and_setop_agree_on_unique_keys` —
  unchanged, still pass (default behavior untouched).
- `discriminator_override_produces_a_compound_unique_key` — `concat_with_discriminator`
  on two branches individually deduplicated on the same column (the exact
  "looks safe but isn't" shape `merge_drops_the_branches_unique_keys` warns
  about) yields `unique_keys == [[discriminator, inner_key…]]`.
- `ordinary_concat_struct_literal_still_drops_unique_keys_by_default` — the
  bare struct literal (`discriminator_unique_key: None`) still drops
  `unique_keys`, confirming the field addition didn't change the literal
  construction path's behavior.
- `no_way_to_fabricate_a_unique_key_without_naming_a_discriminator` — the
  misuse check described above.

## Review fixes

A code review of the initial implementation found two real correctness gaps
in the untested `resolve()`/`canonicalize()` path, plus a documentation
accuracy issue. All three are fixed on the same PR:

1. **`binder.rs`'s `collect_referenced_columns` didn't walk
   `discriminator_unique_key`'s `ColumnRef`s.** This function seeds every
   name a query references into the Binder's usage-derived fallback schema,
   which a schema-less `Scan` leaf (PromQL) falls back to. The `Concat` arm
   was updated with `, ..` only, unlike the analogous `Dedup.cols` case
   (which *is* walked, `push_ref_name`-style). Concretely: a future
   `concat_with_discriminator(branches, discriminator_col, inner_key)` call
   over an open query, where the discriminator column isn't otherwise
   referenced anywhere else in the tree, with a schema-less leaf `Scan` in
   the first branch — the Binder's fallback schema wouldn't contain the
   discriminator name, and `resolve.rs`'s later `resolve_column_ref` call
   would fail `NotFound` for a column the caller correctly named. Fixed:
   the `Concat` arm now pushes `key.discriminator()` and every
   `key.inner_key()` column into the walk, mirroring `Dedup.cols` exactly.

2. **Resolved `ColumnId`s in `discriminator_unique_key` could go stale after
   `canonicalize()` runs.** `resolve_root_with_inherited` calls `resolve()`
   first — which resolves the key's `ColumnRef`s into `ColumnId`s against
   `children.first()`'s output schema *as it stood at that point* — then
   `canonicalize()` runs afterward and can restructure that same first
   branch: `try_promote_heavy_hitter` and `try_rewrite_rownumber_topk` both
   replace a `Limit{Sort{Aggregate}}`/`Filter{...}` shape with a
   differently-shaped `Aggregate`, anywhere within the branch (not only at
   its own top level — the walk is recursive), potentially changing its
   column count/order. `output_schema()` read the previously-resolved
   `ColumnId`s with no consistency check, so a future branch matching one of
   these rewrite triggers could silently produce a wrong `unique_keys` claim
   — a wrong query answer, not a missed optimization (per `cse.rs`'s own
   module doc). Fixed in `canon()` (`canonicalize.rs`): before recursing
   into a `Concat`'s children, if `discriminator_unique_key` is `Some`,
   snapshot `children.first()`'s output schema — exactly the schema
   `resolve.rs` resolved the key against. After the children have been
   canonicalized, re-derive that schema and compare by full equality
   (`Schema` is `PartialEq`/`Eq`); any difference at all — not just a
   column-count/type change, since a same-shaped-but-different schema is
   just as unsafe to trust positionally — drops the key (`None`) rather
   than risk keeping a `ColumnId` that now points at the wrong column or is
   out of bounds. The key is never *re-derived* by guessing at name or
   position: the two rewrites don't preserve column identity in a way
   that's safe to infer, so dropping is the only sound outcome once the
   schema has moved. Two new tests in `canonicalize.rs`'s test module cover
   both outcomes: `concat_discriminator_key_survives_canonicalize_when_first_branch_is_unaffected`
   (an untouched branch keeps its key) and
   `concat_discriminator_key_is_dropped_when_first_branch_gets_rewritten`
   (a branch matching the heavy-hitter promotion trigger — 2 columns
   collapsing to 1 — drops its key, never keeps a wrong one).

3. **Documentation overclaimed the privacy guarantee.** Both the doc comment
   on `ConcatDiscriminatorKey` and this document's "Safety argument" section
   said flatly "there is no path to a non-empty `unique_keys` claim that
   doesn't go through a call site literally writing out which column it's
   proving is distinct" — true for other *Rust code*, but not for
   `#[derive(Deserialize)]`, which is same-module generated code that builds
   the struct directly from field values in arbitrary JSON, bypassing
   `new()` entirely. Fixed by narrowing both to "from other Rust code",
   rejecting unknown assertion fields, and making the external boundary's
   validation obligation explicit. Deserialization itself still cannot prove
   data-level uniqueness.

## Future work (explicitly out of scope here)

- Wiring `concat_with_discriminator` into `histogram_quantiles` (discriminator
  readily available; no current downstream consumer).
- Reopening SQL's rejection of `GROUPING()` so `lower_grouping_sets` has a
  real discriminator (`__grouping_id`) to assert — a separate design decision.
- Any canonicalization rule or CSE/rollup scenario that would actually *read*
  a `Concat`'s asserted `unique_keys` for the first time in the current tree.
