# ASAP Pre/Post-ASAP DAG viewer

The viewer has one visualization mode: **Pre/Post-ASAP**.

- Select one query to see that query's complete pre-ASAP and post-ASAP DAGs.
- Select multiple queries to see two workload-union DAGs: one pre-ASAP union
  and one post-ASAP union. Nodes with the same exporter-assigned workload
  identity are collapsed while query roots and ownership are retained.
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
binder; they are not display-only metadata. PromQL keeps its open metric
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
  --analytical-cost-json '{"inputs":{"data_arrival":"at_rest","input_rows":1000000,"input_bytes":64000000,"source_scan_bytes":64000000,"group_count":1,"group_key_bytes":16,"topk_k":null,"evaluation_count":100},"calibration":{"cost_per_cpu_op":0.000001,"cost_per_scan_byte":0.00000001,"cost_per_retained_byte":0.000000001,"version":"demo-calibration-v1"}}' \
  --sql "SELECT service, COUNT(*) FROM metrics GROUP BY service" --name q1 \
  > /tmp/dag.json
```

Load the JSON with the page's file picker. A post-ASAP visualization requires
`--post-asap`; ordinary exports intentionally omit `post_graph`.

The viewer also accepts the JSON produced by
`export_summary_maintenance_plan`. It renders the materialized summary DAG as
a single lifecycle-plan lane. Selecting a `SummaryAgg` shows the chosen
lifecycle and maintenance mode together with every alternative's cost,
assumptions, and rejection reason. The selected-node panel also shows the
plan-level summary-versus-raw decision, costs, horizon, expected reads, and
evaluation/update rates. Raw-recomputation plans retain that decision summary
even though they have no deployed `SummaryAgg` to annotate.

## Standalone HTML

```sh
python3 tools/dag-viewer/render.py /tmp/dag.json -o /tmp/dag.html
```

The renderer embeds the workload and vendored JavaScript into one file. It
always opens in Pre/Post-ASAP mode; there is no `--mode` option.

## JSON contract

`NamedGraph.graph` is the original pre-ASAP DAG. `NamedGraph.post_graph`
is the complete translated DAG. Every post-ASAP node produced or carried by
a selected replacement directly contains:

```json
{
  "decision": {
    "id": 7,
    "strategy": "SketchAlgorithmStrategy",
    "rationale": "count realizes as a Cms sketch",
    "rank": 0,
    "cost": 5.64051088,
    "role": "replacement_root",
    "baseline_cost": { "value": 164.000000016, "unit": "CostUnits", "source": "Modeled", "model_version": "analytical-resource-v1+demo-calibration-v1" },
    "selected_cost": { "value": 5.64051088, "unit": "CostUnits", "source": "Modeled", "baseline": {"kind": "PreAsapRecomputation"}, "delta": 158.359489136, "benefit_ratio": 0.965606640979 },
    "benefit": { "value": 158.359489136, "unit": "CostUnits", "source": "Modeled", "baseline": {"kind": "PreAsapRecomputation"}, "benefit_ratio": 0.965606640979 }
  }
}
```

The viewer reads this explicit metadata. It never guesses a strategy or
workload-sharing identity from a node label, hash, or client-side signature.
The exporter assigns `workload_node_id`; union rendering reads that mapping
directly.

Node boxes use concrete IR fields: aggregate measures/grouping, sort keys,
filter predicates, projections, sources, summary families, and readout
queries. Category icons are deliberately omitted so they cannot be confused
with IR text.

### Cost/benefit annotations (issue #286)

`decision.baseline_cost` / `.selected_cost` / `.benefit` are structured
[`CostAnnotation`](../../crates/types/src/cost.rs)s: `value` + `unit` +
`source` (`Modeled` / `Measured` / `Unavailable`), optionally `baseline` +
`delta` + `benefit_ratio`, and `model_version`/`benchmark_id`/`inputs` for
provenance. A missing `value` (`source: "Unavailable"`) always renders as
**Not estimated** — the viewer never fabricates a number. With
`--analytical-cost-json`, `dag_export` emits calibrated `CostUnits` and keeps
the CPU operations, peak memory, scan bytes, coefficients, and workload
statistics in `inputs`. Without those inputs, or for an unsupported candidate,
the annotation is `Unavailable`; structural node counts are never substituted.
See the [analytical model design](../../docs/design_docs/asap-aware-mapping/analytical-resource-cost.md).

The same three fields also appear on `TargetReplacement`
(replacement-region baseline/selected/benefit), `NamedGraph.workload_cost` /
`WorkloadGraph.workload_cost` (whole selected-workload cost/benefit, shared
nodes counted once via `decision.id` dedup), and `DagGraph.edge_annotations`
(materialization/read cost on an edge into a genuine DAG merge point — never
a guessed multi-hop path cost). The sidebar shows the full breakdown
(value, unit, provenance, baseline, ratio, inputs) on node/edge click and in
the workload-scope summary; a post-ASAP node with a costed decision also
gets a concise on-graph `▼NN%`/`▲NN%` badge next to its label.

All of this is additive and optional: an export with none of these fields
(anything produced before issue #286) renders exactly as before.

## Tests

```sh
python3 -m unittest discover -s tools/dag-viewer -p test_render.py
cargo test -p asap-devtools --bin dag_export
```
