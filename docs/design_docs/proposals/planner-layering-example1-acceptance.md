# Planner layering, Example 1: acceptance spec (MVP)

Audience: planner designers and the implementers of the planning stages.
Source: [planner-layering.md](planner-layering.md), "Example 1: Aggregation over
dimensions". Tests: `crates/integration-tests/tests/planner_layering_example1.rs`
(helpers in `tests/planner_layering_common/`). Viewer fixture:
`tools/dag-viewer/examples/planner-layering-example1.json`, written by
`stage_pipeline --example planner-layering-1`.

This spec was written by a test designer before the stages existed. A test
whose expectation needs a feature that is not in the stack yet is
`#[ignore]`d with the PR or issue that delivers it.

## Scope

| Stage | Doc (#509) | MVP (this spec) |
|---|---|---|
| Language-specific frontends | 1 workload `LogicalDAG` | same |
| Logical ASAP-aware optimization | Pass 1 (3) × identical-expression rule (2) × window-composition rule (3 × 3) = **54** | Pass 1 only = **3**. Pass 2 is not implemented: the identical-expression rule is #588, window composition #601. |
| Physical ASAP-aware optimization | Materialization options per window form = **156** | Physical operator implementation, every sub-DAG `NotMaterialized` = **3** (the doc's Raw plans). Materialization enumeration is #604. |
| Plan selection | 1 plan | 1 plan |

Every MVP candidate is one of the doc's candidates: in logical ASAP-aware
optimization, the one with separate inputs and no window summary for either
query; in physical ASAP-aware optimization, the one where both queries are
**Raw** (state rebuilt from the last 1 min of raw samples at every refresh).

## Workload

Shared data workload: `continuously_ingesting`, 15 s ingestion interval,
1,000,000 series, about 66,667 samples/s, `zipf`, volume unknown.

| Query | Repeats | `lookback` | `as_of` | Accuracy | Latency |
|---|---|---|---|---|---|
| Q1 `sum by (job) (rate(http_requests_total[1m]))` | 10 s | 1 m | evaluation time | exact | none |
| Q2 `topk by (job) (10, sum_over_time(http_requests_total[1m]))` | 10 s | 1 m | evaluation time | ε = 0.01, δ = 0.001 | ≤ 100 ms |

## Language-specific frontends: 1 candidate

| Query | Chain |
|---|---|
| Q1 | scan `http_requests_total` → range 1m → rate (per series) → sum by (job) |
| Q2 | scan `http_requests_total` → range 1m → sum_over_time (per series) → topk by (job) (10) |

Invariants:

* Exactly one candidate, with one root per query, in workload entry order.
* No summary node (sketch `SummaryAgg`, `SummaryEstimate`, `SummaryMerge`).
* Q1 and Q2 share no node. Sharing is a logical ASAP-aware optimization
  decision.

## Logical ASAP-aware optimization: 3 candidates

Pass 1 targets and their per-computation candidates:

| Query | Target | Candidates |
|---|---|---|
| Q1 | per-series `rate` | exact Rate accumulator |
| Q1 | per-`job` `sum` | exact Sum accumulator |
| Q2 | per-series `sum_over_time` | exact Sum accumulator |
| Q2 | per-`job` top-*k* | exact, Count-Min + heap per job, CountSketch + heap per job |

| Id | Q1 | Q2 top-*k* |
|---|---|---|
| L1 | exact | exact (sort + limit 10 per job) |
| L2 | exact | Count-Min + top-*k* heap, one per job |
| L3 | exact | CountSketch + top-*k* heap, one per job |

Ids follow the enumeration order; tests identify candidates by structure.

Invariants:

* Exactly 3 candidates, one per Q2 option. No duplicates.
* Every candidate covers both queries.
* Q1 never reaches a summary node.
* Every target's exact option is its exact accumulator (`SummaryAgg` with an
  exact family → `FinalizeExactAccumulator`): three per candidate.
* Q2's sketch families are exactly {`CmsWithHeap`, `CountSketchWithHeap`},
  each per subpopulation (job). Exact Q2 has no sketch.
* Only the scan and range nodes may be shared between the queries. No summary
  is shared. (Today nothing is shared.)
* Nothing is pruned: the heap sketches are admitted by ε = 0.01, δ = 0.001,
  and Q1's exact target admits only the exact option.

Ignored until their feature lands:

* Hydra (`HydraCms` over all (job, series) keys, a heap per job) as a third
  Q2 summary (#605).
* A shared-input variant of every Q2 option next to the independent one
  (Pass 2 identical-expression rule, #588).

## Physical ASAP-aware optimization: 3 candidates

| Id | From | Q2 physical operators | Materialization |
|---|---|---|---|
| P1 | L1 | sort (partition by job) → limit 10 | all `NotMaterialized` |
| P2 | L2 | CMS + heap build → top-10 estimate | all `NotMaterialized` |
| P3 | L3 | CountSketch + heap build → top-10 estimate | all `NotMaterialized` |

Q1 is the same in all three: scan → range → rate accumulator → sum
accumulator, each finalized.

Invariants:

* Exactly one physical candidate per logical candidate, and none pruned.
  `from_logical` is a bijection onto the logical ids, so no candidate is
  dropped before plan selection.
* Each physical candidate keeps its logical Q2 option and input sharing.
* Exact top-*k* is implemented as a sort followed by a limit: each job's
  series sorted by the ranked value, descending, then a `Project` that
  restores the logical root columns `[job, topk_10]`.
* A summary Q2 is a sketch build node feeding an estimation node. There is no
  merge node, because there are no window summaries.
* Every node is `NotMaterialized` and runs at query time.

Runtime capability check (not part of the doc): every candidate compiles in
the physical planner. Ignored on two runtime gaps: CMS + heap needs a
non-negative weight contract (#593), and the CountSketch + heap estimate's
output is not the keyed-evaluation shape the physical planner builds (#579).

## Plan selection: 1 plan

| Id | Outcome | Reason |
|---|---|---|
| P1 | selected | the only valid candidate |
| P2 | invalid | Count-Min needs non-negative update weights, and `sum_over_time` weights are not proven non-negative |
| P3 | invalid | a top-*k* output has no guarantee (a heap's bound is per item frequency, not top-*k* membership), and the deployment has no evidence that it meets the target |

Invariants:

* One selected id. Every other candidate is listed once as rejected, with a
  reason, and marked invalid or valid but costlier.
* Only valid candidates are priced. Each valid candidate's cost has one entry
  per DAG node, and `total` is their sum. A `NotMaterialized` node is charged
  once per query that reads it (its multiplier), since it is recomputed for
  each; a materialized node once. Every node is `NotMaterialized` today.
* The selected plan is no more expensive than any valid candidate.
* The selected plan compiles in the physical planner.

Ignored until Pass 2 (#588): for each Q2 option, the shared-input candidate
costs no more than its separate counterpart.

The doc's typical winners need window forms or materialization, and its
"Raw with a shared input" needs Pass 2. Latency bounds are not checked, so
whether an exact Q2 misses 100 ms is not tested.

## Where the doc and the planner differ

1. **Q2's top-*k* options.** The doc lists "exact, Count-Min + heap, Hydra".
   The planner offers exact, Count-Min + heap and CountSketch + heap; Hydra
   comes in #605. The tests follow the planner.
2. **Count-Min + heap is invalid.** The doc expects the accuracy model to
   judge it. Plan selection admits Count-Min only over weights proven
   non-negative, which `sum_over_time` over an arbitrary metric is not
   (counter declarations: #593).
3. **No summary is selectable.** The doc expects a summary for Q2 to win in
   most deployments. A composed top-*k* estimate has no guarantee, so without
   deployment evidence every heap candidate is invalid and the exact plan is
   selected.
4. **Physical ASAP-aware optimization is materialization in the doc's
   example.** Example 1 lists only window-form and materialization options.
   The MVP reads "physical operator implementation" from the physical
   ASAP-aware optimization section ("TopK as a sort followed by a limit"),
   which gives one physical candidate per logical candidate.
5. **Where exact top-*k* becomes sort + limit.** The Pass 1 diagram draws
   exact Q2 as "sort + limit 10 per job", but the frontend emits
   `Aggregate{[TopK]}` and the physical ASAP-aware optimization section calls
   sort + limit a physical choice. The tests require sort → limit only after
   physical ASAP-aware optimization.
6. **Hydra's inner sketch.** The doc says "Hydra over the whole `job`
   column" without naming the inner sketch. The test assumes `HydraCms`.
7. **"Shared summary charged once"** does not apply literally: the doc says
   no summary is shared in Example 1. Plan selection charges a
   `NotMaterialized` node once per consuming query and a materialized one
   once; the tests check that rule. A shared input therefore saves nothing
   until it is materialized (#604).
8. **Latency.** The doc says an exact top 10 rebuilt at every refresh "may
   miss" 100 ms. Plan selection does not check latency yet.
9. **Accuracy of Count-Min heap merges** matters only with tumbling windows,
   which are out of MVP scope.
