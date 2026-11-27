# Plan a workload with asap_planner

`asap_planner` runs the planner's stages on a workload, up to the stage you
choose, and prints what each stage produced: as text, or as a JSON document
for the [Plan Viewer](plan-viewer.md). It reports candidates, selection,
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
cargo run --release -p asap-planner-cli --bin asap_planner -- \
  --workload tools/dag-viewer/workloads/planner-layering/example1.json \
  --run-until stage3
```

The `--` separates Cargo options from planner options. Release builds are
recommended: each stage runs to completion with no time limit, which can take
minutes in a debug build.

## Options

```text
asap_planner (--workload <file>)... --run-until stage0|stage1_pass1|stage1_pass2|stage2|stage3
  [--format text|json] [--verbose] [--out <file>] [--sql-catalog-table <json>]...
  [--cost-inputs <file>] [--deployment-capabilities <file>]
  [--ignore-accuracy-guarantees] [--plan-file <file>]
```

| Option | Meaning |
| --- | --- |
| `--workload <file>` | A `PlanningWorkload` JSON file: the queries, their requirements and recurrence, and the data-source facts. Repeat it to plan several files together, including PromQL, MetricsQL and SQL files; sources they share must have the same facts. Required. |
| `--run-until <stage>` | The last stage to run. Every stage up to and including it is printed. Required. |
| `--format text\|json` | `text` (the default) for reading; `json` for an `asap-stage-pipeline/v2` document. |
| `--verbose` | Text only: print every candidate, edge and pruned candidate in full. By default each stage starts with a legend; memo groups show the groups that read them, the queries that reach them and (from Pass 2) their evaluation grid, without their column properties; candidates that differ only in θ are folded into one line; each input lists its producers' ids by whether they meet its requirement, without the reasons; Stage 2 lists each logical candidate once with its physical candidates under it (`★` marks the selected plan's); and the pruned lists are counts per cause. |
| `--out <file>` | Write to this file instead of standard output. |
| `--sql-catalog-table <json>` | One SQL table a SQL workload reads, as `{"name", "columns": [{"name", "type", "nullable"}], "time_index"}`. Repeat it per table. |
| `--cost-inputs <file>` | The cost model and its costs (`CostInputs` JSON). The files in `tools/dag-viewer/cost-inputs/` are ready to use. Without it, the built-in usage-over-time model is used. Stage 3 costs only usage-over-time models (`aws-fargate-us-east-1.json` is one); a provisioned- or cluster-capacity model ends Stage 3 with an `UnsupportedCostModel` error. |
| `--deployment-capabilities <file>` | What the deployment can do, e.g. `{"raw_data_retained": true, "query_time_results_retained": false}`. Set it to match your deployment; see [Deployment capabilities](#deployment-capabilities). |
| `--ignore-accuracy-guarantees` | Accept candidates without an accuracy guarantee. By default each query's accuracy requirement is enforced. |
| `--plan-file <file>` | Also write the plan Stage 3 selects to this file, as `InstallablePlan` JSON; needs `--run-until stage3`. See [Write a plan file](#write-a-plan-file). |

Any other option, or a missing required one, is a usage error (exit
status 2). An input the planner cannot read exits with status 1. So does a
run in which a stage failed (for example, Pass 1 rejected a query, or no
plan meets the requirements): the output is still written, with the error
under the stage that failed.

With a selected plan, `asap_planner` also compiles it on the Planner's
executor and reports the result; it never changes the exit status. When the
executor cannot run the plan, standard error gets one line

```text
note: the Planner's executor cannot run this plan: <reason>
```

and the JSON document's `stage3_plan_selection` has
`"planner_executor_can_run": false` with the reason in
`planner_executor_error` (otherwise `"planner_executor_can_run": true`).

## Deployment capabilities

The `--deployment-capabilities` file is a `DeploymentCapabilities` JSON
object; an omitted field keeps its default.

`supported_summaries` lists the sketch summary types the deployment builds,
each with the statistics it reads from them, e.g.
`[{"summary_type": "KLL", "readouts": ["Quantile"]}]`. Without the file, or
when the file leaves it out, the planner uses the summary types the
Planner's executor builds. To plan for a runtime that builds every summary
type, pass the ready-to-use
[`every-summary-family.json`](../../tools/dag-viewer/deployment-capabilities/every-summary-family.json),
which lists each one with every statistic it answers.

Besides
`supported_summaries`, `raw_data_retained` and
`query_time_results_retained`, three fields say what the executor cannot
do or what the caller rules out:

```json
{
  "counter_semantics": {"reset_handling": true, "extrapolation": false},
  "state_not_mergeable_across_windows": [{"exact": "Counter"}, {"sketch": "KLL"}],
  "excluded_candidates": [
    {"timing": "query_time", "operator": "Scan"},
    {"summary_family": {"sketch": "KLL"}, "sketch_parameter_range": {"kll_k": {"at_most": 1403}}},
    {"timing": "query_time", "aggregate_measure": "rate"}
  ]
}
```

- `counter_semantics` (default: both `true`): without `extrapolation`, no
  `rate`, `increase` or `delta` and no `Counter` state is planned;
  without `reset_handling`, no `rate`, `increase`, `irate` or `resets`
  and no `Counter` or `IRate` state. One `Counter` state answers both
  `rate` and `increase`; the earlier names `Rate` and `Increase` are
  refused.
- `state_not_mergeable_across_windows` (default none): for each family
  listed, no tumbling-window chain and no state kept across evaluations;
  its state is built and finalized within one evaluation.
- `excluded_candidates` (default none): each entry is a
  `CandidateExclusion` that rules out every physical candidate matching all
  the fields it sets: `timing`, `kept_across_evaluations`, `operator`,
  `per_entity_reduction`, `aggregate_measure` (an `Aggregate` measure's
  JSON `kind`, e.g. `"rate"` or `"i_rate"`), `summary_family`,
  `sketch_parameter_range` and `readout` (`{"summary_type": "KLL",
  "statistic": "Quantile"}`). `aggregate_measure` matches the `Aggregate`
  only: to rule out the counter state as well, add `{"summary_family":
  {"exact": "Counter"}}`. An entry that sets no field, or whose fields match
  no operator in common, is refused.

Stage 2 prints each candidate a capability rules out as
`ExcludedByDeployment(<source>)`, e.g.
`ExcludedByDeployment(counter_not_extrapolated)`, or
`ExcludedByDeployment(excluded_candidates[0])` for the first entry of
`excluded_candidates`. When no plan is left, Stage 3 names those sources.

## Stages

| `--run-until` | Prints, after the stages before it |
| --- | --- |
| `stage0` | The frontends' query DAGs and their memo groups. |
| `stage1_pass1` | Stage 1 Pass 1: each computation's candidates. |
| `stage1_pass2` | Stage 1 Pass 2: the memo after its rules (with a chain group for every legal tumbling-window width), each candidate's logical edges with $\alpha^{\log}$ and why an edge fails, and the candidates Pass 2 eliminated with their causes. |
| `stage2` | Stage 2: the physical candidates, their physical edges with $\alpha^{\mathrm{phys}}$ and why an edge fails, and the candidates pruned with their reasons. |
| `stage3` | Stage 3: the selected plan, its cost per node, its materialized views (node, refresh, retention), each query's accepted accuracy, the candidates Stage 3 pruned and the selection MILP's size. |

Every stage sees every legal tumbling-window width at once, so the selected
plan is the best over all of them.

## Plan a workload file

The example below plans the checked-in
[range-heavy workload](../../tools/dag-viewer/memo-examples/promql/rangeheavy-t8-t5-1h.json)
with raw data retained and AWS Fargate costs:

```sh
echo '{"raw_data_retained": true, "query_time_results_retained": false}' > capabilities.json
cargo run --release -p asap-planner-cli --bin asap_planner -- \
  --workload tools/dag-viewer/memo-examples/promql/rangeheavy-t8-t5-1h.json \
  --deployment-capabilities capabilities.json \
  --cost-inputs tools/dag-viewer/cost-inputs/aws-fargate-us-east-1.json \
  --run-until stage3
```

A SQL workload also needs its tables:

```sh
cargo run --release -p asap-planner-cli --bin asap_planner -- \
  --workload tools/dag-viewer/memo-examples/sql/rollupheavy-t11-service-endpoint-5m.json \
  $(python3 -c 'import json; [print("--sql-catalog-table", json.dumps(t, separators=(",", ":"))) for t in json.load(open("tools/dag-viewer/memo-examples/sql/tables.json"))]') \
  --run-until stage3
```

See [Run and inspect a query](run-a-query.md) for workload, source-fact and
SQL examples. To try a single query, write a one-query workload file.

## Write a plan file

`--plan-file <file>` writes the selected plan as an `InstallablePlan` JSON
file (`wire_version`, the selected DAG and the query texts), the input a
runtime installs; a runtime reads it with `InstallablePlan::from_json`. The
plan file is written whether or not the Planner's executor can run the plan:
another runtime may run it.

```sh
cargo run --release -p asap-planner-cli --bin asap_planner -- \
  --workload tools/dag-viewer/workloads/planner-layering/example1.json \
  --run-until stage3 --plan-file plan.json
```

| Run | Exit status | `plan.json` |
| --- | --- | --- |
| Stage 3 selects a plan the Planner's executor runs | 0 | written |
| Stage 3 selects a plan the Planner's executor cannot run | 0, with the `note:` line on standard error | written |
| Stage 3 selects no plan, or the run fails | 1, with `asap_planner: <why> (no plan file written to plan.json)` when Stage 3 selected none | not written; a file already there is left as it is |

`--plan-file` with a `--run-until` before `stage3` is a usage error (exit
status 2).

## Inspect the result

With `--format json`, open the document in the
[Plan Viewer](plan-viewer.md), or read Stage 3 directly:

```sh
cargo run --release -p asap-planner-cli --bin asap_planner -- \
  --workload tools/dag-viewer/workloads/planner-layering/example1.json \
  --run-until stage3 --format json --out plan.json
python3 -c 'import json; print(json.dumps(json.load(open("plan.json"))["stage3_plan_selection"]["cost"], indent=2))'
```

Sections of stages that were not run are `null`. In
`stage3_plan_selection`, check `plan`, `cost` and any `error`: a written
document alone does not mean planning succeeded. Costs are model predictions, not measured execution times or a
cloud bill.

The JSON document numbers memo groups and candidates canonically (depth
first from the query roots, in query order), while `--format text` prints
the planner's own ids, so one group can be `G2` in the JSON and `G5` in the
text.

## Troubleshooting

- **`asap_planner: command not found`:** use `cargo run ... -- ...`, or the
  binary's path after `cargo build --release -p asap-planner-cli --bin asap_planner`.
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
