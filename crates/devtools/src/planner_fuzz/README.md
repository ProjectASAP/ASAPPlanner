# planner_fuzz

A metamorphic fuzz tester for **missed reuse and sharing** in the planner.

A missed-reuse bug is a plan that no candidate rule generates. The answers are
still correct, and `stage_pipeline --exhaustive` cannot see the bug, because it
only searches the generated candidates. Example (#903): Q1 is
`quantile_over_time(0.9, data[5m])` with an exact target, and Q2 is the same
query at ε = 0.01. The planner builds a KLL for Q2 (0.166 per second) instead
of reading Q1's exact result (0.144 per second).

`planner_fuzz` finds such bugs by relating two workloads whose costs must
compare in a known way.

Audience: planner developers.

## Run it

```sh
cargo run --release -p asap-devtools --bin planner_fuzz -- \
  --seed 1 --cases 1000 --time-budget-s 3600 --out out/run-1 \
  [--relations a,b,tighter-accuracy] [--search legacy|memo-brute-force|exhaustive] \
  [--seed-workload eval/rangeheavy.workload.json]... [--ignore-accuracy-guarantees] \
  [--relative-tolerance 0.01] [--absolute-tolerance 1e-6] \
  [--max-shrink-attempts 60] [--case-timeout-s 120]
```

A run writes these files:

- `summary.md` and `summary.json`: checks per relation (holds, violated,
  distinct violations, inconclusive, crashed), the commonest inconclusive
  reasons, the shrunk findings, cases that timed out or crashed, and the
  design-doc coverage table below with this run's status.
- `findings/<id>-<relation>.md`: one issue draft per distinct shrunk violation,
  in the `asap-issue-authoring` layout. It starts with the example (both
  workloads, both `stage_pipeline` commands, the costs, the selected and
  witness plans, and the violated relation). Next to it are
  `<id>-base.workload.json` and `<id>-related.workload.json`.
  **Nothing is filed**: a person reads the drafts, checks them against the open
  issues, and files the new ones.

A run is reproducible from `--seed`, the same binary and the same seed files.

Each case and each shrink runs in a worker process (the same binary with
`--worker`). A worker is killed after `--case-timeout-s`, so one slow planning
cannot stall the run. Timed-out and crashed cases are listed in the summary:
planning that slow is a finding of its own.

## Workloads

- **Generated** (seeded RNG): 1–3 queries in one language. The data facts are
  declared: series count (100, 1,000 or 10,000), sample interval (10 s or
  15 s), and distinct values per label.
  - PromQL over a gauge `m_gauge` and a counter `m_counter`, both with labels
    `job` (5 values), `instance` (50) and `region` (3).
    - Selectors: instant, or one of `rate`, `increase`, `sum_over_time`,
      `count_over_time`, `avg_over_time`, `max_over_time` and
      `quantile_over_time`.
    - Optional outer aggregation: `sum`, `count`, `max`, `min`, `avg`,
      `quantile` or `topk`, `by` or (rarely) `without`.
    - Optional equality matchers.
  - SQL over a table `metrics(ts, city, host, value)` with `SUM`, `COUNT`,
    `AVG`, `MAX`, `MIN`, `percentile_cont` or `COUNT(DISTINCT host)`, an
    optional `GROUP BY` and equality filters.
  - Windows: 1, 2, 5 or 10 minutes. Repeat intervals: 10, 30 or 60 s.
  - Accuracy targets, from the ladder exact → (0.01, δ 0.001) →
    (0.05, δ 0.01) → (0.1, no δ).
  - A latency bound on 10% of queries.
  - Half the time a new query is a small edit of an earlier one in the same
    workload, so the queries overlap.
- **Seed files** (`--seed-workload`; a third of the cases): 1–3 repeating
  queries drawn from a `PlanningWorkload` file, with the SQL tables from a
  `tables.json` next to it. A file may instead be
  `{"workload": ..., "tables": [...]}`.
  Seed queries have no structured form. Only the relations that need no
  structure apply to them: a, f, i, tighter-accuracy, added-query (when
  structured), no-raw-retention, results-retained, relaxed-latency and
  reordered.

## Relations

### Marginal-cost relations

For a query `q` of workload `W` and a query `q'` derivable from `q`, the tester
builds a **witness**. The witness is `W`'s selected logical plan (its Stage 1
candidate) plus one more root that derives `q'` from `q`'s node in that plan,
by the operator the relation names. It then checks:

```text
cost(W ∪ {q'}) − cost(W) ≤ derive_cost + max(rel_tol · cost(W ∪ {q'}), abs_tol)
derive_cost = cost(witness) − cost(W's plan)
```

**How `derive_cost` is computed.** It comes from Stage 3's per-node prices,
never from a guess:

1. Stage 2 and Stage 3 implement and price the witness the same way they price
   any candidate. Each materialization assignment goes through
   `optimize_physical_asap` and `select_plan`, using the same cost model,
   deployment capabilities, accuracy checks and executor compile check as the
   search. The cheapest assignment is kept.
2. `W`'s selected logical plan is priced the same way. This gives the witness
   a reference point on the same footing.
3. `derive_cost` is the difference between the two. Under cost model 1, a
   candidate's cost is the sum of its nodes' prices. So `derive_cost` is the
   price of the derived operator alone, plus any change it causes in the
   shared nodes' prices (for example, a shared node now materialized once
   instead of computed per reader).
4. Each draft also lists the witness's per-node cost of the nodes only `q'`
   reads, and their operators.

Two rules decide whether a check counts:

- A witness that Stage 2, Stage 3 or validation rejects makes the check
  *inconclusive*, never violated.
- A `Filter` on a group key keeps its input's guarantee in the witness. This
  is because whole groups selected by an exact key keep each remaining row's
  bound. Stage 1's guarantee composition gives no guarantee to any filter over
  approximate rows.

| Relation | `q'` from `q` | Derived by (witness) |
|---|---|---|
| `a-looser-accuracy` | `q` at the next looser target | reading `q`'s node |
| `b-filter-on-group-key` | grouped `q` plus `label = 'value'` on a group key | `Filter` over `q`'s result |
| `c-coarser-grouping` | `q` grouped by one key fewer | exact `Aggregate` (sum / max / min) over `q`'s result, or a sketch `SummaryMerge{group_by}` over `q`'s state |
| `d-nested-shorter-window` | `q` over a shorter window that divides it | `SummaryMerge{newest_panes}` over `q`'s pane chain |
| `e-other-quantile-rank` | `q` at another rank | `q`'s estimate (or exact quantile aggregate) at the other rank, same input |
| `f-exact-duplicate` | `q` | reading `q`'s node |
| `g-outer-over-result` | `topk(3, q)` | the lowering of `topk(3, q)` grafted onto `q`'s node |
| `h-other-statistic` | the count of `q`'s input | `SummaryEstimate(Total)` from `q`'s state, when its family answers it (`FamilyCapability::answers`) |
| `i-other-repeat-interval` | `q` at a longer repeat interval | reading `q`'s node |
| `j-smaller-topk` | `q`'s top-k at k = 3 | `q`'s `Limit` / `TopK` at 3 over the same input |
| `k-union-of-selections` | `q` over `label =~ "v1\|v2"`, with `q`'s `v2` twin added to `W` | `SummaryMerge` of the two states, then `q`'s estimate |

### Monotonicity and invariance relations

`W'` is `W` changed as the relation says.

| Relation | `W'` | Holds when |
|---|---|---|
| `tighter-accuracy` | one query's target one step tighter | `cost(W') ≥ cost(W) − tol` |
| `added-query` | `W` plus a random query | `cost(W') ≥ cost(W) − tol` |
| `no-raw-retention` | raw data not retained | `cost(W') ≥ cost(W) − tol` |
| `results-retained` | query-time results retained across evaluations | `cost(W') ≤ cost(W) + tol` |
| `relaxed-latency` | one query's latency bound removed | `cost(W') ≤ cost(W) + tol` |
| `renamed` | metrics (tables) and labels (columns) renamed | `|cost(W') − cost(W)| ≤ tol` |
| `reordered` | queries in reverse order | `|cost(W') − cost(W)| ≤ tol` |
| `disjoint-copy` | `W` plus a copy over renamed sources | `|cost(W') − 2·cost(W)| ≤ tol` |

When a monotonicity relation is violated, the draft also prices the other
side's logical plan for this side's workload. That is a concrete plan the
search could have selected.

## Shrinking

A violation whose signature is new gets shrunk. The signature is the relation
plus each query's shape without its constants. The shrinker greedily applies
the first change that still reproduces the violation, until no change does or
`--max-shrink-attempts` checks have run. In order, it tries:

1. drop another query;
2. drop a latency bound;
3. make a target exact;
4. drop a matcher, filter or grouping label;
5. drop the outer aggregation;
6. replace a range function with `sum_over_time`.

Each distinct shrunk signature gets one draft.

## Design-doc coverage

Each reuse pattern that the design docs say the planner supports (or plans to
support) is listed below with the relation that checks it. Doc paths are under
`docs/design_docs/`, with line ranges on main at the time of writing.
`summary.md` repeats the table with each run's status. The status column here
is from the one-hour run in the PR that added the tool (12 seeds, default and
memo searches).

<!-- coverage-status -->
