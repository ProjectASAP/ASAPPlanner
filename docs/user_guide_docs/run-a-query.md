# ASAPPlanner CLI user guide

Use the `asap-devtools` commands to plan a workload through the planning
stages, inspect the logical DAG a query lowers into, and inspect corpus
coverage. These commands do not deploy or execute a physical plan.

To develop an application using the Rust library, start with
[Library API: definitions, options, and examples](../develop_docs/library-api.md).

## Choose a command

Run from the repository root with Rust/Cargo installed. Cargo builds the selected
tool on first use.

| Command (`cargo run -p asap-devtools --bin … -- …`) | Input / options | Result |
| --- | --- | --- |
| `stage_pipeline --promql "<query>" --out plan.json` | One or more `--promql` / `--sql` queries, or `--example <name>` | Plans the workload through Stages 0–3 and writes the stage document the [Stage Viewer](stage-viewer.md) shows |
| `convert_rqe_atomic_costs <rqe_atomic_costs.json> <out.json>` | sketch-bench's per-operation cost table | Writes the measured table `stage_pipeline --measured-operation-costs` reads; skipped rows are listed on stderr |
| `show_logical_dag --sample-interval-ms 1000 queries.txt` | File path, or stdin when omitted | Prints each query's logical DAG (Pre-ASAP IR) |
| `analyze_corpora --corpora --sample-interval-ms 1000 --out-dir <dir>` | Repository PromQL corpora, output directory | Writes successful/error IR dumps and summary reports |
| `analyze_corpora --sql-corpora --out-dir <dir>` | Repository SQL corpora, output directory | Writes SQL corpus reports |
| `variant_coverage --sample-interval-ms 1000` | Repository corpora | Reports Pre-ASAP IR variant coverage |
| `sketch_coverage --sample-interval-ms 1000 --max-error 0.01` | Repository corpora; `--max-error` defaults to `0.01` | Reports which summary candidates Pass 1 offers for each successfully lowered query |

## Plan a workload

For build dependencies, workload-file commands, search budgets, and troubleshooting,
see [Plan a workload with stage_pipeline](stage-pipeline.md).

`stage_pipeline` runs the query-language frontends (Stage 0), logical
ASAP-aware optimization (Stage 1), physical ASAP-aware optimization
(Stage 2) and plan selection (Stage 3), and writes one JSON document:

```sh
cargo run -p asap-devtools --bin stage_pipeline -- \
    --promql "topk by (job) (10, sum_over_time(http_requests_total[1m]))" \
    --series-count 1000 --max-error 0.01 --failure-probability 0.001 \
    --enforce-accuracy-guarantees --out plan.json
```

Plan selection prices only from declared facts. `--series-count` (with
the sample interval) or `--rows-per-second` gives each source's arrival
rate; `--rows-held` bounds data at rest and a scan with no window (a SQL
query without a time predicate reads every row held). Without the fact a
scan needs, planning fails with an error that names the source and the
missing fact.

Without `--enforce-accuracy-guarantees`, `stage_pipeline` ignores accuracy
guarantees: every output is accepted whatever its guarantee, and plans are
compared on cost alone. The document's top-level `accuracy_guarantees` says
`ignored` or `enforced`. The document's `stage3_plan_selection.selected_candidates`
names the selected physical candidates, and `stage3_plan_selection.cost`
holds the selected plan's `cost_per_second`. For this query the selected plan
is the exact plan. The Count-Min top-k plan is not selected because the
command declares no metric type, so the samples are not proven non-negative,
and the CountSketch top-k plan because its output has no guarantee. Under
`--exhaustive`, `reference_pricing.rejected` gives each plan's `cause`
(`count_min_weights_not_proven_non_negative`, `no_accuracy_guarantee`) and
reason.
Open the document in the [Stage Viewer](stage-viewer.md) to see every
stage, or run a built-in example of the design document, such as
`--example planner-layering-1 --exhaustive --enforce-accuracy-guarantees`. The [stage_pipeline guide](stage-pipeline.md) describes more options.

## Inspect a query's logical DAG

### Create a query file

Create a text file containing one query per line. Prefix each query with `sql>` or `promql>`.

For example, `queries.txt`:

```text
promql> quantile(0.99, rate(http_requests_total[5m]))
sql> SELECT service, COUNT(*) FROM metrics GROUP BY service
```

Blank lines and lines beginning with `#` are ignored. `show_logical_dag`
accepts `sql>` and `promql>`; MetricsQL is available through the library frontend.
SQL examples use the fixed catalog
`metrics(ts: Timestamp, service: Utf8, region: Utf8, latency: Float64, bytes: Int64)`.
For your own schema, provide a `SqlCatalog` through the library API, or
declare tables with `stage_pipeline --table`.

### Show the logical DAG

Run:

```sh
cargo run -p asap-devtools --bin show_logical_dag -- --sample-interval-ms 1000 queries.txt
```

You can also provide the queries through stdin:

```sh
cargo run -p asap-devtools --bin show_logical_dag -- --sample-interval-ms 1000 < queries.txt
```

Each input line is followed by its debug IR or an `ERR:` message. The tool
never picks a sketch: it shows what a query means, with exact operators only.

To dump and compare every PromQL corpus, run:

```sh
cargo run -p asap-devtools --bin analyze_corpora -- --corpora --sample-interval-ms 1000 --out-dir artifacts/promql_pre_asap
```

This writes one successful-lowering JSONL dump and one error JSONL dump per
corpus, plus per-query JSON files under `<corpus>/` and `<corpus>.errors/`,
`summary.json`, and the heuristic `anomalies.md` report. The anomaly report
compares exact duplicates, whitespace/case-normalized queries, coarse
structural shapes, and distinct expressions that produce identical IR.

The corresponding SQL corpus analysis is:

```sh
cargo run -p asap-devtools --bin analyze_corpora -- --sql-corpora --out-dir artifacts/sql_pre_asap
```

## More inspection commands

### Check IR variant coverage

Parse the query corpora in the repository and report which pre-ASAP IR variants are exercised:

```sh
cargo run -p asap-devtools --bin variant_coverage -- --sample-interval-ms 1000
```

### Check summary coverage

Parse the same query corpora, lower them with an approximate `AccuracyTarget`, run Pass 1 of logical ASAP-aware optimization over each query, and report which summary candidates Pass 1 offers, plus the fraction of each corpus's successfully lowered queries with at least one:

```sh
cargo run -p asap-devtools --bin sketch_coverage -- --sample-interval-ms 1000 --max-error 0.01
```

`--max-error` is optional (defaults to `0.01`). Without an approximate target, Pass 1 offers no summary candidate. See the binary's own doc comment for exactly how coverage is counted.

## Additional examples

Print pre-ASAP IR for several top-k queries:

```sh
cargo run -p asap-devtools --example topk_ir
```

Print representative queries covering the pre-ASAP IR variants:

```sh
cargo run -p asap-devtools --example canonical_examples
```


## Library development and design

- [Library API definitions and examples](../develop_docs/library-api.md)
- [Design overview](../design_docs/README.md)
- [Pre-ASAP IR reference](../develop_docs/pre-asap-ir.md)
- [Post-ASAP IR reference](../design_docs/concepts/post-asap-ir.md)

PromQL commands require `--sample-interval-ms`, the nonzero sample interval in
milliseconds declared for every metric a query reads (`stage_pipeline`
defaults it to 15000). The examples use one second; supply the interval for
your data. `show_logical_dag` requires this option even for SQL-only input
files.
