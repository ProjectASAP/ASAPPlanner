# ASAPPlanner layering: MVP status report

Status as of 2026-10-05. This report lists what the
[ASAPPlanner layering design](planner-layering.md) (#509) MVP has
implemented, the PR that implements each part, and what is left as
follow-up work.

## Contents

- [Summary](#summary)
- [How to read the stack](#how-to-read-the-stack)
- [What is done](#what-is-done)
- [Demo results](#demo-results)
- [What is not done yet](#what-is-not-done-yet)

## Summary

The MVP plans a workload end to end through the four stages of #509:

| Stage | Implemented as |
| --- | --- |
| 0. Frontends | SQL and PromQL lower to one logical DAG on the unified operator IR of #511 |
| 1. Logical ASAP-aware optimization | Pass 1 local alternatives (exact, sketches, HydraCms, filtered aggregates) and Pass 2 sharing rules (identical expressions, summary capability, tumbling panes, shared window segments) |
| 2. Physical ASAP-aware optimization | Materialization per summary: query time, ingestion time, or query time kept across evaluations |
| 3. Plan selection | Accuracy, latency and deployment-capability checks, cost per second including memory, and the cheapest valid plan |

The `stage_pipeline` devtool writes the result of every stage as one JSON
document, and the Stage Viewer (`tools/dag-viewer`) shows it. The six
end-to-end examples of #509 (1, 2, 3a, 3b, 4a, 4b) plan and display; see
[Demo results](#demo-results).

All the work is in 67 draft PRs, stacked in one linear chain on `main` at
`d4869a76` (DataFusion 54). None is merged. At the top of the chain (#641),
`cargo fmt --check`, `cargo clippy -D warnings` and `cargo test --workspace`
pass (1,277 passed, 0 failed, 13 ignored), and so do the viewer tests.

## How to read the stack

Each PR's base is the PR below it. In order, from `main`:

```text
main → #567 … #543            unified IR (#511)
     → #561 … #582            stage pipeline (Stages 1–3) and its facade
     → #578, #583 … #587      crates by stage (#572)
     → #588 … #613            Pass 2, Stage 3 cost, materialization, deployment inputs, examples
     → #574                   Stage Viewer
     → #620 … #634            gaps closed after the MVP demo
     → #616 … #641            removal of the legacy search and cleanup
```

The tables below follow that order. To try the result, check out
`stack/cleanup-12-evidence-api` (#641) and follow the Stage Viewer guide,
`docs/user_guide_docs/stage-viewer.md` (added in #602).

## What is done

### Unified operator IR (#511)

| PR | What it adds |
| --- | --- |
| #567 | Summary coverage metadata (time and population) on summary nodes, so a merge or reuse can be proven safe. Closes #571. Design doc: #573. |
| #560 | Compatible logical summary merges |
| #537 | Logical sub-DAG sharing, canonicalization, and export of the logical DAG without execution timing |
| #539 | SQL lowering through unified operators and scalars |
| #540 | PromQL lowering to the unified operator and scalar IR |
| #541 | The runtime compiles unified operator and scalar DAGs |
| #542 | The planner plans one shared unified workload DAG |
| #543 | Removal of the legacy IR scaffolding; tooling migrated |

### Stage pipeline

| PR | What it adds |
| --- | --- |
| #561 | Stage 1 local logical alternatives without execution timing |
| #575 | Stage 1 whole-workload candidates and the `stage_pipeline` devtool |
| #576 | Stage 2 physical candidates and Stage 3 cost-based selection |
| #577 | Example 1 acceptance spec and end-to-end tests |
| #579 | Top-k sketch readouts return the selected rows; Example 1 expectations follow the planner's output |
| #581 | The facade runs the stage pipeline with tree-DP selection; `MajorPass` retired |
| #582 | Stage 1 no longer depends on the cost model |

### Crates by stage (#572)

| PR | What it adds |
| --- | --- |
| #578 | `asap-types` modules arranged by #511 section |
| #583 | Stage 1 `logical-optimizer` crate |
| #584 | Stage 2 `physical-optimizer` crate |
| #585 | Stage 3 `plan-selection` crate |
| #586 | Pass facade moved into `asap-planner`; `asap-aware-mapping` deleted |
| #587 | `asap-physical-operators` renamed to the `asap-executor` crate |

### Pass 2, Stage 3 cost, materialization and deployment inputs

| PR | What it adds |
| --- | --- |
| #588 | Pass 2 identical-expression rule (shared inputs) |
| #589 | Whole-expression top-k heap sketch over raw samples |
| #594 | Stage 3 cost per second, memory pricing and calibration, with its design doc `stage3-cost-model.md` |
| #593 | Counter metric declarations, so CMS+heap can prove non-negative weights |
| #592 | Coverage time relative to evaluation, and mergeable summary families |
| #591 | Executor: tumbling pane merges |
| #595 | Pass 2 summary-capability rule: one summary sized for the strictest consumer |
| #596 | One UnivMon shared across distinct count, L2 and entropy |
| #597 | Executor: exact distinct count, L2 and entropy; typed UnivMon identities |
| #598 | SQL frontend recognizes the L2 and entropy idioms |
| #599 | Executor runs the exact SQL entropy fallback |
| #601 | Pass 2 window composition: tumbling panes |
| #600 | Executor: HydraCms kernel for count and point queries |
| #604 | Stage 2 materialization: ingestion time or query time per summary, with the latency check |
| #606 | Example 4 acceptance tests with materialization |
| #603 | SQL `COUNT(*)` candidates count rows, not a sample value |
| #605 | Pass 1 offers HydraCms for grouped approximate counts |
| #607 | Executor: filtered aggregates and summary builds |
| #608 | Pass 1: single-measure filtered aggregates |
| #609 | Deployment inputs: the executor's capabilities and raw-data retention as Stage 3 inputs |
| #610 | Deployment inputs in the stage document and the viewer |
| #612 | Example 4 B1/B2 tests as model-relative invariants, and a workload where ingestion-time panes win |
| #613 | `stage_pipeline` examples 2 (SQL) and 4b; Stage 3 prices every plan, and `--max-candidates` only limits the plans written |

### Stage Viewer

| PR | What it adds |
| --- | --- |
| #574 | The viewer redesigned around the stage document: example tabs, workload queries, deployment inputs, the Stage 3 ranking, one lane per stage, node and edge details, and a PromQL query editor. The Pre/Post-ASAP view is removed. |
| #618 | The editor plans SQL queries over declared tables |
| #638 | The stage lanes stacked top to bottom, each as wide as the page |
| #602 | User guide for the Stage Viewer (`docs/user_guide_docs/stage-viewer.md`), in the planner-layering problem-definition docs PR on `main` |

### Gaps closed after the MVP demo

| PR | What it adds |
| --- | --- |
| #620 | Pass 1 offers no per-group sketch for SQL `COUNT(*)`; a test checks that every priced plan binds in the executor |
| #621 | UnivMon L2 certified from layer 0's F2, the executor's L2 readout to match, and UnivMon pricing |
| #627 | Stage 3 checks that a guarantee's error metric fits the requested statistic |
| #625 | Stage 2 "query time, kept" for tumbling panes (Example 4 B3), gated by the deployment's `query_time_retention` |
| #628 | Pass 2 shared window segments across queries over one scan (Example 3 Pattern A) |
| #632 | Stage 3 prices a TimeShift as free and a TimeRange by the rows it keeps |
| #634 | Pass 2 offers more than one segment grid, and Stage 3's cost picks one |
| #640 | Whole-expression top-k keeps its partition keys on the rows it reads. Fixes #639. |

### Removal of the legacy search

| PR | What it removes or moves |
| --- | --- |
| #616 | The `dag_export` devtool |
| #617 | The `asap_types::dag_export` module |
| #622 | The legacy physical-plan cost model |
| #624 | `show_post_asap_ir`; `show_pre_asap_ir` renamed to `show_logical_dag` |
| #629 | Tests select plans through `plan_stages` instead of `global_selection` |
| #630 | The legacy `candidate_selection` |
| #631 | Stage 1's realization helpers moved out of the legacy search |
| #633 | Callers of the legacy search ported to Stage 1 (`sketch_coverage`, corpus tests, executor and frontend tests) |
| #635 | The legacy replacement search and the `CostModel` trait |
| #636 | `AccuracyModel` moved to `plan-selection` |
| #637 | Docs describe the stage pipeline instead of the removed legacy search |
| #641 | The unread accuracy-evidence API |

## Demo results

Each example as planned at the top of the chain (#641). The statistics are
illustrative, so read the costs as a ranking.

| Example | Workload | Selected plan | Cost per second |
| --- | --- | --- | --- |
| 1 | Two PromQL panels over 1M counter series: exact per-job rate, approximate top-10 per job | Q1 exact, Q2 CMS+heap, one shared input scan | 4.62 |
| 2 | SQL distinct sources, entropy and L2 over the last minute of flows | All exact. UnivMon L2 plans are valid but costlier; distinct and entropy have no certified UnivMon model. | 0.0237 |
| 3a | Five p99 reports over 1–5 years, run once | Five KLL sketches over four shared year-aligned segments | 18,104 |
| 3b | p99 over the last 5 min every minute, 1M series | A KLL built at query time | 2.08 |
| 4a | Example 3a repeated monthly | Same plan as 3a | 25.1 |
| 4b | p99 over the last hour every 10 min, 1k series, raw data not kept | Six 10-min KLL panes maintained at ingestion time | 0.902 |

## What is not done yet

### Before merging

| Item | What is needed |
| --- | --- |
| #567 | Reword the problem headline in the PR description, as agreed in its comments: what a summary covers is a node field next to the schema, not part of the schema |
| #537 | Answer the review (changes requested; 10 open threads): why the logical DAG is exported, and the types `wire.rs` duplicates; canonicalization helpers |
| #541 | Answer a pending review question: what the `memory` data source is |

`main` requires every review conversation to be resolved, so the stack merges
from #567 upwards once these are settled.

### Open issues

| Issue | Topic |
| --- | --- |
| #570 | Check a summary's declared coverage population against the filters below it |
| #572 | Crate reorganization tracking issue (implemented by #578 and #583–#587) |
| #580 | Pass 2 sharing and Stage 1 coverage parity with the retired `MajorPass` |
| #615 | Stage 3 cost from evidence (storage I/O, hand-off bytes) |
| #619 | SQL top-k ordered by an alias does not compose a sketch, and the frontend's DAG does not bind |
| #623 | Coverage lost with the legacy `candidate_selection` tests |
| #626 | An exact `Sum` over an Int64 column is priced but does not bind |
| #639 | Whole-expression top-k partition keys (fixed by #640) |

### Known gaps without an issue

**Stage 1 and Pass 2**

- **Windows whose boundaries do not align.** These need true exponential
  histograms, with the error split between the histogram and the sketch.
- **Shared summaries between some queries only.** Pairs of queries, not all
  of them, sharing a window summary are not generated.
- **Pass 1 coverage still missing** (listed in #633):
  - the top-k heap evaluation has no timestamp column;
  - a CMS heap over a rate has no non-negativity proof;
  - Top-K has no item identity on a closed SQL schema;
  - there is no summary over an input with two sources;
  - PromQL HydraCms needs the series-identity column.
- **Filters and HAVING.** A filter is not split per measure, there is no
  HAVING over an aggregate, and there are no filtered summaries at ingestion
  time.
- **Two SQL idioms are not handled:** the `now()` lookback, and integer
  `SUM(c * c)` recognition.

**Stage 2**

- **Rebuilding a shared query-time summary per consumer** (Example 4 A3) is
  not generated.
- **One-off batches get no ingestion-time option** (Example 4 A2), by design.

**Stage 3 and accuracy**

- **UnivMon distinct count and entropy are uncertified.** A certified
  distinct count needs Theta/KMV kernels in the executor. HLL's failure
  probability is unknown, so HLL never meets an ε/δ target.
- **Costs use illustrative statistics.** Real evidence is not connected yet
  (see #615).
- **Storage tiers and retention beyond memory** are not modelled.

**Executor**

- **Keeping query-time panes across evaluations (B3).** The executor
  reports `query_time_retention: false`, so B3 plans are generated but
  rejected. Running them needs kept-pane inputs and outputs in the
  deployment.
- **Exact TopK** is not supported. Under DataFusion 54, `ORDER BY COUNT(*)
  DESC LIMIT k` lowers to TopK, so #620's execution test covers only the
  grouped and ungrouped counts.
- **UnivMon's per-value count build skips NULL items** (#621). SQL `GROUP BY`
  counts NULL as a group.

**To decide**

- Whether to keep the maintained-population rule
  (`MaintainedPopulationStrategy::candidate`). No planner path reaches it any
  more (#641).
