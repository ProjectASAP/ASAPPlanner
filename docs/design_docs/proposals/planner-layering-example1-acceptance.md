# Planner layering, Example 1: acceptance spec (MVP)

Audience: planner designers and the Phase C implementer.
Source: [planner-layering.md](planner-layering.md), "Example 1: Aggregation over
dimensions". Tests: `crates/integration-tests/tests/planner_layering_example1.rs`.
Viewer fixture shape: `tools/dag-viewer/examples/planner-layering-example1.expected.json`.

This spec was written by a test designer who did not implement the stages.

## Scope

| Stage | Doc (#509) | MVP (this spec) |
|---|---|---|
| 0. Frontends | 1 workload `LogicalDAG` | same |
| 1. Logical ASAP | Pass 1 (3) × identical-expression rule (2) × window-composition rule (3 × 3) = **54** | Pass 1 × identical-expression rule = **6**. Window composition is blocked on #511 (#518, #522). |
| 2. Physical ASAP | Materialization options per window form = **156** | Physical operator implementation only, no materialization = **6** (the doc's Raw/Raw plans) |
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

## Stage 1: 6 candidates

| Id | Q1 | Q2 | Input |
|---|---|---|---|
| L1 | exact | exact (per-series sum, top 10 per job) | separate |
| L2 | exact | exact | shared |
| L3 | exact | Count-Min + top-*k* heap, one per job | separate |
| L4 | exact | Count-Min + top-*k* heap, one per job | shared |
| L5 | exact | Hydra (CMS) over all (job, series) keys, heap per job | separate |
| L6 | exact | Hydra (CMS) | shared |

"Shared" means Q1 and Q2 read one `http_requests_total[1m]` input node
(scan and range). Ids and order are illustrative; tests identify candidates by
structure.

Invariants:

* Exactly 6 candidates, one per (Q2 option, separate or shared). No duplicates.
* Every candidate covers both queries.
* Q1 never reaches a summary node.
* The sketch families in Q2 are exactly {`CmsWithHeap` per subpopulation,
  Hydra `HydraCms`}. Exact Q2 has no sketch.
* For each Q2 option, both the independent and the shared variant are present.
* Only the scan and range nodes may be shared. No summary is shared.
* Nothing is pruned: Count-Min and Hydra are admitted by ε = 0.01, δ = 0.001,
  and Q1's exact target admits only the exact option.

## Stage 2: 6 candidates

| Id | From | Q2 physical operators | Timing |
|---|---|---|---|
| P1 | L1 | sort (partition by job) → limit 10 | all query time |
| P2 | L2 | sort → limit 10 | all query time |
| P3 | L3 | CMS + heap build → top-10 estimate | all query time |
| P4 | L4 | CMS + heap build → top-10 estimate | all query time |
| P5 | L5 | Hydra build → top-10 estimate per job | all query time |
| P6 | L6 | Hydra build → top-10 estimate per job | all query time |

Q1 is the same in all six: scan → range → per-series rate → sum by (job).

Invariants:

* Exactly one physical candidate per logical candidate. `from_logical` is a
  bijection onto the Stage 1 ids, so no valid candidate is dropped before
  Stage 3.
* Each physical candidate keeps its logical Q2 option and input sharing.
* Exact TopK is implemented as a sort followed by a limit.
* A summary Q2 is a sketch build node feeding an estimation node. There is no
  merge node, because the MVP has no window summaries.
* No node runs at ingestion time, because the MVP has no materialization.

## Stage 3: 1 plan

Invariants:

* One selected id. Every other candidate is listed once as rejected, with a
  reason, and marked invalid (accuracy, latency or capability) or valid but
  costlier.
* Each candidate's cost has one entry per DAG node, and `total` is their sum.
  A shared node is therefore charged once, for all its consumers.
* The selected plan is no more expensive than any valid candidate.
* For each Q2 option, the shared-input candidate costs no more than its
  separate counterpart.

Expected outcome, not asserted because it depends on the cost and accuracy
models: a shared-input candidate wins, matching the doc's "Raw with a shared
input" winner. An exact Q2 (P1, P2) may be rejected for missing the 100 ms
bound. The doc's other typical winners need window forms or materialization and
are out of MVP scope.

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
   window-composition rule adds 48 of the doc's 54 candidates, and its
   tumbling and sliding variants wait on #511.
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
8. **Other top-*k* families.** The code also knows `CountSketchWithHeap`. The
   doc lists only Count-Min + heap and Hydra, so a third family fails the
   tests until the doc adds it.
9. **Latency.** The doc says an exact top 10 rebuilt at every refresh "may
   miss" 100 ms. Whether P1 and P2 are rejected is left to the cost model.
10. **Accuracy of Count-Min heap merges** matters only with tumbling windows,
    which are out of MVP scope.
