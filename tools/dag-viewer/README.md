# ASAPPlanner Stage Viewer

A browser view of what the planner does with one workload, read from an
`asap-stage-pipeline/v1` document written by the `stage_pipeline` devtool:

- **Stage 0**, the DAG the query-language frontends lower the queries into,
  one root per query;
- **Stage 1**, the logical ASAP candidates: Pass 1's per-computation
  choices, each followed by its Pass 2 identical-expression candidates;
- **Stage 2**, the physical candidate of each composed Stage 1 candidate,
  or the reason Stage 2 pruned it;
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
                "requirements": { "accuracy": "exact", "repeat_interval_ms": 10000 } }] },
  "stage0_query_language_frontends": { "dag": "<flat logical DAG>" },
  "stage1_logical_asap_optimization": {
    "combinations": 9, "capped": false,
    "candidates": [{ "id": "L1", "display_name": "...", "dag": "<flat logical DAG>" },
                   { "id": "L2", "display_name": "...", "error": "..." }]
  },
  "stage2_physical_asap_optimization": {
    "candidates": [{ "id": "P1", "from_logical": "L1", "display_name": "...", "dag": "<PhysicalASAPDAG>" }],
    "pruned": [{ "from_logical": "L3", "display_name": "...", "reason": "..." }]
  },
  "stage3_plan_selection": {
    "costs": { "P1": { "total": 7.9, "unit": "cost_per_second", "source": "...",
                       "per_node": { "<node id>": { "cost": 2.3, "multiplier": 1, "detail": "..." } } } },
    "selected": "P1",
    "rejected": [{ "id": "P2", "valid": true, "reason": "..." }]
  }
}
```

- A flat logical DAG lists its nodes in id order; an operator names its
  inputs by node id. `roots` has one root per query: `{"Operator": id}`, or
  `{"Scalar": expr}` for a standalone scalar query.
- A `PhysicalASAPDAG` has node ids, an edge list, operator roots in `roots`,
  and standalone scalar queries in `scalar_roots` as `[query position,
  expr]`.
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
