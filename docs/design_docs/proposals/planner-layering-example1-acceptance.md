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
| 1. Logical ASAP | Pass 1 (3) × identical-expression rule (2) × window-composition rule (3 × 3) = **54** | Pass 1 (32) × identical-expression rule (2) = **64**. Window composition (blocked on #511: #518, #522) is not implemented. |
| 2. Physical ASAP | Materialization options per window form = **156** | Physical operator implementation only, no materialization = **64** (the doc's Raw/Raw plans, separate or with a shared input) |
| 3. Selection | 1 plan | 1 plan |

Every MVP candidate is one of the doc's candidates: in Stage 1, the one with no
window summary for either query; in Stage 2, the one where both queries are
**Raw** (state rebuilt from the last 1 min of raw samples at every refresh).

## Workload

Shared data workload: `continuously_ingesting`, 15 s ingestion interval,
1,000,000 series, about 66,667 samples/s, `zipf`, volume unknown.
`http_requests_total` is declared a counter (`metric_types`), so its samples
are known to be non-negative.

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
CountSketch + heap for Q2's top-k, and a **whole-expression** Count-Min or
CountSketch + heap: one heap sketch per `job` over the raw samples of the
range, keyed by series identity and weighted by the sample value, which
realizes `topk by (job) (10, sum_over_time(…))` as a whole and absorbs the
`sum_over_time` (no exact per-series sum is computed, so it has no choice of
its own). Pass 1 therefore yields 2 × 2 (Q1) × (3 × 2 + 2) (Q2) = **32**
combinations, and Pass 2's identical-expression rule adds a shared-input
variant of each: **64** candidates. Hydra is not produced yet; its test stays ignored, naming the
missing feature.

## Stage 1: 64 candidates

Every combination of Q1 `rate` {raw, Rate acc} × Q1 `sum` {raw, Sum acc} × Q2
(top-k {exact, Count-Min + heap, CountSketch + heap} × `sum_over_time` {raw,
Sum acc}, or whole-expression {Count-Min + heap, CountSketch + heap}), twice:
L1–L32 with each query reading its own `http_requests_total[1m]` input, and
L33–L64 (labelled "· shared input") with
one scan → range 1m read by both queries. Pass 2 adds the shared variant only
because sharing merges nodes; Stage 3 chooses between the variants by cost.

Invariants:

* Exactly 64 candidates: each combination once with separate inputs and once
  with the shared input. No duplicates.
* Every candidate covers both queries.
* Q1 never reaches a sketch node.
* Only the scan and range nodes may be shared. No summary is shared.
* For each Q2 option, both an independent and a shared variant are present.
* A whole-expression sketch reads the range directly; no `sum_over_time` is
  computed in its candidates.
* Pending (ignored test): the sketch families in Q2 are {`CmsWithHeap`,
  `CountSketchWithHeap`} per subpopulation, over the per-series sums or whole
  expression, and Hydra `HydraCms` (no Hydra alternative yet).

## Stage 2: 64 candidates

`Pn` implements `Ln`. Exact Q2 top-k becomes sort (partition by job) → limit
10; a sketch Q2 is a heap-sketch build → top-10 estimate, which returns the
selected rows (`job`, series identity, `value`); every Q2 option has this
Stage 1 schema. Everything runs at query time.

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
  valid. With the counter declaration that is all 64. Without it, or with
  `http_requests_total` declared a gauge, the runtime rejects the 24
  Count-Min + heap candidates for the reason Stage 3 gives: their update
  weights are not proven non-negative.

### Candidates for manual review

Generated from `tools/dag-viewer/examples/planner-layering-example1.json`.
"acc" is an exact accumulator; "raw" keeps the relational aggregate.

| Id | Label | Q1 choice | Q2 choice | Input | Stage 3 outcome | Reason |
|---|---|---|---|---|---|---|
| P1 (L1) | Q1 exact · Q2 exact | rate: raw; sum: raw | top-k: exact (sort → limit); sum_over_time: raw | separate | valid, costlier | 8.640 vs 4.620 cost/s |
| P2 (L2) | Q1 exact · Q2 exact (Sum acc) | rate: raw; sum: raw | top-k: exact (sort → limit); sum_over_time: Sum acc | separate | valid, costlier | 8.340 vs 4.620 cost/s |
| P3 (L3) | Q1 exact · Q2 CMS+heap | rate: raw; sum: raw | top-k: Count-Min + heap; sum_over_time: raw | separate | valid, costlier | 8.040 vs 4.620 cost/s |
| P4 (L4) | Q1 exact · Q2 CMS+heap (Sum acc) | rate: raw; sum: raw | top-k: Count-Min + heap; sum_over_time: Sum acc | separate | valid, costlier | 7.740 vs 4.620 cost/s |
| P5 (L5) | Q1 exact · Q2 CountSketch+heap | rate: raw; sum: raw | top-k: CountSketch + heap; sum_over_time: raw | separate | valid, costlier | 19.840 vs 4.620 cost/s |
| P6 (L6) | Q1 exact · Q2 CountSketch+heap (Sum acc) | rate: raw; sum: raw | top-k: CountSketch + heap; sum_over_time: Sum acc | separate | valid, costlier | 19.540 vs 4.620 cost/s |
| P7 (L7) | Q1 exact · Q2 whole-expression CMS+heap | rate: raw; sum: raw | top-k and sum_over_time: whole-expression Count-Min + heap over raw samples | separate | valid, costlier | 9.640 vs 4.620 cost/s |
| P8 (L8) | Q1 exact · Q2 whole-expression CountSketch+heap | rate: raw; sum: raw | top-k and sum_over_time: whole-expression CountSketch + heap over raw samples | separate | valid, costlier | 56.840 vs 4.620 cost/s |
| P9 (L9) | Q1 exact (Rate acc) · Q2 exact | rate: Rate acc; sum: raw | top-k: exact (sort → limit); sum_over_time: raw | separate | valid, costlier | 8.340 vs 4.620 cost/s |
| P10 (L10) | Q1 exact (Rate acc) · Q2 exact (Sum acc) | rate: Rate acc; sum: raw | top-k: exact (sort → limit); sum_over_time: Sum acc | separate | valid, costlier | 8.040 vs 4.620 cost/s |
| P11 (L11) | Q1 exact (Rate acc) · Q2 CMS+heap | rate: Rate acc; sum: raw | top-k: Count-Min + heap; sum_over_time: raw | separate | valid, costlier | 7.740 vs 4.620 cost/s |
| P12 (L12) | Q1 exact (Rate acc) · Q2 CMS+heap (Sum acc) | rate: Rate acc; sum: raw | top-k: Count-Min + heap; sum_over_time: Sum acc | separate | valid, costlier | 7.440 vs 4.620 cost/s |
| P13 (L13) | Q1 exact (Rate acc) · Q2 CountSketch+heap | rate: Rate acc; sum: raw | top-k: CountSketch + heap; sum_over_time: raw | separate | valid, costlier | 19.540 vs 4.620 cost/s |
| P14 (L14) | Q1 exact (Rate acc) · Q2 CountSketch+heap (Sum acc) | rate: Rate acc; sum: raw | top-k: CountSketch + heap; sum_over_time: Sum acc | separate | valid, costlier | 19.240 vs 4.620 cost/s |
| P15 (L15) | Q1 exact (Rate acc) · Q2 whole-expression CMS+heap | rate: Rate acc; sum: raw | top-k and sum_over_time: whole-expression Count-Min + heap over raw samples | separate | valid, costlier | 9.340 vs 4.620 cost/s |
| P16 (L16) | Q1 exact (Rate acc) · Q2 whole-expression CountSketch+heap | rate: Rate acc; sum: raw | top-k and sum_over_time: whole-expression CountSketch + heap over raw samples | separate | valid, costlier | 56.540 vs 4.620 cost/s |
| P17 (L17) | Q1 exact (Sum acc) · Q2 exact | rate: raw; sum: Sum acc | top-k: exact (sort → limit); sum_over_time: raw | separate | valid, costlier | 8.540 vs 4.620 cost/s |
| P18 (L18) | Q1 exact (Sum acc) · Q2 exact (Sum acc) | rate: raw; sum: Sum acc | top-k: exact (sort → limit); sum_over_time: Sum acc | separate | valid, costlier | 8.240 vs 4.620 cost/s |
| P19 (L19) | Q1 exact (Sum acc) · Q2 CMS+heap | rate: raw; sum: Sum acc | top-k: Count-Min + heap; sum_over_time: raw | separate | valid, costlier | 7.940 vs 4.620 cost/s |
| P20 (L20) | Q1 exact (Sum acc) · Q2 CMS+heap (Sum acc) | rate: raw; sum: Sum acc | top-k: Count-Min + heap; sum_over_time: Sum acc | separate | valid, costlier | 7.640 vs 4.620 cost/s |
| P21 (L21) | Q1 exact (Sum acc) · Q2 CountSketch+heap | rate: raw; sum: Sum acc | top-k: CountSketch + heap; sum_over_time: raw | separate | valid, costlier | 19.740 vs 4.620 cost/s |
| P22 (L22) | Q1 exact (Sum acc) · Q2 CountSketch+heap (Sum acc) | rate: raw; sum: Sum acc | top-k: CountSketch + heap; sum_over_time: Sum acc | separate | valid, costlier | 19.440 vs 4.620 cost/s |
| P23 (L23) | Q1 exact (Sum acc) · Q2 whole-expression CMS+heap | rate: raw; sum: Sum acc | top-k and sum_over_time: whole-expression Count-Min + heap over raw samples | separate | valid, costlier | 9.540 vs 4.620 cost/s |
| P24 (L24) | Q1 exact (Sum acc) · Q2 whole-expression CountSketch+heap | rate: raw; sum: Sum acc | top-k and sum_over_time: whole-expression CountSketch + heap over raw samples | separate | valid, costlier | 56.740 vs 4.620 cost/s |
| P25 (L25) | Q1 exact (Sum acc, Rate acc) · Q2 exact | rate: Rate acc; sum: Sum acc | top-k: exact (sort → limit); sum_over_time: raw | separate | valid, costlier | 8.240 vs 4.620 cost/s |
| P26 (L26) | Q1 exact (Sum acc, Rate acc) · Q2 exact (Sum acc) | rate: Rate acc; sum: Sum acc | top-k: exact (sort → limit); sum_over_time: Sum acc | separate | valid, costlier | 7.940 vs 4.620 cost/s |
| P27 (L27) | Q1 exact (Sum acc, Rate acc) · Q2 CMS+heap | rate: Rate acc; sum: Sum acc | top-k: Count-Min + heap; sum_over_time: raw | separate | valid, costlier | 7.640 vs 4.620 cost/s |
| P28 (L28) | Q1 exact (Sum acc, Rate acc) · Q2 CMS+heap (Sum acc) | rate: Rate acc; sum: Sum acc | top-k: Count-Min + heap; sum_over_time: Sum acc | separate | valid, costlier | 7.340 vs 4.620 cost/s |
| P29 (L29) | Q1 exact (Sum acc, Rate acc) · Q2 CountSketch+heap | rate: Rate acc; sum: Sum acc | top-k: CountSketch + heap; sum_over_time: raw | separate | valid, costlier | 19.440 vs 4.620 cost/s |
| P30 (L30) | Q1 exact (Sum acc, Rate acc) · Q2 CountSketch+heap (Sum acc) | rate: Rate acc; sum: Sum acc | top-k: CountSketch + heap; sum_over_time: Sum acc | separate | valid, costlier | 19.140 vs 4.620 cost/s |
| P31 (L31) | Q1 exact (Sum acc, Rate acc) · Q2 whole-expression CMS+heap | rate: Rate acc; sum: Sum acc | top-k and sum_over_time: whole-expression Count-Min + heap over raw samples | separate | valid, costlier | 9.240 vs 4.620 cost/s |
| P32 (L32) | Q1 exact (Sum acc, Rate acc) · Q2 whole-expression CountSketch+heap | rate: Rate acc; sum: Sum acc | top-k and sum_over_time: whole-expression CountSketch + heap over raw samples | separate | valid, costlier | 56.440 vs 4.620 cost/s |
| P33 (L33) | Q1 exact · Q2 exact · shared input | rate: raw; sum: raw | top-k: exact (sort → limit); sum_over_time: raw | shared | valid, costlier | 5.920 vs 4.620 cost/s |
| P34 (L34) | Q1 exact · Q2 exact (Sum acc) · shared input | rate: raw; sum: raw | top-k: exact (sort → limit); sum_over_time: Sum acc | shared | valid, costlier | 5.620 vs 4.620 cost/s |
| P35 (L35) | Q1 exact · Q2 CMS+heap · shared input | rate: raw; sum: raw | top-k: Count-Min + heap; sum_over_time: raw | shared | valid, costlier | 5.320 vs 4.620 cost/s |
| P36 (L36) | Q1 exact · Q2 CMS+heap (Sum acc) · shared input | rate: raw; sum: raw | top-k: Count-Min + heap; sum_over_time: Sum acc | shared | valid, costlier | 5.020 vs 4.620 cost/s |
| P37 (L37) | Q1 exact · Q2 CountSketch+heap · shared input | rate: raw; sum: raw | top-k: CountSketch + heap; sum_over_time: raw | shared | valid, costlier | 17.120 vs 4.620 cost/s |
| P38 (L38) | Q1 exact · Q2 CountSketch+heap (Sum acc) · shared input | rate: raw; sum: raw | top-k: CountSketch + heap; sum_over_time: Sum acc | shared | valid, costlier | 16.820 vs 4.620 cost/s |
| P39 (L39) | Q1 exact · Q2 whole-expression CMS+heap · shared input | rate: raw; sum: raw | top-k and sum_over_time: whole-expression Count-Min + heap over raw samples | shared | valid, costlier | 6.920 vs 4.620 cost/s |
| P40 (L40) | Q1 exact · Q2 whole-expression CountSketch+heap · shared input | rate: raw; sum: raw | top-k and sum_over_time: whole-expression CountSketch + heap over raw samples | shared | valid, costlier | 54.120 vs 4.620 cost/s |
| P41 (L41) | Q1 exact (Rate acc) · Q2 exact · shared input | rate: Rate acc; sum: raw | top-k: exact (sort → limit); sum_over_time: raw | shared | valid, costlier | 5.620 vs 4.620 cost/s |
| P42 (L42) | Q1 exact (Rate acc) · Q2 exact (Sum acc) · shared input | rate: Rate acc; sum: raw | top-k: exact (sort → limit); sum_over_time: Sum acc | shared | valid, costlier | 5.320 vs 4.620 cost/s |
| P43 (L43) | Q1 exact (Rate acc) · Q2 CMS+heap · shared input | rate: Rate acc; sum: raw | top-k: Count-Min + heap; sum_over_time: raw | shared | valid, costlier | 5.020 vs 4.620 cost/s |
| P44 (L44) | Q1 exact (Rate acc) · Q2 CMS+heap (Sum acc) · shared input | rate: Rate acc; sum: raw | top-k: Count-Min + heap; sum_over_time: Sum acc | shared | valid, costlier | 4.720 vs 4.620 cost/s |
| P45 (L45) | Q1 exact (Rate acc) · Q2 CountSketch+heap · shared input | rate: Rate acc; sum: raw | top-k: CountSketch + heap; sum_over_time: raw | shared | valid, costlier | 16.820 vs 4.620 cost/s |
| P46 (L46) | Q1 exact (Rate acc) · Q2 CountSketch+heap (Sum acc) · shared input | rate: Rate acc; sum: raw | top-k: CountSketch + heap; sum_over_time: Sum acc | shared | valid, costlier | 16.520 vs 4.620 cost/s |
| P47 (L47) | Q1 exact (Rate acc) · Q2 whole-expression CMS+heap · shared input | rate: Rate acc; sum: raw | top-k and sum_over_time: whole-expression Count-Min + heap over raw samples | shared | valid, costlier | 6.620 vs 4.620 cost/s |
| P48 (L48) | Q1 exact (Rate acc) · Q2 whole-expression CountSketch+heap · shared input | rate: Rate acc; sum: raw | top-k and sum_over_time: whole-expression CountSketch + heap over raw samples | shared | valid, costlier | 53.820 vs 4.620 cost/s |
| P49 (L49) | Q1 exact (Sum acc) · Q2 exact · shared input | rate: raw; sum: Sum acc | top-k: exact (sort → limit); sum_over_time: raw | shared | valid, costlier | 5.820 vs 4.620 cost/s |
| P50 (L50) | Q1 exact (Sum acc) · Q2 exact (Sum acc) · shared input | rate: raw; sum: Sum acc | top-k: exact (sort → limit); sum_over_time: Sum acc | shared | valid, costlier | 5.520 vs 4.620 cost/s |
| P51 (L51) | Q1 exact (Sum acc) · Q2 CMS+heap · shared input | rate: raw; sum: Sum acc | top-k: Count-Min + heap; sum_over_time: raw | shared | valid, costlier | 5.220 vs 4.620 cost/s |
| P52 (L52) | Q1 exact (Sum acc) · Q2 CMS+heap (Sum acc) · shared input | rate: raw; sum: Sum acc | top-k: Count-Min + heap; sum_over_time: Sum acc | shared | valid, costlier | 4.920 vs 4.620 cost/s |
| P53 (L53) | Q1 exact (Sum acc) · Q2 CountSketch+heap · shared input | rate: raw; sum: Sum acc | top-k: CountSketch + heap; sum_over_time: raw | shared | valid, costlier | 17.020 vs 4.620 cost/s |
| P54 (L54) | Q1 exact (Sum acc) · Q2 CountSketch+heap (Sum acc) · shared input | rate: raw; sum: Sum acc | top-k: CountSketch + heap; sum_over_time: Sum acc | shared | valid, costlier | 16.720 vs 4.620 cost/s |
| P55 (L55) | Q1 exact (Sum acc) · Q2 whole-expression CMS+heap · shared input | rate: raw; sum: Sum acc | top-k and sum_over_time: whole-expression Count-Min + heap over raw samples | shared | valid, costlier | 6.820 vs 4.620 cost/s |
| P56 (L56) | Q1 exact (Sum acc) · Q2 whole-expression CountSketch+heap · shared input | rate: raw; sum: Sum acc | top-k and sum_over_time: whole-expression CountSketch + heap over raw samples | shared | valid, costlier | 54.020 vs 4.620 cost/s |
| P57 (L57) | Q1 exact (Sum acc, Rate acc) · Q2 exact · shared input | rate: Rate acc; sum: Sum acc | top-k: exact (sort → limit); sum_over_time: raw | shared | valid, costlier | 5.520 vs 4.620 cost/s |
| P58 (L58) | Q1 exact (Sum acc, Rate acc) · Q2 exact (Sum acc) · shared input | rate: Rate acc; sum: Sum acc | top-k: exact (sort → limit); sum_over_time: Sum acc | shared | valid, costlier | 5.220 vs 4.620 cost/s |
| P59 (L59) | Q1 exact (Sum acc, Rate acc) · Q2 CMS+heap · shared input | rate: Rate acc; sum: Sum acc | top-k: Count-Min + heap; sum_over_time: raw | shared | valid, costlier | 4.920 vs 4.620 cost/s |
| P60 (L60) | Q1 exact (Sum acc, Rate acc) · Q2 CMS+heap (Sum acc) · shared input | rate: Rate acc; sum: Sum acc | top-k: Count-Min + heap; sum_over_time: Sum acc | shared | **selected** | cheapest valid (4.620 cost/s) |
| P61 (L61) | Q1 exact (Sum acc, Rate acc) · Q2 CountSketch+heap · shared input | rate: Rate acc; sum: Sum acc | top-k: CountSketch + heap; sum_over_time: raw | shared | valid, costlier | 16.720 vs 4.620 cost/s |
| P62 (L62) | Q1 exact (Sum acc, Rate acc) · Q2 CountSketch+heap (Sum acc) · shared input | rate: Rate acc; sum: Sum acc | top-k: CountSketch + heap; sum_over_time: Sum acc | shared | valid, costlier | 16.420 vs 4.620 cost/s |
| P63 (L63) | Q1 exact (Sum acc, Rate acc) · Q2 whole-expression CMS+heap · shared input | rate: Rate acc; sum: Sum acc | top-k and sum_over_time: whole-expression Count-Min + heap over raw samples | shared | valid, costlier | 6.520 vs 4.620 cost/s |
| P64 (L64) | Q1 exact (Sum acc, Rate acc) · Q2 whole-expression CountSketch+heap · shared input | rate: Rate acc; sum: Sum acc | top-k and sum_over_time: whole-expression CountSketch + heap over raw samples | shared | valid, costlier | 53.720 vs 4.620 cost/s |

Count-Min + heap is valid, whole-expression or not: Q2 ranks raw samples of
`http_requests_total`, or their `sum_over_time`, and the workload declares the
metric a counter, so Pass 1 attaches the `CounterSamples` non-negativity
proof. The planner never infers this from the `_total` suffix: without the
declaration, or for a gauge, the 24 Count-Min + heap candidates are invalid.
CountSketch admits signed weights either way.

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

Outcome with the built-in models, per evaluation (CPU-ms; Stage 3 reports
these × 0.1 evaluations/s, as cost per second, see
[Stage 3 cost model](stage3-cost-model.md)): P60 (Q1 exact with Rate and Sum
accumulators; Q2 Count-Min + heap over an exact `sum_over_time` accumulator;
shared input) at 46.201, or 4.620 per second, against 73.401 for the same
choices with separate inputs (P28). The scan (23.2) and range (4.0) are priced
once instead of twice. Q2 costs 13.001 there: the Sum accumulator (5.0), the
Count-Min sketch updating 1,000,000 per-series sums × depth 8 (8.0) and its
estimate (0.001). The all-exact P58 (52.201, Q2 19.001 with sort + limit),
selected before the declaration, now ranks 8th: the seven shared-input
Count-Min + heap plans over per-series sums cost 46.201–52.201 (P51 ties P58).
The whole-expression Count-Min + heap plans rank 17th–20th (P63 cheapest at
65.201: 4,000,000 raw samples × depth 8 = 32.0), and the cheapest
whole-expression CountSketch + heap plan, P64, still costs 537.201 (depth 125
+ 1 heap update = 504.0). The doc's other typical winners need
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
11. **Whole-expression top-*k*.** The doc draws Q2's sketch options over
    "input". The planner offers both a heap sketch over the exact per-series
    sums and one over the raw samples of the range (sum_over_time is additive,
    so per-series weights add up in the sketch). Both are kept; Stage 3 ranks
    them by cost.
