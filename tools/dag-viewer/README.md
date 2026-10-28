# ASAPPlanner Stage Viewer

A browser view of what the planner does with one workload, read from an
`asap-stage-pipeline/v1` document written by the `stage_pipeline` devtool.
`stage_pipeline` uses the candidate-connection MILP, the same search used by
`plan_installable`. By default the document keeps the selected plan and its
validation, cost, and search reports. `--exhaustive` uses the same engine to
archive evaluated assignments within the configured time and evaluation
budgets; only `catalog_exhausted` establishes complete traversal. The document
shows:

- **Stage 0**, the query-language frontends' DAG, with one root per query;
- **Stage 1**, local computation choices and reconstructed logical plans for
  evaluated candidate selections;
- **Stage 2**, executable physical plans, their materialization assignments,
  and any recorded physical-legality rejection;
- **Stage 3**, validation, configured objective values, the selected plan,
  and bounded-search status. A feasible selected plan need not be a proven optimum.

To run it, see [RUNNING.md](RUNNING.md).

## The page

- **Examples**: one tab per entry of `examples.json`: #509 Examples 1, 3a
  and 3b, workload-01 of each ASAPQuery random workload set (below), and
  six sharing examples, each one kind of sharing on the smallest subset
  of a paper workload template (below). `#example3b` in the URL opens that
  example.
- **Workload sets** (button): one table per set listed in
  `workloads/index.json`, one row per workload with its query count, query
  labels, the queries (text, how often, accuracy) and the sources they
  read. Clicking a row plans it with the local server (`/api/plan-workload`:
  `stage_pipeline --workload`, default search, the set's
  `deployment_capabilities`, `cost_model` and `accuracy_guarantees`, and
  for a SQL workload the tables in the `tables.json` next to its file).
  Without the server, a row
  with a precomputed `document` opens it and the others say they need the
  server. `#workload-sets` in the URL opens the section.
- **Query workload** with each query's accuracy, latency, recurrence,
  predictability and range evaluation, and a line that counts the
  candidates at each stage.
- **Data workload** beside it: the arrival and one row per declared source
  with exactly the facts `SourceFacts` has (series count, metric type,
  non-negative values, sample interval, stream norms, distinct bound,
  distinct values per label, distinct values per label set) and
  the queries whose text names it, plus the deployment capabilities the
  run planned with.
- **Stage 3 · plans by cost**: every candidate the document shows. First the
  selected plan, then the other valid ones cheapest first, then the
  rejected ones with their reasons (or "not judged" when plan selection
  failed with an error), then the Stage 2 pruned ones and the
  Stage 1 candidates that did not compose, each with its reason. The header
  says how many candidates Stage 1 enumerated, how many the document shows
  when it caps them, and why Stage 1 or plan selection failed when it did.
  Clicking a row shows it in the lanes. The header also names the search and
  what it checked, why the workload was searched as one when it was
  (`searched_as_one`, e.g. "searched as one: not in joint space") and the
  classes of interchangeable queries that took one shared choice
  (`interchangeable_queries`, e.g. "interchangeable queries: (p50, p90,
  p99)"); under the default search the list has the selected plan
  and the rejections Stage 3 recorded, which are not clickable.
- **Three lanes**, stacked top to bottom so each is as wide as the page:
  Stage 0, the chosen Stage 1 candidate, and its Stage 2 candidate
  with each node's timing (also its fill: green at ingestion time, orange
  at query time; the border keeps its category) and Stage 3 cost (`× n` when a node that is not
  materialized is computed once per reader). Each lane is the whole
  workload's DAG, so a node shared by several queries appears once. Nodes
  are data sources, relational operators or summary operators; summary
  builds print their configuration (for example `CmsWithHeap · depth 7 ·
  heap 100 · width 272`) and whether there is one per group, one per
  series, or one shared instance. A standalone scalar query (`time()`,
  `scalar(v)`, `1 + 2`) is a dashed `Scalar` node over the nodes it reads.
  Query roots have a thick border. A node that several queries' roots reach
  says `shared ×n`. A `PaneChain` is one node (`30 panes × 20 s`); clicking
  it draws its panes, newest first, each a `TimeShift` by its offset (none
  for the newest), a `TimeRange` of one pane and the pane's build with its
  window (`window t−40 s … t−20 s`), and clicking a pane folds them back.
  This is display only: the document keeps the chain.
- **Enumerated DAGs**, between the Stage 0 and Stage 1 lanes, shown only
  when the document has `memo.enumeration`: the complete candidate DAGs the
  memo stands for. The header counts them after each memo step (`Stage 0: 1 ·
  Pass 1: 49 · Pass 2: ≥ 1000 · concrete states: ≥ 1000`; `≥` and the limit when counting stopped), says how
  many of the enumerated step's DAGs the document lists, and, when they were
  priced, how many are cheaper than the selected plan. Each row names the
  groups whose candidate differs from the selected plan's choice
  (`G7 → #2 CountSketch+heap (pass1)`: group, candidate index, its label and
  origin when the step's memo lists the candidate; a group a choice does not
  name takes candidate 0). The selected choice is over the last memo step;
  when an earlier step was enumerated, the header says so and only each
  row's own groups are compared. A row also shows `budget share < 1` when
  a chosen candidate shares its budget, and, when priced, its cost and
  validity (`✗ invalid` with the cause). A row cheaper than the selected
  plan is red; the selected choice says `✓ selected`. Clicking a row draws
  its DAG in the Stage 1 lane; the document holds every listed choice's
  composed DAG, so the page composes nothing. Such a DAG has no Stage 2
  plan, so the Stage 2 lane is cleared. Its roots are its root groups, in
  increasing group id; the lane draws each as the root of the queries in its
  group's `roots_of`, or labels it `root group #n` when that cannot be
  matched (the enumerated step's memo is not in the document). When the enumerated step is earlier than the last
  one, no row is marked selected.
- **Highlighting**: the **highlight query** menu dims, in every lane, what
  that query's root does not reach. Clicking a node dims everything but
  what it reads and what reads it, directly or through other nodes; clicking
  the background goes back to the query's highlight.
- **Materialized views** under the Stage 2 lane: the selected plan's
  materialized views as `plan_installable` gives them, one row per Stage 2 node: its
  coverage definition, operator and summary parameters, panes (`30 × 20 s`
  for a pane chain), how it is refreshed, its retention, the Stage 3 cost and the
  queries that read it. A chain's row lists its installed panes (node id,
  pane, window, retention) under **installed**. Clicking a row selects its
  node in the Stage 2 lane. Stage 3 predicts no
  per-node state size, so the table has none. The lane's header also shows
  the shown candidate's Stage 3 usage.
- **Search summary**, under the materialized views: candidate-connection reports
  show the incumbent cost, available lower bound and gap, refinement count,
  and solver status. Historical documents may also contain a legacy memo-plan
  view of shared units. Rendering that archived format does not enable a second
  planner implementation; new documents use the candidate-connection report.
- **Memo**, when the document has a `memo` section: the memo after each
  step, one tab per step (`Stage 0`, `Stage 1, Pass 1`, `Stage 1, Pass 2`,
  concrete states; a step production did not run is not there), opening on
  the last. Each memo group is a box `G<id> · <label>` with its candidate
  count (`745 candidates (64 listed)` when the document lists only some) and
  the queries it is the root of; its implementation candidates sit inside
  it, bordered by origin (Pass1, SharedBuild, SummaryResult, SummaryInput,
  Panes, Rollup, TopKReuse, WidenedScans), with a double border and
  `✚ new` when the step added them. An edge goes from each candidate to
  each child group it reads (at Stage 0, which has no candidates, from the
  group). Readers sit above what they read. A group listing more than 12
  candidates is drawn as one node per origin with its count (`360 Panes`);
  clicking a count lists them, clicking the group folds them back. On the
  last tab the selected plan's candidates (`selected_choice`) are filled
  green with `✓ selected`, and so are the edges they read along. The header
  counts the step's groups and candidates and what it added over the
  previous step. Clicking a candidate shows its own nodes in the details,
  down to the nodes of the child groups it reads; clicking a group shows
  its node, its child and parent groups, queries and counts per origin.
  The **highlight query** menu also dims the groups that query does not
  reach. Documents without `memo` show no Memo panel.
- **Details**: click a node for its operator, the queries reaching it, timing, materialization,
  output schema, guarantee and Stage 3 cost; click an edge for its schema
  and data state.

Other documents open with **Open stage document or workload…**, by
dropping a file on the page, or with `?doc=<path>` for a file served next
to `index.html`. A `PlanningWorkload` JSON file (one with a
`query_workload`) is planned by the local server instead, with the default
deployment capabilities and cost model.

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
                             "query_labels": ["q01-sum-fine", "..."],
                             "document": "examples/random-mixed-seed-862-workload-01.json" }] }] }
```

`deployment_capabilities`, `cost_model` (`usage-over-time`,
`aws-fargate-us-east-1` or `provisioned-capacity`), `accuracy_guarantees`
(`ignored`, the default, or `enforced`: the server then passes
`--enforce-accuracy-guarantees`), `query_labels` (one per
query, in entry order) and `document` (a precomputed stage document) are
optional. A PlanningWorkload declares no SQL tables, so a set of SQL
workloads puts them in `workloads/<set>/tables.json`, a JSON array of
`stage_pipeline --table` declarations (`{"name": "flows", "time_index": 0,
"columns": [{"name": "ts", "type": "timestamp", "nullable": false}, ...]}`);
the page sends them with the workload (`/api/plan-workload`'s `tables`) and
the server passes one `--table` each. An `examples.json` entry planned from
a workload file reads the same file. A new set is a directory of files and
an index entry; no code changes. `crates/devtools/tests/stage_pipeline.rs` checks that every file
parses and passes `PlanningWorkload::validate`.

The three ASAPQuery sets (seed-862, seed-862-x5, seed-862-long; 10
workloads each, PromQL queries only) were converted from ASAPQuery's random
workload manifests with ε = 1 − the accuracy SLA and δ = 0.3, and are
planned as the MVP deployment: raw data not retained, Fargate prices,
accuracy guarantees enforced.

## Query editor

With the local server running, **Query editor** plans SQL and PromQL
queries with one ε for every query. SQL queries read the tables declared in
the editor (one column per line, `name TYPE [NOT NULL]`, and an optional
time column). The editor also declares the source facts plan
selection prices from (series, rows/s and rows held for every source; an
empty field declares nothing). The server runs `stage_pipeline` with
`--sql`, `--promql`, `--name`, `--table`, `--series-count`,
`--rows-per-second` and `--rows-held`, and the page shows the result, or
the error naming the source and the fact a plan needed but nobody
declared. Accuracy guarantees
are ignored, `stage_pipeline`'s default.

## Document format

`stage_pipeline` writes:

```json
{
  "format": "asap-stage-pipeline/v1",
  "search": "default",
  "accuracy_guarantees": "ignored",
  "workload": { "queries": [{ "id": "q1", "language": "promql", "text": "...",
                "requirements": { "accuracy": { "max_error": 0.0, "failure_probability": 0.0, "accuracy_scope": "whole_result" },
                                  "repeat_interval_ms": 10000 },
                "recurrence": { "repeated": { "fixed_interval": 10000 } },
                "predictability": { "predictable": { "known_at": null } } }],
                "data_workload": { "arrival": { "ingesting": { "rows_per_second": 200000.0 } },
                                   "sources": { "data": { "series_count": 10000, "metric_type": "gauge", "values_non_negative": true,
                                                          "sample_interval": 100, "stream_norms": null,
                                                          "max_distinct_per_evaluation": null } } } },
  "deployment_capabilities": { "supported_summaries": [{ "family": "Kll", "readouts": ["Quantile"] }, "..."], "raw_data_retained": true,
                               "query_time_results_retained": false },
  "stage0_query_language_frontends": { "dag": "<flat logical DAG>" },
  "stage1_logical_asap_optimization": {
    "combinations": 9, "capped": false,
    "pass1_pruned": [{ "target": 0, "query": "Q1",
                       "pruned": [{ "candidate": "DDSketch", "reason": "UnsupportedByDeployment" }],
                       "fused_pruned": [] }],
    "candidates": [{ "id": "L1", "display_name": "...", "dag": "<flat logical DAG>" },
                   { "id": "L2", "display_name": "...", "error": "..." }]
  },
  "stage2_physical_asap_optimization": {
    "candidates": [{ "id": "P1", "from_logical": "L1", "display_name": "...", "dag": "<PhysicalASAPDAG>" }],
    "pruned": [{ "from_logical": "L3", "display_name": "...", "reason": "..." }]
  },
  "stage3_plan_selection": {
    "costs": { "P1": { "cost_per_second": 7.9, "cpu_core_seconds_per_second": 0.0041,
                       "unit": "cost_per_second", "source": "...",
                       "usage": { "cpu_ops_per_second": 4100210.0, "scanned_bytes_per_second": 38400000.0,
                                  "average_memory_bytes": 0.0, "peak_memory_bytes": 16481088,
                                  "disk_bytes": 192000000, "peak_cpu_cores": 2,
                                  "latency_per_query": [{ "compaction_ms": 0.0, "query_ms": 33.2 }] },
                       "per_node": { "<node id>": { "operator": "Scan", "timing": "query_time",
                                                     "cpu_core_seconds_per_second": 0.0012,
                                                     "cost_per_second": 2.3, "multiplier": 1, "detail": "...",
                                                     "price_source": "analytical" } } } },
    "accepted": { "P1": [{ "target_metric": null, "accepted_metric": "rank", "sketches": ["Kll"],
                           "summary": "metric not set; accepted rank (Kll)" }] },
    "selected": "P1",
    "rejected": [{ "id": "P2", "cause": "costlier", "reason": "..." }],
    "search_rejected": [{ "id": "P5", "cause": "accuracy_target_missed", "reason": "..." }],
    "rejected_by_cause": { "raw_data_not_retained": 533, "costlier": 12 },
    "materialized_views": [{ "node_id": 2, "refresh": "at_ingestion",
                             "coverage": { "definition_id": "98f2b35a…", "selection": ["..."] },
                             "inputs": { "raw_data_windows": [{ "source": { "TimeSeries": { "metric": "data" } },
                                                                "end_offset_ms": 0, "width_ms": 20000 }],
                                         "materialized_views": [] },
                             "retention_ms": 600000,
                             "stage2_node_id": 1, "installed_position": 0, "installed_count": 30 }],
    "search": { "candidates_checked": 206, "components": [{ "queries": [0, 1], "candidates_checked": 204, "selected": "P61-m19" }],
                "queries": [{ "candidates_checked": 3, "discarded": [{ "choice": [[4, 0]], "cause": "no_accuracy_guarantee", "reason": "..." }] }],
                "searched_as_one": null, "raw_scan_assignments_skipped": 0, "raw_scan_candidates_skipped": 0,
                "runner_up_cost_per_second": null },
    "front": []
  }
}
```

- A flat logical DAG lists its nodes in id order; an operator names its
  inputs by node id. `roots` has one root per query: `{"Operator": id}`, or
  `{"Scalar": expr}` for a standalone scalar query.
- A `PhysicalASAPDAG` has node ids, an edge list, operator roots in `roots`,
  and standalone scalar queries in `scalar_roots` as `[query position,
  expr]`.
- A query's `requirements.accuracy` is its accuracy target's own JSON
  (`accuracy_metric` omitted when not set, `failure_probability` omitted without a
  δ). An approximate target without a metric also has
  `requirements.default_metric`, the metric the query's result is naturally
  stated in (null for a quantile), for display only.
- `stage3_plan_selection.accepted` has, per shown valid candidate and query,
  the metric the target set (null when not set) and the metric Stage 3
  accepted (null for an exact output).
- `costs` has, per shown valid candidate, its resource `usage` and its
  `cost_per_second` under the run's cost model (`--cost-model`). A
  candidate's `cost_per_second` is null under cost model 3, which has no
  prices; a node's is null under cost models 2 and 3, which price or limit
  the whole candidate's peaks. `front` lists the Pareto front under cost
  model 3's `pareto-front` objective, and is empty otherwise.
- `deployment_capabilities` repeats the run's `--deployment-capabilities`
  inputs with defaults filled in. `pass1_pruned` has, per Pass 1 target with
  a pruned candidate, the target's index, the query it is listed under, and
  each pruned candidate of the target (`pruned`) and of its fused
  computation (`fused_pruned`) with the reason. The page does not show
  either section yet.
- A node's `per_node` entry holds its `operator` (as the measured cost
  table names it), `timing` (`ingestion_time` or `query_time`), its
  predicted CPU in core-seconds per second (`cpu_core_seconds_per_second`),
  `cost_per_second`, `multiplier`, `detail` and `price_source` (`measured`
  when `--measured-operation-costs` priced it, else `analytical`), the
  names `plan_installable`'s predicted usage gives them. A candidate's
  `cpu_core_seconds_per_second` is its nodes' sum. These CPU fields are
  report-only: prices do not read them back.
- Each `stage3_plan_selection.rejected` entry has a `cause`: a failed check
  (`failure_probability_resize_failed`, `accuracy_target_missed`,
  `count_min_weights_not_proven_non_negative`,
  `per_entity_aggregate_not_executable`, `executor_cannot_compile`,
  `no_accuracy_guarantee`, `raw_data_not_retained`, `latency_bound_exceeded`,
  `latency_over_repeat_interval`, `over_cluster_capacity`) or a loss under
  the cost model (`costlier`, `slower`, `more_memory`,
  `not_lowest_latency_on_pareto_front`, `dominated`,
  `tied_lost_on_enumeration_order`).
- `accuracy_guarantees` is `ignored` (`stage_pipeline`'s default: Stage 3
  accepts every query output regardless of its guarantee and compares the
  candidates on cost alone; every accepted output has
  `accepted_regardless_of_guarantee: true`, and `guarantee_check_rejection`
  names the cause the guarantee check would have rejected it with, or is
  null when its guarantee met the target) or
  `enforced` (`--enforce-accuracy-guarantees`, as `plan_installable` plans
  with the default `PlanSelectionSettings`). The Stage 3 header shows it;
  an older document without it is still valid, any other value is not.
- `search` is `candidate-connections`, or `exhaustive` for traversal of the
  same finite catalog with evaluated plans and rejections archived. Exhaustive
  mode is not an independent reference optimizer; time and evaluation budgets
  still apply. `--max-combinations` bounds evaluated assignments.
- `stage3_plan_selection.search.candidate_connections` reports catalog size,
  objective, calibration scope, termination status, nullable lower bound/gap,
  generation/model/solver/validation timings, and refinement counts. A timeout
  incumbent is validated but does not certify unexplored candidates. Cluster
  objectives can have no scalar lower bound. Historical committed documents
  with old memo reports remain readable; those engines cannot be selected.
- `stage3_plan_selection.warnings` holds one-line warnings about the
  workload and the selected plan: a PromQL query applying `rate`,
  `increase` or `irate` to a metric the data workload declares a gauge
  (prefixed with the query id; the query still plans as written), and the
  plan's peak memory when over 10^12 bytes and the cost model declares no
  memory capacity; absent when there are none. The candidate list's header
  shows them.
- `search_rejected` lists the rejections that name no candidate the
  document holds: under the default search, the first rejection of each
  cause (their candidates are not kept), and the search's own (e.g. `q2`
  when every own candidate of that query was discarded). The page lists
  them after the other rows, not clickable.
- `workload.data_workload` is the `DataWorkload` the run planned with
  (`arrival`, and per source exactly the `SourceFacts` fields). Labels,
  label hierarchies and per-label cardinalities are not declarable there,
  so the page shows none. Each query's `recurrence` and `predictability` are
  its workload entry's own JSON.
- `materialized_views` are the selected plan's materialized views from
  `installable_plan_of` (what `plan_installable` returns): the node id in
  the installable DAG (pane chains expanded into panes), `refresh`
  (`at_ingestion` or `on_first_read`), `coverage`, `inputs` and
  `retention_ms`. `stage2_node_id` is the node of the selected Stage 2
  candidate it is installed from (null when Stage 3 recorded no cost to map
  it by), `installed_position` its position among that node's installed
  nodes (a pane chain's panes, newest first) and `installed_count` their
  number. Empty without a selection; `materialized_views_error` says why when
  the plan is not installable.
- `rejected_by_cause` counts the rejections by cause: every candidate's
  under `--exhaustive`, the recorded ones under the default search.
- Under `--exhaustive` the document shows at most `--max-displayed`
  physical candidates: the selected one, the other valid ones cheapest
  first, then the rejected ones taking each cause in turn, and the Stage 1
  candidates they come from.
- When Pass 1 fails, Stage 1 has `error` and no candidates. When selection
  fails, `selected` is null, with the rejections or with `error`.
- `memo` (left out with `--no-memo`) is the memo the search built, from
  the planning round whose plan was selected: `steps`, one per memo step
  (`stage0`, `pass1`, `pass2`: local window composition, `concrete_states`:
  the Pass 2 memo rules and the concrete summary-state groups), each with
  its groups (`id`, `label`, `children`, `parents`, `queries`, `roots_of`,
  `dag`) and their candidates (`candidate_count`,
  `candidate_count_by_origin`, `candidates_capped`, and the first
  `--max-memo-candidates`, default 64, under `candidates`: `index`,
  `implements`, `children`, `origin`, `budget_share`, `added`, `label`,
  `dag`, `group_of_node`); `selected_choice`, the selected plan's
  candidate index per group of the last step (null with
  `selected_choice_unavailable` when it has none); and `enumeration`: each
  step's number of candidate DAGs up to `--memo-enumeration-limit` (default
  1000) under `per_step`, and the first `--max-enumerated` (default 200)
  `choices` of `step` (the last step enumerated completely within that
  many, else the last step), each composed into its `dag`. The page does
  not draw it yet. `examples/memo-ratio-of-quantiles.json` has one.

The viewer checks the document before showing it: one root per query, node,
edge and root references, `scalar_roots` positions, `from_logical`, the Stage 3 ids, costs and
`per_node` keys, the data workload's shape, and each materialized view's fields
and `stage2_node_id`, and, when present, `memo.enumeration`: per-step
counts, the listed choices' ids, group → candidate maps, DAGs (one root per
root group) and the optional pricing fields. Documents written before these
sections are still shown. If a check fails, it shows the problems instead.

## Files

- `index.html`, `app.js`: the page.
- `stages.js`: document checks, the candidate list, lane elements and node
  labels, pane-chain expansion, the queries reaching each node and the
  highlighted neighborhoods; no DOM. `stages.test.mjs`: its Node tests.
- `enumeration.js`: the Enumerated DAGs panel and the checks of
  `memo.enumeration`. `enumeration.test.mjs`: its Node tests, over
  `test-fixtures/memo-enumeration-small.json` (a hand-written priced memo
  for Example 1).
- `memo.js`: the Memo view: checks of `memo.steps` and
  `memo.selected_choice`, the per-step diff, a step's elements with preset
  positions, a candidate's own nodes (DOM-free), and the panel that draws
  them. `memo.test.mjs`: its Node tests, over the hand-written
  `test-fixtures/memo-small.json` (a three-step memo over Example 1's two
  queries) and the committed planner memo example when present.
- `node-style.js`: the operator-kind table (`KIND_CATEGORY_JSON`, checked
  against the IR by `crates/devtools/tests/viewer_contract.rs`) and the
  node groups.
- `editor.js`: the query editor.
- `workload-sets.js`: the workload-set browser.
- `workloads/`: the workload sets (`index.json` and the `PlanningWorkload`
  files).
- `server.py`: serves the page, writes missing example documents into
  `out/` (git-ignored), and plans editor queries.
- `examples.json`: the example tabs. A built-in example (`example`) is
  written into `out/`; a workload example (`workload`, with its
  `deployment_capabilities` and `cost_model`) is committed under
  `examples/`. The committed documents are
  `examples/planner-layering-example1.json` and workload-01 of each
  ASAPQuery set, the six sharing examples and the memo example. Historical snapshots retain
  their original plans; `crates/devtools/tests/stage_pipeline.rs` checks input
  consistency and newly generated plan validity and materialized-view linkage.
  Timed searches are not required to reproduce snapshot IDs or prove optimality.
  `server.py --regenerate` rewrites the snapshots.
  An entry's `accuracy_guarantees` (`ignored` when absent) says whether it
  is planned with `--enforce-accuracy-guarantees`; every current entry
  enforces, since each story is about which plans meet their accuracy
  target, and a workload example matches its set.
- `memo-examples/`: the sharing examples' `PlanningWorkload` files
  (`promql/`, and `sql/` with its `tables.json`). Each is the smallest
  subset of an ASAPQuery paper workload template that shows one kind of
  sharing, with the template's data facts (W-RangeHeavy: `data`, 10^4
  series sampled every 5 ms, 2 × 10^6 rows/s; W-RollupHeavy: `http`, 10^4
  series, every 5 ms, at most 10^6 distinct values per evaluation), raw
  data retained, AWS Fargate prices and accuracy guarantees enforced. Their
  `examples.json` entries name `"search": "candidate-connections"`, which server.py
  passes as `--search`; their documents are committed under `examples/`.
  `ratio-of-quantiles-5m.json` (one ratio of two equal per-series quantiles
  plus a third, every minute, ε = 0.01, δ = 0.001) shows the `memo`
  section: the memo after each step.
- `cytoscape.min.js`, `dagre.min.js`, `cytoscape-dagre.js`: vendored, so the
  page works offline.

## Tests

From `tools/dag-viewer`:

```bash
python3 -m unittest test_viewer test_server
```

`stages.js` and `memo.js` also have Node tests, which load the elements into the vendored
cytoscape (headless), from the repository root:

```bash
node --test tools/dag-viewer/stages.test.mjs tools/dag-viewer/enumeration.test.mjs tools/dag-viewer/memo.test.mjs
```

The JavaScript runs in V8 through `py_mini_racer` (`pip install
py-mini-racer==0.6.0`) against a stub DOM, so no browser is needed; without
it those tests are skipped. The scalar-query and `/api/plan` tests need a
built `stage_pipeline` (`cargo build -p asap-devtools --bin
stage_pipeline`). The Rust side:

```bash
cargo test -p asap-devtools --test viewer_contract --test stage_pipeline
```
