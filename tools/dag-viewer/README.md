# ASAPPlanner Stage Viewer

A browser view of what the planner does with one workload, read from an
`asap-stage-pipeline/v2` document written by the `asap_planner` devtool
(`asap_planner --workload <file> --run-until stage3 --format json`). The
document shows:

- **Stage 0**, the query-language frontends' DAG, with one root per query;
- **Stage 1**, the logical candidates of the memo and the selected plan's
  logical DAG (`L1`);
- **Stage 2**, the selected plan as an executable physical DAG (`P1`), with
  each node's timing and lifetime, every physical candidate with its
  producer edges, and the physical candidates Stage 2 pruned;
- **Stage 3**, the selected plan's cost, the accuracy it accepted per query,
  its materialized views, the candidates it pruned and the search report.

To run it, see [RUNNING.md](RUNNING.md).

## The page

- **Examples**: one tab per entry of `examples.json`: #509 Examples 1, 3a
  and 3b. The local server plans each one the first time it starts.
  `#example3b` in the URL opens that example.
- **Workload sets** (button): one table per set listed in
  `workloads/index.json`, one row per workload with its query count, query
  labels, the queries (text, how often, accuracy) and the sources they
  read. Clicking a row plans it with the local server, as the set says
  (deployment capabilities, cost model, accuracy guarantees). Without the
  server, the rows say they need it. `#workload-sets` in the URL opens the
  section.
- **Query workload** with each query's accuracy, latency, recurrence,
  predictability and range evaluation, and a line that counts the
  candidates at each stage.
- **Data workload** beside it: the arrival and one row per declared source
  with exactly the facts it declares (series count, metric type,
  non-negative values, sample interval, stream norms, distinct bound,
  distinct values per label, distinct values per label set) and the queries
  whose text names it, plus the deployment capabilities the run planned
  with.
- **Stage 3 · plans by cost**: the selected plan, then the Stage 1
  candidates that did not compose, each with its error. The header counts the candidates, names the search and its report
  (status, cost lower bound, MILP size, times), says whether accuracy
  guarantees were enforced or ignored, shows the run's warnings, and says
  why Stage 1 or plan selection failed when it did.
- **Three lanes**, stacked top to bottom so each is as wide as the page:
  Stage 0, the Stage 1 candidate, and its Stage 2 plan with each node's
  timing (also its fill: green at ingestion time, orange at query time) and
  Stage 3 cost (`× n` when a node that is not materialized is computed once
  per consumer). Each lane is the whole workload's DAG, so a node shared by
  several queries appears once and says `shared ×n`. Summary builds print
  their configuration (for example `CmsWithHeap · depth 7 · heap 100 ·
  width 272`) and whether there is one per group, one per series, or one
  shared instance. A standalone scalar query (`time()`, `scalar(v)`,
  `1 + 2`) is a dashed `Scalar` node over the nodes it reads. Query roots
  have a thick border. A `SummaryTumblingWindowChain` is one node (`6
  tumbling windows × 10 s`); clicking it draws its tumbling windows, newest
  first, each with its window (`window t−20 s … t−10 s`), and clicking a
  tumbling window folds them back.
- **Highlighting**: the **highlight query** menu dims, in every lane, what
  that query's root does not reach. Clicking a node dims everything but
  what it reads and what reads it; clicking the background goes back to the
  query's highlight.
- **Materialized views** under the Stage 2 lane: the selected plan's
  materialized views, one row per Stage 2 node: its coverage definition,
  operator and summary parameters, tumbling windows (`6 × 10 s` for a
  tumbling-window chain), how it is refreshed, its retention, the Stage 3
  cost and the queries that read it. A chain's row lists its installed
  tumbling windows under **installed**. Clicking a row selects its node in
  the Stage 2 lane.
- **Memo**: the memo after each planner step, one tab per step (`Stage 0`,
  `Stage 1, Pass 1`, local window composition, `Stage 1, Pass 2`), opening
  on the last. Each memo group is a box `G<id> · <label>` with its candidate count
  and the queries it is the root of; its candidates sit inside it, bordered
  by origin, marked `✚ new` when the step added them. A group with more
  than 12 candidates is drawn as one node per origin with its count;
  clicking a count lists them. Each candidate box shows its parameters θ
  and window layout ω. On the last tab the selected plan's candidates are
  filled green with `✓ selected` (a group may have several, one per
  execution context). Clicking a group shows its operator, children,
  parents, queries, its candidates with θ, ω and per-input parameter
  floors, and the candidates Stage 2 (`G<group>[<index>] (timing,
  lifetime): reason`) and Stage 3 (`C<id>: reason`, with the candidate
  that dominates it) pruned there; Pass 1 records none. Clicking a
  candidate also lists, on the last tab, its Stage 2 physical candidates
  `C<id> (timing, lifetime)` and, per input, every producer edge: `✓` when
  admissible, `✗` with the rejection otherwise, `selected` when the
  selected plan uses it.
- **Details**: click a node for its operator, the queries reaching it,
  timing, lifetime, output schema, guarantee and Stage 3 cost; click
  an edge for its schema and data state.

Other documents open with **Open stage document or workload…**, by dropping
a file on the page, or with `?doc=<path>` for a file served next to
`index.html`. A `PlanningWorkload` JSON file (one with a `query_workload`)
is planned by the local server instead, with the default deployment
capabilities and cost model. The viewer checks a document before showing
it and lists the problems instead when a check fails. It reads only
`asap-stage-pipeline/v2` documents; write an older one again with
`asap_planner`.

## The memo and pruned-candidate fields

The fields the Memo view reads (`asap_planner`'s header comment describes
the whole document):

- `memo.steps[].groups[]`: `id`, `label`, `operator` (the group's
  expression as `Operator` JSON, children as group ids), `children`,
  `parents`, `queries`, `roots_of`, `candidate_count`,
  `candidate_count_by_origin`, `candidates_capped` and `candidates`;
- each candidate (one operator over its input groups): `index`, `origin`
  (`pass1`, `summary_input`, `tumbling_windows`, `rollup`, `top_k_reuse`,
  `widened_scans`), `added`, `label`, `operator`, `children` (its input
  groups), `fused_groups`, `logical_properties` (`parameters` θ as
  `SketchParams` JSON or null, `window_layout` ω, e.g.
  `{"tumbling": {"width_ms": 10000}}`, or null) and `input_requirements`
  (one `{parameter_floor_for_required_accuracy}` per input);
- `memo.selected_candidates`: per group id of the last step, the indices of
  the selected plan's candidates there; absent without a selected plan;
- `stage2_physical_asap_optimization.physical_candidates[]`: `id`, `group`,
  `index_in_group`, `properties` (`timing`, `lifetime`) and `inputs`, each
  with its `group`, `requirement` and `edges` (`producer`,
  `satisfies_logical_requirement`, `satisfies_physical_requirement`,
  `rejection`, null when admissible, and `selected`);
- `stage2_physical_asap_optimization.pruned_candidates[]`: `group`,
  `index_in_group`, `properties` and `reason`;
- `stage3_plan_selection.pruned_candidates[]`: `group`, `candidate` (a
  physical candidate id), `reason` and `dominated_by` (a physical candidate
  id or null).

## Workload sets

`workloads/index.json` lists sets of `PlanningWorkload` files under
`workloads/<set>/<workload>.json`:

```json
{ "format": "asap-workload-sets/v1",
  "sets": [{ "id": "random-mixed-seed-862", "name": "Random mixed · seed-862",
             "description": "...", "source": "where the workloads come from",
             "deployment_capabilities": { "raw_data_retained": false, "query_time_results_retained": false },
             "cost_model": "aws-fargate-us-east-1",
             "accuracy_guarantees": "enforced",
             "workloads": [{ "id": "workload-01", "file": "random-mixed-seed-862/workload-01.json",
                             "query_labels": ["q01-sum-fine", "..."] }] }] }
```

`deployment_capabilities`, `cost_model` (`usage-over-time` or
`aws-fargate-us-east-1`: the cost inputs in `cost-inputs/<name>.json`), `accuracy_guarantees` (`ignored`, the default,
or `enforced`) and `query_labels` (one per query, in entry order) are
optional. A PlanningWorkload declares no SQL tables, so a set of SQL
workloads puts them in `workloads/<set>/tables.json`, a JSON array of table
declarations (`{"name": "flows", "time_index": 0, "columns": [{"name":
"ts", "type": "timestamp", "nullable": false}, ...]}`), which are planned
with the workload. A new set is a directory of files and an index entry.

The three ASAPQuery sets (seed-862, seed-862-x5, seed-862-long; 10
workloads each, PromQL queries only) were converted from ASAPQuery's random
workload manifests with ε = 1 − the accuracy SLA and δ = 0.3, and are
planned as the MVP deployment: raw data not retained, Fargate prices,
accuracy guarantees enforced. Stage 3 searches exactly, so planning one of
them can take several minutes.

`memo-examples/` holds smaller `PlanningWorkload` files, each the smallest
subset of an ASAPQuery paper workload template that shows one kind of
sharing; open one with **Open stage document or workload…**.

## Document format

The `asap-stage-pipeline/v2` document is described in the header comment of
`crates/devtools/src/bin/asap_planner.rs`.

## Tests

From `tools/dag-viewer`:

```bash
python3 -m unittest test_viewer test_server
node --test *.test.mjs
```

The Python tests run the page's JavaScript through `py_mini_racer` (`pip
install py-mini-racer==0.6.0`); without it those tests are skipped. Tests
over a planned document need a built `asap_planner` (`cargo build -p
asap-devtools --bin asap_planner`) and are skipped without it.
