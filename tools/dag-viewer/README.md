# ASAPPlanner Stage Viewer

A browser view of what the planner does with one workload, read from an
`asap-stage-pipeline/v1` document written by the `stage_pipeline` devtool
(#509):

- **Stage 0**, the logical DAG the frontends lower the queries into, one root
  per query;
- **Stage 1**, the logical ASAP candidates from Pass 1's local alternatives and
  Pass 2's sharing rules;
- **Stage 2**, the physical candidates of each, which differ in what runs at
  ingestion time;
- **Stage 3**, which checks accuracy, latency and the deployment's
  capabilities, prices every valid plan per second, and selects the cheapest.

To run it, see [RUNNING.md](RUNNING.md).

## The page

- **Examples**: one tab per entry of `examples.json`, the six #509 examples
  (1, 2, 3a, 3b, 4a, 4b). Each says what the workload is and why its plan
  wins. `#example4b` in the URL opens that example.
- **Workload queries** with their accuracy, latency and recurrence.
- **Deployment inputs**: the exact aggregates the executor computes, each
  sketch with the estimates it can be read for, whether it maintains state
  at ingestion time, its memory budget, whether it keeps raw data (when it
  does not, query-time plans pay to keep the samples they read), the cost
  model with its calibration constants, and the accuracy model.
- **Stage 3 · plans by cost**: every plan, cheapest first, marked selected,
  valid but costlier, or invalid with the reason. Clicking one shows it in
  the lanes. When a document carries only the cheapest plans, the list says
  how many of how many.
- **Three lanes**: Stage 0, the chosen Stage 1 candidate, and the chosen
  Stage 2 candidate with each node's timing and Stage 3 cost. Nodes are data
  sources, relational operators, or summary operators; summary builds print
  their configuration (for example `CmsWithHeap · depth 7 · heap 100 · width
  272`) and whether there is one per group, one per series, or one shared
  instance. Query roots have a thick border.
- **Details**: click a node for its operator, output schema, coverage,
  guarantee and cost; click an edge for its schema and data state.

Other documents open with **Open stage document…**, by dropping a file on
the page, or with `?doc=<path>` for a file served next to `index.html`.

## Query editor

With the local server running, **Query editor** plans PromQL or SQL queries
(one per line, one language per run) with an optional ε and δ for every query.
PromQL also takes the sample interval. SQL queries read the tables declared
in the tables box, one JSON object per line:

```json
{"name": "flows", "columns": [{"name": "ts", "type": "timestamp", "nullable": false},
  {"name": "src_ip", "type": "utf8", "nullable": false}], "time_index": 0}
```

Column types are `timestamp`, `utf8`/`string`, `float64`/`double` and
`int64`/`bigint`; `nullable` defaults to true and `time_index` (the time
column's position) is optional. The server runs `stage_pipeline --promql …`
or `stage_pipeline --table <json> … --sql …` and the page shows the result.

## Document format

```json
{
  "format": "asap-stage-pipeline/v1",
  "workload": { "queries": [{ "id": "Q1", "language": "promql", "text": "...",
                "requirements": { "accuracy": "exact", "latency_ms": 100, "repeat_interval_ms": 10000 } }] },
  "stage0_logical": { "dag": "<LogicalASAPDAG>" },
  "stage1_logical_asap": { "candidates": [{ "id": "L1", "label": "...", "dag": "<LogicalASAPDAG>" }] },
  "stage2_physical_asap": { "candidates": [{ "id": "P1", "from_logical": "L1", "label": "...", "dag": "<PhysicalASAPDAG>" }] },
  "stage3_selection": {
    "costs": { "P1": { "total": 12.5, "unit": "cpu_ms_per_s", "source": "analytical-cost-v1",
                       "per_node": { "<node id>": { "cost": 3.2, "detail": "..." } } } },
    "selected": "P1",
    "rejected": [{ "id": "P2", "valid": true, "reason": "costlier" }]
  },
  "deployment": { "capabilities": { "...": "..." }, "cost_model": { "...": "..." }, "accuracy_model": { "...": "..." } },
  "shown_of": { "logical": 486, "physical": 486, "priced": 486 }
}
```

DAGs use the serde JSON of `LogicalASAPDAG` and `PhysicalASAPDAG`:

- `roots` lists one root per workload query, in workload order. A logical
  root is `{"Operator": id}` or `{"Scalar": expr}`; a physical root is a node
  id. A single `root` is still accepted.
- `requirements`, and the stages after stage 0, are optional.
- Every stage-2 candidate must be either `selected` or listed in
  `rejected`.
- `deployment` (the deployment inputs Stage 3 used) and `shown_of` (present
  when the document carries only the cheapest plans) are optional.

The viewer checks the document before rendering it:

- node, edge and root references;
- one root per query;
- `from_logical`;
- physical `output_state.timing` and `data_state`;
- Stage 3 ids, costs and `per_node` keys;
- that a later stage never appears without the stage before it.

If any check fails, the viewer lists the problems and doesn't load the
document.

## Files

- `index.html`, `app.js`: the page.
- `stages.js`: document validation, ranking, lane elements and labels.
- `node-style.js`: the operator-kind table (`KIND_CATEGORY_JSON`, checked
  against the IR by `crates/devtools/tests/viewer_contract.rs`) and the
  three node groups.
- `editor.js`: the query editor.
- `server.py`: serves the page, writes missing example documents into
  `out/`, and plans editor queries.
- `examples.json`: the example tabs; `examples/`: the hand-written sample
  and the Example 1 fixture `crates/devtools/tests/stage_pipeline.rs`
  compares against.
- `cytoscape.min.js`, `dagre.min.js`, `cytoscape-dagre.js`: vendored, so the
  page works offline.

## Tests

From `tools/dag-viewer`:

```bash
python3 -m unittest test_viewer
```

The JavaScript runs in V8 through `py_mini_racer` (`pip install
py-mini-racer==0.6.0`), against a stub DOM, so no browser is needed; without
it those tests are skipped.
