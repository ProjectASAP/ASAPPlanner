# ASAPPlanner Stage Viewer

A browser view of what the planner does with one workload, read from an
`asap-stage-pipeline/v2` document written by the `asap_planner` devtool
(`asap_planner --workload <file> --run-until stage3 --format json`). The
document shows:

- **Stage 0**, the query-language frontends' DAG, with one root per query;
- **Stage 1**, the memo's logical candidates after Pass 1 and Pass 2, the
  logical edges between them ($\alpha^{\log}$ and why it is 0), and the
  candidates Pass 2 pruned with their root causes;
- **Stage 2**, every physical candidate with its physical edges
  ($\alpha^{\mathrm{phys}}$ and why it is 0), and the physical candidates
  Stage 2 pruned with their reasons;
- **Stage 3**, the selected plan (a physical DAG) with each node's timing,
  lifetime and cost, the accuracy it accepted per query, its materialized
  views, the candidates it pruned and the size and times of its MILP, or why
  no plan was selected.

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
  candidates at each stage: the Pass 2 memo's logical candidates and how
  many Pass 2 pruned, the physical candidates and how many Stage 2 and
  Stage 3 pruned, and whether a plan was selected.
- **Data workload** beside it: the arrival and one row per declared source
  with exactly the facts it declares (series count, metric type,
  non-negative values, sample interval, stream norms, distinct bound,
  distinct values per label, distinct values per label set) and the queries
  whose text names it, plus the deployment capabilities the run planned
  with.
- **Stage 3 · plan selection**: one row, the selected plan with its cost
  per second and the accuracy Stage 3 accepted per query, or why no plan
  was selected. The header says why Stage 1 or Stage 2 failed when it did,
  shows the selection's warnings, says whether accuracy guarantees were
  enforced or ignored, and names the search with its MILP's size and times.
- **Two lanes**, stacked top to bottom so each is as wide as the page:
  Stage 0 and Stage 3's plan, with each plan node's timing (also its fill:
  green at ingestion time, orange at query time) and Stage 3 cost (`× n`
  when a node that is not materialized is computed once per consumer).
  Each lane is the whole workload's DAG, so a node shared by several queries
  appears once and says `shared ×n`. Summary builds print their summary type
  (`summary: Kll`, `summary: exact Sum`) and grouping; a
  `SummaryTumblingWindowChain` prints its tumbling-window width. A
  standalone scalar query (`time()`, `scalar(v)`, `1 + 2`) is a dashed
  `Scalar` node over the nodes it reads. Query roots have a thick border.
- **Highlighting**: the **highlight query** menu dims, in every lane, what
  that query's root does not reach. Clicking a node dims everything but
  what it reads and what reads it; clicking the background goes back to the
  query's highlight.
- **Materialized views** under the plan lane: one row per view: its plan
  node, coverage definition, operator and summary type, how it is
  refreshed, what its refresh reads, its retention, the raw-data windows it
  scans, the node's Stage 3 cost and the queries that read it. A plan that
  is not installable says why instead. Clicking a row selects its node in
  the plan lane.
- **Memo**: the memo after each planner step, one tab per step (`Stage 0`,
  `Stage 1, Pass 1`, `Stage 1, Pass 2`), opening on the last. Each memo
  group is a box `G<id> · <label>` with its candidate count (and how many
  Pass 2 pruned) and the queries it is the root of; its candidates sit
  inside it, bordered by origin, marked `✚ new` when the step added them
  and `✗ pruned` (dashed) when Pass 2 pruned them; those (and a group left
  with none) are hidden until **show pruned candidates** is ticked. A group with more than
  12 candidates is split into counts by its first dimension that varies:
  origin, sketch family, each θ field (e.g. `k`, `depth`, `width`), then
  tumbling-window width. Clicking a count drills into it, splitting again
  until at most 12 remain, which are then drawn one by one; the group's
  label shows the path (`origin=Pass1 › depth=5`), clicking the group goes
  back one level, and the group's details offer the other varying
  dimensions to split by instead. An arrow goes from a candidate (or count) to
  each group it reads, or once from the group when all its candidates read the
  same groups; a line above the drawing says how to read it. Each candidate box shows its parameters θ. On the last
  tab the selected plan's candidates are filled green with `✓ selected` (a
  group may have several, one per execution context). Clicking a group
  shows its operator, children, parents, queries, its candidates with θ,
  and the candidates pruned there: by Pass 2 (`#<index>: cause`, with
  `through G<group>[<index>] → …` when it was pruned because the consumers
  it is traced through were), by Stage 2 and by Stage 3 (`#<index>
  (timing, lifetime): reason`, e.g. `dominated by C<id>`). Clicking a
  candidate shows its θ and required accuracy per input and, on the Pass 2
  tab, why Pass 2 pruned it, its logical edges per input (`✓ #<producer>`
  when $\alpha^{\log} = 1$, `✗ #<producer>: α^log = 0: …` otherwise), its
  Stage 2 physical candidates `C<id> (timing, lifetime)` with their
  physical edges per input (`✓ C<producer>`, or `✗ C<producer>: α^phys =
  0: …`; `selected` when the plan uses it), and the physical candidates of
  it Stage 2 and Stage 3 pruned.
- **Theme**: light by default; **Dark theme** (top right) switches it and is remembered
  in the browser.
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

## The fields the viewer reads

The fields the viewer reads (`asap_planner`'s header comment describes the
whole document):

- `stage0_query_language_frontends.dag`: `nodes` (each an `operator`, its
  inputs as node indices) and one `roots` entry per query (`{"Operator":
  id}` or `{"Scalar": expr}`);
- `stage1_logical_asap_optimization`: `pruned_candidates[]`, Pass 2's
  eliminated candidates (`group`, `index_in_group` and `causes`, each with
  `through_pruned_candidates` (`{group, index_in_group}` list), its `root`
  reason and its `text`); `error` when Stage 1 failed;
- `stage2_physical_asap_optimization`: `physical_candidates[]` (`id`,
  `group`, `index_in_group`, `properties` (`timing`, `lifetime`) and
  `inputs`, each with its `group` and `edges`: `producer`, a physical
  candidate id, `satisfies_physical_requirement`, `rejection`, null when it
  holds, and `selected`), `pruned_candidates[]` (`group`, `index_in_group`,
  `properties` and `reason`: a snake_case string, or `{"dominated_by":
  id}`) and `error`;
- `stage3_plan_selection`: `plan` (a `PhysicalASAPDAG`: `nodes` with `id`,
  `payload`, `output_schema`, `output_state`, `lifetime` and `guarantee`;
  `edges`; `roots`; `scalar_roots` as `[query position, expr]`; null
  without a plan), `cost` (`cost_per_second`, null under cost model 3,
  `usage` and `per_node`, keyed by plan node id, with `cost_per_second`,
  `multiplier` and `detail`), `accepted_accuracy_per_query` (`summary`),
  `pruned_candidates` (as Stage 2's), `milp_variables`,
  `milp_constraints`, `milp_construction_ms`, `milp_solve_ms`,
  `materialized_views` (`node_id`, a plan node, `refresh`,
  `coverage.definition_id`, `inputs` and `retention_ms`),
  `materialized_views_error`, `warnings` and `error`;
- `memo.steps[].groups[]`: `id`, `label`, `operator` (the group's
  expression as `Operator` JSON, children as group ids), `children`,
  `parents`, `queries`, `roots_of`, `candidate_count`,
  `candidate_count_by_origin` and `candidates`;
- each candidate (one operator over its input groups): `index`, `origin`
  (`pass1`, `tumbling_windows`, `rollup`, `top_k_reuse`), `added`,
  `label`, `operator`, `children` (its input groups), `fused_groups`,
  `parameters` (θ as `SketchParams` JSON, null when exact) and
  `required_accuracy` (one null or `{metric, max_error,
  failure_probability}` per input); at the `pass2` step also `pruned` and
  `inputs` (per input, its `group` and `edges`: the logical edge from each
  candidate of the input group, with `producer` (its index there),
  `satisfies_logical_requirement` and `rejection`, null when it holds);
- `memo.selected_candidates`: per group id of the last step, the indices of
  the plan's candidates there; absent without a plan.

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
planned as the MVP deployment: raw data not retained, Fargate costs,
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

`test-fixtures/p90-kll.json` is the document `asap_planner` writes for one
p90 query with accuracy enforced; write it again when the document changes:

```bash
asap_planner --workload test-fixtures/p90-workload.json \
  --deployment-capabilities test-fixtures/kll-only-capabilities.json \
  --run-until stage3 --format json --out test-fixtures/p90-kll.json
```
