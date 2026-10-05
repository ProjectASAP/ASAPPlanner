# Metrics-observability corpus coverage

This note describes the PromQL corpora derived from the metrics-observability
benchmark dump and the commands used to reproduce their measurements. The
audience is developers evaluating changes to the PromQL front end or ASAP
replacement logic. Measured counts intentionally belong in the pull request
description, not in this document or the tests, because the benchmark data and
planner behavior can change.

## Sources and preprocessing

The benchmark source is the `queries/` directory under the local benchmark
checkout. The extractor takes that benchmark checkout as its first argument;
the path is intentionally environment-specific and is not recorded here.

```text
<benchmark-root>/metrics_observability/queries
```

The extractor converts query logs, Grafana dashboard JSON, and Prometheus YAML
rules into deduplicated, one-query-per-line fixtures:

```sh
python3 tools/metrics_observability/extract_promql.py \
  <benchmark-root>/metrics_observability \
  crates/frontend-promql/tests/observability/data/metrics_observability
```

The generated corpora are CMU logs, CMU dashboards, CMU rules, Claude logs,
Grafana dashboards, Kubernetes-mixin dashboards, Kubernetes-mixin alerts,
Kubernetes-mixin rules, and Promset. Grafana variable entries are retained in
the fixtures so extraction changes remain visible; some are not standalone
PromQL (`label_values(...)`, dashboard values, or template-bearing
expressions).

The repository already contains separate corpora for Prometheus examples,
o11y-bench, and awesome-prometheus-alerts. They are not duplicated here.

## Tool roles

- `extract_promql.py` is a benchmark-ingestion tool. It handles the source
  formats and writes deterministic fixtures; it does not parse or judge
  PromQL.
- `metrics_observability.rs` is the executable corpus test. It runs the
  PromQL parser/lowerer and Stage 1 (Pass 1 and composition) per query, and
  prints the current measurements.
- `summarize_misses.py` is an optional report formatter. It consumes the
  test's opt-in `POST_ASAP_MISS` lines and groups misses by coarse root shape.

The test prints totals, parse errors, lowering errors, pre-ASAP successes,
post-ASAP candidates, unchanged queries, and post-ASAP errors. `Pre-ASAP` means
that parsing and lowering produced an `OperatorNode` DAG. `Post-ASAP candidate`
means one of the query's first 64 Stage 1 choices composes to a candidate
that contains an ASAP operator (`contains_asap()`). `Unchanged` is a successful
pre-ASAP query for which only the pass-through choice composes to such a
candidate, and a post-ASAP error is a query Pass 1 rejects or whose
pass-through choice does not compose (`support::stage1_plan`).

## Scope

The measurement runs Pass 1 on each query alone. It does not measure Pass 2's
sharing across queries, or Stage 2 and Stage 3.

## Alerts

The CMU rules corpus includes alert and recording-rule YAML under `cmu_chad`,
including `alerting_rules.yaml`, `recording_rules.yaml`, and exported
Prometheus rules. The existing awesome-prometheus-alerts corpus separately
covers its alert expressions. Kubernetes-mixin alert YAML is not included in
the new six-corpus baseline; only its generated dashboards are.

## Common sketch-only misses

The miss-report helper groups successful pre-ASAP queries without a
post-ASAP candidate into coarse root-pattern categories. These are diagnostic,
not semantic equivalence classes. Common categories include selector-root
binary expressions, parenthesized/binary expressions, bare selectors, and
root functions such as `sum`, `histogram_quantile`, `increase`, `absent`,
`topk`, and `scalar`.

Many misses are expected because Pass 1 only realizes a single-measure
aggregate with a summary; a query without one (a bare selector, a per-series
function) keeps its exact plan.

## Reproducing and refreshing the baseline

From the metrics-observability worktree:

```sh
# Regenerate fixtures when benchmark source files change.
python3 tools/metrics_observability/extract_promql.py \
  <benchmark-root>/metrics_observability \
  crates/frontend-promql/tests/observability/data/metrics_observability

# Re-run totals, errors, pre-ASAP, and Stage 1 post-ASAP candidates.
cargo test -p asap-frontend-promql --test metrics_observability -- --nocapture

# Recreate the miss-pattern report.
METRICS_OBSERVABILITY_REPORT=1 \
  cargo test -p asap-frontend-promql --test metrics_observability -- --nocapture \
  > /tmp/metrics-observability-report.txt 2>&1
python3 tools/metrics_observability/summarize_misses.py \
  /tmp/metrics-observability-report.txt
```

After changes to parser or lowering logic, capture the printed counts and miss
categories in the pull request description. The test intentionally avoids
coverage floors and corpus-size assertions; its stable guarantees are that
fixtures are non-empty, every query is classified exactly once, and processing
does not panic.
