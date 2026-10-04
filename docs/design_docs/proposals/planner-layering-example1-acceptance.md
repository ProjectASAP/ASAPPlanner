# Planner layering, Example 1: acceptance spec (MVP)

Audience: planner designers and the Phase C implementer.
Source: [planner-layering.md](planner-layering.md), "Example 1: Aggregation over
dimensions". Tests: `crates/integration-tests/tests/planner_layering_example1.rs`.
Viewer fixture shape: `tools/dag-viewer/examples/planner-layering-example1.expected.json`.

This spec was written by a test designer who did not implement the stages.
The candidate counts were later changed to follow the planner's output (user
decision, see [Candidate counts](#candidate-counts)).

## Scope

| Stage | Doc (#509) | MVP (this spec) |
|---|---|---|
| 0. Frontends | 1 workload `LogicalDAG` | same |
| 1. Logical ASAP | Pass 1 (3) × identical-expression rule (2) × window-composition rule (3 × 3) = **54** | Pass 1 (24) × identical-expression rule (2) = **48**. Window composition (blocked on #511: #518, #522) is not implemented. |
| 2. Physical ASAP | Materialization options per window form = **156** | Physical operator implementation only, no materialization = **48** (the doc's Raw/Raw plans, separate or with a shared input) |
| 3. Selection | 1 plan | 1 plan |

Every MVP candidate is one of the doc's candidates: in Stage 1, the one with no
window summary for either query; in Stage 2, the one where both queries are
**Raw** (state rebuilt from the last 1 min of raw samples at every refresh).

## Workload

Shared data workload: `continuously_ingesting`, 15 s ingestion interval,
1,000,000 series, about 66,667 samples/s, `zipf`, volume unknown.

| Query | Repeats | `lookback` | `as_of` | Accuracy | Latency |
|---|---|---|---|---|---|
| Q1 `sum by (job) (rate(http_requests_total[1m]))` | 10 s | 1 m | evaluation time | exact | none |
| Q2 `topk by (job) (10, sum_over_time(http_requests_total[1m]))` | 10 s | 1 m | evaluation time | ε = 0.01, δ = 0.001 | ≤ 100 ms |

## Stage 0: 1 candidate

| Query | Chain |
|---|---|
| Q1 | scan `http_requests_total` → range 1m → rate (per series) → sum by (job) |
| Q2 | scan `http_requests_total` → range 1m → sum_over_time (per series) → topk by (job) (10) |

Invariants:

* Exactly one candidate, with one root per query, in workload entry order.
* No summary node (sketch `summary_agg`, `summary_estimate`, `summary_merge`).
* Q1 and Q2 share no node. Sharing is a Stage 1 decision.

## Candidate counts

The original spec expected 6 Stage 1 candidates: 3 Q2 options (exact,
Count-Min + heap, Hydra) × separate or shared input. It did not account for
Pass 1's exact-accumulator options: each of Q1's `rate` and `sum` and Q2's
`sum_over_time` may stay a raw aggregate or become an exact accumulator
(`SummaryAgg` → `FinalizeExactAccumulator`). Pass 1 also offers
CountSketch + heap for Q2's top-k. Pass 1 therefore yields
2 × 2 (Q1) × 3 × 2 (Q2) = **24** combinations, and Pass 2's
identical-expression rule adds a shared-input variant of each: **48**
candidates. Hydra is not produced yet; its test stays ignored, naming the
missing feature.

## Stage 1: 48 candidates

Every combination of Q1 `rate` {raw, Rate acc} × Q1 `sum` {raw, Sum acc} × Q2
top-k {exact, Count-Min + heap, CountSketch + heap} × Q2 `sum_over_time`
{raw, Sum acc}, twice: L1–L24 with each query reading its own
`http_requests_total[1m]` input, and L25–L48 (labelled "· shared input") with
one scan → range 1m read by both queries. Pass 2 adds the shared variant only
because sharing merges nodes; Stage 3 chooses between the variants by cost.

Invariants:

* Exactly 48 candidates: each combination once with separate inputs and once
  with the shared input. No duplicates.
* Every candidate covers both queries.
* Q1 never reaches a sketch node.
* Only the scan and range nodes may be shared. No summary is shared.
* For each Q2 option, both an independent and a shared variant are present.
* Pending (ignored test): the sketch families in Q2 are {`CmsWithHeap`,
  `CountSketchWithHeap`} per subpopulation and Hydra `HydraCms` (no Hydra
  alternative yet).

## Stage 2: 48 candidates

`Pn` implements `Ln`. Exact Q2 top-k becomes sort (partition by job) → limit
10; a sketch Q2 is a heap-sketch build → top-10 estimate, which returns the
selected rows (`job`, series identity, `value`). Everything runs at query time.

Invariants:

* Exactly one physical candidate per logical candidate. `from_logical` is a
  bijection onto the Stage 1 ids, so no valid candidate is dropped before
  Stage 3.
* Each physical candidate keeps its logical Q2 option and input sharing.
* Exact TopK is implemented as a sort followed by a limit (16 candidates).
* A summary Q2 is a sketch build node feeding an estimation node. There is no
  merge node, because the MVP has no window summaries.
* No node runs at ingestion time, because the MVP has no materialization.
* The runtime's physical planner compiles every candidate Stage 3 finds
  valid, and rejects the 16 Count-Min + heap candidates for the reason Stage 3
  gives: their update weights are not proven non-negative.

### Candidates for manual review

Generated from `tools/dag-viewer/examples/planner-layering-example1.json`.
"acc" is an exact accumulator; "raw" keeps the relational aggregate.

| Id | Label | Q1 choice | Q2 choice | Input | Stage 3 outcome | Reason |
|---|---|---|---|---|---|---|
| P1 (L1) | Q1 exact · Q2 exact | rate: raw; sum: raw | top-k: exact (sort → limit); sum_over_time: raw | separate | valid, costlier | 86.401 vs 52.201 cpu ms |
| P2 (L2) | Q1 exact · Q2 exact (Sum acc) | rate: raw; sum: raw | top-k: exact (sort → limit); sum_over_time: Sum acc | separate | valid, costlier | 83.401 vs 52.201 cpu ms |
| P3 (L3) | Q1 exact · Q2 CMS+heap | rate: raw; sum: raw | top-k: Count-Min + heap; sum_over_time: raw | separate | invalid | q2: CmsWithHeap needs non-negative update weights, and these are not proven non-negative |
| P4 (L4) | Q1 exact · Q2 CMS+heap (Sum acc) | rate: raw; sum: raw | top-k: Count-Min + heap; sum_over_time: Sum acc | separate | invalid | q2: CmsWithHeap needs non-negative update weights, and these are not proven non-negative |
| P5 (L5) | Q1 exact · Q2 CountSketch+heap | rate: raw; sum: raw | top-k: CountSketch + heap; sum_over_time: raw | separate | valid, costlier | 198.401 vs 52.201 cpu ms |
| P6 (L6) | Q1 exact · Q2 CountSketch+heap (Sum acc) | rate: raw; sum: raw | top-k: CountSketch + heap; sum_over_time: Sum acc | separate | valid, costlier | 195.401 vs 52.201 cpu ms |
| P7 (L7) | Q1 exact (Rate acc) · Q2 exact | rate: Rate acc; sum: raw | top-k: exact (sort → limit); sum_over_time: raw | separate | valid, costlier | 83.401 vs 52.201 cpu ms |
| P8 (L8) | Q1 exact (Rate acc) · Q2 exact (Sum acc) | rate: Rate acc; sum: raw | top-k: exact (sort → limit); sum_over_time: Sum acc | separate | valid, costlier | 80.401 vs 52.201 cpu ms |
| P9 (L9) | Q1 exact (Rate acc) · Q2 CMS+heap | rate: Rate acc; sum: raw | top-k: Count-Min + heap; sum_over_time: raw | separate | invalid | q2: CmsWithHeap needs non-negative update weights, and these are not proven non-negative |
| P10 (L10) | Q1 exact (Rate acc) · Q2 CMS+heap (Sum acc) | rate: Rate acc; sum: raw | top-k: Count-Min + heap; sum_over_time: Sum acc | separate | invalid | q2: CmsWithHeap needs non-negative update weights, and these are not proven non-negative |
| P11 (L11) | Q1 exact (Rate acc) · Q2 CountSketch+heap | rate: Rate acc; sum: raw | top-k: CountSketch + heap; sum_over_time: raw | separate | valid, costlier | 195.401 vs 52.201 cpu ms |
| P12 (L12) | Q1 exact (Rate acc) · Q2 CountSketch+heap (Sum acc) | rate: Rate acc; sum: raw | top-k: CountSketch + heap; sum_over_time: Sum acc | separate | valid, costlier | 192.401 vs 52.201 cpu ms |
| P13 (L13) | Q1 exact (Sum acc) · Q2 exact | rate: raw; sum: Sum acc | top-k: exact (sort → limit); sum_over_time: raw | separate | valid, costlier | 85.401 vs 52.201 cpu ms |
| P14 (L14) | Q1 exact (Sum acc) · Q2 exact (Sum acc) | rate: raw; sum: Sum acc | top-k: exact (sort → limit); sum_over_time: Sum acc | separate | valid, costlier | 82.401 vs 52.201 cpu ms |
| P15 (L15) | Q1 exact (Sum acc) · Q2 CMS+heap | rate: raw; sum: Sum acc | top-k: Count-Min + heap; sum_over_time: raw | separate | invalid | q2: CmsWithHeap needs non-negative update weights, and these are not proven non-negative |
| P16 (L16) | Q1 exact (Sum acc) · Q2 CMS+heap (Sum acc) | rate: raw; sum: Sum acc | top-k: Count-Min + heap; sum_over_time: Sum acc | separate | invalid | q2: CmsWithHeap needs non-negative update weights, and these are not proven non-negative |
| P17 (L17) | Q1 exact (Sum acc) · Q2 CountSketch+heap | rate: raw; sum: Sum acc | top-k: CountSketch + heap; sum_over_time: raw | separate | valid, costlier | 197.401 vs 52.201 cpu ms |
| P18 (L18) | Q1 exact (Sum acc) · Q2 CountSketch+heap (Sum acc) | rate: raw; sum: Sum acc | top-k: CountSketch + heap; sum_over_time: Sum acc | separate | valid, costlier | 194.401 vs 52.201 cpu ms |
| P19 (L19) | Q1 exact (Sum acc, Rate acc) · Q2 exact | rate: Rate acc; sum: Sum acc | top-k: exact (sort → limit); sum_over_time: raw | separate | valid, costlier | 82.401 vs 52.201 cpu ms |
| P20 (L20) | Q1 exact (Sum acc, Rate acc) · Q2 exact (Sum acc) | rate: Rate acc; sum: Sum acc | top-k: exact (sort → limit); sum_over_time: Sum acc | separate | valid, costlier | 79.401 vs 52.201 cpu ms |
| P21 (L21) | Q1 exact (Sum acc, Rate acc) · Q2 CMS+heap | rate: Rate acc; sum: Sum acc | top-k: Count-Min + heap; sum_over_time: raw | separate | invalid | q2: CmsWithHeap needs non-negative update weights, and these are not proven non-negative |
| P22 (L22) | Q1 exact (Sum acc, Rate acc) · Q2 CMS+heap (Sum acc) | rate: Rate acc; sum: Sum acc | top-k: Count-Min + heap; sum_over_time: Sum acc | separate | invalid | q2: CmsWithHeap needs non-negative update weights, and these are not proven non-negative |
| P23 (L23) | Q1 exact (Sum acc, Rate acc) · Q2 CountSketch+heap | rate: Rate acc; sum: Sum acc | top-k: CountSketch + heap; sum_over_time: raw | separate | valid, costlier | 194.401 vs 52.201 cpu ms |
| P24 (L24) | Q1 exact (Sum acc, Rate acc) · Q2 CountSketch+heap (Sum acc) | rate: Rate acc; sum: Sum acc | top-k: CountSketch + heap; sum_over_time: Sum acc | separate | valid, costlier | 191.401 vs 52.201 cpu ms |
| P25 (L25) | Q1 exact · Q2 exact · shared input | rate: raw; sum: raw | top-k: exact (sort → limit); sum_over_time: raw | shared | valid, costlier | 59.201 vs 52.201 cpu ms |
| P26 (L26) | Q1 exact · Q2 exact (Sum acc) · shared input | rate: raw; sum: raw | top-k: exact (sort → limit); sum_over_time: Sum acc | shared | valid, costlier | 56.201 vs 52.201 cpu ms |
| P27 (L27) | Q1 exact · Q2 CMS+heap · shared input | rate: raw; sum: raw | top-k: Count-Min + heap; sum_over_time: raw | shared | invalid | q2: CmsWithHeap needs non-negative update weights, and these are not proven non-negative |
| P28 (L28) | Q1 exact · Q2 CMS+heap (Sum acc) · shared input | rate: raw; sum: raw | top-k: Count-Min + heap; sum_over_time: Sum acc | shared | invalid | q2: CmsWithHeap needs non-negative update weights, and these are not proven non-negative |
| P29 (L29) | Q1 exact · Q2 CountSketch+heap · shared input | rate: raw; sum: raw | top-k: CountSketch + heap; sum_over_time: raw | shared | valid, costlier | 171.201 vs 52.201 cpu ms |
| P30 (L30) | Q1 exact · Q2 CountSketch+heap (Sum acc) · shared input | rate: raw; sum: raw | top-k: CountSketch + heap; sum_over_time: Sum acc | shared | valid, costlier | 168.201 vs 52.201 cpu ms |
| P31 (L31) | Q1 exact (Rate acc) · Q2 exact · shared input | rate: Rate acc; sum: raw | top-k: exact (sort → limit); sum_over_time: raw | shared | valid, costlier | 56.201 vs 52.201 cpu ms |
| P32 (L32) | Q1 exact (Rate acc) · Q2 exact (Sum acc) · shared input | rate: Rate acc; sum: raw | top-k: exact (sort → limit); sum_over_time: Sum acc | shared | valid, costlier | 53.201 vs 52.201 cpu ms |
| P33 (L33) | Q1 exact (Rate acc) · Q2 CMS+heap · shared input | rate: Rate acc; sum: raw | top-k: Count-Min + heap; sum_over_time: raw | shared | invalid | q2: CmsWithHeap needs non-negative update weights, and these are not proven non-negative |
| P34 (L34) | Q1 exact (Rate acc) · Q2 CMS+heap (Sum acc) · shared input | rate: Rate acc; sum: raw | top-k: Count-Min + heap; sum_over_time: Sum acc | shared | invalid | q2: CmsWithHeap needs non-negative update weights, and these are not proven non-negative |
| P35 (L35) | Q1 exact (Rate acc) · Q2 CountSketch+heap · shared input | rate: Rate acc; sum: raw | top-k: CountSketch + heap; sum_over_time: raw | shared | valid, costlier | 168.201 vs 52.201 cpu ms |
| P36 (L36) | Q1 exact (Rate acc) · Q2 CountSketch+heap (Sum acc) · shared input | rate: Rate acc; sum: raw | top-k: CountSketch + heap; sum_over_time: Sum acc | shared | valid, costlier | 165.201 vs 52.201 cpu ms |
| P37 (L37) | Q1 exact (Sum acc) · Q2 exact · shared input | rate: raw; sum: Sum acc | top-k: exact (sort → limit); sum_over_time: raw | shared | valid, costlier | 58.201 vs 52.201 cpu ms |
| P38 (L38) | Q1 exact (Sum acc) · Q2 exact (Sum acc) · shared input | rate: raw; sum: Sum acc | top-k: exact (sort → limit); sum_over_time: Sum acc | shared | valid, costlier | 55.201 vs 52.201 cpu ms |
| P39 (L39) | Q1 exact (Sum acc) · Q2 CMS+heap · shared input | rate: raw; sum: Sum acc | top-k: Count-Min + heap; sum_over_time: raw | shared | invalid | q2: CmsWithHeap needs non-negative update weights, and these are not proven non-negative |
| P40 (L40) | Q1 exact (Sum acc) · Q2 CMS+heap (Sum acc) · shared input | rate: raw; sum: Sum acc | top-k: Count-Min + heap; sum_over_time: Sum acc | shared | invalid | q2: CmsWithHeap needs non-negative update weights, and these are not proven non-negative |
| P41 (L41) | Q1 exact (Sum acc) · Q2 CountSketch+heap · shared input | rate: raw; sum: Sum acc | top-k: CountSketch + heap; sum_over_time: raw | shared | valid, costlier | 170.201 vs 52.201 cpu ms |
| P42 (L42) | Q1 exact (Sum acc) · Q2 CountSketch+heap (Sum acc) · shared input | rate: raw; sum: Sum acc | top-k: CountSketch + heap; sum_over_time: Sum acc | shared | valid, costlier | 167.201 vs 52.201 cpu ms |
| P43 (L43) | Q1 exact (Sum acc, Rate acc) · Q2 exact · shared input | rate: Rate acc; sum: Sum acc | top-k: exact (sort → limit); sum_over_time: raw | shared | valid, costlier | 55.201 vs 52.201 cpu ms |
| P44 (L44) | Q1 exact (Sum acc, Rate acc) · Q2 exact (Sum acc) · shared input | rate: Rate acc; sum: Sum acc | top-k: exact (sort → limit); sum_over_time: Sum acc | shared | **selected** | cheapest valid (52.201 cpu ms) |
| P45 (L45) | Q1 exact (Sum acc, Rate acc) · Q2 CMS+heap · shared input | rate: Rate acc; sum: Sum acc | top-k: Count-Min + heap; sum_over_time: raw | shared | invalid | q2: CmsWithHeap needs non-negative update weights, and these are not proven non-negative |
| P46 (L46) | Q1 exact (Sum acc, Rate acc) · Q2 CMS+heap (Sum acc) · shared input | rate: Rate acc; sum: Sum acc | top-k: Count-Min + heap; sum_over_time: Sum acc | shared | invalid | q2: CmsWithHeap needs non-negative update weights, and these are not proven non-negative |
| P47 (L47) | Q1 exact (Sum acc, Rate acc) · Q2 CountSketch+heap · shared input | rate: Rate acc; sum: Sum acc | top-k: CountSketch + heap; sum_over_time: raw | shared | valid, costlier | 167.201 vs 52.201 cpu ms |
| P48 (L48) | Q1 exact (Sum acc, Rate acc) · Q2 CountSketch+heap (Sum acc) · shared input | rate: Rate acc; sum: Sum acc | top-k: CountSketch + heap; sum_over_time: Sum acc | shared | valid, costlier | 164.201 vs 52.201 cpu ms |

Count-Min + heap stays invalid: Q2 ranks `sum_over_time` of raw samples, and
nothing in the workload declares `http_requests_total` non-negative (no metric
type), so neither existing proof (`UnitCount`, `ResetAwareCounterDerivative`)
applies.

## Stage 3: 1 plan

Invariants:

* One selected id. Every other candidate is listed once as rejected, with a
  reason, and marked invalid (accuracy, latency or capability) or valid but
  costlier.
* Each priced candidate's cost has one entry per DAG node, and `total` is
  their sum. A shared node is therefore charged once, for all its consumers.
  Stage 3 prices valid candidates only.
* The selected plan is no more expensive than any valid candidate.
* For each Q2 option, the shared-input candidate costs no more than its
  separate counterpart.
* A shared-input candidate is selected, matching the doc's "Raw with a shared
  input" winner. It saves exactly one scan and one range node over the same
  choices with separate inputs.

Outcome with the built-in models: P44 (P20's choices, all exact with exact
accumulators for Q1's rate and sum and Q2's sum_over_time, over the shared
input) at 52.201 cpu ms, against 79.401 for P20. The scan (23.2) and range
(4.0) are priced once instead of twice. The doc's other typical winners need
window forms or materialization and are out of MVP scope.

## Ambiguities and MVP deviations

1. **Stage 2 is materialization-only in the doc.** Example 1 lists only
   window-form or materialization options. The MVP reads "physical operator
   implementation" from the Stage 2 section: "TopK as a sort followed by a
   limit". This gives one physical candidate per logical candidate. The doc
   names no alternative implementation (for example hash versus sort
   aggregation) for this example.
2. **Where exact TopK becomes sort + limit.** The Pass 1 diagram already
   draws exact Q2 as "sort + limit 10 per job", but the Stage 2 section calls
   this a physical choice. Today the frontend emits `aggregate[top_k]`. The
   spec requires sort → limit only by Stage 2.
3. **Minimal Pass 2.** Only the identical-expression rule applies. The
   window-composition rule adds the rest of the doc's 54 candidates, and its
   tumbling and sliding variants wait on #511. Sharing the range selector
   needs its rows to have a provable key, which CSE's legality rule requires:
   a PromQL series has at most one sample per timestamp, so (series identity,
   timestamp) is declared as the scan's key.
4. **"Shared summary charged once"** does not apply literally: the doc says no
   summary is shared in Example 1. The tests check the general form (each node
   charged once) on the shared input node.
5. **Workload DAG with two roots.** `LogicalASAPDAG` has one `root`, but
   Example 1 needs one DAG for both queries. The stubs carry `query_roots`
   until the export supports several roots.
6. **Where cost is computed.** The viewer contract puts `cost` on Stage 2
   candidates, but the doc says only Stage 3 uses the cost model. The spec has
   Stage 3 produce costs, and the viewer document attaches them to Stage 2
   entries.
7. **Hydra's inner sketch.** The doc says "Hydra over the whole `job`
   column" without naming the inner sketch. The spec assumes `HydraCms`, the
   construction with the proven guarantee for frequencies.
8. **Other top-*k* families.** The code also offers `CountSketchWithHeap`.
   The doc lists only Count-Min + heap and Hydra; the tests follow the
   planner and include it.
9. **Latency.** The doc says an exact top 10 rebuilt at every refresh "may
   miss" 100 ms. Whether exact-Q2 candidates are rejected is left to the cost model.
10. **Accuracy of Count-Min heap merges** matters only with tumbling windows,
    which are out of MVP scope.
