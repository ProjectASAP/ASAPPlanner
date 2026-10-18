# ASAPPlanner Stage Viewer

A browser view of what the planner does with one workload, read from an
`asap-stage-pipeline/v1` document written by the `stage_pipeline` devtool:

- **Stage 0**, the DAG the query-language frontends lower the queries into,
  one root per query;
- **Stage 1**, the logical ASAP candidates: Pass 1's per-computation
  choices, each followed by its Pass 2 identical-expression candidates;
- **Stage 2**, the physical candidates of each composed Stage 1 candidate,
  one per materialization assignment (`P<n>`, then `P<n>-m<k>`), or the
  reason Stage 2 pruned it;
- **Stage 3**, which checks every physical candidate, prices the valid ones
  per second and selects the cheapest.

To run it, see [RUNNING.md](RUNNING.md).

## The page

- **Examples**: one tab per entry of `examples.json` (#509 Examples 1, 3a
  and 3b). `#example3b` in the URL opens that example.
- **Workload queries** with their accuracy, latency and recurrence, and a
  line that counts the candidates at each stage.
- **Stage 3 · plans by cost**: every candidate the document shows. First the
  selected plan, then the other valid ones cheapest first, then the
  rejected ones with their reasons (or "not judged" when plan selection
  failed with an error), then the Stage 2 pruned ones and the
  Stage 1 candidates that did not compose, each with its reason. The header
  says how many candidates Stage 1 enumerated, how many the document shows
  when it caps them, and why Stage 1 or plan selection failed when it did.
  Clicking a row shows it in the lanes.
- **Three lanes**, stacked top to bottom so each is as wide as the page:
  Stage 0, the chosen Stage 1 candidate, and its Stage 2 candidate
  with each node's timing and Stage 3 cost (`× n` when a node that is not
  materialized is computed once per reader). Each lane is the whole
  workload's DAG, so a node shared by several queries appears once. Nodes
  are data sources, relational operators or summary operators; summary
  builds print their configuration (for example `CmsWithHeap · depth 7 ·
  heap 100 · width 272`) and whether there is one per group, one per
  series, or one shared instance. A standalone scalar query (`time()`,
  `scalar(v)`, `1 + 2`) is a dashed `Scalar` node over the nodes it reads.
  Query roots have a thick border.
- **Details**: click a node for its operator, timing, materialization,
  output schema, guarantee and Stage 3 cost; click an edge for its schema
  and data state.

Other documents open with **Open stage document…**, by dropping a file on
the page, or with `?doc=<path>` for a file served next to `index.html`.

## Query editor

With the local server running, **Query editor** plans SQL and PromQL
queries with one ε for every query. SQL queries read the tables declared in
the editor (one column per line, `name TYPE [NOT NULL]`, and an optional
time column). The server runs `stage_pipeline` with `--sql`, `--promql`,
`--name` and `--table` and the page shows the result.

## Document format

`stage_pipeline` writes:

```json
{
  "format": "asap-stage-pipeline/v1",
  "workload": { "queries": [{ "id": "q1", "language": "promql", "text": "...",
                "requirements": { "accuracy": { "max_error": 0.0, "failure_probability": 0.0, "accuracy_scope": "whole_result" },
                                  "repeat_interval_ms": 10000 } }] },
  "deployment_capabilities": { "supported_summaries": null, "raw_data_retained": true,
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
    "costs": { "P1": { "cost_per_second": 7.9, "unit": "cost_per_second", "source": "...",
                       "usage": { "cpu_ops_per_second": 4100210.0, "scanned_bytes_per_second": 38400000.0,
                                  "average_memory_bytes": 0.0, "peak_memory_bytes": 16481088,
                                  "disk_bytes": 192000000, "peak_cpu_cores": 2,
                                  "latency_per_query": [{ "compaction_ms": 0.0, "query_ms": 33.2 }] },
                       "per_node": { "<node id>": { "cost_per_second": 2.3, "multiplier": 1, "detail": "...",
                                                     "price_source": "analytical" } } } },
    "accepted": { "P1": [{ "target_metric": null, "accepted_metric": "rank", "sketches": ["Kll"],
                           "summary": "metric not set; accepted rank (Kll)" }] },
    "selected": "P1",
    "rejected": [{ "id": "P2", "cause": "costlier", "reason": "..." }],
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
- A node's `per_node` entry holds `cost_per_second`, `multiplier`,
  `detail` and `price_source` (`measured` when `--measured-operation-costs`
  priced it, else `analytical`).
- Each `stage3_plan_selection.rejected` entry has a `cause`: a failed check
  (`failure_probability_resize_failed`, `accuracy_target_missed`,
  `raw_data_not_retained`, `latency_bound_exceeded`, `over_cluster_capacity`)
  or a loss under the cost model (`costlier`, `slower`, `more_memory`,
  `not_lowest_latency_on_pareto_front`, `dominated`,
  `tied_lost_on_enumeration_order`).
- When Pass 1 fails, Stage 1 has `error` and no candidates. When selection
  fails, `selected` is null, with the rejections or with `error`.

The viewer checks the document before showing it: one root per query, node,
edge and root references, `scalar_roots` positions, `from_logical`, and the Stage 3 ids, costs and
`per_node` keys. If a check fails, it shows the problems instead.

## Files

- `index.html`, `app.js`: the page.
- `stages.js`: document checks, the candidate list, lane elements and node
  labels; no DOM.
- `node-style.js`: the operator-kind table (`KIND_CATEGORY_JSON`, checked
  against the IR by `crates/devtools/tests/viewer_contract.rs`) and the
  node groups.
- `editor.js`: the query editor.
- `server.py`: serves the page, writes missing example documents into
  `out/` (git-ignored), and plans editor queries.
- `examples.json`: the example tabs. `examples/planner-layering-example1.json`
  is the one committed document: `crates/devtools/tests/stage_pipeline.rs`
  checks that it is current.
- `cytoscape.min.js`, `dagre.min.js`, `cytoscape-dagre.js`: vendored, so the
  page works offline.

## Tests

From `tools/dag-viewer`:

```bash
python3 -m unittest test_viewer test_server
```

The JavaScript runs in V8 through `py_mini_racer` (`pip install
py-mini-racer==0.6.0`) against a stub DOM, so no browser is needed; without
it those tests are skipped. The scalar-query and `/api/plan` tests need a
built `stage_pipeline` (`cargo build -p asap-devtools --bin
stage_pipeline`). The Rust side:

```bash
cargo test -p asap-devtools --test viewer_contract --test stage_pipeline
```
