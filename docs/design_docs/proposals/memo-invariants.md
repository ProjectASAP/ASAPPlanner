# Memo invariants

> Status: proposal; implemented by PR 2 of #1354. Audience: planner
> developers. Implements T4, T5 and T25 of the testing-infrastructure
> design (#1353), decisions D12–D14.

Example used below: `q0 = sum by (job) (rate(m[5m]))`, `q1 = sum(rate(m[5m]))`.
Stage 0 groups: G0 `Scan m` → G1 last 5m → G2 `rate` → G3 `sum by (job)`
(q0), G4 `sum` (q1). Pass 2 adds a candidate of G4 that sums G3.

## Error

A failed check returns `InvariantViolation { check, group, origin }`, never
a panic. `origin` is the rule recorded on the candidate that broke it, or
the stage after which the check ran. After Stage 0 or Pass 1 the planner
returns it as `PerComputationCandidateError::Invariant`, later as
`SelectionError::Invariant`, so `asap_planner` can give it an exit code
(PR 7).

## On insertion (always on)

In `Memo::find_or_add_group` / `Memo::add_candidate`, before the write.

1. A group has one lineage and one sign entry per column. G3 has 2 columns,
   so 2 of each.
2. A new group or candidate reads only existing groups. Reading G9 in a
   5-group memo fails (today: panics).
3. A new candidate has one input requirement per input.
4. A new candidate that does not type-check over its inputs, or whose
   sketch parameters do not fit its group, fails, naming its rule.

Not a violation: a candidate whose output differs from its group's. Pass 2
rules guess by shape and rely on this refusal; it stays an ordinary
rejection. Pass 1 already treats it as an error.

## After every stage (always on)

`Memo::check_consistency` and its siblings, called by the planner facade
(`run_planning_until`) after Stage 0, Pass 1, Pass 2 and Stage 2. Stage 3
only reads the memo, so after it only check 12 runs.

5. Each group's stored "used by" list is correct. G3's includes G4.
6. Each group's stored "queries" list is correct. G3's is {q0, q1}.
7. Every group is used by some query (no orphans).
8. No two groups compute the same expression.
9. No group holds the same candidate twice.
10. A stage only adds: no group or candidate it was given is gone; the
    queries are unchanged.
11. After Stage 2: every physical candidate points to an existing memo
    candidate.
12. After Stage 3: each query's plan output has its Stage 0 group's schema.
    q0's plan root outputs (`job`, value), as G3 does.

## After every stage, behind `--check-invariants`

These re-derive per candidate, so they are costly (D12). The golden runner
and `planner_fuzz` always pass the flag.

13. Every candidate has its group's output schema.
14. Every candidate has its group's `aggregate_coverage` (which rows of
    which data it summarizes), compared exactly. A false alarm (0–10m vs.
    0–5m + 5–10m) is reported before the comparison is loosened.
