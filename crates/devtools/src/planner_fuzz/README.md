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
  [--relations a,b,tighter-accuracy] [--search candidate-connections|exhaustive] \
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
  - SQL over a table `metrics(ts, city, host, src_ip, value)` with `SUM`,
    `COUNT`, `AVG`, `MAX`, `MIN`, `percentile_cont`, `COUNT(DISTINCT host)`,
    or a frequency statistic of the high-cardinality `src_ip` (one value per
    series): `COUNT(DISTINCT src_ip)`, or the L2 or entropy idiom of
    planner-layering Example 2 (ungrouped). An optional `GROUP BY` and
    equality filters.
  - Windows: 1, 2, 5 or 10 minutes; a related query (below) may also take
    30 minutes or 1 hour. Repeat intervals: 10, 30 or 60 s.
  - Accuracy targets, from the ladder exact → (0.01, δ 0.001) →
    (0.05, δ 0.01) → (0.1, no δ).
  - A latency bound on 10% of queries.
  - Half the time a new query is a small edit of an earlier one in the same
    workload, so the queries overlap: another window, grouping, operator or
    quantile rank, `rate` swapped for `increase`, or another frequency
    statistic of `src_ip`.
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
by the operator the relation names. Three witnesses change that plan too: m
and n lengthen the pane chain `q` reads (so `q` merges its newest panes of
the longer chain); l's `q'` also reads a sibling query, so `W` is planned
with the sibling added; and h's second witness is `W ∪ {q'}` planned with
UnivMon as the only sketch family. Each is a plan for `W ∪ {q'}`. The tester
then checks:

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
| `g-outer-over-result` | an operator over `q`, by `q`'s outer operator: `topk(3, q)` (sum), `bottomk(3, q)` (count), `max(q)` (max), `quantile(0.5, q)` (avg), `q > 0` (min, quantile) | the operator's lowering grafted onto `q`'s node |
| `h-other-statistic` | SQL, ungrouped: another frequency statistic of `q`'s column (distinct count → row count, L2 → distinct count, entropy → L2) | a `SummaryEstimate` of it from `q`'s state, when its family answers it (`FamilyCapability::answers`); or `W ∪ {q'}` planned with UnivMon as the only sketch family, priced with every family |
| `i-other-repeat-interval` | `q` at a longer repeat interval | reading `q`'s node |
| `j-smaller-topk` | `q`'s top-k at k = 3 | `q`'s `Limit` / `TopK` at 3 over the same input |
| `k-union-of-selections` | `q` over `label =~ "v1\|v2"`, with `q`'s `v2` twin added to `W` | `SummaryMerge` of the two states, then `q`'s estimate |
| `l-binary-over-result` | PromQL: `q{label="v"} / q` on a label `q` does not group by, or `q / q_count` for a `sum_over_time` `q` with no outer operator or `sum` (avg as sum/count) | the division's lowering grafted onto `q`'s node |
| `m-longer-window` | `q` over the shortest longer window (up to 1 h) that is a multiple of `q`'s; not for `rate` or `increase` | `q`'s pane chain lengthened: every merge of it in `W`'s plan reads its old newest panes, and `q'` merges the whole chain |
| `n-longer-window-rollup` | `q` over a longer window, as in m, grouped by one key fewer | m's derivation, then c's rollup |
| `o-equivalent-range-function` | `q` with `rate` and `increase` swapped (not under `count`) | `q`'s result times (or divided by) the window in seconds, grafted onto `q`'s node |

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
is from a one-hour run when the tool was added: 12 seeds, the default search
and the memo search, 1,697 cases and 46,927 checks. The rows for g, h and l
to o are from 20 ten-minute seeds with the default search when those
relations were added (the patterns of the ASAPWorkloads query catalog,
`ProjectASAP/ASAPWorkloads` `docs/query-catalog.md`, cited by number). In the
status column, "conclusive" means the check either held or was violated.

| Pattern | Doc reference | Relation | Status on main (one-hour run) |
|---|---|---|---|
| Looser accuracy target reads the tighter build (an exact result meets any ε) | accuracy-models.md "Sharing across consumers" L308–368; planner-layering.md L530–533, L640–641 | `a-looser-accuracy` | VIOLATED (208 of 2,596 conclusive checks): #903 |
| Slice: a filter on a group key over a grouped result or summary (planned) | asap-primitive-schema.md "Other uses of coverage" L846–848, L118 | `b-filter-on-group-key` | VIOLATED (995 of 1,003): #891, #902 (#689 for approximate inputs) |
| Rollup to a coarser grouping: exact, or sketch `SummaryMerge{group_by}` | planner-layering.md rule table L588; asap-primitive-schema.md §5.6 L670, L754–794, L830–844; joint-planning-search.md L324 | `c-coarser-grouping` | VIOLATED (167 of 389): new drafts (exact rollup; sketch rollup over an inner range function) |
| Scan widening: scans of one metric with different labels share one scan | planner-layering.md Pass 2 L597–607 | `c-coarser-grouping` (sketch rollup witness) | covered by c; no separate violation |
| Nested window: a shorter window merges the newest panes of a longer window's chain | planner-layering.md L790–807; asap-primitive-schema.md §5.6 L669 | `d-nested-shorter-window` | VIOLATED (73 of 250): new draft (exact sums); compare #873 (rate/increase) |
| Another quantile rank from the same sketch | planner-layering.md L631–634, L804–817 | `e-other-quantile-rank` | VIOLATED (39 of 465), exact percentiles only: new draft (unshared exact aggregates) |
| Identical expressions (common-subexpression elimination) | planner-layering.md rule table L583–585, Algorithm 2 L655–658 | `f-exact-duplicate` | VIOLATED (92 of 2,915), exact query-time aggregates: new draft |
| A query over another query's result (top-k over a shared sum) | joint-planning-search.md L177–187; planner-layering.md L583–585 | `g-outer-over-result` | holds (552 conclusive checks). With the operator following q's (ASAPWorkloads #4, #12, #17, #18): VIOLATED (6 of 49), `topk(3, …)` over an exact instant sum and `… > 0` over an approximate quantile |
| Summary capability: another statistic the family answers (`FamilyCapability::answers`) | planner-layering.md rule table L586, L611–617, Example 2 L1306–1370 | `h-other-statistic` | KLL and KMV answer one statistic, so h now checks SQL frequency statistics of a high-cardinality column (planner-layering Example 2; ASAPWorkloads #57, #107–#108): holds (5 conclusive checks) |
| gcd pane grid across repeat intervals | planner-layering.md L791–809 | `i-other-repeat-interval` | VIOLATED (49 of 2,717): new draft; compare #731 |
| Exact top-k reuse (smaller k) | planner-layering.md rule table L589; joint-planning-search.md L325 | `j-smaller-topk` | holds (62 conclusive checks) |
| Merge of disjoint selections | asap-primitive-schema.md §4.2 L194–232, §5.6 Example B L721–752 | `k-union-of-selections` | never conclusive: Stage 1 composes no guarantee for the merged estimate |
| Panes kept across evaluations when query-time results are retained | planner-layering.md Materialization L889–893 | `results-retained` | holds (1,442 conclusive checks) |
| Window = interval: the whole window as a one-pane tumbling chain | planner-layering.md Materialization L895–905 | — | not checked: a Stage 2 choice within one query |
| Rollup over newest panes (windowed rollup) | planner-layering.md L809–823 | — | not checked: a composition of c and d |
| avg as sum/count: shared exact accumulators | planner-layering.md L372–383, L502–503; ASAPWorkloads #8 | `l-binary-over-result` | holds (15 conclusive checks, with the ratio row below); checks that the sum is shared, not one accumulator for both |
| A ratio with a shared operand: a filtered share of a rate (5xx share), one rate over another | ASAPWorkloads #11, #12, #30, #126 | `l-binary-over-result` | holds (see above) |
| Longer window over the same panes: one chain serves 5m and 1h windows | planner-layering.md L790–807; ASAPWorkloads #1–#2, #19, #122 | `m-longer-window` | VIOLATED (2 of 7): SQL count and percentile at a longer window build new state |
| Longer window rolled up to a coarser grouping | ASAPWorkloads #1–#2, #61–#62, #65 | `n-longer-window-rollup` | never conclusive yet: needs a grouped q planned with a pane chain |
| `rate` and `increase` of one counter read one state | ASAPWorkloads #1–#2 | `o-equivalent-range-function` | VIOLATED (6 of 6): the second function builds its own state; compare #873 |
| Stored-state reuse; sliding windows, exponential histograms, SummarySubtract | asap-primitive-schema.md L849, L851–869; planner-layering.md L618–630, L827 | — | not checked: planned or not implemented |

Monotonicity and invariance relations (no doc pattern; they guard the search):
- `added-query`, `no-raw-retention`, `relaxed-latency`, `renamed`, `reordered` and `disjoint-copy` held in every conclusive check.
- `tighter-accuracy` was violated 4 times in 2,152 checks. One case is a default-search miss: `--exhaustive` finds the plan. The others are #903.
