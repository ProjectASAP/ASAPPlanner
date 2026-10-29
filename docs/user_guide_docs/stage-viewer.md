# See how ASAPPlanner plans a workload: the Stage Viewer

The Stage Viewer is a web page that shows what ASAPPlanner does with one
workload at each
[planning stage](../design_docs/proposals/planner-layering.md#asapplanner-system-overview-planning-stages),
and which plan it selects. It reads one JSON document per workload, written
by `stage_pipeline`. It does not run the plans.

## Start it

From the repository root, with Rust/Cargo and Python 3 installed:

```sh
python3 tools/dag-viewer/server.py
```

The server builds `stage_pipeline` (a few minutes the first time), writes the
example documents and prints the address, `http://127.0.0.1:8000`. On a
remote machine, forward the port first: `ssh -L 8000:127.0.0.1:8000 <host>`.

| Option | Use it to |
| --- | --- |
| `--port 8765` | Serve on another port |
| `--skip-build` | Reuse an already-built `stage_pipeline` |
| `--regenerate` | Rewrite the example documents after a planner change |

## Look at an example

The tabs at the top open the examples. Each starts with a sentence saying
what the workload is. Start with:

- **1 · dashboard panels**: two dashboard panels; the all-exact
  plan wins.
- **3b · live p99 panel**: the whole-window KLL beats exact and
  the panes.
- **Ratio of quantiles (memo after each step)**: `avg by (job) (quantile_over_time(0.5, data[5m]) /
  quantile_over_time(0.5, data[5m]) + quantile_over_time(0.9, data[5m]))`.
  This one shows the memo: 7 groups, and 7 × 7 = 49 complete DAGs after
  Pass 1.

`#example3b` in the address opens a tab directly.

The costs in the examples rest on illustrative statistics. Read them as a
ranking, not as a prediction.

## Read the page

From top to bottom:

| Panel | What it shows |
| --- | --- |
| Query workload | Each query with its accuracy (`exact`, or `ε=…, δ=…`), latency bound and how often it runs. A line under it counts the candidates per stage. |
| Data workload | The facts given about each source (series count, sample interval, distinct values per label, …) and the deployment capabilities. |
| Stage 3 · plans by cost | The selected plan (`✓ selected`) and the plans it beat: valid but costlier, or `✗ invalid` with the reason. Click a plan to draw it in the lanes. |
| Stage 0 lane | The queries as lowered by the frontends. Data flows bottom-up. |
| Enumerated DAGs | The complete candidate DAGs the memo stands for, one choice of candidate per group. The header counts them after each step. Each row names the groups where it differs from the selected plan. Click a row to draw that DAG in the Stage 1 lane; the Stage 2 lane is then empty, since an enumerated DAG has no physical plan. |
| Stage 1 and Stage 2 lanes | The logical candidate, and the physical candidate with each node's timing and cost. The Stage 2 lane lists the logical candidate's physical candidates, one per materialization choice, each with its cost or why it is invalid; click one to draw it. |
| Materialized views | What a deployment would install for the selected plan, one row per Stage 2 node. |
| Memo | The memo after each step, one tab per step (Stage 0, Pass 1 and two Pass 2 tabs). Each box is a memo group, with its candidates inside. A double border with `✚ new` marks candidates that step added. On the last tab, the selected plan's candidates are green. A group with many candidates shows one count per origin; click a count to list them. |

The Enumerated DAGs and Memo panels appear only when the document has a
`memo` section.

In the lanes:

- blue nodes are data sources, grey are relational operators and purple are
  summary operators;
- a thick border marks a query's result, and `shared ×n` marks a node that n
  queries read;
- click a `PaneChain` node to unfold its panes, and click again to fold them.

To follow one query, pick it in **highlight query**: everything it does not
reach dims, in the lanes and in the Memo. Click a node to see its details;
click the background to go back. Drag to pan and scroll to zoom.

## Plan your own queries

**In the page:** click **Query editor**, enter SQL or PromQL queries (and
the tables SQL reads), set ε and the data facts, and click **Plan**. Or
click **Workload sets** and pick a row.

**From the command line:**

```sh
cargo run -p asap-devtools --bin stage_pipeline -- \
    --promql "topk by (job) (10, sum_over_time(http_requests_total[1m]))" \
    --series-count 1000 --max-error 0.01 --failure-probability 0.001 \
    --enforce-accuracy-guarantees --out plan.json
```

Then open `plan.json` with **Open stage document or workload…**, or drop it
on the page.

Useful options:

- `--enforce-accuracy-guarantees`: reject plans that miss a query's accuracy
  target. Without it, guarantees are ignored and plans are compared on cost
  alone. The Stage 3 header says which applied.
- `--price-enumerated <n>`: also cost the first n enumerated DAGs through
  Stage 2 and Stage 3. Each row then shows its cost or why it is invalid. A
  DAG cheaper than the selected plan turns red, which would point at a
  planner bug.
- `--no-memo`: leave the memo out, for a smaller document.

For more options and examples, see
[Plan a workload with stage_pipeline](stage-pipeline.md).

## When something goes wrong

| Symptom | What to do |
| --- | --- |
| `Could not load the planner output: …` | The document is from an older `stage_pipeline`; write it again. |
| `Planning failed` with a network error or `HTTP 501` | The page was opened without the server. Start `server.py` and open the address it prints. |
| The examples don't change after a planner change | Restart the server with `--regenerate`. |
| No Memo or Enumerated DAGs panel | The document has no `memo` section (it was written with `--no-memo`, or by an older `stage_pipeline`). |

For the document format and the viewer's code, see
[`tools/dag-viewer/README.md`](../../tools/dag-viewer/README.md).
