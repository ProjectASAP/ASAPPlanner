# ASAPPlanner CLI user guide

Use the `asap-devtools` commands to inspect query IR, export graphs, and inspect
corpus coverage. These commands do not deploy or execute a physical plan.

To develop an application using the Rust library, start with
[Library API: definitions, options, and examples](../developer_docs/library-api.md).
That guide explains how to choose strategies and models, rank candidates, and
work with lifecycle capabilities.

## Choose a command

Run from the repository root with Rust/Cargo installed. Cargo builds the selected
tool on first use.

| Command (`cargo run -p asap-devtools --bin … -- …`) | Input / options | Result |
| --- | --- | --- |
| `show_pre_asap_ir queries.txt` | File path, or stdin when omitted | Prints canonical Pre-ASAP IR |
| `show_post_asap_ir queries.txt` | Same query file format | Prints a representative Post-ASAP binding using a fixed approximate target; not all ranked alternatives |
| `dag_export --promql "<query>"` | One PromQL expression | Exports a query graph for inspection |
| `dag_export --sql "<query>"` | One SQL expression using the tool's catalog | Exports a query graph for inspection |
| `analyze_corpora --corpora --out-dir <dir>` | Repository PromQL corpora, output directory | Writes successful/error IR dumps and summary reports |
| `analyze_corpora --sql-corpora --out-dir <dir>` | Repository SQL corpora, output directory | Writes SQL corpus reports |
| `variant_coverage` | Repository corpora | Reports Pre-ASAP IR variant coverage |
| `sketch_coverage --epsilon 0.01` | Repository corpora; epsilon defaults to `0.01` | Reports sketch/reuse opportunities among successfully lowered queries |

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

Blank lines and lines beginning with `#` are ignored.

### Show the Pre-ASAP IR

Run:

```sh
cargo run -p asap-devtools --bin show_pre_asap_ir -- queries.txt
```

You can also provide the queries through stdin:

```sh
cargo run -p asap-devtools --bin show_pre_asap_ir < queries.txt
```

To dump and compare every PromQL corpus, run:

```sh
cargo run -p asap-devtools --bin analyze_corpora -- --corpora --out-dir artifacts/promql_pre_asap
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

### Inspect a representative Post-ASAP IR

Run:

```sh
cargo run -p asap-devtools --bin show_post_asap_ir -- queries.txt
```

Or through stdin:

```sh
cargo run -p asap-devtools --bin show_post_asap_ir < queries.txt
```

`show_post_asap_ir` uses an approximation target of ε = 0.01 and displays a
representative binding using the sketch strategy. It does not show the complete
ranked workload candidate set or choose a deployment lifecycle. Its SQL examples
use a fixed demonstration catalog, not your database schema. Use the library
workflow below to retain alternatives and provide your own models.

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

### Check IR variant coverage

Parse the query corpora in the repository and report which pre-ASAP IR variants are exercised:

```sh
cargo run -p asap-devtools --bin variant_coverage
```

### Check sketch-replacement coverage

Parse the same query corpora, lower them with an approximate `AccuracyTarget`, and report what fraction of each corpus's successfully-lowered queries got a genuine sketch alternative (`SketchApproximation`, e.g. KLL vs. DDSketch) and/or a cross-query common-subexpression-reuse candidate (`CommonSubexpressionReuse`):

```sh
cargo run -p asap-devtools --bin sketch_coverage -- --epsilon 0.01
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

- [Library API definitions and examples](../developer_docs/library-api.md)
- [Design overview](../design_docs/README.md)
- [Pre-ASAP IR reference](../design_docs/pre-asap-ir.md)
- [Post-ASAP IR reference](../design_docs/post-asap-ir.md)
