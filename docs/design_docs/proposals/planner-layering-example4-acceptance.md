# Planner layering, Example 4: acceptance spec

Audience: planner designers and the implementers of materialization in
physical ASAP-aware optimization and its pricing in plan selection.
Source: [planner-layering.md](planner-layering.md), "Example 4: Materialization
of window summaries in physical planning" and the Materialization section.
Tests: `crates/integration-tests/tests/planner_layering_example4.rs` (helpers
in `tests/planner_layering_common/`; workloads in
`asap_devtools::planner_layering_examples`).

This spec was written by a test designer who does not implement the stages.
Tests run the stages through `planner_layering_common::run_stages`. A test
that needs a missing feature is `#[ignore]`d with the PR that delivers it.

## Workloads

Example 3's two patterns ([its spec](planner-layering-example3-acceptance.md)),
with Pattern A varied as the doc does:

| Variant | Recurrence | Predictability | `arrival` |
|---|---|---|---|
| A, as given | batch, `invocations: 1` at T | `ad_hoc` | `Mixed { rows_per_second }` |
| A, monthly | repeating every 30 days, window ending at each run | `Predictable { known_at: T }` | `Mixed { rows_per_second }` |
| A, at rest | batch, `invocations: 1` at T | `ad_hoc` | `AtRest` (no ingestion rate) |
| B | repeating every 1 min | `Predictable` | `Ingesting { rows_per_second }` |

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

**Today:** Pass 2 builds tumbling panes (`PaneChain`), and physical
ASAP-aware optimization enumerates `NotMaterialized`, `MatAtIngestion` and
`MatAtQuery` (state reused within one evaluation) for every summary build.
B1 and B2 exist. B3 (panes built at query time and kept across
evaluations) is offered when the deployment declares
`query_time_results_retained` in its `deployment_capabilities`: Stage 2 then
adds `MatAtQueryKeptAcrossEvaluations` for a tumbling-pane chain that builds
one new pane per evaluation. Pattern A's shared EH needs window segments shared across
queries (#628, #634), so A1–A3 do not exist yet. B3's
4 kept panes plus the newest cost 8,884.53 per second under cost model 1,
between B2 (2.183) and B1 (13,326.53).

## Invariants and tests

**Reading materialization.** The physical DAG records each node's
`Materialization` (#667). Tests read it through
`planner_layering_common::materialization(p, node)` without the storage
medium (`MatAtIngestion`, `MatAtQuery`, `NotMaterialized`), and the kept
event-time span through `retention_ms(p, node)`. The IR does not record
retention, so the adapter derives it the way plan selection prices kept
state: a chain of `N` panes of width `w` keeps `N·w`, plus one pane while
maintained at ingestion time; a whole-window build keeps its window, plus
the next one at ingestion time; each plus the offset of the shifts below
it.

### Constraints over every candidate

| Invariant | Test | Status |
|---|---|---|
| Every node upstream of an ingestion-time node also runs at ingestion time (all variants) | `stage2_physical_asap_optimization_ingestion_time_upstream_is_ingestion_time` | passes: B1 and the monthly variant's maintained panes |
| With data at rest, no node runs at ingestion time | `stage2_physical_asap_optimization_at_rest_runs_nothing_at_ingestion_time` | passes |
| A materialized output covers its consumers: retention ≥ max over its readers of lookback + offset | `stage2_physical_asap_optimization_materialized_output_covers_its_consumers` | passes: B1 keeps 6 min ≥ 5 min |

### Pattern A

| Invariant | Test | Status |
|---|---|---|
| The shared EH yields exactly A1, A2, A3 | `stage2_physical_asap_optimization_a_shared_eh_has_three_materialization_options` | ignored: the shared EH (#628, #634) |
| At rest, A2 is not generated; A1 and A3 remain | `stage2_physical_asap_optimization_a_at_rest_drops_the_ingestion_time_option` | ignored: the shared EH (#628, #634) |
| A materialized EH is one build node read by all five queries, priced once | `stage2_physical_asap_optimization_a_materialized_eh_is_built_once_for_all_consumers` | ignored: the shared EH (#628, #634) |
| A3 costs strictly more than A1 (five builds instead of one: a `NotMaterialized` node is charged per consuming query) | `stage3_plan_selection_a_rebuilding_per_query_costs_more_than_building_once` | ignored: the shared EH (#628, #634) |
| As given, A1 is the cheapest of the three | `stage3_plan_selection_a_once_adhoc_prefers_the_query_time_eh` | ignored: the shared EH (#628, #634) |
| Monthly, A2's cost relative to A1 drops: cost(A2)/cost(A1) monthly < as given | `stage3_plan_selection_a_monthly_amortizes_ingestion_time_maintenance` | ignored: the shared EH (#628, #634) |

### Pattern B

| Invariant | Test | Status |
|---|---|---|
| The tumbling KLL candidate yields exactly B1, B2, B3 | `stage2_physical_asap_optimization_b_tumbling_kll_has_three_materialization_options` | passes (with `query_time_results_retained`) |
| Until B3: the tumbling KLL candidate yields B1 and B2, both priced within the 200 ms latency bound (implementer test) | `stage2_physical_asap_optimization_b_tumbling_kll_has_b1_and_b2` | passes |
| B1 builds at ingestion time; its merge and estimate run at query time | `stage2_physical_asap_optimization_b_b1_builds_at_ingestion_and_merges_at_query_time` | passes |
| B3 runs nothing at ingestion time | `stage2_physical_asap_optimization_b_b3_keeps_query_time_windows` | passes |
| The sliding KLL yields exactly ingestion time and query time, kept | `stage2_physical_asap_optimization_b_sliding_kll_has_two_materialization_options` | ignored: sliding windows (no PR yet) |
| B2 costs at least B1 and B3 | `stage3_plan_selection_b_rebuilding_every_window_costs_most` | ignored: cost model 1 contradicts it. With query-time results retained, B3 keeps 4 panes for 8,884.5 cost/s and B1 6 for 13,326.5, while B2 rebuilds them for 2.183. Needs a rewrite relative to the cost model |
| With the built-in models, B1 is the cheapest of the three | `stage3_plan_selection_b_prefers_ingestion_time_tumbling_windows` | ignored: cost model 1 contradicts it. B1 keeps 6 panes of 1M per-series KLLs (about 107 GB) for 13,326.5 cost/s; B2 rebuilds them for 2.183, and the whole-window KLL (P2, 2.100) is selected. Needs B3 and a rewrite relative to the cost model |
| Each cost model chooses what its deployment pays for: cost model 1 the whole-window KLL; cost model 2 the exact plan (smallest peak memory); cost model 3 with lowest latency B1 in 256 GB, and the whole-window KLL in 64 GB, where B1 is invalid on peak memory | `stage3_plan_selection_b_cost_models_choose_differently` | passes |
| Under cost model 3, the Pareto front over peak memory, disk and latency is {exact, whole-window KLL, B1} and B1 is selected; B2 is dominated by the whole-window KLL; least memory selects the exact plan | `stage3_plan_selection_b_pareto_front_over_memory_disk_and_latency` | passes |

### Selection direction, relative to the cost model

* A3 > A1 and B2 ≥ B1, B3 hold under any model that charges each build: A3
  and B2 do strictly more of the same work.
* A1 ≤ A2 as given, and B1 ≤ B3, depend on the cost model (cost model 1, 2
  or 3). The doc's reasons are
  that A2 maintains years of history for one batch, and that B3 adds the
  newest build to every evaluation and needs raw data at query time. The
  tests assert the doc's direction under the built-in models; if the model
  prices an ingestion-time update above a query-time one, B3 may win and the
  doc allows it ("B3 can win when ingestion-time work is expensive").
* Monthly recurrence is tested as a ratio, so it does not depend on whether A2
  wins outright ("A2 can win").

## Ambiguities and conflicts with the planner

1. **Charging A3 (decided).** A3 is one logical EH node that is not
   materialized, so it is rebuilt by each of its five consumers. Plan
   selection charges a `NotMaterialized` node once per consuming query
   (`NodeCost::multiplier`) and a materialized node once; physical ASAP-aware
   optimization does not duplicate the node. So A3 costs five EH builds and
   A1 one, as the tests require. The A3 tests stay ignored only because the
   shared EH and its materialization options do not exist yet.
2. **Recurrence input (resolved).** Materialization depends on
   `recurrence`, `predictability` and `arrival`: `optimize_physical_asap`
   takes the workload entries (`QueryWorkloadEntry`: recurrence,
   predictability, accuracy target, latency bound) and the data workload,
   and `select_plan` the same entries.
3. **Monthly variant.** The doc does not say whether each monthly run still
   ends at T or at its own run time, nor give `known_at`. Tests use a
   repeating entry whose window ends at each run, known at T.
4. **Retention unit.** "Kept 5 min" is read as the event-time span the stored
   state covers. For A1, which lives only for the batch, that is 5 y of
   history. The test requires retention ≥ each reader's lookback + offset.
5. **Backfill.** A2 backfills old data once. Whether that one-time cost is
   charged to the plan, and how it is amortized, is not stated; not tested.
6. **Pruning A2 at rest.** "A2 is not generated" is read as physical
   ASAP-aware optimization not generating it, not a plan selection
   rejection.
7. **Upstream of an ingestion-time node.** The constraint covers the scan and
   range too, so an ingestion-time build's whole input chain must be marked
   ingestion time in the physical DAG.
8. **Other logical candidates.** "Every other logical candidate gets its own
   physical candidates the same way" gives no counts; tests count only the
   shared candidate's options.
9. **B3 retention.** The doc says B3 keeps 5 min but uses "4 kept" windows
   plus the newest. The test asks for 5 min (the query window).

## Devtool

No `--example planner-layering-4`. Pattern A and B run as
`planner-layering-3a` and `3b`, and Pattern B's materialization choices
(B1, B2, and B3 with
`--deployment-capabilities '{"query_time_results_retained": true}'`) appear in
the `3b` document; `--cost-model` selects cost model 1, 2
or 3 (with `--cluster-memory-bytes`, `--cluster-disk-bytes` and
`--cluster-objective` for cost model 3). A useful id still needs:

* the variant (as given, monthly, at rest) selectable in `stage_pipeline`;
* the viewer showing each materialized node's retention (it shows each
  node's materialization already).
