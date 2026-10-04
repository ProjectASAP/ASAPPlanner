# Planner layering, Example 4: acceptance spec

Audience: planner designers and the implementers of Stage 2 materialization
and its pricing in Stage 3.
Source: [planner-layering.md](planner-layering.md), "Example 4: Materialization
of window summaries in physical planning" and the Materialization section.
Tests: `crates/integration-tests/tests/planner_layering_example4.rs` (helpers
and workloads in `tests/planner_layering_common/`).

This spec was written by a test designer who does not implement the stages.
Tests run `plan_selection::plan_stages`. A test that needs a missing feature
is `#[ignore]`d with the feature's name.

## Workloads

Example 3's two patterns ([its spec](planner-layering-example3-acceptance.md)),
with Pattern A varied as the doc does:

| Variant | Recurrence | Predictability | `arrival` |
|---|---|---|---|
| A, as given | batch, `invocations: 1` at T | `ad_hoc` | `mixed` |
| A, monthly | repeating every 30 days, window ending at each run | `Predictable { known_at: T }` | `mixed` |
| A, at rest | batch, `invocations: 1` at T | `ad_hoc` | `at_rest` (no ingestion rate) |
| B | repeating every 1 min | `Predictable` | `continuously_ingesting` |

## Expected physical candidates

Only the physical candidates of the shared logical candidate are specified;
every other logical candidate gets its own the same way (not counted).

**Pattern A, shared EH of KLLs (read by q1–q5).**

| Candidate | Materialization of the EH | Expected |
|---|---|---|
| A1 | query time, kept for the batch | generated; selected as given |
| A2 | ingestion time, old data backfilled once | generated unless `at_rest`; gains with monthly recurrence |
| A3 | not materialized: each query rebuilds it | generated; costs more than A1 |

**Pattern B, 1-min tumbling KLLs.**

| Candidate | Materialization of the tumbling KLLs | Expected |
|---|---|---|
| B1 | ingestion time, kept 5 min; merge 5 + p99 at query time | generated; usually selected |
| B2 | not materialized: rebuild all 5 from 5 min of raw samples | generated; costs at least B1 and B3 |
| B3 | query time, kept 5 min: build only the newest | generated; no ingestion-time node |

**Pattern B, sliding window (L = 5 min, s = 1 min).** Ingestion time or query
time, both kept. Not materializing it is the no-window plan, so it is not a
separate option.

**Today:** no window summaries and no materialization; every node runs at
query time, so none of these candidates exists.

## Invariants and tests

**Reading materialization.** The export records only an execution timing per
node. Tests read Stage 2's choice through
`planner_layering_common::materialization(p, node)` (`IngestionTime`,
`QueryTimeKept`, `NotMaterialized`; today it maps ingestion time to
`IngestionTime` and query time to `NotMaterialized`) and the kept event-time
span through `retention_ms(p, node)` (`None` today). The materialization
implementer makes both read the new IR.

### Constraints over every candidate

| Invariant | Test | Status |
|---|---|---|
| Every node upstream of an ingestion-time node also runs at ingestion time (all variants) | `stage2_ingestion_time_upstream_is_ingestion_time` | passes (vacuously today) |
| With data at rest, no node runs at ingestion time | `stage2_at_rest_runs_nothing_at_ingestion_time` | passes (vacuously today) |
| A materialized output covers its consumers: retention ≥ max over its readers of `lookback` + offset | `stage2_materialized_output_covers_its_consumers` | passes (vacuously today) |

### Pattern A

| Invariant | Test | Status |
|---|---|---|
| The shared EH yields exactly A1, A2, A3 | `stage2_a_shared_eh_has_three_materialization_options` | ignored: window composition and materialization |
| At rest, A2 is not generated; A1 and A3 remain | `stage2_a_at_rest_drops_the_ingestion_time_option` | ignored: same |
| A materialized EH is one build node read by all five queries, priced once | `stage2_a_materialized_eh_is_built_once_for_all_consumers` | ignored: same |
| A3 costs strictly more than A1 (five builds instead of one) | `stage3_a_rebuilding_per_query_costs_more_than_building_once` | ignored: same |
| As given, A1 is the cheapest of the three | `stage3_a_once_adhoc_prefers_the_query_time_eh` | ignored: same |
| Monthly, A2's cost relative to A1 drops: cost(A2)/cost(A1) monthly < as given | `stage3_a_monthly_amortizes_ingestion_time_maintenance` | ignored: same, and recurrence in `plan_stages` |

### Pattern B

| Invariant | Test | Status |
|---|---|---|
| The tumbling KLL candidate yields exactly B1, B2, B3 | `stage2_b_tumbling_kll_has_three_materialization_options` | ignored: window composition and materialization |
| B1 builds at ingestion time; its merge and estimate run at query time | `stage2_b_b1_builds_at_ingestion_and_merges_at_query_time` | ignored: same |
| B3 runs nothing at ingestion time | `stage2_b_b3_keeps_query_time_windows` | ignored: same |
| The sliding KLL yields exactly ingestion time and query time, kept | `stage2_b_sliding_kll_has_two_materialization_options` | ignored: same |
| B2 costs at least B1 and B3 | `stage3_b_rebuilding_every_window_costs_most` | ignored: same |
| With the built-in models, B1 is the cheapest of the three | `stage3_b_prefers_ingestion_time_tumbling_windows` | ignored: same |

### Selection direction, relative to the cost model

* A3 > A1 and B2 ≥ B1, B3 hold under any model that charges each build: A3
  and B2 do strictly more of the same work.
* A1 ≤ A2 as given, and B1 ≤ B3, depend on the model. The doc's reasons are
  that A2 maintains years of history for one batch, and that B3 adds the
  newest build to every evaluation and needs raw data at query time. The
  tests assert the doc's direction under the built-in models; if the model
  prices an ingestion-time update above a query-time one, B3 may win and the
  doc allows it ("B3 can win when ingestion-time work is expensive").
* Monthly recurrence is tested as a ratio, so it does not depend on whether A2
  wins outright ("A2 can win").

## Ambiguities and conflicts with the planner

1. **Charging A3.** Stage 3 charges each node once, which is right for a
   shared, materialized summary. A3 is one logical EH node that is not
   materialized, so it is rebuilt by each of its five consumers. The tests
   require A3 to cost more than A1. Either Stage 3 charges a non-materialized
   node once per consuming query, or Stage 2 duplicates it per consumer
   (which makes A3 look like five independent EHs). Decision needed.
2. **Recurrence input.** Materialization depends on `recurrence`,
   `predictability` and `arrival`, but `plan_stages` takes only the queries,
   accuracy targets and data workload. It needs the query workload (or the
   per-entry recurrence) for A2's amortization and for B1.
3. **Monthly variant.** The doc does not say whether each monthly run still
   ends at T or at its own run time, nor give `known_at`. Tests use a
   repeating entry whose window ends at each run, known at T.
4. **Retention unit.** "Kept 5 min" is read as the event-time span the stored
   state covers. For A1, which lives only for the batch, that is 5 y of
   history. The test requires retention ≥ each reader's `lookback` + offset.
5. **Backfill.** A2 backfills old data once. Whether that one-time cost is
   charged to the plan, and how it is amortized, is not stated; not tested.
6. **Pruning A2 at rest.** "A2 is not generated" is read as Stage 2 pruning
   (no candidate), not a Stage 3 rejection.
7. **Upstream of an ingestion-time node.** The constraint covers the scan and
   range too, so an ingestion-time build's whole input chain must be marked
   ingestion time in the export.
8. **Other logical candidates.** "Every other logical candidate gets its own
   physical candidates the same way" gives no counts; tests count only the
   shared candidate's options.
9. **B3 retention.** The doc says B3 keeps 5 min but uses "4 kept" windows
   plus the newest. The test asks for 5 min (the query window).

## Devtool

No `--example planner-layering-4`. Pattern A and B already run as
`planner-layering-3a` and `3b`, and until Stage 2 materializes something,
Example 4's variants plan identically. A useful id needs:

* Pass 2 window composition and Stage 2 materialization;
* the workload's recurrence passed to `plan_stages`;
* the variant (as given, monthly, at rest) selectable in `stage_pipeline`;
* the viewer showing each node's materialization and retention (today only
  `output_state.timing` is exported).
