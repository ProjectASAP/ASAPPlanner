# ASAPPlanner CLI user guide

Use the `asap-devtools` commands to inspect query IR, plan workloads, and inspect
corpus coverage. These commands do not deploy or execute a physical plan.

To develop an application using the Rust library, start with
[Library API: definitions, options, and examples](../develop_docs/library-api.md).
That guide explains how to plan a workload with the stage pipeline, inspect
Stage 1's alternatives, and supply deployment models.

## Choose a command

Run from the repository root with Rust/Cargo installed. Cargo builds the selected
tool on first use.

| Command (`cargo run -p asap-devtools --bin … -- …`) | Input / options | Result |
| --- | --- | --- |
| `show_logical_dag --data-ingestion-interval-ms 1000 queries.txt` | File path, or stdin when omitted | Prints canonical Pre-ASAP IR |
| `analyze_corpora --corpora --data-ingestion-interval-ms 1000 --out-dir <dir>` | Repository PromQL corpora, output directory | Writes successful/error IR dumps and summary reports |
| `analyze_corpora --sql-corpora --out-dir <dir>` | Repository SQL corpora, output directory | Writes SQL corpus reports |
| `variant_coverage --data-ingestion-interval-ms 1000` | Repository corpora | Reports Pre-ASAP IR variant coverage |
| `sketch_coverage --data-ingestion-interval-ms 1000 --epsilon 0.01` | Repository corpora; epsilon defaults to `0.01` | Lists, per query, the sketch alternatives Stage 1 offers, and each corpus's coverage |

## Inspect a query from the command line

Run these commands from the repository root. They are inspection tools; their
demonstration defaults are not the configuration of your downstream deployment.

### Create a query file

Create a text file containing one query per line. Prefix each query with `sql>` or `promql>`.

For example, `queries.txt`:

```text
promql> quantile(0.99, rate(http_requests_total[5m]))
sql> SELECT service, COUNT(*) FROM metrics GROUP BY service
```

Blank lines and lines beginning with `#` are ignored. The file/stdin tool
accepts `sql>` and `promql>`; MetricsQL is available through the library frontend.
SQL examples use the fixed catalog
`metrics(ts: Timestamp, service: Utf8, region: Utf8, latency: Float64, bytes: Int64)`.
For your own schema, provide a `SqlCatalog` through the library API.

### Show the Pre-ASAP IR

Run:

```sh
cargo run -p asap-devtools --bin show_logical_dag -- --data-ingestion-interval-ms 1000 queries.txt
```

You can also provide the queries through stdin:

```sh
cargo run -p asap-devtools --bin show_logical_dag -- --data-ingestion-interval-ms 1000 < queries.txt
```

To dump and compare every PromQL corpus, run:

```sh
cargo run -p asap-devtools --bin analyze_corpora -- --corpora --data-ingestion-interval-ms 1000 --out-dir artifacts/promql_pre_asap
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

### See how the planner plans a workload

```sh
cargo run -p asap-devtools --bin stage_pipeline -- --promql "<PromQL query>" --epsilon 0.01 --out plan.json
```

writes the planner's four stages for the query. Open `plan.json` in the Stage Viewer, which also plans PromQL queries from its editor; see [`tools/dag-viewer/RUNNING.md`](../../tools/dag-viewer/RUNNING.md).

### Check IR variant coverage

Parse the query corpora in the repository and report which pre-ASAP IR variants are exercised:

```sh
cargo run -p asap-devtools --bin variant_coverage -- --data-ingestion-interval-ms 1000
```

### Check sketch-replacement coverage

Parse the same query corpora, lower them with an approximate `AccuracyTarget`, and list, per query, the sketch alternatives (e.g. KLL and DDSketch) Stage 1's Pass 1 offers, with the fraction of each corpus's successfully-lowered queries that got at least one:

```sh
cargo run -p asap-devtools --bin sketch_coverage -- --data-ingestion-interval-ms 1000 --epsilon 0.01
```

`--epsilon` is optional (defaults to `0.01`).

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

PromQL commands require `--data-ingestion-interval-ms` with the nonzero source
sample cadence in milliseconds. The examples use a one-second cadence; supply
the interval for your data. The mixed-input `show_logical_dag` tool requires
this option even for SQL-only input files.
