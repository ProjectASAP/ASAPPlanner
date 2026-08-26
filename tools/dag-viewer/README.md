# ASAP Pre/Post-ASAP DAG viewer

The viewer has one visualization mode: **Pre/Post-ASAP**.

- Select one query to see that query's complete pre-ASAP and post-ASAP DAGs.
- Select multiple queries to see two workload-union DAGs: one pre-ASAP union
  and one post-ASAP union. Structurally identical nodes and edges are
  collapsed while query roots and ownership are retained.
- Pre-ASAP nodes show only their original IR content.
- Post-ASAP nodes show their translated IR content and the explicit planner
  decision carried by that node.
- **Show edge schemas** optionally labels each data-flow edge with the source
  node's output columns and types. It is off by default to keep large DAGs
  readable.

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

The Python terminal streams those stages while they run. The server binds to
localhost by default and invokes `dag_export` with an argv array, not a
shell command.

## Export JSON directly

```sh
cargo run -p asap-devtools --bin dag_export -- \
  --post-asap --epsilon 0.01 \
  --sql "SELECT service, COUNT(*) FROM metrics GROUP BY service" --name q1 \
  > /tmp/dag.json
```

Load the JSON with the page's file picker. A post-ASAP visualization requires
`--post-asap`; ordinary exports intentionally omit `post_graph`.

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
    "cost": 3.0,
    "role": "replacement_root"
  }
}
```

The viewer reads this explicit metadata. It never guesses a strategy from a
node label, hash, or client-side signature. Union signatures are used only to
collapse structurally identical visualization nodes; they do not infer
planner decisions.

Node boxes use concrete IR fields: aggregate measures/grouping, sort keys,
filter predicates, projections, sources, summary families, and readout
queries. Category icons are deliberately omitted so they cannot be confused
with IR text.

## Tests

```sh
python3 -m unittest discover -s tools/dag-viewer -p test_render.py
cargo test -p asap-devtools --bin dag_export
```
