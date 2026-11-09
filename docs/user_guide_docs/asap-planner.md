# Plan a workload with asap_planner

`asap_planner` runs the planner's stages on a workload, up to the stage you
choose, and prints what each stage produced: as text, or as a JSON document
for the [Stage Viewer](stage-viewer.md). It reports candidates, selection,
modeled costs and accuracy checks; it does not deploy or execute the plan.

## Build and run

Run from an up-to-date ASAPPlanner repository root. You need Rust/Cargo,
a C/C++ toolchain, CMake, and the libclang shared library. HiGHS is built as a
native dependency; its Rust bindings use libclang.

On Ubuntu/Debian, install the native build dependencies with:

```sh
sudo apt-get update
sudo apt-get install build-essential cmake libclang-dev
```

Run it through Cargo; a checkout does not put `asap_planner` on your `PATH`:

```sh
cargo run --release -p asap-devtools --bin asap_planner -- \
  --workload tools/dag-viewer/workloads/planner-layering/example1.json \
  --run-until stage3
```

The `--` separates Cargo options from planner options. Release builds are
recommended: each stage runs to completion with no time limit, which can take
minutes in a debug build.

## Options

```text
asap_planner (--workload <file>)... --run-until stage0|stage1_pass1|stage1_pass2|stage2|stage3
  [--format text|json] [--full] [--out <file>] [--sql-catalog-table <json>]...
  [--cost-inputs <file>] [--deployment-capabilities <file>]
  [--ignore-accuracy-guarantees]
```

| Option | Meaning |
| --- | --- |
| `--workload <file>` | A `PlanningWorkload` JSON file: the queries, their requirements and recurrence, and the data-source facts. Repeat it to plan several files together, including PromQL and SQL files; sources they share must have the same facts. Required. |
| `--run-until <stage>` | The last stage to run. Every stage up to and including it is printed. Required. |
| `--format text\|json` | `text` (the default) for reading; `json` for an `asap-stage-pipeline/v2` document. |
| `--full` | Text only: print every candidate, edge and pruned candidate. By default the candidates of a memo group that differ only in θ are folded into one line (their indices, each θ parameter's values, and how many Pass 2 pruned, by cause), the candidates Pass 2 kept are printed in full with their edges, and the pruned lists of Pass 2 and Stage 2 are counts per cause. |
| `--out <file>` | Write to this file instead of standard output. |
| `--sql-catalog-table <json>` | One SQL table a SQL workload reads, as `{"name", "columns": [{"name", "type", "nullable"}], "time_index"}`. Repeat it per table. |
| `--cost-inputs <file>` | The cost model and its costs (`CostInputs` JSON). The files in `tools/dag-viewer/cost-inputs/` are ready to use. Without it, the built-in usage-over-time model is used. Stage 3 costs only usage-over-time models (`aws-fargate-us-east-1.json` is one); a provisioned- or cluster-capacity model ends Stage 3 with an `UnsupportedCostModel` error. |
| `--deployment-capabilities <file>` | What the deployment can do, e.g. `{"raw_data_retained": true, "query_time_results_retained": false}`. Set it to match your deployment. |
| `--ignore-accuracy-guarantees` | Accept candidates without an accuracy guarantee. By default each query's accuracy requirement is enforced. |

Any other option, or a missing required one, is a usage error (exit
status 2). An input the planner cannot read exits with status 1. So does a
run in which a stage failed (for example, Pass 1 rejected a query, or no
plan meets the requirements): the output is still written, with the error
under the stage that failed.

## Stages

| `--run-until` | Prints, after the stages before it |
| --- | --- |
| `stage0` | The frontends' query DAGs and their memo groups. |
| `stage1_pass1` | Stage 1 Pass 1: each computation's candidates. |
| `stage1_pass2` | Stage 1 Pass 2: the memo after its rules (with a chain group for every legal tumbling-window width), each candidate's logical edges with $\alpha^{\log}$ and why an edge fails, and the candidates Pass 2 eliminated with their causes. |
| `stage2` | Stage 2: the physical candidates, their physical edges with $\alpha^{\mathrm{phys}}$ and why an edge fails, and the candidates pruned with their reasons. |
| `stage3` | Stage 3: the selected plan, its cost per node, each query's accepted accuracy, the candidates Stage 3 pruned and the selection MILP's size. |

Every stage sees every legal tumbling-window width at once, so the selected
plan is the best over all of them.

## Plan a workload file

The example below plans the checked-in
[range-heavy workload](../../tools/dag-viewer/memo-examples/promql/rangeheavy-t8-t5-1h.json)
with raw data retained and AWS Fargate costs:

```sh
echo '{"raw_data_retained": true, "query_time_results_retained": false}' > capabilities.json
cargo run --release -p asap-devtools --bin asap_planner -- \
  --workload tools/dag-viewer/memo-examples/promql/rangeheavy-t8-t5-1h.json \
  --deployment-capabilities capabilities.json \
  --cost-inputs tools/dag-viewer/cost-inputs/aws-fargate-us-east-1.json \
  --run-until stage3
```

A SQL workload also needs its tables:

```sh
cargo run --release -p asap-devtools --bin asap_planner -- \
  --workload tools/dag-viewer/memo-examples/sql/rollupheavy-t11-service-endpoint-5m.json \
  $(python3 -c 'import json; [print("--sql-catalog-table", json.dumps(t, separators=(",", ":"))) for t in json.load(open("tools/dag-viewer/memo-examples/sql/tables.json"))]') \
  --run-until stage3
```

See [Run and inspect a query](run-a-query.md) for workload, source-fact and
SQL examples. To try a single query, write a one-query workload file.

## Inspect the result

With `--format json`, open the document in the
[Stage Viewer](stage-viewer.md), or read Stage 3 directly:

```sh
cargo run --release -p asap-devtools --bin asap_planner -- \
  --workload tools/dag-viewer/workloads/planner-layering/example1.json \
  --run-until stage3 --format json --out plan.json
python3 -c 'import json; print(json.dumps(json.load(open("plan.json"))["stage3_plan_selection"]["cost"], indent=2))'
```

Sections of stages that were not run are `null`. In
`stage3_plan_selection`, check `plan`, `cost` and any `error`: a written
document alone does not mean planning succeeded. Costs are model predictions, not measured execution times or a
cloud bill.

## Troubleshooting

- **`asap_planner: command not found`:** use `cargo run ... -- ...`, or the
  binary's path after `cargo build --release -p asap-devtools --bin asap_planner`.
- **`unknown argument ...`:** the option does not exist. Earlier
  `stage_pipeline` options such as `--promql`, `--example`, `--table`,
  `--cost-model` and `--milp-time-limit-s` were removed: write a workload
  file, use `--sql-catalog-table` and `--cost-inputs`. There is no time limit.
- **`--run-until is required`:** name the last stage to run.
- **Cannot read the workload:** paths are relative to your working
  directory; an absolute path avoids ambiguity.
- **`Unable to find libclang`:** install `libclang-dev`, or point
  `LIBCLANG_PATH` at the directory containing `libclang.so` (a directory, not
  the file name), then re-run the same command.
