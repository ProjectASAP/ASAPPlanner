# Would the proposed tests have caught the #1207 stack bugs?

Source: "#1207 Stack Bug Ledger" (108 bugs, PRs 1–9, as of 2026-11-12).
Test IDs refer to [the testing infrastructure proposal](README.md). Classification is a judgement from
each bug's title, example and cause; nothing was run.

"Caught" assumes a test input reaches the buggy path. Invariants and
relations then fail automatically; a golden fails only on a *later change*,
or if the reviewer notices the blessed output is wrong.

## Summary

| Bucket | Bugs | Caught automatically? |
|---|---|---|
| A. Crash, invalid memo or plan | 14 | Yes, by invariants (T4, T5, T10, T25, T26, T28) or by any test that plans through the path |
| B. Missed sharing | 16 | Yes, by T8 positive cases, T9 fuzz, T15 order invariance, T3 same-group |
| C. Wrong reuse (shared what must not be) | 6 | Yes, if T8's negative cases are written |
| D. Cost model | 9 | 5 by a new invariant (T27); 4 not caught |
| E. Over-pruning (plans lost or infeasible) | 18 | 4 by T9/T13 relations; 14 only if golden cases state their expected outcome |
| F. Broken accuracy guarantee | 19 | 4 by T14 property tests; 15 need execution |
| G. Wrong answer (semantics) | 18 | No; need execution |
| H. Tooling, reports, wire format, duplication | 8 | Out of scope, or mutation testing |

Totals (108): ~49 caught automatically by proposed or suggested tests; ~14
caught only with an "expected outcome" assertion in golden cases; ~33 need
an execution oracle (deferred); ~12 out of scope or not caught.

## A. Crash, invalid memo or plan (14)

- `1230-1` sign derivation gave one more column than the schema → T25/T4.
- `1232-18`, `1232-F1`, `1234-F4` orphan groups after a failed way → T5,
  if memo consistency includes "no orphan groups".
- `1232-Q1` pending group ids miswired → T5.
- `1232-C4` schema checked after the memo write → T4.
- `1235-bug3` Stage 2 offers properties validation refuses → T10
  (`validate()` always on).
- `1236-old-5` realizer rejects a selected plan; `1234-G6` NaN in (K);
  `1238-old-6` post-check hard error; `1238-old-4` big-M infeasible → any
  Stage 3 test through the path crashes.
- `1235-bug2` raw buffers under a no-raw-data deployment → T26.
- `1235-H3`, `1235-L8a` selected plan the executor refuses → T28 below.

## B. Missed sharing (16)

- Rule positive cases (T8), especially with real front-end output, not
  hand-built memos (`1234-F3`, `1234-R4` fail only on production PromQL):
  `1234-old-C`, `1234-old-D`, `1252`, `1267`, `1234-F3`, `1234-G8`,
  `1234-R4`, `1232-F3ab`, `1227-old-Q4`, `1264-c`, `1233-ordinal`,
  `1233-selfjoin`.
- Order dependence → T15: `1234-C12`.
- Same-group assertions (T3): `1268` (NaN literal, only if a case has
  one), `951-scans`, `1232-F2`.
- `1252` is a regression of #902: a committed fuzz seed or golden catches
  it.
- T24 (rule removal) also flags a rule that never fires on production
  input.

## C. Wrong reuse (6)

T8 negative cases ("must not reuse"): `1234-old-B` (filters compared by
index across schemas), `1234-F6` and `1234-G1` (chain offered to a reader
whose grid it doesn't divide), `1233-A1` (unrelated computed keys equal),
`1266-a` and `1266-b` (different aggregates get one definition).

## D. Cost model (9)

New invariant needed (T27 below): `1236-old-2`, `1237-old-cost`,
`1237-bug1a` (negative edge cost), `1237-bug1c`, `1237-D3`.
Not caught: `1237-D8` (both sides share the buggy code), `1236-bug1b`,
`1236-bug4` (costs wrong by 1000×, consistently), `1236-D14`. These need a
cost reference (measured costs).

## E. Over-pruning (18)

- Relations: `1238-old-1` and `1234-F2` (one query's read changes
  another's plan) → T9 added-query relation; `1234-D1`, `1234-A1` →
  T13-style tighter/looser target relations.
- The rest show up as an exact or infeasible plan where a sketch plan was
  expected: `1234-old-A`, `1227-P1`, `1236-old-4`, `1236-I7`,
  `1238-old-2`, `1238-old-3`, `1234-bug6`, `1238-A6`, `1234-G5`,
  `1234-R2a`, `1234-R2b`, `1234-R3`, `1234-R6`.
  A golden catches these only if the case states what it expects
  (`1239-old-1`: snapshots recorded 9 of 10 sketch workloads as infeasible
  and nobody noticed). See "expected outcome" below.

## F. Broken accuracy guarantee (19)

- T14 property tests over generated values, including −0.0, NaN, ±Inf,
  q ∉ [0, 1]: `1230-neg0`, `1230-sign-q`, `1271`, `1230-sign-cs`.
- Not caught without execution (or a reviewer who knows the math):
  `1234-old-E`, `1230-flip-default`, `1234-F1`, `1234-Q6`, `1234-G2`,
  `1234-G3`, `1234-G4`, `1234-G7`, `1234-H1`, `1234-H8`, `1283`,
  `1234-R1`, `1234-R5`, `1238-old-5`, `1238-univmon-distinct`.

## G. Wrong answer (18)

Need execution (deferred), or a differential against DataFusion /
Prometheus: `1265` (CAST unit), `1233-C1`, `1233-or`, `1233-B6`, `1280`,
`1232-F3c`, `1232-Q5`, `1232-C9`, `1234-F5`, `1234-G9`, `1230-interval`,
`1230-defid`, `1264-a`, `1236-D15`, `1236-old-1`, `1236-old-3`,
`1239-gap7`, `1236-old-8`.
A T1/T6 golden shows some of these (`1264-a` in
`depends_on_evaluation_time`) but only a reviewer would notice.

## H. Tooling, reports, wire format, duplication (8)

- Test-tool bugs: `1239-old-1`, `1239-old-R9`, `1239-old-3`,
  `1239-old-4`, `1239-text-edges`, `1238-bug7` (report text).
- `1234-wire`: serialized plan changed without a version bump → a
  wire-format round-trip golden (not proposed).
- `1234-old-G` (errors swallowed), `1234-old-H` (two copies of a table
  disagree; `1232-Q5` in G is the same pattern) → mutation testing may flag these; a rule against
  duplicated tables would prevent them.

## What this suggests adding

Outcome: "no orphan groups" was added to T5. T27, T28 and expected-outcome
assertions were considered and not adopted; execution stays deferred.

- **T27 objective = plan_cost**: after every Stage 3 solve, the MILP
  objective equals `plan_cost` of the selected plan, and every edge cost is
  ≥ 0. Catches 5 bugs.
- **T28 selected plan is installable**: the planner validates the selected
  plan against the executor's capabilities (`installable::validate`) in
  every test. Catches 2.
- **Expected outcome in every golden case**: a case states "feasible",
  "uses a sketch" or "shares state X", checked as an assertion, so a wrong
  bless fails. Catches the over-pruning class that goldens alone missed
  (`1239-old-1`).
- **T5 includes "no orphan groups"**. Catches 3.
- **Revisit deferring execution**: ~35 accuracy and semantic bugs are
  invisible to every other oracle.
