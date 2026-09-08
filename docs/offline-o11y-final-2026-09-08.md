# Offline Sketch Evidence and o11y: Final Report

Audience: developers. This report covers the offline evidence path for #322,
without real-time error feedback. The original dirty working directory was left unchanged.

## Completed Integration

- Versioned artifacts, JSON Schema, and a public CostModel provider match parameters, query semantics, distributions, environments, and validity intervals. Unknown measurements remain unavailable.
- Actual CMS/CountSketch parameter sweeps measure offline error, separate construction/update/read/merge CPU, live heap allocations, serialized size, and allocated file blocks.
- Fixed-snapshot point-frequency comparisons use explicit observed-error thresholds, formal parameter minima, CPU/retained byte-seconds weights, and an exact baseline on the same data.
- The backend filters unsupported layouts before recommending and binding frequency configurations. When exact execution wins or evidence is insufficient, it preserves the original query without silently rounding selected parameters.
- o11y planner/control-plane replay and exact fixed-snapshot reference measurements cover seven explicitly supported queries.

For interfaces and reproduction instructions, see the [evidence contract](developer_docs/offline-sketch-evidence.md),
[benchmark commands](../tools/empirical-bench/README.md), and [replay guide](user-guide/o11y-replay.md).

## Measured Results

sketch-bench is pinned to revision `87f619e843fd2e4da784160d4e205a0d0d55f032`.
The uniform and Zipf datasets each contain 20,000 i64 keys, with a key space of
1,000, seed 42, and Zipf exponent 1.1. There are 18 sketch configurations and
two exact baselines; CPU measurements use five trials after two warmups.
Accuracy comes from one offline experiment per fixed dataset, not a guarantee
across distributions or real-time ground truth. The comparison assumes one
build, 1,000 queries, 300 seconds of retention, and no merges.

| Scenario | Uniform / Zipf results |
| --- | --- |
| CPU-only, with a 1% or 5% observed mean relative-error limit | Both select exact execution; no sketch CPU benefit |
| Generic comparator, with memory weighting | CMS 2720×5; retained heap decreases by 81.68%, while CPU increases by approximately 0.037 / 0.182 ms, respectively |
| Backend-compatible configuration, with memory weighting | CMS 4096×5; retained heap decreases by approximately 72.4%, while CPU also increases |

The memory-weighted objective is `CPU ns + 0.01 × retained byte-seconds`. These
weights express an illustrative preference, not a conversion measured from
hardware. Exact retained heap is 296,976 B; CMS 2720×5 uses 54,400 B, and
4096×5 uses 81,920 B. Memory figures measure requested allocation bytes using
a separate System allocator probe; CPU measurements use jemalloc. These memory
figures are not process RSS. Serialized size and allocated file blocks do not
establish disk-throughput benefits.

See `results-sweep/MEASUREMENTS.md` in the [verified experiment archive](../tools/empirical-bench/ARTIFACTS.md).
Earlier frequency measurements and control-plane replay in `results/` remain
as the initial baseline. Final frequency comparisons and control-plane results
use `results-sweep/`; costs from the two runs must not be mixed.

## o11y Coverage and Limitations

The replay reuses the repository's existing 27 PromQL fixtures. It does not run
upstream LLM-agent scoring or use real scenario data.

| Check | Result |
| --- | --- |
| Planner candidate coverage | 26/27 have exact summary candidates; 1/27 uses raw fallback; no sketch candidates |
| Complete lifecycle costs in the original replay | Insufficient evidence; all 27/27 conservatively fall back to raw execution |
| Control-plane exact/default/empirical binding | All three modes bind 27/27 successfully; each retains the original query in 5 root plans |
| Seven exact reference queries | Raw execution and cached-result reads are measured on fixed synthetic gauge snapshots, using the public CostModel and actual selection flow |
| Remaining 20 queries | No complete reference implementation; benefits remain unavailable |

New fallbacks preserve the original IR for selector, sort, and comparison/filter
roots, increasing successful binding from 24/27 to 27/27. Binding does not mean
every plan is executable by the summary executor. Column updates in the new IR
are supported; keyed/non-column updates and warm-tier BinaryOp remain explicitly rejected.

The seven reference queries cover instant sums, window maxima, and sums of
window averages: 300 series, three jobs, one finite gauge sample per minute,
and 60 repeated reads of the same snapshot. The retained state is the complete
exact query result, not a sliding window with advancing time. Measurements
exclude network, disk, protocol serialization, and output-label materialization;
memory figures count logical value bytes. Benefits from repeated reads cannot
be extrapolated to production end-to-end speedups. Machine-readable results are
available as `results/o11y-exact-snapshot.json` in that archive.

Quantile measurements, real o11y traces, time drift, post-merge error, online
update and retirement costs, and complete production benefits remain outside
this coverage. Point-frequency error cannot be applied to quantiles or count_over_time.

## Validation and Delivery

- Planner mapping: 349 unit tests pass.
- Devtools: 3 exact benchmark tests and 4 replay tests pass; the recommendation CLI is verified with six actual requests.
- Benchmark export: 5 Python tests pass.
- Backend: 633 control-plane and 973 data-plane library tests pass; 7 new integration tests pass.
- Agents cross-reviewed the provider, measurement, and binding code; artifact text, JSON, source hashes, and provenance were checked successfully.

The backend pins the published planner core commit
`dfdf6b5c1f7d667394a4ea1f56fb3786af04228d`. Normal builds and final replay do not
require local Cargo source patches. Core planner code, benchmark tools, and backend
integration are reviewed in separate PRs. Full generated results are distributed
as an experiment artifact, not embedded in the source diff. No PR is automatically
merged and the issue is not automatically closed.
