# Offline sketch-bench evidence (developer guide)

This driver runs ProjectASAP/sketch-bench's real `approxbench` CLI and adapts its
schema-v5 reports to the planner's versioned evidence schema. It measures CMS and
CountSketch using `asap_sketchlib` RegularPath/Vector2D plus an exact Polars
frequency baseline, on deterministic uniform and Zipf streams. No collector,
query backend, runtime ground truth, or runtime error feedback is involved.

```bash
git clone https://github.com/ProjectASAP/sketch-bench.git /path/to/sketch-bench
git -C /path/to/sketch-bench checkout 87f619e843fd2e4da784160d4e205a0d0d55f032
(cd /path/to/sketch-bench && cargo build --release --locked -p aqpbm-cli)
python3 tools/empirical-bench/run.py --bench-repo /path/to/sketch-bench --output /tmp/offline-evidence --sweep
python3 -m unittest discover -s tools/empirical-bench -p 'test_*.py'
```

Build from the sketch-bench directory so its `.cargo/config.toml` applies
`target-cpu=native`, as in the checked-in run. The driver records the source
revision, dirty status, executable checksum, full invocations, compiler, CPU,
OS, implementation, and parameters. It pins Polars to one worker. Five measured
runs follow two warmups per timed operation by default. Upstream accuracy executes
one offline truth comparison for the fixed generated dataset, recorded as
`error.trials=1`; it is run separately because accuracy of insert is undefined.
These timings are repeated observations
within one process, not independent experiments: standard deviations are reported,
but no confidence interval or cross-distribution guarantee is inferred.
`manifest.json.runtime_provenance` distinguishes observed host metadata and the
driver's enforced thread setting from declared build preconditions. The runtime
string's release/native/jemalloc settings describe the documented required build;
the driver does not independently recover compiler or allocator flags from the
executable. Its SHA-256 identifies that binary without proving those build flags.
The checked-in run used a shared development host without CPU affinity or an
exclusive core reservation; timing estimates should be remeasured on deployment
hardware before using them for capacity decisions.

Outputs:

- `raw.json`: unmodified per-invocation sketch-bench flat reports, including wall
  times, CPU user/system samples, process RSS/allocator readings, and all errors.
- `manifest.json`: execution and environment provenance.
- `operation-reports.json`: original reports before upstream flattening; preserve
  per-operation memory, including exact prepared-index state.
- `memory-probe.json`: requested-allocation samples with each live sketch, using
  the same library and upstream data generator on identical inputs.
- `planner-evidence.json`: normalized sketch measurements, with unknowns null.
- `exact-baselines.json`: exact frequency index costs on the same input.
- `resource-probe.json`: disjoint live-state construction/update/read/merge CPU,
  actual serialized snapshot size and allocated filesystem blocks.
- `comparison-evidence.json`: query-bound sketch records and exact baselines;
  its required `disjoint_live_state_v1` timing contract excludes duplicate setup
  and destruction charges.
- `request-*.json`: fixed-snapshot CPU-only comparisons at 1%/5% observed mean
  relative error, plus an explicitly illustrative memory-weighted comparison.

The default sketch parameters match formal planner sizing for epsilon=0.01, delta=0.01:
CMS width 272, depth 5; CountSketch width 30,000, depth 83. Input defaults to
20,000 i64 keys, key-space size 1,000, seed 42; Zipf exponent is 1.1. Actual
distinct-key counts come from the ground-truth probe population. Errors are
average absolute relative point-frequency errors over **all distinct keys**.
They are not error of a total COUNT, or a formal epsilon bound. Aggregate error
over repeated deterministic inputs does not establish a confidence interval.
`--sweep` additionally measures CMS widths 512/2720/4096/27200/32768 (depth 5)
and CountSketch widths 300/3000 (depth 83). The power-of-two CMS widths support
the backend frequency extension's actual parameter constraints. An offline
accuracy threshold does not authorize deployment below formal minimum sizes.

Original upstream CPU reports use paired user+system samples and preserve wrapper
allocation/destruction overhead. The current comparison exporter instead uses
`resource_probe.rs`, linked to the exact release libraries and jemalloc. It times
live-state phases separately with `CLOCK_PROCESS_CPUTIME_ID`: empty constructor,
updates, exact prepare, reads, and sketch merge. Holder allocation and destruction
are outside the constructor timer; constructors/destructors are outside the other
phase timers. Queries cover every distinct key ten times against an unchanged
snapshot, in a reproducible rotated sorted order. The exact implementation is
the upstream public `PolarsFrequencyCore<i64>`, not a replacement algorithm.
The model's scope ends with state retained after reading; retirement is excluded
on both sides. Wall time remains in upstream raw reports and is never called CPU.

`retained_bytes` and `peak_bytes` use the companion `memory_probe.rs`, linked to
the exact release libraries built by sketch-bench. Its counting System allocator
measures requested heap allocation bytes across construction and insertion,
keeping the sketch alive at the final snapshot; input generation is outside
the measurement. This is independent of allocator-resident pages and does not
measure jemalloc overhead. Five runs must release every sketch allocation after
destruction. CPU results come from a separate uninstrumented jemalloc probe.
The exact heap probe warms Polars twice, then measures construction/ingestion/
prepare with the exact state alive. Its readings are accepted only when every
post-destruction allocation balance returns to zero; otherwise the exact footprint
stays explicitly formula based. `--export-only --refresh-memory` refreshes this
heap evidence without rerunning or replacing CPU timings.
Upstream counter-storage formula, process RSS and allocator readings remain in
raw reports only. The resource probe calls the actual `serialize_to_bytes` API
and verifies estimate-equivalent deserialization for every input key. It writes
one snapshot to a fresh local temporary file, flushes it, and records allocated
blocks times 512 separately from logical byte length; the file is then removed.
Directory/inode overhead and a runtime write schedule are outside this disk
measurement. State bytes are never substituted for disk usage. Validity is a
30-day reproducibility policy, not a statistical claim of future applicability.

The exact baseline inserts the input into a buffer, then `prepare` executes
Polars group-by/count and creates a HashMap. Reads query that prepared exact
index. Compare `empty_build + (insert_per_item * N) + prepare + Q * read` with
sketch `empty_build + (update_per_item * N) + Q * read`. All components use
disjoint timed regions. A sketch can save state while losing CPU to this exact index.
These comparisons are offline point-frequency microbenchmarks; they do not
measure an o11y PromQL query or end-to-end deployed speedup.

`results/` preserves the earlier partial run whose constructor/disk/serialization
were unknown. `results-sweep/` contains the complete frequency component run;
its additional probe must not retroactively change the earlier measurements.
