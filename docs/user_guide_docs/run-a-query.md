# ASAPPlanner CLI user guide

Use the `asap-devtools` commands to inspect query IR and summary
candidates, or export graphs. These commands do not deploy or execute a physical plan.

To develop an application using the Rust library, start with
[Library API: definitions, options, and examples](../develop_docs/library-api.md).
That guide explains how to choose strategies and models, rank candidates, and
work with lifecycle capabilities.

## Choose a command

Run from the repository root with Rust/Cargo installed. Cargo builds the selected
tool on first use.

| Command (`cargo run -p asap-devtools --bin … -- …`) | Input / options | Result |
| --- | --- | --- |
| `show_pre_asap_ir queries.txt` | File path, or stdin when omitted | Prints canonical Pre-ASAP IR |
| `show_post_asap_ir queries.txt` | Same query file format | Prints all sketch-strategy Post-ASAP candidates using a fixed approximate target, in cost-model order |
| `dag_export --promql "<query>"` | One PromQL expression | Exports a query graph for inspection |
| `dag_export --sql "<query>"` | One SQL expression using the tool's catalog | Exports a query graph for inspection |

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

Blank lines and lines beginning with `#` are ignored. These two file/stdin
commands support `sql>` and `promql>` prefixes; MetricsQL is available through
the [library frontend](../develop_docs/library-api.md#lower-a-query-into-pre-asap-ir).

The SQL inspection tools use the fixed table
`metrics(ts: Timestamp, service: Utf8, region: Utf8, latency: Float64, bytes: Int64)`.
Use that schema for these examples. To plan against your own tables, construct a
`SqlCatalog` through the library API.

### Show the Pre-ASAP IR

Run:

```sh
cargo run -p asap-devtools --bin show_pre_asap_ir -- queries.txt
```

You can also provide the queries through stdin:

```sh
cargo run -p asap-devtools --bin show_pre_asap_ir < queries.txt
```

### Inspect Post-ASAP IR candidates

Run:

```sh
cargo run -p asap-devtools --bin show_post_asap_ir -- queries.txt
```

Or through stdin:

```sh
cargo run -p asap-devtools --bin show_post_asap_ir < queries.txt
```

`show_post_asap_ir` uses an approximation target of ε = 0.01 and prints every
available binding from the sketch strategy for each query, numbered in cost-model
order. If no candidate is available, it prints the pre-ASAP fallback as candidate
1. It does not show the complete ranked workload candidate set or choose a
deployment lifecycle. Its SQL examples use a fixed demonstration catalog, not
your database schema. Use the
[library workflow](../develop_docs/library-api.md#generate-and-rank-candidates) to retain workload alternatives
and provide your own models.

### Interpret the output

Each input line is followed by its debug IR or an `ERR:` message. The SQL
`COUNT(*)` example should lower to an aggregate grouped by `service`.
Post-ASAP output may contain `SummaryAgg` state, `SummaryEstimate` readouts, or
`KeepPreAsap` for exact work. An approximate target permits approximation; it does
not guarantee that a sketch is legal or selected. The command inspects a plan
and does not return query data.

For all alternatives, costs and structured rejection details, use the
[library search workflow](../develop_docs/library-api.md#generate-and-rank-candidates).

## More inspection commands

### Export a query DAG

Export pre-ASAP IR for SQL or PromQL queries for use with the interactive DAG viewer:

```sh
cargo run -p asap-devtools --bin dag_export -- --sql "<SQL query>"
```

or:

```sh
cargo run -p asap-devtools --bin dag_export -- --promql "<PromQL query>"
```

See [`tools/dag-viewer/RUNNING.md`](../../tools/dag-viewer/RUNNING.md) for instructions on running the DAG viewer.

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
- [Post-ASAP IR reference](../develop_docs/post-asap-ir.md)

For repository-wide regression analysis, see [corpus verification](../develop_docs/corpus-verification.md).
