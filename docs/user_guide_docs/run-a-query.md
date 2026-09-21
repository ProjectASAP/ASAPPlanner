# ASAPPlanner CLI user guide

Use the `asap-devtools` commands to inspect query IR, export graphs, and inspect
corpus coverage. These commands do not deploy or execute a physical plan.

To develop an application using the Rust library, start with
[Library API: definitions, options, and examples](../develop_docs/library-api.md).
That guide explains how to choose strategies and models, rank candidates, and
work with lifecycle capabilities.

## Choose a command

Run from the repository root with Rust/Cargo installed. Cargo builds the selected
tool on first use.

| Command (`cargo run -p asap-devtools --bin … -- …`) | Input / options | Result |
| --- | --- | --- |
| `show_pre_asap_ir --data-ingestion-interval-ms 1000 queries.txt` | File path, or stdin when omitted | Prints canonical Pre-ASAP IR |
| `show_post_asap_ir --data-ingestion-interval-ms 1000 queries.txt` | Same query file format | Prints all sketch-strategy Post-ASAP candidates using a fixed approximate target, in cost-model order |
| `dag_export --data-ingestion-interval-ms 1000 --promql "<query>"` | One PromQL expression | Exports a query graph for inspection |
| `dag_export --sql "<query>"` | One SQL expression using the tool's catalog | Exports a query graph for inspection |
| `analyze_corpora --corpora --data-ingestion-interval-ms 1000 --out-dir <dir>` | Repository PromQL corpora, output directory | Writes successful/error IR dumps and summary reports |
| `analyze_corpora --sql-corpora --out-dir <dir>` | Repository SQL corpora, output directory | Writes SQL corpus reports |
| `variant_coverage --data-ingestion-interval-ms 1000` | Repository corpora | Reports Pre-ASAP IR variant coverage |
| `sketch_coverage --data-ingestion-interval-ms 1000 --epsilon 0.01` | Repository corpora; epsilon defaults to `0.01` | Reports sketch/reuse opportunities among successfully lowered queries |

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

Blank lines and lines beginning with `#` are ignored. The two file/stdin tools
accept `sql>` and `promql>`; MetricsQL is available through the library frontend.
SQL examples use the fixed catalog
`metrics(ts: Timestamp, service: Utf8, region: Utf8, latency: Float64, bytes: Int64)`.
For your own schema, provide a `SqlCatalog` through the library API.

### Show the Pre-ASAP IR

Run:

```sh
cargo run -p asap-devtools --bin show_pre_asap_ir -- --data-ingestion-interval-ms 1000 queries.txt
```

You can also provide the queries through stdin:

```sh
cargo run -p asap-devtools --bin show_pre_asap_ir -- --data-ingestion-interval-ms 1000 < queries.txt
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

### Inspect Post-ASAP IR candidates

Run:

```sh
cargo run -p asap-devtools --bin show_post_asap_ir -- --data-ingestion-interval-ms 1000 queries.txt
```

Or through stdin:

```sh
cargo run -p asap-devtools --bin show_post_asap_ir -- --data-ingestion-interval-ms 1000 < queries.txt
```

`show_post_asap_ir` uses an approximation target of ε = 0.01 and prints every
available binding from the sketch strategy for each query, numbered in cost-model
order. If no candidate is available, it prints the pre-ASAP fallback as candidate
1. It does not show the complete ranked workload candidate set or choose a
deployment lifecycle. Its SQL examples use a fixed demonstration catalog, not
your database schema. Use the
[library workflow](../develop_docs/library-api.md) to retain workload alternatives
and provide your own models.

The default strategy generates DDSketch quantile-ratio candidates even when no
input-domain evidence is available. Such candidates have `guarantee: None`:
they do not claim a certified end-to-end accuracy bound. They also remain
visible in a target-aware `PlanSpace` so the downstream backend can decide
whether to select them using its own evidence. Planner's automatic
`global_selection` skips them; their presence alone does not show that they
meet the requested target.

Each input line is followed by its debug IR or an `ERR:` message. Post-ASAP
output may contain summary state, readouts or exact `KeepPreAsap` work. An
approximate target permits approximation; it does not guarantee a legal or
certified sketch. The tool prints plans, not query results.

## More inspection commands

### Export a query DAG

Export pre-ASAP IR for SQL or PromQL queries for use with the interactive DAG viewer:

```sh
cargo run -p asap-devtools --bin dag_export -- --sql "<SQL query>"
```

or:

```sh
cargo run -p asap-devtools --bin dag_export -- --data-ingestion-interval-ms 1000 --promql "<PromQL query>"
```

See [`tools/dag-viewer/RUNNING.md`](../../tools/dag-viewer/RUNNING.md) for instructions on running the DAG viewer.

### Check IR variant coverage

Parse the query corpora in the repository and report which pre-ASAP IR variants are exercised:

```sh
cargo run -p asap-devtools --bin variant_coverage -- --data-ingestion-interval-ms 1000
```

### Check sketch-replacement coverage

Parse the same query corpora, lower them with an approximate `AccuracyTarget`, and report what fraction of each corpus's successfully-lowered queries got a genuine sketch alternative (`SketchApproximation`, e.g. KLL vs. DDSketch) and/or a cross-query common-subexpression-reuse candidate (`CommonSubexpressionReuse`):

```sh
cargo run -p asap-devtools --bin sketch_coverage -- --data-ingestion-interval-ms 1000 --epsilon 0.01
```

`--epsilon` is optional (defaults to `0.01`). See the binary's own doc comment for exactly how "coverage" is defined and attributed back to each query.

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
the interval for your data. The mixed-input `show_pre_asap_ir` and
`show_post_asap_ir` tools require this option even for SQL-only input files.
