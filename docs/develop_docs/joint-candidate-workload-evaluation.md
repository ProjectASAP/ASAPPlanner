# Joint candidate search: ASAPQuery workload evaluation

Developer evaluation, 2026-10-24. These are exploratory optimizer measurements and modeled execution costs, not measured query-runtime performance. Single debug-build runs on a shared host do not establish a speedup.

## Workloads and settings

The exact viewer manifests are `tools/dag-viewer/workloads/random-mixed-seed-862/workload-01.json`, `random-mixed-seed-862-x5/workload-01.json`, and `random-mixed-seed-862-long/workload-01.json`. Each contains nine original PromQL queries. The x5 manifest increases metric cardinality from 10,000 to 50,000 series and total ingestion from 200,000 to 1,000,000 rows/s. The long manifest retains the original cardinality and triples window lengths. These are three representative manifests from the viewer's thirty-workload ASAPQuery collection.

All runs retain the viewer's settings: raw data and query-time results are not retained, AWS Fargate us-east-1 prices, and accuracy guarantees enforced (error target 0.05, failure probability 0.3). No query was rewritten. The separately labeled three-query subset contains original workload-01 queries q03, q04, and q05, including their outer `avg` operators, with unchanged requirements and data facts.

The external wall deadline is 120 seconds per run. Memo-MILP receives a 120-second internal budget; candidate-connections receives 30 seconds and the route's 128-refinement cap. These differing internal budgets are intentional diagnostic settings, not an equal-budget comparison. The budget is checked between solver/evaluator calls, so a final call can overshoot it. A killed process does not provide a completed plan, even if an internal incumbent existed. An exit code of zero is not sufficient: the devtool can serialize a selection error with `selected: null`.

## Latest migration checkpoint: sharing, accuracy pruning and cost bounds

At `c196e423`, compatible scan-label projections can share one pane state; certified query-closure cuts remove size-independent accuracy failures, and conservative ingestion Scan/DDSketch pane costs supply nonzero lower bounds. The same three unchanged nine-query manifests all produced plans accepted by the accuracy evaluator.

| Workload | Wall s | Peak RSS KiB | Modeled cost/s | Reported gap | Termination |
|---|---:|---:|---:|---:|---|
| seed-862 | 21.31 | 92468 | 1.63143868138e-06 | 15.56% | solver GapLimit |
| seed-862-x5 | 15.03 | 91904 | 8.15816843789e-06 | 15.57% | solver GapLimit |
| seed-862-long | 33.08 | 111624 | 4.69721193613e-06 | 13.47% | time limit |

The seed-862 cost is 45.5% below the preceding contract-aware baseline and approximately 3.5% above the legacy selected cost. All runs retained `joint-plan-1`; better candidate generation improved the initial plan, while the new lower bounds improved the optimization certificate. They do not demonstrate proven optimality or equal-budget runtime speedup. The two `GapLimit` runs stopped because the solver adapter did not expose a certified bound for its nonoptimal-status proposal; the validated incumbent and last certified bound were retained. The long run exceeded its cooperative 30-second search budget during a final call. Broader rule coverage, tighter contextual bounds and budget enforcement remain migration work.

Artifacts use the `migrated-bounds-seed862` prefix in the local artifact directory. The JSON report records executable SHA-256, all stage timings, model sizes, rejection counts and query cuts. Earlier results below are retained as historical checkpoints.

## Earlier checkpoint results

Wall time includes CLI startup, parsing/lowering, planning, validation, and JSON serialization. Peak RSS is from `/usr/bin/time -v` and measures planner-process memory, not selected-plan memory.

| Workload | Requested search | Outcome | Wall s | Peak RSS KiB | Modeled cost/s |
|---|---|---|---:|---:|---:|
| seed-862 | legacy | selected | 93.705 | 171240 | 1.57641903686e-06 |
| seed-862-x5 | legacy | timeout | 120.015 | 168396 | — |
| seed-862-long | legacy | timeout | 120.245 | 168320 | — |
| seed-862 | memo-milp | selected | 72.062 | 100668 | 1.58015019404e-06 |
| seed-862-x5 | memo-milp | selected | 89.717 | 102796 | 7.90114096225e-06 |
| seed-862-long | memo-milp | selected | 97.601 | 115124 | 4.56191027772e-06 |
| seed-862 | candidate-connections | selected (time_limit) | 30.881 | 88516 | 2.9951419551e-06 |
| seed-862-x5 | candidate-connections | selected (time_limit) | 30.866 | 89948 | 1.49766818577e-05 |
| seed-862-long | candidate-connections | selected (time_limit) | 32.372 | 102724 | 8.74774358445e-06 |
| seed862-original-q03-q04-q05 | legacy | selected | 2.112 | 68008 | 1.38362805309e-06 |
| seed862-original-q03-q04-q05 | candidate-connections | selected (time_limit) | 30.440 | 81724 | 2.77165140146e-06 |

Unsupported results and runs without an incumbent are failures to produce a plan, not faster planning results. Requested Memo-MILP may route to the existing broader search; inspect the emitted search report before attributing results to a solver.

Final candidate-connection runs all stopped at the time limit with lower bound zero and relative gap 1.0. Each retained its validated seed incumbent (`joint-plan-1`); none proves optimality. Their modeled costs are about 1.90–1.92 times the existing Memo-MILP plans for the three full workloads, and the subset costs about twice the legacy subset plan. A shorter configured run is not evidence of better optimization throughput at equal plan quality.

| Workload | Refinements | Missing-guarantee rejections | Accepted queries |
|---|---:|---:|---:|
| random-mixed-seed-862 | 91 | 90 | 9 |
| random-mixed-seed-862-x5 | 90 | 89 | 9 |
| random-mixed-seed-862-long | 87 | 86 | 9 |
| seed862-original-q03-q04-q05 | 102 | 85 | 3 |

## Selected-plan estimates

| Workload / search | Accuracy-accepted queries | Worst modeled latency ms | Modeled peak memory bytes | Modeled CPU cores | Modeled disk bytes |
|---|---:|---:|---:|---:|---:|
| random-mixed-seed-862 / legacy | 9 | 1.1 | 1313441848 | 10 | 0 |
| random-mixed-seed-862 / memo-milp | 9 | 1.1 | 1314242152 | 10 | 0 |
| random-mixed-seed-862-x5 / memo-milp | 9 | 5.5 | 6571202152 | 10 | 0 |
| random-mixed-seed-862-long / memo-milp | 9 | 3.1 | 3874242152 | 10 | 0 |
| random-mixed-seed-862 / candidate-connections | 9 | 1.09 | 2533281848 | 10 | 0 |
| random-mixed-seed-862-x5 / candidate-connections | 9 | 5.45 | 12666401848 | 10 | 0 |
| random-mixed-seed-862-long / candidate-connections | 9 | 3.09 | 7462881848 | 10 | 0 |
| seed862-original-q03-q04-q05 / legacy | 3 | 1.1 | 1194720808 | 4 | 0 |
| seed862-original-q03-q04-q05 / candidate-connections | 3 | 1.09 | 2397760744 | 4 | 0 |

Accuracy acceptance is the planner's analytical/evidence validation, not an empirical error measurement. The JSON artifacts retain per-query acceptance details and latency estimates, CPU operations/s, scan bytes/s, average memory, and selection/rejection reports. No stream was executed to measure actual error, throughput, or latency.

## Refinement diagnostics

- `random-mixed-seed-862` / `candidate-connections`: status `time_limit`, 91 refinements, 35 groups, 73 logical candidates, 361 physical configurations, 436 compatible connections. Memo/rule expansion 26.180011999999998 ms; inventory construction 17.634411999999998 ms; model construction 11580.880698999998 ms; solver 14816.506773000003 ms; validation 3519.164243999999 ms; peak model 1875 variables / 3921 constraints.
- `random-mixed-seed-862-x5` / `candidate-connections`: status `time_limit`, 90 refinements, 35 groups, 73 logical candidates, 361 physical configurations, 436 compatible connections. Memo/rule expansion 27.610678999999998 ms; inventory construction 17.677854 ms; model construction 11589.098659000001 ms; solver 14763.246854999998 ms; validation 3543.2489290000008 ms; peak model 1875 variables / 3921 constraints.
- `random-mixed-seed-862-long` / `candidate-connections`: status `time_limit`, 87 refinements, 35 groups, 73 logical candidates, 361 physical configurations, 436 compatible connections. Memo/rule expansion 26.066677 ms; inventory construction 17.579928000000002 ms; model construction 10910.626024 ms; solver 13944.631198999996 ms; validation 5165.354629000002 ms; peak model 1875 variables / 3918 constraints.
- `seed862-original-q03-q04-q05` / `candidate-connections`: status `time_limit`, 102 refinements, 14 groups, 34 logical candidates, 174 physical configurations, 250 compatible connections. Memo/rule expansion 12.285896 ms; inventory construction 10.058915 ms; model construction 6252.24072 ms; solver 9736.373036000003 ms; validation 13980.816669000005 ms; peak model 993 variables / 2226 constraints.
- `random-mixed-seed-862` / existing `memo-milp`: memo build 223.66578900000002 ms, 31 groups / 147 candidates; solver 53671 ms, 35 rounds, 6755 variables / 14154 constraints; reported gap 5.2454049999154154e-06, time-limit reached False.
- `random-mixed-seed-862-x5` / existing `memo-milp`: memo build 219.144218 ms, 31 groups / 147 candidates; solver 66159 ms, 35 rounds, 6757 variables / 14156 constraints; reported gap 5.296824867621686e-06, time-limit reached False.
- `random-mixed-seed-862-long` / existing `memo-milp`: memo build 219.51357299999998 ms, 31 groups / 147 candidates; solver 69754 ms, 35 rounds, 6758 variables / 14158 constraints; reported gap 5.419282214762817e-06, time-limit reached False.

Missing phase timings are unavailable in that binary/report, not zero. Candidate-generation time is not interchangeable with total CLI wall time.

## Coverage interpretation

The first full-workload attempt stopped on fused-computation metadata; this is an adapter coverage limitation. The unchanged quantile subset also requires a window implementation absent from the initial restricted catalog: each query reads a 100-second window every second, while raw data is not retained. Stage 2 only permits whole-window ingestion states when the evaluation interval is a multiple of the window width (`ingestion_eligibility` in `physical-asap-optimization`). These queries therefore need supported pane-chain candidates. A Pass-1/shared-summary catalog without pane expansion cannot supply them. Rejecting that restricted catalog does not mean the workload is infeasible: the existing broader searches produce pane-based plans.

The final measured catalog includes local panes and shared identical summary/pane states, preserves canonical input boundaries and their annotations, and seeds search with bounded contract-aware candidate choices. This seed favors numeric-value accuracy contracts for value requirements instead of rank-only summaries. It produces a validated incumbent for all three full workloads in these runs; subsequent complete-assignment refinement still spends most validations rejecting outputs without usable accuracy guarantees. Cross-width/horizon pane reuse, rollup, scan widening, and top-k reuse expansion are not included. Refer to the observed-results table for the final executable's measurements. A time-limit incumbent is validated but is not a proven optimum; no incumbent means no plan was produced within the configured search budget.

Historical artifacts preserve the progression: local panes alone produced a valid subset plan at 4.140868130764761e-6/s; adding identical pane sharing before the final boundary-metadata fix found 2.7716514014591154e-6/s within 30 seconds. Both exceeded the legacy subset's 1.3836280530907367e-6/s. These diagnostics motivated coverage work; neither is evidence of an execution-cost improvement over the existing planner. Full-assignment evaluator refinement with zero certified local cost bounds remains expensive and provides a weak optimization bound.

## Reproduction and artifacts

```bash
LIBCLANG_PATH=/users/zz_y/.local/lib/python3.10/site-packages/clang/native \
CARGO_TARGET_DIR=/mydata/target-joint-candidate-connections \
CARGO_BUILD_JOBS=4 cargo build -p asap-devtools --bin stage_pipeline

/usr/bin/time -v -o run.time.txt timeout 120s \
  /mydata/target-joint-candidate-connections/debug/stage_pipeline \
  --workload tools/dag-viewer/workloads/random-mixed-seed-862/workload-01.json \
  --search candidate-connections --milp-time-limit-s 30 \
  --cost-model aws-fargate-us-east-1 --enforce-accuracy-guarantees \
  --deployment-capabilities '{"raw_data_retained":false,"query_time_results_retained":false}' \
  --out run.json
```

For the existing baseline, use `--search legacy` and omit `--milp-time-limit-s`. For the existing memo formulation, use `--search memo-milp --milp-time-limit-s 120`.

Local artifacts: `/tmp/asap-workload-benchmark-20261024/`. Saved executables reside in `/mydata/asap-workload-benchmark-20261024-binaries/`, linked from the artifact directory; final runs use `stage_pipeline-contract-baseline`. This directory contains original result JSON, timing/RSS logs, stderr, exact command arrays, runner scripts, the unchanged subset manifest, summary JSON, and saved baseline binaries. `before-coverage-pruning/`, `before-shared-panes/`, and `before-metadata-preserving-boundaries/` preserve earlier restricted-route runs; those runs motivated further coverage/pruning work and must not be presented as successful workload coverage.

Environment: `rustc 1.98.1 (48a229cea 2026-09-01)`, `Linux 5.15.0-187-generic x86_64 GNU/Linux`, 56 logical CPUs, unoptimized debug build. Initial baseline build was based on `b1d1691efc47e4e5a5d17c202b5839ea8636bc48` with in-flight local changes; `environment.json` records its executable SHA-256. Source edits continued during the session, so executable identities and per-run artifacts are the precise measurement references.

Other tests and an unrelated long-running planner occupied the shared host; some diagnostic runs overlapped. These results diagnose coverage and search behavior, and are not controlled performance comparisons. Repeat release-build trials on an idle host before making optimization-throughput claims.

## Historical diagnostic: baseline without accuracy-contract preference

A subsequent diagnostic binary added bounded same-graph baseline seeding and rejection counters. The unchanged seed862 nine-query workload still produced no incumbent in 30.145 seconds: all 91 validation rejections were `no_accuracy_guarantee`. The unchanged three-query subset produced `joint-plan-3` in 30.507 seconds at the same modeled cost, 2.7716514014591154e-6/s, with 82 missing-guarantee rejections among 100 refinements. Its lower bound remains zero and reported relative gap is 1.0. The x5 and long workloads were not rerun for this diagnostic. This isolates accuracy-admissible candidate selection as the immediate issue; additional complete-assignment exclusions alone are not resolving it efficiently.

The diagnostic executable is `stage_pipeline-bounded-baseline`; its SHA-256 and exact run artifacts are retained in the local artifact directory. Earlier full results remain under `before-bounded-baseline/`.

## Verified cause of the subset's extra state cost

The archived bounded-baseline selected DAG confirms two state producers in `joint-plan-3`. Scan 0 exposes `job`, `region`, and `service` plus series identity; PaneChain 1 feeds both the P99 reader (node 3, original q03) and P75 reader (node 25, q05). Scan 12 exposes only `region` plus series identity; a second PaneChain 13 feeds P95 (node 15, q04). Both are DDSketch configurations with alpha 0.05, 100 panes of 1,000 ms, and per-entity state. Each retains 1,179,680,000 bytes and contributes 1.3590971293056488e-6/s to modeled cost.

The legacy selected DAG `P6-m3` has only the wider Scan 0 and PaneChain 1. Its single Merge 2 feeds all three quantile readers (nodes 3, 12, and 22). Thus the extra chain, scan, and repeated merge work are directly visible in the selected plans; this is not merely a speculation based on total cost. At that earlier checkpoint, the catalog's exact-schema connections did not provide the conversion needed to reuse the wider state for the region-only consumer. The legacy DAG demonstrates that wider-input reuse occurs, although this export does not identify the particular rule that introduced it. Compact extracted evidence is in `subset-sharing-evidence.json` in the artifact directory.
