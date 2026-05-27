# `intent_algebra` reconciliation plan (ASAPController ⇄ ASAPQuery-backend)

> **Status (prerequisite done):** the *intra-repo* reconciliation — PR #4's
> name-based SQL L3 ⇄ PR #5's positional L3 — is complete on `feat/promql-l1-l3`.
> Both front ends (PromQL + SQL) now lower onto **one positional IR** via the
> shared `convert_root`: `expr_ir` is the SQL∪PromQL scalar superset, `AggIntent`
> carries `col: Option<ColumnId>`, the converter is bottom-up schema-aware, and
> the DataFusion front end emits relational L2. The plan below (ASAP ⇄
> control_plane) is the *next* step, now unblocked.

ASAPController's `crates/core/src/intent_algebra` is a slimmed, refactored fork
of the canonical L3 IR in `ASAPQuery-backend/control_plane/src/intent_algebra`.
This is the first concrete step of the L4/L5 consolidation: produce **one**
canonical `intent_algebra` so that (a) both repos stop drifting, and (b)
control_plane's L4 (optimizer + sketch_algebra) and L5 (physical + emit) can be
ported onto the shared IR the PromQL/SQL front-ends already lower into.

**Base = control_plane's L3** (the richer original); ASAPController's
correctness fixes and multi-language front-end are layered on top.

## Drift summary (evidence)

Line counts, `intent_algebra/*.rs`, control_plane (CP) vs ASAPController (ASAP):

| file | CP | ASAP | finding |
|---|---:|---:|---|
| `schema.rs` | 332 | 332 | **byte-identical** (already shared) |
| `query_expr.rs` | 1193 | 445 | ASAP relational nodes ⊆ CP; CP's "extra" variants are the **inlined scalar IR** ASAP split into `expr_ir.rs` |
| `agg_intent.rs` | 673 | 245 | **additive both ways**: CP has 11 ASAP lacks; ASAP has `StdDev`/`Variance` CP lacks |
| `relational.rs` | 920 | 212 | CP superset, **incl. `Project`** (ASAP lacks; SQL needs it) |
| `lower.rs` | 845 | 294 | CP richer; ASAP carries the per-branch-binding fix + accuracy threading |
| `binder.rs` | 300 | 189 | CP richer; same `SchemaCatalog`/`UsageDerivedCatalog` API |
| `cse.rs` | 324 | 228 | ASAP carries the structural-`PartialEq` key fix |
| `column_resolution.rs` | 465 | 177 | CP richer |
| `mod.rs` | 125 | 41 | **same export shape**, CP exports more |
| `expr_ir.rs`, `names.rs` | inlined | separate | file-organization difference |

Verdict: a **contained merge**, not a rewrite. `schema.rs` is already shared, the
node set is a clean superset, `AggIntent` is a union, and the public module API
matches. Cost concentrates in `lower.rs` and `expr_ir.rs`.

Behavioral specifics found in CP:
- `convert_root(legacy)` takes **no accuracy** — it hardcodes `Exact` /
  `Epsilon(0.05)` in the converter. ASAP threads a per-query `AccuracyTarget`.
- CP's converter threads **one root schema to all branches** — i.e. it has the
  same per-branch-binding bug ASAP already fixed.
- CP `AggIntent` carries `accuracy` and the 11 extra intents
  (`Changes/Resets/Deriv/Delta/PredictLinear/Absent/Present/Idelta/Irate/HoltWinters/Frequency`),
  but **lacks `StdDev`/`Variance`** — it fans those out into a `Merge` of
  quantile aggregates in `lower.rs`.
- CP sources `AccuracyTarget` / `BindingName` from a `types_v2` module.

Two payoffs from basing on CP's L3:
- Several functions ASAPController currently **rejects** (`changes`, `resets`,
  `deriv`, `delta`, `predict_linear`, `absent`, `present`) gain intents → become
  lowerable.
- `irate` becomes distinguishable from `rate` (CP has a separate `Irate` intent);
  our `rate≡irate` equivalence was a consequence of the slimmer vocabulary.

## Decisions to settle (sign-off needed)

| # | Fork | Options | Recommendation |
|---|---|---|---|
| **D1** | `StdDev` / `Variance` | CP's `Merge`-of-quantiles fan-out **vs** ASAP's first-class `AggIntent` | **CP fan-out** (less L4 work); add first-class intents only if L4 can bind them |
| **D2** | scalar IR location | CP inlines in `query_expr`/`relational` **vs** ASAP's separate `expr_ir.rs` (`L3Expr`) | **ASAP's `expr_ir.rs`**, extended to CP's scalar superset |
| **D3** | shared scalar types home | CP `types_v2` **vs** ASAP `names.rs` + `types.rs` (`AccuracyTarget`, `BindingName`, `QueryId`) | **one `core::types` module**; both already expose the same names |

## File-by-file tasks

Base = control_plane's file unless noted; "port" = bring ASAP's delta onto the CP base.

| file | base | tasks | effort | risk |
|---|---|---|---|---|
| `schema.rs` | identical | **no-op** — adopt as-is | none | none |
| `query_expr.rs` | CP | adopt CP node set (ASAP ⊆ CP); per **D2** reference `expr_ir::L3Expr` instead of inline scalars; confirm CP covers ASAP methods (`output_schema_in`, …) | low | low |
| `agg_intent.rs` | CP | adopt CP's full vocabulary; resolve **D1**; reconcile `output_column` / `agg_accuracy` | low–med | low |
| `relational.rs` | CP | adopt CP (incl. `Project`); per **D2** point scalar refs at `expr_ir` | low | low |
| `lower.rs` | CP | **main work**: (1) port per-branch binding to `BinaryOp`/`Join`/`SetOp` arms; (2) add `acc: &AccuracyTarget` to `convert`/`convert_root`, replace hardcoded defaults; (3) verify nested-aggregate convert supports the two-level `sum(rate)` shape; (4) reflect **D1** | **med** | med |
| `binder.rs` | CP | adopt CP; verify `SchemaCatalog`/`UsageDerivedCatalog` parity | low | low |
| `cse.rs` | CP | **port** the structural-`PartialEq` key fix if CP keys by `Debug` | low | low |
| `column_resolution.rs` | CP | adopt CP | low | low |
| `expr_ir.rs` | ASAP (keep, **D2**) | **med work**: extend `L3Expr` to CP's scalar superset (`FunctionCall`, `InList`, `Between`, `IsNull`, `ScalarSubquery`, `Cast`) + our `Regex`/`NotRegex` | med | low |
| `names.rs` | merge → `types` | fold `BindingName`/`QueryId` into the shared types module (**D3**); update CP's `types_v2` imports | low | low |
| `mod.rs` | CP + ASAP | union the exports; keep `expr_ir` + shared `types` | low | low |

## Cross-cutting (part of the merge, outside `intent_algebra`)

- **`types_v2` → `core::types`** (D3): one home for `AccuracyTarget`, `BindingName`, `QueryId`.
- **Front-ends**: keep ASAPController's **PromQL + SQL** lowering (CP has PromQL only) and the correctness fixes (`sum(rate)`, `histogram_quantile`, matcher canonicalization) — retarget onto the unified `QueryExpr`. Lives in `crates/lower`.
- **Tests**: bring the `conformance` / `equivalence` / `corpus` suites onto the unified crate and merge with CP's `lower.rs` / `cse.rs` unit tests. Use the full suite as the acceptance gate.

## Recommended order

1. `schema.rs` (free) + `types`/`names.rs` (D3) — shared primitives.
2. `expr_ir.rs` to CP's scalar superset (D2) — unblocks the type layer.
3. `query_expr.rs` + `relational.rs` + `agg_intent.rs` (D1) — the type layer.
4. `binder.rs` + `column_resolution.rs` + `cse.rs` (+ port the CSE key fix).
5. `lower.rs` — per-branch binding + accuracy threading (the one med-risk file).
6. Retarget front-ends + bring tests; run the full ASAPController suite against the unified crate.

## Effort summary

- **adopt-CP (low):** schema, query_expr, relational, binder, column_resolution, mod.
- **port-our-fix (low):** cse, names/types.
- **real work (med):** `lower.rs` (per-branch binding + accuracy), `expr_ir.rs` (scalar superset).
- **decision with semantic weight:** D1 only.

Open prerequisite (tracked separately): host the unified crate as **(A)** a cross-repo shared crate or **(B)** absorb control_plane into the ASAPController monorepo (design.md §5/§8). Recommendation: **B** for the core IR — a cross-repo git dependency on the central IR forces lock-step two-repo changes (cf. the `promql-parser` private-mirror friction).
