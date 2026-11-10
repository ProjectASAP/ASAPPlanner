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
| `asap_planner --workload <file> --run-until <stage>` | `PlanningWorkload` JSON files | Runs the planner's stages up to `<stage>` and prints each stage's output as text, or with `--format json` as the document the [Plan Viewer](plan-viewer.md) shows |
| `convert_rqe_atomic_costs <rqe_atomic_costs.json> <out.json>` | sketch-bench's per-operation cost table | Writes a measured table to set as `measured_operation_costs` in an `asap_planner --cost-inputs` file; skipped rows are listed on stderr |
| `analyze_corpora --corpora --sample-interval-ms 1000 --out-dir <dir>` | Repository PromQL corpora, output directory | Writes successful/error IR dumps and summary reports |
| `analyze_corpora --sql-corpora --out-dir <dir>` | Repository SQL corpora, output directory | Writes SQL corpus reports |
| `variant_coverage --sample-interval-ms 1000` | Repository corpora | Reports Pre-ASAP IR variant coverage |
| `sketch_coverage --sample-interval-ms 1000 --max-error 0.01` | Repository corpora; `--max-error` defaults to `0.01` | Reports which summary candidates Pass 1 offers for each successfully lowered query |

## Plan a workload

For build dependencies, every option and troubleshooting, see
[Plan a workload with asap_planner](asap-planner.md).

`asap_planner` runs the query-language frontends (Stage 0), logical
ASAP-aware optimization (Stage 1), physical ASAP-aware optimization
(Stage 2) and plan selection (Stage 3) on a workload file, and prints what
each stage produced:

```sh
cargo run --release -p asap-devtools --bin asap_planner -- \
    --workload tools/dag-viewer/workloads/planner-layering/example1.json \
    --run-until stage3
```

The workload file holds the queries, their accuracy requirements and how
often they run, and the data-source facts plan selection costs from: each
source's series count and sample interval, or rows per second. Without the
fact a scan needs, planning fails with an error that names the source and
the missing fact.

Each query's accuracy requirement is enforced unless you pass
`--ignore-accuracy-guarantees`. The output ends with Stage 3's selection:
the selected physical candidates, the plan's `cost_per_second` with each
node's charge, each query's accepted accuracy, and the search status
(`Optimal` when the plan is proven the cheapest). For this example the
selected plan is exact for both queries. Add `--format json --out plan.json`
and open the document in the [Plan Viewer](plan-viewer.md) to see every
stage.

## Inspect a query's logical DAG

`--run-until stage0` prints each query's logical DAG (Pre-ASAP IR) as the
frontends lowered it, and its memo groups. It never picks a sketch: it shows
what a query means, with exact operators only.

```sh
cargo run -p asap-devtools --bin asap_planner -- \
    --workload tools/dag-viewer/workloads/planner-layering/example1.json \
    --run-until stage0
```

A SQL workload also needs the tables it reads, one `--sql-catalog-table`
per table; see [Plan a workload with asap_planner](asap-planner.md).

### Analyze the corpora

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

The corpus commands require `--sample-interval-ms`, the nonzero sample
interval in milliseconds of every metric a PromQL query reads. The examples
use one second; supply the interval for your data. A workload file declares
each metric's `sample_interval` in its data workload instead.
