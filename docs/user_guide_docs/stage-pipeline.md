# Plan a workload with stage_pipeline

`stage_pipeline` plans queries through Stages 0–3 and writes a JSON document
for the [Stage Viewer](stage-viewer.md). It reports candidate selection,
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

Use Cargo to build and run the command; a checkout does not install
`stage_pipeline` on your shell's `PATH`:

```sh
cargo run --release -p asap-devtools --bin stage_pipeline -- \
  --promql 'sum_over_time(requests[5m])' \
  --series-count 1000 --sample-interval-ms 15000 \
  --enforce-accuracy-guarantees \
  --milp-time-limit-s 30 \
  --out plan.json
```

The `--` separates Cargo options from planner options. `--out` is required.
Release builds are recommended for planning-performance measurements. Initial
compilation can take several minutes and is separate from the planning budget.

Alternatively, build once and run the binary directly:

```sh
cargo build --release -p asap-devtools --bin stage_pipeline
"${CARGO_TARGET_DIR:-target}/release/stage_pipeline" \
  --example planner-layering-1 --enforce-accuracy-guarantees --out example.json
```

## Plan a workload file

Use a `PlanningWorkload` JSON file containing query requirements, recurrence
and data-source facts. The example below uses the checked-in
[range-heavy workload](../../tools/dag-viewer/memo-examples/promql/rangeheavy-t8-t5-1h.json).
Run it from the repository root; no input file needs to be created.

```sh
cargo run --release -p asap-devtools --bin stage_pipeline -- \
  --workload tools/dag-viewer/memo-examples/promql/rangeheavy-t8-t5-1h.json \
  --deployment-capabilities '{"raw_data_retained":true,"query_time_results_retained":false}' \
  --cost-model aws-fargate-us-east-1 \
  --enforce-accuracy-guarantees \
  --milp-time-limit-s 280 \
  --out rangeheavy-plan.json
```

`stage_pipeline` has one search engine, `candidate-connections` (the
document's `search` field names it); there is no `--search` option.

The deployment declaration allows retained raw data but does not allow query-time
results to be kept across evaluations. Set capabilities to match your deployment.
For file inputs, accuracy targets come from the workload file, not the inline
query flags `--max-error`, `--failure-probability`, or `--accuracy-metric`.

`--workload` can repeat to jointly plan multiple files, including separate
PromQL and SQL workloads. Shared-source facts must agree; SQL inputs also need
appropriate `--table` declarations. See [Run and inspect a query](run-a-query.md)
for workload, source-fact, and SQL examples.

## Search, accuracy, and costs

| Option | Meaning |
| --- | --- |
| `--enforce-accuracy-guarantees` | Enforce the queries' accuracy requirements. Without it, this inspection tool ignores accuracy guarantees, although executability checks still apply. |
| `--milp-time-limit-s 280` | Set the planning time budget in seconds. A timeout may return a validated incumbent or no selected plan; it does not establish optimality. |
| `--cost-model aws-fargate-us-east-1` | Compare plans using the configured AWS Fargate pricing model. Other supported names are `usage-over-time`, `provisioned-capacity`, and `cluster-capacity`. |
| `--max-displayed 64` | Limit candidates shown in the document; this is not a search budget. |
| `--exhaustive --max-combinations 10000` | Enumerate up to this many memo choices of the selected round, implement each with Stage 2 and price every resulting plan with the whole-workload reference pricer; its selection, costs and rejections are under `reference_pricing`. It is a reference, not Stage 3's search. |
| `--plan-selection-settings settings.json` | Load selection settings instead of `--cost-model`, cluster flags, or `--measured-operation-costs`. |
| `--no-memo` | Leave out the `memo` section (the memo after each step and the enumerated candidate DAGs), for a smaller document. |
| `--price-enumerated 50` | Cost the first 50 enumerated candidate DAGs through Stage 2 and Stage 3, and flag any cheaper than the selected plan. Off by default. |

For inline queries, supply the data facts needed for pricing: for example,
`--series-count` and `--sample-interval-ms`, or `--rows-per-second`.
`--rows-held` describes retained rows for scans that need that bound.
Workload files carry these facts in their data workload. Missing facts are not
interpreted as zero cost.

## Inspect the result

Open the generated JSON file in the [Stage Viewer](stage-viewer.md), or inspect
Stage 3 directly:

```sh
python3 - <<'PY'
import json
with open('plan.json') as source:
    document = json.load(source)
print('Format:', document['format'])
print(json.dumps(document['stage3_plan_selection'], indent=2))
PY
```

The document format is `asap-stage-pipeline/v1`. Check `selected_dag`,
`cost`, `search_report` and any `error` in `stage3_plan_selection`, and the
per-round counts and times in `planning_report`. A written JSON document
alone does not mean planning succeeded: `selected_dag` may be null.
A selected plan is not necessarily a proven optimum; inspect the search
status (`search_report.status`) and the lower bound. Costs are model predictions, not measured
execution times or an actual cloud bill.

## Troubleshooting

- **`stage_pipeline: command not found`:** use `cargo run ... -- ...`, or the
  binary's explicit path after building. Building does not add it to `PATH`.
- **`unknown argument --search`:** the option was removed; drop it. The
  candidate-connection search is the only search. For any other
  `unknown argument`, check that your branch and binary are current.
- **`--out is required`:** supply an output JSON path; create its parent directory
  first if needed.
- **Cannot read the workload:** paths are relative to your working directory;
  an absolute input path avoids ambiguity.
- **`Unable to find libclang`:** install `libclang-dev`, or point
  `LIBCLANG_PATH` at the directory containing an existing `libclang.so`:

  ```sh
  export LIBCLANG_PATH=/path/to/llvm/lib
  # Re-run the original cargo command; cargo clean is not needed.
  ```

  The value is a directory, not the library filename. For example, if libclang
  is supplied by the Python `libclang` package, its directory may be
  `.../site-packages/clang/native`. Confirm that `libclang.so` exists there.
  CMake's unused-variable warnings are not the cause of this bindgen failure.
