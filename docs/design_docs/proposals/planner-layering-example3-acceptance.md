# Planner layering, Example 3: acceptance spec

Audience: planner designers and the implementer of Pass 2's
window-composition rule.
Source: [planner-layering.md](planner-layering.md), "Example 3: Aggregation over
windows" and the Pass 2 section. Tests:
`crates/integration-tests/tests/planner_layering_example3.rs` (helpers and the
shared Example 3/4 workloads in `tests/planner_layering_common/`). Devtool:
`stage_pipeline --example planner-layering-3a` and `planner-layering-3b`.

This spec was written by a test designer who does not implement the stages.
Tests run the stages through `planner_layering_common::run_stages`. A test
that needs a missing feature is `#[ignore]`d with the PR that delivers it.

## Workload

Shared data workload, except Pattern A's `arrival` is `mixed`.

**Pattern A.** One `query_batch`, `invocations: 1` at T, `ad_hoc`, ε = 0.005,
δ = 0.01. Tests use T = 2026-01-01T00:00:00Z and a 365-day year.

| Query | `lookback` | `as_of` |
|---|---|---|
| q1 `quantile_over_time(0.99, latency_ms[5y])` | 5 y | T |
| q2 `quantile_over_time(0.99, latency_ms[1y])` | 1 y | T |
| q3 `quantile_over_time(0.99, latency_ms[1y] offset 1y)` | 1 y | T − 1 y |
| q4 `quantile_over_time(0.99, latency_ms[1y] offset 2y)` | 1 y | T − 2 y |
| q5 `quantile_over_time(0.99, latency_ms[3y] offset 2y)` | 3 y | T − 2 y |

**Pattern B.** `quantile_over_time(0.99, latency_ms[5m])`, every 1 min,
`lookback` 5 m, `as_of` evaluation time, ε = 0.01, δ = 0.01, ≤ 200 ms.

## Expected candidates per stage

| Stage | Doc | Today |
|---|---|---|
| Language-specific frontends | A: five scan → range → quantile chains. B: one. | Same; q3–q5 also have a `time_shift`. |
| Logical ASAP-aware optimization, Pass 1 | A: exact or KLL over its own interval per query, 2⁵ = 32. B: exact, KLL. | A: exact, KLL (k = 547), DDSketch per query, 3⁵ = **243**. B: exact, KLL (k = 269), DDSketch, **3**. |
| Logical ASAP-aware optimization, Pass 2 | A: window composition adds one EH of KLLs over [T − 5 y, T], one merge + p99 estimate per query, plus a candidate for every grouping of two or more queries onto shared window summaries ("hundreds"). B: each Pass 1 option × {none, sliding L = 5 min s = 1 min (5 active windows), 1-min tumbling (merge 5)} = 6. Independent kept. | Not implemented: no shared input, no window summaries. |
| Physical ASAP-aware optimization | See Example 4. | One physical candidate per logical candidate, every sub-DAG `NotMaterialized`. |
| Plan selection | Not stated for Example 3. | Every candidate is valid. A: five independent KLLs (DDSketch ties on cost). B: KLL (DDSketch ties). |

## Invariants and tests

### Pattern A

| Invariant | Test | Status |
|---|---|---|
| Both workloads are valid; Pattern B repeats every 1 min over 5 min | `workload_encodes_example3` | passes |
| Each query lowers to scan → (time shift) → range → quantile | `language_specific_frontends_a_lowers_each_query_to_its_interval` | passes |
| One summary-free frontend DAG; the queries share no node | `language_specific_frontends_a_one_summary_free_dag_without_sharing` | passes |
| Each query has an exact and a KLL option | `logical_asap_optimization_a_pass1_offers_exact_and_kll_per_query` | passes |
| Five independent KLLs, each read by one query, all the same size | `logical_asap_optimization_a_keeps_five_independent_klls` | passes |
| The identical-expression rule shares only raw input (scan, range, shift) and keeps the unshared variant | `logical_asap_optimization_a_identical_expression_rule_shares_only_raw_input` | ignored: #588 |
| One EH of KLLs over ≥ 5 y is read by all five queries; each has its own merge feeding a p99 estimate | `logical_asap_optimization_a_window_composition_adds_one_eh_for_all_five` | ignored: #628, #634 |
| The EH candidate coexists with the five independent KLLs | `logical_asap_optimization_a_keeps_independent_and_shared_window_summaries` | ignored: #628, #634 |
| Every pair of queries shares a window summary in some candidate | `logical_asap_optimization_a_window_composition_groups_every_pair` | ignored: #628, #634 |
| One selected id, the rest explained; the selected plan is the cheapest valid one | `plan_selection_a_selects_cheapest_valid` | passes |
| Each node of a valid candidate is charged once per consuming query while `NotMaterialized` (all today), once when materialized | `plan_selection_a_charges_each_node_per_consuming_query` | passes |
| For the same per-computation choices, sharing the scan costs no more than separate scans | `plan_selection_a_shared_scan_is_not_costlier` | ignored: #588, #632 (see ambiguity 7) |

### Pattern B

| Invariant | Test | Status |
|---|---|---|
| The query lowers to scan → range 5m → quantile, no summary | `language_specific_frontends_b_lowers_to_one_range_quantile` | passes |
| Exact and KLL options | `logical_asap_optimization_b_pass1_offers_exact_and_kll` | passes |
| Each summary option appears with no window, a 5-min sliding window with a 1-min slide, and 1-min tumbling windows | `logical_asap_optimization_b_window_composition_adds_sliding_and_tumbling_per_option` | ignored: tumbling #601, sliding no PR yet |
| Window parameters are legal: L divides W; s divides L and the evaluation interval; a tumbling length divides W and the interval | `logical_asap_optimization_b_window_parameters_are_legal` | passes (vacuously today) |
| Sliding with L = W has no merge; tumbling merges before the estimate | `logical_asap_optimization_b_merge_only_where_the_window_form_needs_it` | ignored: tumbling #601, sliding no PR yet |
| A window form that merges uses a mergeable summary (KLL, DDSketch), in both patterns | `logical_asap_optimization_window_merges_use_mergeable_summaries` | passes (vacuously today) |
| One selected, cheapest valid, each node charged once | `plan_selection_b_selects_cheapest_valid` | passes |

**Reading window summaries.** The IR has no window summary yet, so tests read
it through `planner_layering_common::window_form(dag, build)`, which returns
`WindowForm::None` today. The window-composition implementer makes it return
`Sliding { length_ms, slide_ms }`, `Tumbling { length_ms }` or
`ExponentialHistogram { horizon_ms }` from the new IR. Merges are read as
`SummaryMerge` nodes between the build and the `SummaryEstimate`.

## Ambiguities and conflicts with the planner

1. **Pass 1 families.** The doc offers exact and KLL; the planner also offers
   DDSketch. Tests check presence of exact and KLL, and require window forms
   for every summary option.
2. **Exact in window forms.** Pattern B counts 2 × 3 = 6, so the exact option
   also gets sliding and tumbling forms. Tumbling needs a mergeable exact
   state for quantiles (the retained values), which the planner has no
   accumulator for. Tests do not require window forms for exact.
3. **Groupings.** "One candidate for every way of grouping two or more queries
   onto shared window summaries" leaves open whether a grouping is a set
   partition (several shared summaries at once) and how ungrouped queries
   combine with their Pass 1 options. Tests require only every pair and all
   five.
4. **Yearly tumbling windows** "would also work" for Pattern A. The doc does
   not say whether Pass 2 generates them. Tests neither require nor forbid
   them.
5. **EH accuracy.** A boundary inside an EH bucket is approximate. Whether the
   accuracy model charges that against ε = 0.005 is not stated; not tested.
6. **`as_of` and `offset`.** The table gives `as_of` = T − 1 y and the query
   text has `offset 1y`. We read `as_of` as the window's end (the result of
   the offset at T), not as a second shift. The frontend lowers the offset
   from the text.
7. **Plan selection statistics.** Time-shifted scans are priced as one minute
   of data instead of years, and a 1-y range over a shared 5-y scan passes
   all of its rows (#632). Once Pass 2 shares the scan (#588), these
   estimates decide whether sharing looks cheaper, so the invariant test
   waits on both.
8. **Identical-expression sharing.** Not mentioned by the doc. Once
   implemented (#588), the planner can share the `latency_ms` scan and the
   2-y time shift read by q4 and q5 (the shift sits below the range).
9. **Additional sliding forms.** L shorter than W (merged) is allowed by the
   Pass 2 rules but not listed for Pattern B. Tests allow extra forms.
10. **Latency.** Pattern B's ≤ 200 ms is not checked by any test; plan
    selection does not check latency yet.
