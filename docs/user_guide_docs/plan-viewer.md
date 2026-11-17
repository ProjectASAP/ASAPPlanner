# See how ASAPPlanner plans a workload: the Plan Viewer

The Plan Viewer is a web page that shows what ASAPPlanner does with one
workload at each
[planning stage](../design_docs/proposals/planner-layering.md#asapplanner-system-overview-planning-stages),
and which plan it selects. It reads one JSON document per workload, written
by `asap_planner --run-until stage3 --format json`. It does not run the
plans.

## Start it

From the repository root, with Rust/Cargo and Python 3 installed:

```sh
python3 tools/dag-viewer/server.py
```

The server builds `asap_planner` (a few minutes the first time), writes the
example documents into `tools/dag-viewer/out/` and prints the address, `http://127.0.0.1:8000`. On a
remote machine, forward the port first: `ssh -L 8000:127.0.0.1:8000 <host>`.

| Option | Use it to |
| --- | --- |
| `--port 8765` | Serve on another port |
| `--skip-build` | Reuse an already-built `asap_planner` |
| `--regenerate` | Rewrite the example documents after a planner change |

## Look at an example

The tabs at the top open the examples. Each starts with a sentence saying
what the workload is. Start with:

- **1 · dashboard panels**: two dashboard panels; the all-exact
  plan wins.
- **3a · historical p99 batch**: five p99 reports over 1 to 5 years, run
  once.
- **3b · live p99 panel**: the whole-window KLL beats exact and
  the tumbling windows.

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
| Stage 1 and Stage 2 lanes | The logical candidate, and the physical candidate with each node's timing and cost. |
| Materialized views | What a deployment would install for the selected plan, one row per Stage 2 node. |
| Memo | The memo after each step, one tab per step (Stage 0, Pass 1 and two Pass 2 tabs). Each box is a memo group, with its candidates inside. A double border with `✚ new` marks candidates that step added. On the last tab, the selected plan's candidates are green. A group with many candidates shows one count per origin; click a count to list them. |

The Memo panel appears only when the document has a `memo` section.

In the lanes:

- blue nodes are data sources, grey are relational operators and purple are
  summary operators;
- a thick border marks a query's result, and `shared ×n` marks a node that n
  queries read;
- click a `SummaryTumblingWindowChain` node to unfold its tumbling windows, and click again to fold them.

To follow one query, pick it in **highlight query**: everything it does not
reach dims, in the lanes and in the Memo. Click a node to see its details;
click the background to go back. Drag to pan and scroll to zoom.

## Plan your own queries

**In the page:** click **Open stage document or workload…** and pick a
`PlanningWorkload` JSON file; the server plans it through Stage 3 and shows
it. Or click **Workload sets** and pick a row.

**From the command line:**

```sh
cargo run --release -p asap-devtools --bin asap_planner -- \
    --workload tools/dag-viewer/workloads/planner-layering/example1.json \
    --run-until stage3 --format json --out plan.json
```

Then open `plan.json` with **Open stage document or workload…**, or drop it
on the page. Each query's accuracy requirement is enforced unless you pass
`--ignore-accuracy-guarantees`; the Stage 3 header says which applied. For
every option, see [Plan a workload with asap_planner](asap-planner.md).

## When something goes wrong

| Symptom | What to do |
| --- | --- |
| `Could not load the planner output: …` | The document is from an older planner; write it again with `asap_planner`. |
| `Planning failed` with a network error or `HTTP 501` | The page was opened without the server. Start `server.py` and open the address it prints. |
| The examples don't change after a planner change | Restart the server with `--regenerate`. |
| No Memo panel | The document has no `memo` section (it was written by an older planner). |

For the document format and the viewer's code, see
[`tools/dag-viewer/README.md`](../../tools/dag-viewer/README.md).
