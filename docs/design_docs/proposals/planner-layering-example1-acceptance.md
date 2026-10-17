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
| Query-language frontends | 1 workload `LogicalDAG` | same |
| Logical ASAP-aware optimization | Pass 1 (3) × identical-expression rule (2) × window-composition rule (3 × 3) = **54** | Pass 1 (3), each with 2 identical-expression candidates and 6 window-composition candidates (Q1's rate and Q2's option in six 10-s tumbling panes, alone or both, each also with a shared scan) = **27**. Sliding windows are not implemented. |
| Physical ASAP-aware optimization | Materialization options per window form = **156** | Physical operator implementation, plus materialization: each logical candidate with every sub-DAG `NotMaterialized` (27: the doc's Raw plans, with panes rebuilt at every refresh), and each pane chain also `MatAtIngestion(Memory)` (30 more) = **57** |
| Plan selection | 1 plan | 1 plan |

Every MVP candidate is one of the doc's candidates: in logical ASAP-aware
optimization, the one with separate inputs and no window summary for either
query; in physical ASAP-aware optimization, the one where both queries are
**Raw** (state rebuilt from the last 1 min of raw samples at every refresh).

## Workload

Shared data workload: `arrival` = `Ingesting { rows_per_second }`, about
66,667 samples/s; `sources["http_requests_total"]` has `series_count` =
1,000,000, `sample_interval` = 15 s and `metric_type` = counter.

| Query | Repeats | `lookback` | `as_of` | Accuracy | Latency |
|---|---|---|---|---|---|
| Q1 `sum by (job) (rate(http_requests_total[1m]))` | 10 s | 1 m | evaluation time | exact | none |
| Q2 `topk by (job) (10, sum_over_time(http_requests_total[1m]))` | 10 s | 1 m | evaluation time | ε = 0.01, δ = 0.001 | ≤ 100 ms |

## Query-language frontends: 1 candidate

| Query | Chain |
|---|---|
| Q1 | scan `http_requests_total` → range 1m → rate (per series) → sum by (job) |
| Q2 | scan `http_requests_total` → range 1m → sum_over_time (per series) → topk by (job) (10) |

Invariants:

* Exactly one candidate, with one root per query, in workload entry order.
* No summary node (sketch `SummaryAgg`, `SummaryEstimate`, `SummaryMerge`).
* Q1 and Q2 share no node. Sharing is a logical ASAP-aware optimization
  decision.

## Logical ASAP-aware optimization: 3 Pass 1 candidates (27 in all)

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
| L10 | exact | Count-Min + top-*k* heap, one per job |
| L19 | exact | CountSketch + top-*k* heap, one per job |

Ids follow the enumeration order; tests identify candidates by structure.

Invariants:

* Exactly 3 candidates with whole windows and no shared input, one per Q2
  option. No duplicates. Each is followed by its 8 Pass 2 candidates.
* Every candidate covers both queries.
* Q1 never reaches a summary node.
* Every target's exact option is its exact accumulator (`SummaryAgg` with an
  exact family → `FinalizeExactAccumulator`): three per candidate with exact
  Q2, two with a heap Q2 (Q1's rate and sum). Q2's heaps are whole-expression
  heaps, Q2 top-*k*'s fused computation: they absorb its `sum_over_time`,
  which is then not computed.
* Q2's sketch families are exactly {`CmsWithHeap`, `CountSketchWithHeap`},
  each per subpopulation (job). Exact Q2 has no sketch.
* Only the scan and range nodes may be shared between the queries. No summary
  is shared.
* Nothing is pruned: the heap sketches are admitted by ε = 0.01, δ = 0.001,
  and Q1's exact target admits only the exact option.

Ignored until their feature lands:

* Hydra (`HydraCms` over all (job, series) keys, a heap per job) as a third
  Q2 summary (#605).

## Physical ASAP-aware optimization: 57 candidates

The three whole-window candidates without a shared input:

| Id | From | Q2 physical operators | Materialization |
|---|---|---|---|
| P1 | L1 | sort (partition by job) → limit 10 | all `NotMaterialized` |
| P10 | L10 | CMS + heap build → top-10 estimate | all `NotMaterialized` |
| P19 | L19 | CountSketch + heap build → top-10 estimate | all `NotMaterialized` |

Q1 is the same in all three: scan → range → rate accumulator → sum
accumulator, each finalized.

Invariants:

* Every logical candidate keeps its all-`NotMaterialized` physical candidate,
  and none is pruned, so no candidate is dropped before plan selection. A
  logical candidate with `n` pane chains has 2ⁿ physical candidates: each
  chain `NotMaterialized` or `MatAtIngestion(Memory)` (ids `P<n>-m<k>`).
  Only pane chains are placed at ingestion time; Q1's rate panes may be,
  because continuously ingesting PromQL data is taken to arrive in time
  order per series.
* Each physical candidate keeps its logical Q2 option and input sharing.
* Exact top-*k* is implemented as a sort followed by a limit: each job's
  series sorted by the ranked value, descending, then a `Project` that
  restores the logical root columns `[job, topk_10]`.
* A summary Q2 is a sketch build node feeding an estimation node. There is no
  merge node, because there are no window summaries.
* In the whole-window candidates, every node is `NotMaterialized` and runs
  at query time.

Runtime capability check (not part of the doc): every candidate compiles in
the physical planner. Ignored on two runtime gaps: a candidate's CMS + heap
weights are an open requirement that only the selected plan's confirmation
answers, and the heaps' top-*k* estimate output is not the keyed-evaluation
shape the physical planner builds (#579).

## Plan selection: 1 plan

| Id | Outcome | Reason |
|---|---|---|
| P1 | selected | all exact, at 7.94 per second (cost model 1) |
| P2, P3 | valid · costlier | the shared-scan and shared-scan-and-range candidates of P1; they tie with it up to rounding and lose on enumeration order |
| P10 | invalid | Count-Min + heap: a top-*k* output has no guarantee (a heap's bound is per item frequency, not top-*k* membership), and the deployment has no evidence that it meets the target |
| P19 | invalid | CountSketch + heap: the same reason |

The 38 heap candidates (whole window or panes, rebuilt or maintained) are
invalid for that reason; the other 18 exact candidates are valid but
costlier. Count-Min's weights are the raw samples of `http_requests_total`,
which the workload declares a counter, so they are proven non-negative;
without that declaration Count-Min is also rejected on its weights. Exact
panes rebuilt at every refresh cost 8.54, and maintaining a query's exact
panes at ingestion time costs 12.94.

Invariants:

* One selected id. Every other candidate is listed once as rejected, with a
  reason, and marked invalid or valid but costlier.
* Only valid candidates are priced. Each valid candidate's cost has one entry
  per DAG node, and `cost_per_second` is their sum. A materialized node is charged
  once. A `NotMaterialized` node is charged once per consumer that reads it
  (its multiplier), where a consumer is a query or a materialized node and
  a read stops at materialized nodes.
  `stage3_plan_selection_charging_helper_*` check the helper on a hand-built candidate with a
  materialized node.
* The selected plan is no more expensive than any valid candidate.
* The selected plan compiles in the physical planner.
* For each Q2 option, the shared-input candidate costs no more than its
  separate counterpart.

The doc's typical winners need summaries that plan selection can accept for
Q2, or materialization that pays off; neither happens here. The latency
check runs: P1's Q2 takes about 46 ms per evaluation, within its 100 ms.

## Where the doc and the planner differ

1. **Q2's top-*k* options.** The doc lists "exact, Count-Min + heap, Hydra".
   The planner offers exact, Count-Min + heap and CountSketch + heap; Hydra
   comes in #605. The tests follow the planner.
2. **Count-Min + heap is invalid.** The doc expects the accuracy model to
   judge it. Plan selection admits Count-Min only over weights proven
   non-negative; the workload declares `http_requests_total` a counter
   (`sources[metric].metric_type`), which proves them. It still rejects the
   candidate, because a top-*k* output has no guarantee (item 3).
3. **No summary is selectable.** The doc expects a summary for Q2 to win in
   most deployments. A composed top-*k* estimate has no guarantee, so without
   deployment evidence every heap candidate is invalid and the exact plan is
   selected.
4. **Physical ASAP-aware optimization is materialization in the doc's
   example.** Example 1 lists only window-form and materialization options.
   The planner also reads "physical operator implementation" from the
   physical ASAP-aware optimization section ("TopK as a sort followed by a
   limit"), and enumerates materialization only for summary builds: each
   pane chain is `NotMaterialized` or `MatAtIngestion(Memory)`.
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
   once; the tests check that rule. Stage 2 materializes only summary
   builds, so a shared input saves nothing here.
8. **Latency.** The doc says an exact top 10 rebuilt at every refresh "may
   miss" 100 ms. With the illustrative calibration it takes about 46 ms, so
   the exact plan meets the bound.
9. **Accuracy of Count-Min heap merges.** The heap panes are invalid like
   their whole-window builds, so the approximate merge is never judged.
10. **Input sharing in Pass 2.** The doc gives each Pass 1 candidate "2 input
    choices (separate or shared)". Following Algorithm 2 line 31 (one
    candidate per non-empty subset of the sharing options), the planner
    gives 3: separate, shared scan, and shared scan and range selector.
