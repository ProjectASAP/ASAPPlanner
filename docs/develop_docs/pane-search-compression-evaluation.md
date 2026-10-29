# Pane-search compression evaluation

This report is for planner developers. It separates three changes: storing repeated supplier lists once, factoring equivalent candidate connections, and spending a bounded search budget across progressively larger pane-width domains. Storage compression alone does not reduce the number of MILP variables. Searching fewer widths before a timeout does not prove equivalence to the complete domain.

## Reproduction

Use frozen `stage_pipeline` executables; record their SHA-256 hashes. Do not run the four-case comparison concurrently with another benchmark. Both sides use the same input files, AWS pricing, deployment capabilities, enforced accuracy, a 30-second cooperative planning budget, and a 90-second external process timeout. Each case runs sequentially. Whole-process wall time and peak RSS include plan export and validation, so wall time may exceed the cooperative planning budget.

```bash
python3 tools/benchmark_pane_search.py summarize BEFORE_BINARY BEFORE_DIR
python3 tools/benchmark_pane_search.py run AFTER_BINARY AFTER_DIR
python3 tools/benchmark_pane_search.py compare BEFORE_DIR AFTER_DIR COMPARISON.md
```

`summarize` imports existing plan, timing, and `summary.json` artifacts without executing the planner again. Each directory receives `results.json`, with the binary hash, input fingerprints, full search reports, selected cost/usage, and acceptance checks. Comparison rejects mismatched inputs, budgets, or concurrency. Missing old metrics remain **unreported**, rather than zero.

Broader acceptance uses the same runner:

```bash
python3 tools/benchmark_pane_search.py run AFTER_BINARY VIEWER_DIR --suite viewer --jobs 2
python3 tools/benchmark_pane_search.py run AFTER_BINARY ASAP_DIR --suite asap --jobs 2
```

For an archived 30-workload baseline without input fingerprints, compare only cost and acceptance after checking its original pricing configuration:

```bash
python3 tools/benchmark_pane_search.py compare OLD_ASAP_DIR ASAP_DIR QUALITY.md --quality-only
```

This mode checks serialized workloads, deployment capabilities, accuracy modes, Stage 0 DAGs, and objective/calibration settings. It deliberately excludes planning-time and memory comparisons; the old runner must confirm the pricing profile because historical documents do not serialize it.


These suites contain the nine viewer workload examples and 30 mixed ASAPQuery workloads, respectively. They check query coverage, enforced accuracy, finite cost, valid cost bounds, no stored-output errors, and no global bound, gap, optimum, or exhaustion claim while pane widths remain pending. Existing Rust/JavaScript tests remain responsible for DAG structure and installation invariants.

## Baseline

The existing four-case run is complete. Its frozen executable is `/mydata/asap-final-validation/stage-pipeline-final-composed-budget`; raw artifacts are under `/mydata/pane-compression-validation/before`. All four plans cover every query with enforced guarantees and no stored-output error.

| Workload | Queries accepted | Wall seconds | Peak RSS KiB | Physical configurations | Compatible connections | Peak MILP variables | Modeled cost/second |
|---|---:|---:|---:|---:|---:|---:|---:|
| mixed seed 862, workload 01 | 9/9 | 36.08 | 510400 | 5785 | 147279 | 159153 | 1.60464455e-6 |
| long mixed seed 862, workload 01 | 9/9 | 32.94 | 415108 | 5785 | 147279 | 159153 | 4.62805740e-6 |
| rangeheavy t8/t5, 1h | 3/3 | 33.67 | 447128 | 17252 | 1705248 | 44034 | 8.33239804e-5 |
| rangeheavy t5, 15m/1h | 2/2 | 33.26 | 949392 | 3505 | 212790 | 390981 | 1.00719775e-5 |

All four report `time_limit`, with `full_catalog_completed=true`. That flag means configuration generation completed, not that every assignment was evaluated or a global optimum was proved. Peak MILP size can be smaller than the reported final compatibility graph when the deadline prevents constructing a model for that graph. The baseline does not report unique supplier sets or physically stored supplier IDs.

## Acceptance criteria

Record candidate counts, logical compatibility connections, unique supplier sets, stored supplier IDs, peak MILP variables/constraints, generation/inventory/model/solve/validation times, whole-process RSS, selected cost, and accuracy. Also record attempted width domains, pending widths, catalog completion, solver status, and gap. Report before/after values together; do not infer a complete-domain model reduction from a smaller intermediate round.

Independent correctness checks should cover:

- Supplier-list sharing preserves membership, order, and duplicate input positions. Mutating one shared list must not mutate another list. Cached compatibility includes every semantic/configuration field used by the uncached check.
- Tiny complete-domain problems retain the same feasible assignments and minimum evaluated cost before and after factoring. Include multiple readers, different materializations, duplicate input positions, optional dependencies, and shared upstream producers. Reconstruction must restore every factored operation and preserve costs, accuracy, and query coverage.
- Terminal unary factoring must not absorb a node with additional consumers, observable materialization/state, unsupported error propagation, or different execution demand. Shared producer work remains charged once; reader-specific work remains per use.
- Progressive pane-width domains are nested; each round adds `Panes` candidates to the candidate memo only for newly admitted pane widths. Each round rebuilds its physical inventory and MILP, so previously considered assignments may be revisited. The domains eventually include the complete legal finite domain when the budget permits. A restricted-domain optimum never becomes a global certificate; timeout preserves the best validated incumbent.
- The four-case benchmark must show measurable changes on the dense rangeheavy cases. Report cost regressions or incomplete-domain effects explicitly. Full 30-workload and nine-viewer acceptance must pass without relaxing accuracy or latency requirements.

## After-change results

Final sequential measurements use production revision `ba7dca2c`, including the report-only snapshot fix. The original after-run is retained separately; the table below uses only `after-final` artifacts.

Before binary SHA-256: `8e182907f9c840c7134876e7d626fdb95223270b4c012ff16735dcf366fefff2`. After binary SHA-256: `52533a57f715f51b889e4f031281f29730607b85a71611ad03e468655fec6b0b`.

All four workloads passed with the **same selected modeled cost** as their respective baselines and all 23 query requirements accepted. No planner regression was running during these four sequential measurements.

| Workload | Wall seconds, before → after | RSS KiB, before → after | Peak MILP variables, before → after | Peak MILP constraints, before → after |
|---|---:|---:|---:|---:|
| mixed 01 | 36.08 → 30.69 | 510400 → 207948 | 159153 → 55501 | 306928 → 105689 |
| long mixed 01 | 32.94 → 31.67 | 415108 → 203444 | 159153 → 55501 | 306928 → 105689 |
| rangeheavy t8/t5 | 33.67 → 30.10 | 447128 → 353472 | 44034 → 111171 | 59826 → 332191 |
| rangeheavy 15m/1h | 33.26 → 30.42 | 949392 → 459012 | 390981 → 198129 | 1108599 → 558375 |

The two-window rangeheavy case used **51.7% less peak RSS** and **49.3% fewer peak MILP variables** in this run. Conversely, the t8/t5 case solved a larger intermediate model: peak variables increased from 44,034 to 111,171 while RSS fell 20.9%. Its baseline generated a dense catalog but did not solve that entire model within the budget. These are same-budget observations, not same-domain compression ratios. This combined-change experiment does not isolate how much of the memory difference comes from supplier storage, factoring, or the progressive domain policy.

All after-plans report pending widths, no global bound/gap, and no global optimum or exhaustion certificate. The attempted rounds were SeedGrids, then Expanding with maximum parts 4, 8, 16, 32, and 64. The following counters belong to the most recent reported inventory, identified explicitly; a later interrupted preparation round does not overwrite them with zero.

| Workload | Inventory domain | Physical configurations | Logical compatible connections | Unique supplier lists | Stored supplier IDs |
|---|---|---:|---:|---:|---:|
| mixed 01 | Expanding { max_parts: 16 } | 3324 | 69018 | 2124 | 3159 |
| long mixed 01 | Expanding { max_parts: 16 } | 3324 | 69018 | 2124 | 3159 |
| rangeheavy t8/t5 | Expanding { max_parts: 16 } | 8164 | 395168 | 1265 | 8163 |
| rangeheavy 15m/1h | Expanding { max_parts: 64 } | 2809 | 137982 | 1356 | 2323 |

Within these after-inventories, stored supplier IDs are 95.4%–98.3% fewer than the logical connection count. This measures list-content storage sharing only; it excludes allocator metadata and does not imply that equivalent MILP connection variables disappeared. The baseline lacks this storage instrumentation. Baseline candidate/connection counts describe complete width catalogs; the after counts above describe intermediate catalogs, so their differences combine factoring and restricted enumeration.

Reported cumulative phase times, in seconds (before → after):

| Workload | Local generation | Memo expansion | Inventory | Model construction | Solver | Validation |
|---|---:|---:|---:|---:|---:|---:|
| mixed 01 | 0.007 → 0.007 | 7.175 → 4.314 | 6.743 → 6.178 | 2.834 → 1.011 | 16.716 → 7.423 | 1.182 → 1.040 |
| long mixed 01 | 0.008 → 0.007 | 7.005 → 4.326 | 6.808 → 6.154 | 2.823 → 1.018 | 10.408 → 5.501 | 3.526 → 2.858 |
| rangeheavy t8/t5 | 0.002 → 0.002 | 3.189 → 1.579 | 10.513 → 4.355 | 0.431 → 1.849 | 5.486 → 8.890 | 1.784 → 0.350 |
| rangeheavy 15m/1h | 0.001 → 0.001 | 1.609 → 2.241 | 1.580 → 2.392 | 5.298 → 4.492 | 23.081 → 19.424 | 0.586 → 0.586 |

These phase values are the emitted report counters. Interrupted preparation rounds may not be fully attributed to individual phases; their sum therefore does not equal whole-process wall time. Wall time and peak RSS cover the complete process. Do not treat every reported phase reduction as a measured reduction in all work of that kind.

Artifacts: `/mydata/pane-compression-validation/{before,after-final}/results.json`, full plan JSON and GNU time logs in the same directories, and `comparison-final.md` with all before/after counters. Single runs do not establish statistical significance. Costs and accuracy are validated planner-model results, not measured stream-runtime performance.

### Broader acceptance

The 30 mixed ASAPQuery workloads passed **30/30**, with **270/270** query requirements accepted. Their selected modeled costs match the previous composed-budget binary **exactly in all 30 cases**, including workload 08. Serialized workloads, deployment capabilities, accuracy modes, and Stage 0 DAGs match the archived baseline. Both runners use the same AWS pricing profile and enforced accuracy; this is a quality comparison, not a planning-time comparison.

The archived baseline is `/mydata/asap-final-validation/final-workloads-composed-budget/results.json`, using the same before-binary hash listed above. Current results are in `asap-final/results.json`; `asap-quality-comparison.md` contains every case's costs and acceptance counts. Some final interrupted-round reports omit objective/calibration labels; the comparator explicitly marks these unknown and relies on the verified original runner settings rather than interpreting the default calibration value as zero-priced execution.

The nine viewer workloads also passed **9/9**, with **40/40** query requirements accepted, including the composed-quantile ratio and SQL rollup examples. Their artifacts are in `viewer-final/results.json`. Broad acceptance uses two concurrent workloads and the same 30-second budget, and may overlap regression tests. Its timings are deliberately excluded from the sequential performance comparison.

### Final regression follow-up

The regression suite found a genuine optimization regression outside window planning. Two identical SQL percentiles over an at-rest table were sent through progressive pane rounds solely because they repeat every ten minutes. No computation had a pane-eligible window, so the rounds rebuilt the same 40-configuration, 150-connection model and restarted refinement. The 128-evaluation budget expired with two identical unmaterialized KLL builds, costing 0.007678692288679677 per second instead of the baseline's one shared query-materialized build at 0.006000017830006526 (+27.98%). The original pointer-sharing assertion failed both in the suite and in isolation; it was retained unchanged.

Revision `fe171f64` bypasses pane-domain expansion when the initial computations contain neither a positive range window nor an existing pane chain. It searches the complete candidate domain directly; at-rest status alone does not disable pane planning. The same SQL fixture now passes the original sharing test and selects one shared KLL with exactly the baseline cost. Its report records 58 evaluations, `optimal`, one `Complete` round, and no pending widths. This fix avoids repeated identical-domain searches rather than increasing the budget or relaxing correctness checks.

The final frozen executable is `/mydata/pane-compression-validation/stage-pipeline-after-gate-final`, SHA-256 `a3234d62d2a6717e3772aba240959f6a224a4e1e031fe9cba27854079f8622f2`, built from `fe171f64`. SQL before/after artifacts are `/tmp/identical-sql-{before,after,gate-fixed}.json`; the unchanged integration test log is `/tmp/sql-sharing-pane-gate-after.log`.

A fresh 30-second mixed-workload check on this final executable accepted all nine queries and retained the same modeled cost, 0.0000016046445458277792 per second. Its interrupted expansion report preserves `monetary_cost`, calibration scope `not_required`, factor 1, and the inventory source `Expanding { max_parts: 8 }` with 2,733 configurations and 49,897 connections. Pending widths still suppress global optimality certificates. Artifacts are in `/mydata/pane-compression-validation/final-gate-check/`. This targeted follow-up verifies the route/report fixes; the four-case measurements and 39-workload acceptance above remain attributed to their original frozen binary, not this later revision.
