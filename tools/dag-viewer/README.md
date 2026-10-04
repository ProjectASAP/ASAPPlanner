# ASAP Pre/Post-ASAP DAG viewer

The viewer has two visualization modes, chosen with the header switch:
**Pre/Post-ASAP** (below) and **Stages** (see "Stages view"). Stages is
enabled, and opens by default, only when an `asap-stage-pipeline/v1`
document is loaded.

- Select one query to see that query's complete pre-ASAP and post-ASAP DAGs.
- Select multiple queries to see two workload-union DAGs: one pre-ASAP union
  and one post-ASAP union. Nodes with the same exporter-assigned workload
  identity are collapsed while query roots and ownership are retained.
- The **All** checkbox left of the query strip selects or deselects every
  query at once, and shows an indeterminate state while only some are
  selected. It changes the selection only; **Clear all** in the header is a
  different operation and discards the loaded workload itself.
- Drag anywhere on the canvas to pan, including on a lane's own background.
- Pre-ASAP nodes show only their original IR content.
- Post-ASAP nodes show their translated IR content and the explicit planner
  decision carried by that node.
- Click any edge to inspect its schema, source and target nodes, and how the
  target operation derives its output schema in the details panel.
- The details panel shows the selected workload's bound table/metric schemas
  and can be resized by dragging its left edge.
- A post-ASAP node whose winning decision carries a cost/benefit annotation
  shows a concise `▼NN%`/`▲NN%` badge next to its label; the sidebar and the
  workload-scope summary show the full baseline/selected/benefit breakdown,
  with units and provenance, wherever the export provides one — see "Cost/
  benefit annotations" below.

There are no separate Single, Compare, or Union modes.

## Stages view

The Stages view shows one planner run through the stages of
[planner layering](../../docs/design_docs/proposals/planner-layering.md),
as three lanes side by side:

1. **Logical**: the stage-0 logical DAG for the whole workload.
2. **Logical ASAP**: one stage-1 candidate. Pick it in the **Logical ASAP
   candidates** list above the canvas, which shows each candidate's label.
3. **Physical ASAP**: one stage-2 candidate. Each node shows its timing
   (`⏱ query time` or `⏱ ingestion time`; ingestion-time nodes also have a
   double border).

A DAG has one root per workload query. Each root is marked and labelled
`root of <query id>`.

Costs come only from Stage 3. When the document has a Stage 3 result, the
physical lane's nodes show their cost. The lane header shows the
candidate's total cost, unit, cost source, rank, and outcome. The
**Physical ASAP candidates** list ranks every candidate cheapest first and
shows each outcome:

- **✓ selected**: the plan Stage 3 chose.
- **valid, not selected**: a valid plan that costs more.
- **✗ invalid**: a plan that fails a requirement, for example a latency
  bound.

Rejected plans also show Stage 3's reason. Without a Stage 3 result, the
physical lane and the list show no costs.

Summary nodes show their configuration, for example
`summary: CmsWithHeap · depth 7 · heap 100 · width 272`, and whether they
keep one state per group (or series) or one shared Hydra state.

When the document has a `deployment` section, a **Deployment inputs** list
shows what the deployment supplied: the exact aggregates it can compute and
the sketches it can build with the estimates each can be read for,
whether it maintains state at ingestion time, its memory budget, whether it
keeps raw data (query-time plans pay for raw retention when it does not),
the cost model with its calibration constants, and the accuracy model.

Clicking a physical candidate shows it in lane 3, and shows the logical
candidate it implements in lane 2. That logical candidate is outlined in
the list and in the lane.

The **Workload queries** list shows each query and its requirements
(accuracy, latency, repeat interval) when the document gives them.

A document from a run that stopped early still loads. The lanes for the
stages it doesn't contain show **not produced**.

Click a node to see its payload, output schema, guarantee and coverage. A
root also shows its query's requirements. Physical nodes also show their
output timing and Stage 3 cost. Click an edge to see its intermediate
schema, and for physical edges, its data state.

To open the sample document, start the server (see "Interactive query
editor") and go to
<http://127.0.0.1:8000/?doc=examples/stage-pipeline.sample.json>. The `doc`
parameter takes any JSON file path served next to `index.html`. You can also
load a stage document with the file picker.

The sample, `examples/stage-pipeline.sample.json`, is hand-written. It
follows #509's Example 1 with both queries:

- Q1, `sum by (job) (rate(http_requests_total[1m]))`, exact;
- Q2, `topk by (job) (10, sum_over_time(http_requests_total[1m]))`,
  ε = 0.01, δ = 0.001, at most 100 ms.

It has four logical candidates:

- exact;
- Count-Min with a heap for Q2;
- Hydra for Q2;
- exact with one input shared by both queries.

It also has four physical candidates, all at query time. In the exact
candidates, Q2's top 10 runs as sort → limit per job. In the summary
candidates, it runs as build → estimate. The costs are illustrative, not
planner output.

### Document format

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
  }
}
```

DAGs use the serde JSON of `LogicalASAPDAG` and `PhysicalASAPDAG`:

- `roots` lists one root per workload query, in workload order. A logical
  root is `{"Operator": id}` or `{"Scalar": expr}`; a physical root is a node
  id. A single `root` is still accepted.
- `requirements`, and the stages after stage 0, are optional.
- Every stage-2 candidate must be either `selected` or listed in
  `rejected`.

The viewer checks the document before rendering it:

- node, edge and root references;
- one root per query;
- `from_logical`;
- physical `output_state.timing` and `data_state`;
- Stage 3 ids, costs and `per_node` keys;
- that a later stage never appears without the stage before it.

If any check fails, the viewer lists the problems and doesn't load the
document.

## Interactive query editor

From the repository root:

```sh
python3 tools/dag-viewer/server.py
```

Open <http://127.0.0.1:8000>, expand **Query editor**, add SQL or PromQL
queries, choose an epsilon, and click **Plan selected workload**. The backend
runs the real pipeline:

1. SQL/PromQL parsing and lowering
2. pre-ASAP DAG generation
3. ASAP-aware mapping
4. post-ASAP DAG generation

The editor includes built-in `metrics` and `hosts` table schemas. Enable the
schemas each SQL query uses with its checkboxes, or add a table with custom
columns using one readable `name TYPE [NOT NULL]` declaration per line and
an optional time-index column. The compact `name:type!` form remains accepted
for compatibility. These definitions are passed to `dag_export` and used by the SQL
schema_resolver; they are not display-only metadata. PromQL keeps its open metric
model and shows an inferred schema based on the labels and values used by the
query. The sidebar groups identical input schemas and lists every selected
query that uses each one.

The Python terminal streams those stages while they run. The server binds to
localhost by default and invokes `dag_export` with an argv array, not a
shell command.

## Export JSON directly

```sh
cargo run -p asap-devtools --bin dag_export -- \
  --post-asap --epsilon 0.01 \
  --planner-cost-json "$PLANNER_PHYSICAL_EVIDENCE" \
  --sql "SELECT service, COUNT(*) FROM metrics GROUP BY service" --name q1 \
  > /tmp/dag.json
```

Load the JSON with the page's file picker. `--planner-cost-json` is a complete
physical-evidence document: an immutable `evidence_version`, calibration, and
target records containing the exact target node (a serialized pre-ASAP
`OperatorNode`) and comparison scope.
Each exact replacement candidate owns its complete logical-node
`PhysicalNodeEvidence`; summary candidates additionally own their bound
`PhysicalDAG`. Candidate-local evidence prevents statistics for one physical
alternative from satisfying another. Candidate matching includes the complete
exported plan, including accuracy guarantees, and never uses a hash or strategy
name; derived floating constants allow only a one-ULP JSON round-trip tolerance.
Duplicate, conflicting, unused, or missing records fail closed. The old
`--analytical-cost-json` spelling accepts the new document as an alias; its old
compact aggregation payload is rejected with a migration error.

`--default-cost` is the alternative cost source for a workload with no
deployment to measure yet, and is mutually exclusive with
`--planner-cost-json`:

```sh
cargo run -p asap-devtools --bin dag_export -- \
  --post-asap --default-cost --epsilon 0.01 \
  --sql "SELECT service, COUNT(*) FROM metrics GROUP BY service" --name q1 \
  > /tmp/dag.json
```

It ranks candidates with the planner's structural `DefaultCostModel`, so the
structure of the export is real — which replacements the search found, which
one won per group, and the merged post-ASAP DAG — while no cost is exported
at all. Every `CostAnnotation` stays `Unavailable` with no `value` and renders
as **Not estimated**; the structural ranking number is never serialized. Use
it to see what ASAPPlanner does with a workload before there is a deployment
to calibrate against, and `--planner-cost-json` once there is.

Without either flag, `--post-asap` exports the raw DAG only.

## Standalone HTML

```sh
python3 tools/dag-viewer/render.py /tmp/dag.json -o /tmp/dag.html
```

The renderer embeds the workload and vendored JavaScript into one file. It
always opens in Pre/Post-ASAP mode; there is no `--mode` option.

## JSON contract

`NamedDAG.dag` is the original pre-ASAP DAG. `NamedDAG.post_dag`
is the complete translated DAG. Every post-ASAP node produced or carried by
a selected replacement directly contains:

```json
{
  "decision": {
    "id": 7,
    "strategy": "ASAPStrategies",
    "rationale": "count realizes as a Cms sketch",
    "rank": 0,
    "cost": 1.14001088,
    "role": "replacement_root",
    "baseline_cost": { "value": 104.0032, "unit": "CostUnits", "source": "Modeled", "model_version": "analytical-cost-v1+example-calibration-v1", "evidence_version": "example-evidence-v1" },
    "selected_cost": { "value": 1.14001088, "unit": "CostUnits", "source": "Modeled", "baseline": {"kind": "PreAsapRecomputation"}, "delta": 102.86318912, "benefit_ratio": 0.9890386941940248 },
    "benefit": { "value": 102.86318912, "unit": "CostUnits", "source": "Modeled", "baseline": {"kind": "PreAsapRecomputation"}, "benefit_ratio": 0.9890386941940248 }
  }
}
```

The viewer reads this explicit metadata. It never guesses a strategy or
workload-sharing identity from a node label, hash, or client-side signature.
The exporter assigns `workload_node_id`; union rendering reads that mapping
directly.

Node boxes use concrete IR fields: aggregate measures/grouping, sort keys,
filter predicates, projections, sources, summary families, and evaluation
queries. A node's `kind` is the operator variant name (`Operator::kind_name`):
a `NonASAPOp` such as `Aggregate` or `Values`, or an `ASAPOp` such as
`SummaryAgg` or `EvaluatePopulation`. `node-style.js` maps each kind to a color
category. Scalar expressions are not nodes; an operator a scalar expression
reads (`scalar(v)`, `EXISTS (subquery)`) is a child node, shown in `detail`
as `{"scalar_ref": <node id>}`. Schemas list their entries under `fields`. Category icons are deliberately omitted so they cannot be confused
with IR text.

### Cost/benefit annotations (issue #286)

`decision.baseline_cost` / `.selected_cost` / `.benefit` are structured
[`CostAnnotation`](../../crates/types/src/cost.rs)s: `value` + `unit` +
`source` (`Modeled` / `Measured` / `Unavailable`), optionally `baseline` +
`delta` + `benefit_ratio`, and `model_version`/`evidence_version`/
`benchmark_id`/`inputs` for provenance. `model_version` identifies the
analytical formulas and calibration; `evidence_version` independently
identifies the immutable catalog/runtime generation. A missing `value`
(`source: "Unavailable"`) always renders as
**Not estimated** — the viewer never fabricates a number. A complete physical
planner export keeps CPU operations, peak memory, scan bytes, coefficients,
and workload statistics in `inputs`. Without complete physical evidence, the
annotation is `Unavailable`; structural node counts are never substituted,
including under `--default-cost`, where they rank the candidates and are then
discarded.
See the [analytical model design](../../docs/design_docs/asap-aware-mapping/analytical-resource-cost.md).


The checked-in viewer fixture makes its illustrative comparison reproducible.
It models 100 evaluations of 100 million 64-byte rows with 100,000 groups.
The raw path charges one scan plus three hash/key/accumulator operations per
row, so CPU is `100 × 100,000,000 × 4 = 40 billion` operations; it reads
`100 × 6.4 GB = 640 GB` and retains
`100,000 × (8-byte key + 8-byte accumulator + 16-byte hash metadata) = 3.2 MB`.
One incrementally built depth-5 CMS charges
`100,000,000 × 5 = 500 million` counter updates, reads the 6.4 GB source once,
and retains `272 × 5 × 8 = 10,880` bytes. With coefficients `1e-9` per CPU
operation, `1e-10` per scan byte, and `1e-9` per peak-memory byte, the displayed totals are
`104.0032` and `1.14001088` cost units. These are explicit fixture assumptions,
not statistics inferred by the viewer.

The same three fields also appear on `TargetReplacement`
(replacement-region baseline/selected/benefit), `NamedDAG.workload_cost` /
`WorkloadDAG.workload_cost` (whole selected-workload cost/benefit, shared
decisions counted once via `decision.id` dedup). `ExportDAG.edge_annotations`
is reserved for a higher layer that has physical evidence for a particular
edge; DAG sharing alone never creates an edge cost. The sidebar shows the full breakdown
(value, unit, provenance, baseline, ratio, inputs) on node/edge click and in
the workload-scope summary; a post-ASAP node with a costed decision also
gets a concise on-DAG `▼NN%`/`▲NN%` badge next to its label.

All of this is additive and optional: an export with none of these fields
(anything produced before issue #286) renders exactly as before.

## Tests

```sh
python3 -m unittest discover -s tools/dag-viewer -p test_render.py
cargo test -p asap-devtools --bin dag_export
```

The JavaScript tests, including the Stages validation, lane, and ranking
tests, need `py_mini_racer` (`pip install py-mini-racer`). Without it they
are skipped.
