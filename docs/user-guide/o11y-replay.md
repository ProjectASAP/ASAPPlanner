# Offline o11y workload replay

For users evaluating planner coverage using offline sketch-bench evidence.
`o11y_replay` runs the existing 27-query o11y fixture through lowering,
workload-wide candidate search, accuracy checks, selection and summary lifecycle
planning. It launches no data plane or downstream control-plane server.

```sh
cargo run -p asap-devtools --bin o11y_replay -- --supplemental > replay.json
cargo run -p asap-devtools --bin o11y_replay -- \
  --supplemental --evidence planner-evidence.json --context context.json > empirical-replay.json
```

The first command compares exact requirements with the default approximate
planner (`--epsilon 0.01`). The second adds an empirical planning run using a
versioned offline evidence artifact and an explicit applicability context.
The context JSON contains `distribution`, `environment`, and
`now_unix_seconds`; copy the full descriptors of the intended offline dataset
and execution environment, and choose the evidence evaluation time explicitly.
Configuration, distribution, environment and validity dates must match.
An absent measurement stays unavailable and falls back to default planning.

`--queries FILE` accepts one PromQL query per nonblank, noncomment line.
`--evaluations 60` models repeated reads of the same fixed historical input;
`--now-ms 1788825600000` fixes the as-of time. These are scenario assumptions,
not timings extracted from o11y task execution. Query windows, subqueries and
offsets remain encoded in the query IR. The lifecycle horizon is one hour and
the assumed data is at rest. `--supplemental` adds two quantile queries and a
`count_over_time` query as a separate corpus. Its total sample count has different
readout semantics from integer-stream point-frequency sketch-bench data; matching
sketch configuration does not establish matching query readout/error semantics.

The JSON distinguishes:

- `coverage`: whether a reachable site has a sketch candidate, exact summary
  candidate, exact fallback, or a lowering rejection. Candidate coverage does
  not establish that the whole query can execute from summaries.
- `root_plan` and `lifecycle_plan`: selected root structure and deployable
  lifecycle result. Missing complete costs can make lifecycle planning retain
  exact recomputation despite available sketch candidates.
- `offline_evidence`: matched primitive measurements and provenance, or an
  explicit missing/incompatible/stale reason. Error statistics remain offline
  observations for the measurement's recorded query, separate from the
  planner's formal result guarantee.
- `heuristic_score`: a dimensionless planner score, never CPU nanoseconds.
  Search and selection wall-clock timings are one local sample. Reporting and
  materialization timings include JSON export work.
- End-to-end resource savings: `null` until complete raw execution, residual
  operators, grouping cardinalities, sharing and deployment costs exist. Do not
  divide heuristic scores to claim runtime speedup, or sum nested candidate
  costs as a whole-query cost.

The fixture identifies an upstream snapshot date, not an upstream commit; this
tool retains that limitation explicitly. It reuses the repository's existing
fixture, whose provenance is documented there. It does not claim to run the
upstream agent benchmark or its scenario data.

## Measure supported exact-query reference implementations

```sh
cargo run --release -p asap-devtools --bin o11y_exact_bench -- 300 60 > exact-snapshot.json
```

This profiles seven explicitly admitted o11y queries: instantaneous sums,
per-series window maxima, and a sum of per-series window averages. The synthetic
dataset has 300 series across three job labels, finite gauge values, and one
sample per minute inside the selected window. The 60 invocations repeat the
identical immutable snapshot and evaluation time. They are not advancing live
windows. Unsupported queries are reported as unavailable.

The reference deployment retains the complete exact result of an admitted root
summary. It measures the raw value kernel and the retained result read separately
with a process CPU clock, and estimates one build plus repeated reads against
repeated raw execution. Both timed kernels include result allocation/destruction;
charging the build this way is conservative because its result is actually
retained. Only the profiled exact SummaryAgg signatures receive costs through
the public CostModel boundary; actual search, global selection, and root
materialization determine the reported choice. Unprofiled candidates and raw
fallback nodes never inherit those costs.

The output identifies this as a Rust reference implementation, not the deployed
backend. It includes filtering and value reduction/readout but excludes disk,
network, protocol serialization and shared output-label materialization. Memory
is logical stored value bytes, not measured process RSS or a claim that raw data
can be deleted. Large savings from reusing identical results do not establish
the benefit of live sliding-window maintenance.

## Compare query-matched sketch configurations

First restore the saved inputs using the [artifact download instructions](../../tools/empirical-bench/ARTIFACTS.md),
or generate them with the benchmark driver. Result files are not checked in.

```sh
cargo run -p asap-devtools --bin offline_recommend -- \
  tools/empirical-bench/results-sweep/comparison-evidence.json \
  tools/empirical-bench/results-sweep/request-uniform-are001.json
```

The companion comparison artifact includes disjoint construction, ingestion,
prepare and read CPU evidence for the exact frequency index and sketch rungs.
The request declares its offline observed-error criterion, fixed snapshot,
probe population, formal parameter minima if applicable, and resource weights.
The JSON reports accepted and rejected configurations, the exact reference,
selection, and dimensional tradeoffs. CPU-only and memory-weighted requests can
choose different plans. These point-frequency observations do not price or
bound error for the o11y gauge queries above.
