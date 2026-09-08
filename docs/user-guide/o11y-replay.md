# Offline o11y evaluation through the backend

For developers evaluating the actual control-plane planning integration.
The canonical path is:

`PromQL corpus -> ASAPQuery-backend parser -> ASAPPlanner -> backend typed binder/fallback`

There is no standalone planner-only `o11y_replay` command. Run the backend's
`offline_planner_replay` example through its reproducibility wrapper instead.

## Run the control-plane replay

From an ASAPQuery-backend checkout, with its normal sibling dependencies available:

```sh
python3 tools/run-offline-planner-replay.py \
  --queries /path/to/ASAPPlanner/crates/frontend-promql/tests/observability/data/o11y_bench_promql.txt \
  --evidence /path/to/planner-evidence.json \
  --context /path/to/context.json \
  --output /tmp/control-plane-o11y.json
```

The backend uses its pinned planner dependency; a planner source checkout is
only needed here to locate the vendored query fixture. Omit evidence and context
together to run exact/default modes only. Supplying both adds empirical mode.
An optional `--planner /path/to/ASAPPlanner` enables a local source override for
development; it is not the default evaluation path.

The [backend guide](https://github.com/ProjectASAP/ASAPQuery-backend/blob/codex/empirical-o11y-322/control_plane/docs/offline-sketch-evidence.md)
documents the actual parser/binder and evidence assumptions.
The [benchmark driver](https://github.com/ProjectASAP/ASAPQuery-backend/blob/codex/empirical-o11y-322/tools/empirical-bench/README.md)
and [artifact instructions](https://github.com/ProjectASAP/ASAPQuery-backend/blob/codex/empirical-o11y-322/tools/empirical-bench/ARTIFACTS.md)
also live in the backend repository.

## Interpret coverage conservatively

- A parse failure is not a planner or backend execution result.
- A bound plan means the backend accepted a plan returned through its planner integration.
- `root_raw_fallback` means the complete original query remains selected; this is not summary acceleration.
- `raw_subtrees` includes source scans underneath summaries and cannot be counted as root fallback.
- `planning_elapsed_ns` measures the parser/binder path, not query execution.
- Binding does not establish deployable placement, successful data-plane execution, or resource savings.

The recorded final evaluation bound all 27 fixtures in exact/default/empirical
modes; each mode retained five raw root fallbacks. Earlier standalone planner
candidate counts are historical diagnostics, not backend support coverage.
No collectors, storage services, query servers or upstream agent scoring run.
Point-frequency error measurements do not establish error bounds for these
PromQL queries.

## Backend-only execution prototype

The target is real data-plane execution, not just binding coverage:
OpenMetrics data and o11y queries enter ASAPQuery-backend; the backend calls
ASAPPlanner and executes either a valid ASAP plan or a local exact baseline.
No ASAPCollector or Prometheus process is required by the intended standalone
prototype. The implementation and its current limitations belong to
[backend PR #524](https://github.com/ProjectASAP/ASAPQuery-backend/pull/524).

The supplied dataset is `/mydata/metrics.txt`, an OpenMetrics file with 305,026
samples across 106 series. Preserve it read-only. The backend adapter owns
timestamp conversion and Remote Write encoding; OpenMetrics text cannot be
posted directly to `/api/v1/write`. The currently pinned upstream task corpus
has 28 query occurrences (24 unique queries); do not equate that corpus with the
older 27-query fixture used for the historical binding report.

Acceptance requires matching exact/ASAP query results before reporting latency
or resource reductions, with identical data, labels and evaluation times.
Report construction/ingestion costs separately from steady-state queries.
Unsupported queries and exact fallbacks are not accelerated successes.
Do not substitute the earlier synthetic result-cache experiment for this test.

## Optional backend diagnostic

The small `offline_recommend` diagnostic also lives in ASAPQuery-backend.
It compares measured fixed-snapshot frequency scenarios, not PromQL execution:

```sh
cargo run -p control_plane --example offline_recommend -- \
  /path/to/comparison-evidence.json /path/to/request.json
```

All benchmark producers and execution tools live in the backend repository.
This Planner PR contains documentation only; the public evidence and resource
contracts are reviewed separately in #357.
