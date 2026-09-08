# Offline Sketch Evidence Integration and o11y Planning Evaluation

> This document preserves the historical state of the first experiment, including
> its uncommitted changes, 24/27 binding result, and unmeasured construction/disk costs.
> See the [final report](offline-o11y-final-2026-09-08.md) for subsequent implementation,
> final results, and delivery status.

## Execution Results

Three agents worked in parallel on the offline evidence provider, real sketch-bench
measurements, and planner replay. The main agent completed control-plane integration,
version compatibility, and integration validation. See the
[execution plan](design_docs/empirical-o11y-execution-plan.md) for the full sequence.

Implementation and measurements used isolated worktrees. Existing conflicts and
changes in the original workspace were left untouched:

- Planner: `/mydata/asapplanner-empirical-o11y`, baseline `378a754`.
- Control plane: `/mydata/ASAPQuery-backend/.worktrees/empirical-o11y-322`, baseline `95131d8`.
- sketch-bench: `/mydata/sketch-bench-empirical-322`, pinned to `87f619e843fd2e4da784160d4e205a0d0d55f032`, with no source changes.

No commits, pushes, PRs, or issue closures had been made at this stage. Changes in
both product worktrees remained available for local review.

## Implemented Path for #322

1. A versioned JSON artifact, JSON Schema, explicitly labeled synthetic test fixture, and real measurement files.
2. Offline error, CPU, memory, distribution, exact parameters, environment, sample counts, variability, validity periods, and provenance.
3. An evidence-based ranking interface on the public `CostModel`, plus control-plane evidence lookup using its actual sizing parameters.
4. Ranking by update CPU when all candidates have compatible measurements; default ordering is preserved for missing, expired, ambiguous, or configuration/environment/distribution-incompatible evidence.
5. Tests showing that changing compatible evidence changes actual binding while preserving parameters and accuracy guarantees. Real measurements support the default CMS preference; no benefit or decision change was manufactured.

This ranking minimizes per-update CPU, rather than a combined CPU/memory/disk
objective. Offline errors remain observations for their corresponding queries;
they are neither promoted to formal guarantees nor used directly to shrink sketches.
The lifecycle interface exposes only supported partial costs. A complete plan
remains unestimable when semantically matched readout, retention, retirement, or
raw/residual costs are missing. Realtime ground truth, self-estimated error, and
online feedback are outside this experiment's scope.

## Data and Measurements

The real sketch-bench compares CMS, CountSketch, and an exact Polars group-by +
HashMap frequency index. Each dataset contains 20,000 i64 keys drawn from a
key-space of 1,000 with seed 42. Uniform contains 1,000 distinct keys; Zipf
(exponent 1.1) contains 952. Each CPU measurement has five runs after two warmups.
Errors come from one offline accuracy experiment on each fixed dataset.

| Distribution / algorithm | Update CPU (ns/item) | Read CPU (ns/key) | Retained / peak requested heap | Mean absolute relative frequency error |
| --- | ---: | ---: | ---: | ---: |
| Uniform CMS | 36.64 | 49.80 | 5,440 B | 163.47% |
| Uniform CountSketch | 1,179.19 | 3,813.40 | 9,960,000 B | 0 (this offline sample) |
| Zipf CMS | 35.68 | 44.33 | 5,440 B | 231.65% |
| Zipf CountSketch | 948.19 | 3,911.34 | 9,960,000 B | 0 (this offline sample) |

CMS uses parameters 272 × 5; CountSketch uses 30,000 × 83. CMS's additive epsilon
bound is relative to total stream mass, not a 1% relative-error bound for each key.
The errors above must not be interpreted as satisfying the latter. CountSketch's
zero observed error does not guarantee zero error on other data.

Memory is measured with an independent live-object probe that counts requested
heap bytes, excluding input data, allocator page overhead, and whole-process RSS.
Allocator differences between the CPU and memory probes are documented separately.
Disk usage, serialized size, and empty-sketch construction CPU were unmeasured
at this stage and remain null in this historical run.

For 1,000 point-frequency queries after loading these datasets, the measured CPU
components sum to:

| Dataset | CMS | Exact index | CountSketch |
| --- | ---: | ---: | ---: |
| Uniform | 0.783 ms | 0.741 ms | 27.397 ms |
| Zipf | 0.758 ms | 0.799 ms | 22.875 ms |

These are offline component estimates that include exact-index preparation but
exclude unmeasured sketch construction. They therefore cannot establish a complete
break-even point or deployed speedup. The exact index's 290,816 B state footprint
comes from an upstream formula; its difference from requested heap must not be
called a measured reduction in total memory. See the
[measurement report and raw data](../tools/empirical-bench/results/MEASUREMENTS.md).

## o11y Results

The evaluation uses the repository's existing 27 PromQL fixtures: a vendored
snapshot from 2026-07-17, Git blob `ea2eee98d78c5c9354363dc378303d98ac141672`,
without a verifiable upstream commit. This experiment did not run the upstream
LLM-agent scoring benchmark or measure a real o11y data distribution.

| Evaluation layer | Result |
| --- | --- |
| Planner query candidate coverage | 26/27 have exact-summary candidates; 1/27 falls back to raw; 0/27 have approximate-sketch candidates |
| Planner complete lifecycle selection | 27/27 conservatively fall back to raw because complete physical-cost evidence is missing |
| Control-plane parser + typed binder | Each of exact/default/empirical binds 24/27 and rejects 3/27 |
| Separate supplemental queries | Two quantile queries and one count_over_time query; both default and empirical modes can produce sketch candidates |
| Measurement applicability | CMS/CountSketch configurations for count_over_time match update-cost measurements; quantile measurements are missing |

The three forms rejected by the control plane are a bare metric selector,
`sort_desc(...)`, and `... > 0`. These are not parser failures: the typed binder
at this stage has no corresponding bindable root candidate. Successful binding
also does not establish deployable execution; some bound results still fall back
to raw queries. Concat metadata handling and Summary BinaryOp compatibility were
added to connect the newer Planner. Binary operations unsupported by the warm tier
retain an explicit fallback.

Measured evidence did not change o11y selection because these queries have no
corresponding sketch candidates. CMS update CPU is substantially lower for the
supplemental query, so empirical ranking preserves CMS first. Offline point-frequency
error is not treated as count_over_time or o11y result error.

Complete-query CPU and memory benefits are null. Quantifying o11y benefits in the
next stage requires measurements of exact-summary and raw/residual operators on
the corresponding metric data, group cardinalities, windows, and execution
frequencies, followed by complete physical-cost comparison. Adding only more
sketch-algorithm measurements cannot supply those missing inputs.

## Validation and Reproduction

- Planner mapping unit tests: 342 passed.
- Planner replay: four passed; see the benchmark README for export normalization tests.
- All control-plane library unit tests: 633 passed.
- New control-plane integration tests: three passed, covering changed binding, conservative fallback, and binary-operation fallback.
- Other agents cross-checked the provider, replay, control-plane integration, and benchmark measurement semantics.

Reproduction entry points:

- [Real benchmark commands and measurement methods](../tools/empirical-bench/README.md)
- [Planner replay commands](user-guide/o11y-replay.md)
- Control plane: `tools/run-offline-planner-replay.py` and
  `control_plane/docs/offline-sketch-evidence.md` in its worktree.
- Output directory: `tools/empirical-bench/results/`, containing Uniform/Zipf
  evidence, contexts, planner replay, and control-plane replay. Control-plane
  reports record the actual commands and both repository revisions.

These results are reproducible offline experiments and a planning-coverage report,
not conclusions about online end-to-end performance.
