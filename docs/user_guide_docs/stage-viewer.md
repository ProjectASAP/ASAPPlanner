# See how ASAPPlanner plans a workload: the Stage Viewer

The Stage Viewer shows, in a browser, what ASAPPlanner does with one workload
at each of its planning stages, and why it selects the plan it selects. Use it
to check a plan by hand, to compare the alternatives the planner considered,
and to see how the deployment's capabilities and the query requirements
change the choice.

The stages are those of
[ASAPPlanner's design](../design_docs/proposals/planner-layering.md#asapplanner-system-overview-planning-stages):

| Stage | What the viewer shows |
| --- | --- |
| 0. Frontends | The logical DAG the queries lower into, one root per query |
| 1. Logical ASAP-aware optimization | The logical ASAP candidates: each choice of exact or approximate computation per query (Pass 1), with shared inputs, shared summaries and tumbling panes (Pass 2) |
| 2. Physical ASAP-aware optimization | The physical candidates of each logical candidate, which differ in what runs at ingestion time and what runs at query time |
| 3. Plan selection | Each physical candidate checked for accuracy, latency and deployment capabilities, priced per second, and the cheapest selected |

The viewer reads one JSON document per workload, written by the
`stage_pipeline` command. It does not run the plans.

> **Based on [PR #574](https://github.com/ProjectASAP/ASAPPlanner/pull/574)**
> (branch `stack/509-viewer-stages`), which redesigns the viewer, together with
> the PRs it is stacked on: #613 (`stage_pipeline` examples 2 and 4b, and
> pricing every plan), #610 (deployment inputs in the document) and #609
> (deployment capabilities). Until they are merged, check out
> `stack/509-viewer-stages` to use the viewer as described here; `main` still
> has the earlier Pre/Post-ASAP viewer.

## Start the viewer

Run from the repository root, with Rust/Cargo and Python 3 installed:

```sh
python3 tools/dag-viewer/server.py
```

The server builds `stage_pipeline` (the first build takes a few minutes),
writes the built-in example documents into `tools/dag-viewer/out/`, and
prints the address to open:

```text
ASAPPlanner Stage Viewer: http://127.0.0.1:8000
```

| Option | Use it to |
| --- | --- |
| `--port 8765` | Serve on another port |
| `--host 0.0.0.0` | Accept connections from other machines (the default accepts only local ones) |
| `--skip-build` | Reuse an already-built `stage_pipeline` from `target/debug/`, or from `$CARGO_TARGET_DIR/debug/` if that variable is set |
| `--regenerate` | Rewrite the example documents, for example after changing the planner |

If the server runs on a remote machine, forward the port and open
`http://127.0.0.1:8000` locally:

```sh
ssh -L 8000:127.0.0.1:8000 <remote host>
```

Press `Ctrl+C` in the server's terminal to stop it.

## Look at an example

The tabs at the top open the examples of the design document's
[end-to-end examples](../design_docs/proposals/planner-layering.md#end-to-end-examples).
Each tab starts with a sentence that says what the workload is and why its
plan wins.

| Tab | Workload | What to look for |
| --- | --- | --- |
| 1 · dashboard panels | Two PromQL panels over 1M counter series every 10 s: an exact per-job rate, and the approximate top-10 series per job | The exact query stays exact, the top-k uses one CMS+heap over the raw samples, and both read one shared scan |
| 2 · SQL flow statistics | Distinct sources, entropy and L2 of per-source counts over the last minute of flows | Every UnivMon plan is invalid with the reason "no accuracy model for UnivMon", so an exact plan wins |
| 3a · historical p99 batch | Five p99 reports over 1–5 years, run once | The cheapest plan shares one input scan across the five KLL sketches |
| 3b · live p99 panel | p99 over the last 5 min every minute, 1M series | Keeping KLL panes from ingestion time costs more memory than building the sketch at each evaluation, and rebuilding all panes at query time is over the 200 ms latency bound |
| 4a · monthly p99 reports | Example 3a repeated monthly | The same plan, with its cost amortized over a month |
| 4b · panes that pay off | p99 over the last hour every 10 min, 1k series sampled every second, raw data not kept by the deployment | Maintaining six 10-min KLL panes at ingestion time wins |

To link to one example, add its name after `#`, for example
`http://127.0.0.1:8000/#example4b`.

The workload statistics in the examples (series counts, ingestion rates) are
illustrative. Read the costs as a ranking, not as a prediction of real
resource use.

## Read the page

### Workload queries

One box per query, with its language and text and the requirements the
planner must meet:

- **accuracy**: `exact`, or `ε=…, δ=…` (relative error ε with probability at
  least 1 − δ);
- **latency**: the maximum time an evaluation may take at query time;
- **every N s**: how often a repeating query runs.

The line under the queries counts the candidates at each stage, for example
`1 logical DAG → 88 logical ASAP → 112 physical → 1 selected (40 invalid, 71
valid but costlier)`.

### Deployment inputs

What the deployment told the planner, which Stage 3 uses to accept or reject
plans:

| Row | Meaning |
| --- | --- |
| Exact aggregates | Exact computations the executor can run (Sum, Count, Min, Max, Increase, Rate) |
| Sketches → estimates | Each sketch the executor can build, with the estimates it can be read for, for example `Kll → Quantile`. A plan that needs anything else is invalid. |
| Ingestion-time maintenance | Whether the deployment can keep state up to date as data arrives |
| Keep query-time results across evaluations | Whether results built at query time can be kept for the next evaluation |
| Memory budget | The most state the deployment can keep, or none |
| Raw data | Whether the deployment keeps raw samples anyway. If it does not, plans that read raw data at query time pay to keep the samples they read. |
| Cost model | The cost model, its unit, and its calibration constants, such as the cost of one CPU operation and of keeping one byte for one second |
| Accuracy model | The model that decides whether a sketch meets a query's accuracy target |

### Stage 3 · plans by cost

Every physical plan, cheapest first, with its total cost per second:

- **✓ selected**: the plan ASAPPlanner chooses.
- **valid · costlier**: meets every requirement, but costs more.
- **✗ invalid**: fails a check, with the reason. Examples: an accuracy target
  the sketch cannot guarantee, an evaluation over the latency bound, or a
  summary the deployment cannot build.

Each label names the choice per query, for example `Q1 exact · Q2 CMS+heap ·
shared input`, and, for physical plans, what runs at ingestion time, for
example `ingestion time: Kll ×6 panes`.

Click a plan to show it in the three lanes. For workloads with many plans,
the document may carry only the cheapest ones. The list then says so, for
example `showing 64 of 486 plans, cheapest first; Stage 3 priced 486 and
selected among all of them`. The selection is made over all plans either
way.

### The three lanes

| Lane | Shows |
| --- | --- |
| Stage 0 · Logical DAG | The queries as lowered by the frontends |
| Stage 1 · Logical ASAP DAG | The logical candidate of the plan you picked. The **candidate** menu shows any other. |
| Stage 2 · Physical ASAP DAG | The plan you picked, with each node's timing (ingestion time or query time) and Stage 3 cost. The **candidate** menu shows any other. |

Data flows from the bottom up. Node colors:

- blue: data sources;
- grey: relational operators (filter, project, aggregate, sort, …);
- purple: summary operators (build a sketch or exact accumulator, merge,
  estimate, finalize).

A thick border marks a query's result. A sketch build shows its configuration
and how many instances it keeps, for example `summary: CmsWithHeap · depth 7
· heap 100 · width 272` and `instances: one per group`.

Drag to pan and scroll to zoom.

### Details

Click a node to see:

- its operator and node id;
- for a summary, its parameters and instances;
- which query it answers;
- when it runs and its Stage 3 cost;
- its output schema and coverage (the data and time range a summary
  represents);
- the accuracy guarantee it carries;
- the full operator as JSON.

Click an edge to see the schema and data state it carries.

## Plan your own queries

### In the page

Click **Query editor**, enter PromQL queries one per line, and click
**Plan**:

| Field | Meaning |
| --- | --- |
| ε | Relative error allowed for every query. Leave it empty for exact queries. |
| δ | Failure probability allowed. Needs ε. |
| sample interval (ms) | How often each series is sampled (default 15000) |

The editor plans the queries as one batch that runs once. It declares only
the sample interval, so the planner uses default statistics for everything
else. SQL is not available in the editor yet; use the command below with a
built-in example, or a document written elsewhere.

### From the command line

`stage_pipeline` writes the document the viewer reads:

```sh
cargo run -p asap-devtools --bin stage_pipeline -- \
    --promql "topk by (job) (10, sum_over_time(http_requests_total[1m]))" \
    --epsilon 0.01 --delta 0.001 --out plan.json
```

| Option | Meaning |
| --- | --- |
| `--promql <query>` | A query; repeat for several |
| `--epsilon`, `--delta` | Accuracy for every query; without them the queries are exact |
| `--interval-ms <ms>` | Sample interval (default 15000) |
| `--example <name>` | A built-in example instead of `--promql`: `planner-layering-1`, `-2`, `-3a`, `-3b`, `-4a` or `-4b` |
| `--max-candidates <n>` | How many plans the document carries, cheapest first (default 64). Stage 3 still prices every plan. |
| `--out <file>` | Where to write the document |

Open the document with **Open stage document…**, by dropping the file on the
page, or, if the file is under `tools/dag-viewer/`, with
`http://127.0.0.1:8000/?doc=<path relative to tools/dag-viewer>`.

## When something goes wrong

| Symptom | What to do |
| --- | --- |
| `Could not load the planner output: …` with a list of problems | The document is not a valid `asap-stage-pipeline/v1` document. Write it again with the current `stage_pipeline`. |
| `Planning failed: …` in the editor | The message comes from `stage_pipeline`, for example a query the PromQL frontend does not support. The server's terminal shows the full error. |
| `Planning failed` with a network error or `HTTP 501` | The page was opened without the server (for example as a file, or from another web server). Start `server.py` and open the address it prints. |
| The examples do not change after a planner change | Restart the server with `--regenerate`. |
| `stage_pipeline does not exist; start without --skip-build` | Start the server without `--skip-build`, or build it first: `cargo build -p asap-devtools --bin stage_pipeline` |

For the document format and the viewer's code, see
[`tools/dag-viewer/README.md`](../../tools/dag-viewer/README.md).
