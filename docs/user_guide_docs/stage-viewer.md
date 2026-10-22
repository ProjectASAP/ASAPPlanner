# See how ASAPPlanner plans a workload: the Stage Viewer

The Stage Viewer shows, in a browser, what ASAPPlanner does with one workload
at each of its planning stages, and why it selects the plan it selects. Use it
to check a plan by hand, to compare the alternatives the planner considered,
and to see how the deployment's cost model and the query requirements
change the choice.

The stages are those of
[ASAPPlanner's design](../design_docs/proposals/planner-layering.md#asapplanner-system-overview-planning-stages):

| Stage | What the viewer shows |
| --- | --- |
| 0. Frontends | The logical DAG the queries lower into, one root per query |
| 1. Logical ASAP-aware optimization | The logical ASAP candidates: each choice of exact or approximate computation per query (Pass 1), with shared inputs, shared summaries and tumbling panes (Pass 2) |
| 2. Physical ASAP-aware optimization | The physical candidates of each logical candidate, which differ in what is materialized: state maintained at ingestion time, a node built once per evaluation at query time and read by several queries, or nothing |
| 3. Plan selection | Each physical candidate checked for accuracy and latency, compared under the deployment's cost model (by default priced per second), and one selected |

The viewer reads one JSON document per workload, written by the
`stage_pipeline` command. It does not run the plans.

`stage_pipeline` plans with the planner's default search, the one a
deployment runtime gets from `plan_installable`: it checks each query's own
candidates first, searches queries over disjoint sources separately, and keeps
only the best plan, so it plans workloads with many queries in seconds. It
also plans with the models `plan_installable` uses: when the deployment
declares no supported summaries, only the summaries the Planner's executor
runs are offered, and a PromQL workload's plans must compile with the
executor (a SQL workload's table scans are not checked). The
document then shows the selected plan, the rejections the search recorded and
what it checked. With `--exhaustive` it instead composes and prices every
candidate and the document lists them all, for small workloads or to compare;
the tabs' examples are written that way.

Unlike `plan_installable` with its default settings, `stage_pipeline`
ignores accuracy guarantees unless you pass `--enforce-accuracy-guarantees`:
Stage 3 accepts every query output whatever its guarantee and compares the
plans on cost alone (executability checks, such as the executor's compile
check, still apply). The Stage 3 header says `accuracy guarantees ignored`
or `accuracy guarantees enforced`. The example tabs and the workload sets
enforce guarantees, since their stories are about which plans meet their
accuracy targets. Plans from the query editor and from a `PlanningWorkload`
file you open ignore them; on the command line, pass the flag to enforce
them.

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
| 1 · dashboard panels | Two PromQL panels over 1M counter series every 10 s: an exact per-job rate, and the approximate top-10 series per job | The exact query stays exact; the CMS+heap and CountSketch+heap top-k plans are rejected with their reasons, so the all-exact plan wins; the plans that share the two queries' scan and range selector tie with it, and the independent plan is kept on enumeration order. Pass 2 also builds Q1's rate and Q2's option in six 10-s tumbling panes, each alone or both (27 logical plans); rebuilt at every refresh, the panes cost more than the whole window. Stage 2 adds 30 plans that maintain Q1's rate panes, Q2's panes or both at ingestion time (57 physical plans); continuously ingesting PromQL data is taken to arrive in time order, so the rate panes may be maintained. Keeping seven panes of 1M per-series states costs more than rebuilding them (12.94 against 8.54 per second for either query's exact panes), so P1, all exact, is still selected at 7.94 |
| 3a · historical p99 batch | Five p99 reports over 1–5 years, run once | All 972 plans are priced or rejected: 243 Pass 1 plans, each with 3 shared-input plans. Each query builds its own sketch at query time. P485 is selected: KLL for all five queries. 844 plans are invalid: every plan with DDSketch, because each query's target is a rank error and DDSketch bounds a relative error. The all-exact plan costs about 16% more |
| 3b · live p99 panel | p99 over the last 5 min every minute, 1M series | P2, KLL, is selected at 2.100 per second; exact over the window's raw samples costs 2.400. Pass 2 adds KLL and DDSketch in five 1-min tumbling panes (5 plans in all): rebuilt at every evaluation, the KLL panes cost 2.183, the whole build plus merging the panes. DDSketch misses the rank target. Memory a query holds only while it runs is not charged. Stage 2 adds the KLL and DDSketch panes maintained at ingestion time (7 plans in all): maintained, the KLL panes keep six panes of 1M per-series KLLs, 872 bytes each (its fixed 744 and an item array grown for its 4 samples), 5.2 GB, for 654.5 per second, so P2 still wins. Every priced plan meets the 200 ms latency bound: the whole-window KLL takes 126 ms per evaluation, the panes rebuilt at query time 131 ms and the maintained panes 7 ms |

To link to one example, add its name after `#`, for example
`http://127.0.0.1:8000/#example3b`.

The other examples of the design document have no tab. Write their
documents with `stage_pipeline` (see [From the command line](#from-the-command-line))
and open them with **Open stage document…**:

| Example | Command options | What to look for |
| --- | --- | --- |
| 2 · SQL flow statistics | `--example planner-layering-2 --exhaustive --enforce-accuracy-guarantees` | Distinct sources, entropy and L2 of per-source counts over the last minute of flows. All 82 logical plans compose; none is pruned. The SQL frontend recognizes Q2 as entropy and Q3 (its counts cast to DOUBLE) as L2, each with an exact or UnivMon option and an exact population count for the empty-input NULL. 16 plans come from Pass 1. Pass 2 adds the plans that share identical nodes and, by the summary-capability rule, the plans in which one UnivMon serves two of the queries or all three, sized with each parameter's largest value among them. Stage 2 adds 102 plans in which a node that two queries read is built once per evaluation at query time and read by both (`query time, shared: …` in the label), 184 physical plans in all. 164 plans are invalid: HLL or UnivMon for Q1 (120) because its bound has "failure probability None" (both need the source's distinct bound, which this example does not declare), and UnivMon for Q2 (44) because entropy has no certified bound. 20 are valid: Q1 exact or KMV, Q2 exact, and Q3 exact or UnivMon, each alone, sharing identical nodes, or with the shared node built once at query time. Q3's UnivMon keeps its L2 guarantee through the exact population count joined beside it and the `CASE` that decides the empty-input NULL on that count. Theta is not offered: `asap_sketchlib` has no Theta sketch. P1 (all exact) is selected at 6.08 per second; KMV on Q1 costs the same to three decimals, and UnivMon on Q3 costs 8.08. The shared plans cost 7.44 or more when the shared node is not materialized, because it is then charged once per query that reads it, and 7.04 when it is built once at query time |
| 4 · one workload, three cost models | `--example planner-layering-3b --exhaustive --enforce-accuracy-guarantees` with `--cost-model` | Example 3b's live p99 panel under each cost model. Cost model 1 (`usage-over-time`, the default) selects P2, the whole-window KLL, at 2.100 per second. Cost model 2 (`provisioned-capacity`) selects P1, exact, at 1024.0 per second against 1125.0 for P2, because it prices peak memory, and the KLL states, each grown for its 20 samples, hold 1 GB against the exact plan's 192 MB. Cost model 3 (`cluster-capacity --cluster-memory-bytes 256000000000 --cluster-disk-bytes 1000000000000`) selects by `--cluster-objective`: `lowest-latency` selects P3-m1, the KLL panes maintained at ingestion time (7 ms per evaluation); `least-memory` selects P1; `pareto-front` lists P1, P2 and P3-m1 and selects P3-m1. With 4 GB of memory (`--cluster-memory-bytes 4000000000`), the plans with KLL panes are over the limit and invalid, `lowest-latency` selects P2, and the front is P1 and P2. Under cost model 3 the plans have no price. With `--deployment-capabilities '{"query_time_results_retained": true}'`, Stage 2 adds P3-m2, the KLL panes built at query time and kept across evaluations, at 436.53 per second under cost model 1; with `'{"raw_data_retained": false}'`, every plan that scans raw data at query time is invalid and P3-m1, the panes maintained at ingestion time, is selected at 654.53 |

The workload statistics in the examples (series counts, ingestion rates) are
illustrative. Read the costs as a ranking, not as a prediction of real
resource use.

## Read the page

### Workload queries

One box per query, with its language and text and the requirements the
planner must meet:

- **accuracy**: `exact`, or `ε=…, δ=… (metric)` (error ε in that metric
  with probability at least 1 − δ); `(metric not set; default …)` when the
  query leaves the metric to the planner, which then accepts a plan in any
  metric that meets ε and δ (the default is the metric the query's result is
  naturally stated in, shown for reference);
- **latency**: the maximum time an evaluation may take at query time;
- **every N s**, **once** or **N invocations**: how often the query runs;
- **predictable**, **ad hoc** or **unknown**: whether its demand is known
  ahead of time;
- **range of … in … steps**: a range query's span and step.

### Data workload

Beside the queries, the facts the planner was given about the data: how it
arrives (`ingesting · 200000 rows/s`, `at rest`, `mixed`) and, per source
(a PromQL metric or a SQL table), its series count, metric type, whether
its values are non-negative, its sample interval, its stream-norm bounds and
its distinct-count bound, `—` for a fact the workload does not declare. The
**read by** column lists the queries whose text names the source. The line
above the table shows the deployment capabilities the plan was made for
(for example `raw data not retained`). A workload cannot declare labels,
their hierarchy or their cardinalities, so the panel shows none.

The line under the queries counts the candidates at each stage, for example,
in Example 1, `Candidates per stage: 1 logical DAG → 27 logical ASAP → 57
physical → 1 selected (38 invalid, 18 valid but costlier)`.

### Stage 3 · plans by cost

The header names the search. Under the default search, the list has the
selected plan and the rejections the search recorded (the first of each
cause, marked `not kept`, not clickable), and the header says how many
physical candidates it checked, how many independent components it searched
separately, and, per query, how many of its own candidates it discarded and
why. Under `--exhaustive`, the list has every plan:
the selected plan first, then the other valid plans cheapest first, each
with its cost per second; then the invalid plans with their reasons, the
plans Stage 2 pruned, and the Stage 1 candidates that did not compose.
Under cost model 3 the plans have no price: the list shows the selected
plan and marks the other valid ones `valid · costlier` without a reason;
the document's `stage3_plan_selection.rejected` holds the reasons (for
example `slower: latency 144.0 ms, …`).
A node that is not materialized is recomputed for each query that reads it,
so a node shared by two queries is charged twice; a materialized node, at
ingestion or at query time, is charged once, and the nodes below it once for
it (plus once for each query that also reads them directly).

- **✓ selected**: the plan ASAPPlanner chooses.
- **valid · costlier**: meets every requirement, but costs more.

A selected or costlier plan also says, per query, which metric its accuracy
was accepted in, e.g. `q1: metric not set; accepted rank (Kll)`.
- **✗ invalid**: fails a check, with the reason. Examples: an accuracy target
  the sketch cannot guarantee, an evaluation over the latency bound, a
  query-time scan of raw data the deployment does not retain, or, under cost
  model 3, a plan over the cluster's memory or disk, or a PromQL plan the
  executor cannot compile. A summary the deployment
  cannot build is never offered: the document's
  `stage1_logical_asap_optimization.pass1_pruned` lists it with its reason.

Each label names the choice per query, for example `Q1 exact (Sum acc, Rate
acc) · Q2 CMS+heap`, what Pass 2 shares or splits into panes, for example
`shared Scan (Q1, Q2)` or `Kll for Q1 in 5 × 1m panes`, and, for physical
plans, what is materialized: `ingestion time: Kll ×5 panes` for state
maintained as data arrives, `query time, shared: Count acc` for a node built
once per evaluation and read by several queries.

Click a plan to show it in the three lanes. For workloads with many plans,
the document may carry only the first ones in enumeration order, plus the
selected one. The list's header then says so, for example `972 candidates
(document shows 64)` in Example 3a: the selected plan, the other valid plans
cheapest first, then the invalid ones taking each reason in turn, with a
count per reason. The selection is made over all plans
either way.

### The three lanes

| Lane | Shows |
| --- | --- |
| Stage 0 · Query-language frontends | The queries as lowered by the frontends |
| Stage 1 · Logical ASAP candidate | The logical candidate of the plan you picked. The **candidate** menu shows any other. |
| Stage 2 · Physical ASAP candidate | The plan you picked, with each node's timing (ingestion time or query time) and Stage 3 cost. The **candidate** menu shows any other. |

Data flows from the bottom up. Node colors:

- blue: data sources;
- grey: relational operators (filter, project, aggregate, sort, …);
- purple: summary operators (build a sketch or exact accumulator, merge,
  estimate, finalize);
- dashed: a standalone scalar query, such as `time()` or `1 + 2`.

A thick border marks a query's result, and `shared ×n` marks a node that n
queries' roots reach (n > 1). A pane chain (`PaneChain`, tumbling panes a
summary is built in) is one node that says how many panes of what width it
has, for example `30 panes × 20 s`. Click it to draw each pane, newest
first: a `TimeShift` by the pane's offset (none for the newest), a
`TimeRange` of one pane, and the pane's build with its window relative to
the evaluation time `t`, for example `window t−40 s … t−20 s`. Click a pane
to fold the chain again.

To follow one query, pick it in **highlight query** above the lanes: every
lane dims what that query's root does not reach. Click a node to dim
everything except what it reads and what reads it; click the background to
go back. A sketch build shows its configuration
and how many instances it keeps, for example `summary: CmsWithHeap · depth 7
· heap 100 · width 272` and `instances: one per group`.

Drag to pan and scroll to zoom.

### Stored outputs

Under the Stage 2 lane, what a deployment runtime would store for the
selected plan, as `plan_installable` reports it: one row per Stage 2 node,
with its coverage definition (equal for two nodes that compute the same
thing), its operator and summary parameters, its panes (`30 × 20 s` for a
pane chain), whether it is built at ingestion or at query time and kept,
how long it is kept, the Stage 3 cost of its Stage 2 node, and the queries
that read it. Open **installed nodes** in a pane chain's row for each pane
the runtime installs: its node id, its window and how long it is kept. Click a row to select its node in the Stage 2
lane. A plan that computes everything at query time has no stored outputs.

### Details

Click a node to see:

- its operator and node id;
- the queries whose root reaches it;
- for a summary, its parameters and instances;
- which query it answers;
- when it runs and its Stage 3 cost;
- its output schema and coverage (the data and time range a summary
  represents);
- the accuracy guarantee it carries;
- the full operator as JSON.

Click an edge to see the schema and data state it carries.

## Browse workload sets

Click **Workload sets** to list the sets in
`tools/dag-viewer/workloads/index.json`, one table per set (pick the set in
the menu). Each row is one workload: its query count, the query labels,
the queries themselves (open **queries** for each one's text, how often it
runs and its accuracy target) and the sources they read. Click a row to
plan the workload: the server runs `stage_pipeline --workload` with the
default search and the set's deployment capabilities and cost model, and
the page shows its stages. Planning a 9-query workload takes about 15
seconds.

| Set | Workloads | What it is |
| --- | --- | --- |
| Random mixed · seed-862 | 10 | ASAPQuery's random workloads (seed 862): nine repeating PromQL queries each (sums, quantiles, top-k sums, a top-k increase and a top-k rate) over 10,000 series per metric sampled every 100 ms, lookbacks of 1 to 20 minutes |
| Random mixed · seed-862-x5 | 10 | The same queries over five times as many series |
| Random mixed · seed-862-long | 10 | The same queries with three times longer lookbacks |

Each query's target is ε = 0.05 (1 − its accuracy SLA) with δ = 0.3 and no
metric set. The sets are planned as the MVP deployment: raw data not
retained, priced at AWS Fargate us-east-1 rates, accuracy guarantees
enforced. Without the server (for
example, when the page is served as static files), workload-01 of each set
opens its precomputed document, which is also an example tab, and the other
rows say they need the server.

To add a set, put its `PlanningWorkload` files in
`tools/dag-viewer/workloads/<set>/` and add an entry to `index.json` (see
[`tools/dag-viewer/README.md`](../../tools/dag-viewer/README.md#workload-sets)).
A `PlanningWorkload` file can also be opened directly with **Open stage
document or workload…**.

## Plan your own queries

### In the page

Click **Query editor**, declare the tables the SQL queries read, enter SQL
or PromQL queries, and click **Plan**:

| Field | Meaning |
| --- | --- |
| Input tables | One table per row: a name, one column per line (`name TYPE [NOT NULL]`) and an optional time column. Only SQL queries read them; PromQL schemas are inferred from the metrics a query reads. |
| Queries | One query per row: a name, the language (SQL or PromQL), the tables a SQL query reads, and its text. Unchecked rows are left out of the workload. |
| ε | Error allowed for every query (default 0.01). |
| metric | The error metric ε is stated in: Absolute, Relative or Rank. `default` leaves it to the planner, which accepts a plan in any metric that meets ε; a quantile query must name one. |

The editor plans the checked queries as one batch that runs once, with no
failure probability δ. It declares a 15 s sample interval for every metric
the PromQL queries read and no other data facts, so the planner uses
default statistics for everything else. It ignores accuracy guarantees,
`stage_pipeline`'s default. For δ, another sample interval, another cost
model or enforced guarantees, use the command below.

### From the command line

`stage_pipeline` writes the document the viewer reads:

```sh
cargo run -p asap-devtools --bin stage_pipeline -- \
    --promql "topk by (job) (10, sum_over_time(http_requests_total[1m]))" \
    --max-error 0.01 --failure-probability 0.001 --out plan.json
```

| Option | Meaning |
| --- | --- |
| `--promql <query>`, `--sql <query>` | A query; repeat and mix for several, in workload order |
| `--name <name>` | Names the query before it (default `q1`, `q2`, …) |
| `--workload <file>` | A `PlanningWorkload` JSON file instead of queries: its queries, their requirements and recurrence, and its data workload. Each query's accuracy comes from the file, so `--max-error`, `--failure-probability` and `--accuracy-metric` do not apply. |
| `--table '<json>'` | A table the SQL queries read, for example `{"name": "metrics", "columns": [{"name": "ts", "type": "timestamp"}, {"name": "service", "type": "utf8"}], "time_index": 0}`; repeat for several |
| `--max-error`, `--failure-probability` | Accuracy (ε, δ) for every query; without them the queries are exact |
| `--accuracy-metric <metric>` | The error metric of that accuracy: `absolute`, `absolute_stream_l1`, `absolute_stream_l2`, `relative` or `rank`. Without it, a plan is accepted in whatever metric meets ε (and δ). |
| `--sample-interval-ms <ms>` | Sample interval of every metric the PromQL queries read (default 15000) |
| `--example <name>` | A built-in example instead of queries: `planner-layering-1`, `-2`, `-3a` or `-3b`; add `--exhaustive --enforce-accuracy-guarantees` to see every plan its story describes and the plans it rejects for their guarantees |
| `--cost-model <model>` | How plan selection compares valid plans: `usage-over-time` (cost model 1, the default: CPU, scanned bytes and memory over time, priced per second), `provisioned-capacity` (cost model 2: peak memory, peak CPU cores and disk, priced) or `cluster-capacity` (cost model 3: no prices) |
| `--cluster-memory-bytes <n>`, `--cluster-disk-bytes <n>`, `--cluster-cpu-cores <n>` | Cost model 3 only: the cluster's limits; a plan over one is invalid. The CPU limit is optional. |
| `--cluster-objective <objective>` | Cost model 3 only: `lowest-latency`, `least-memory` or `pareto-front` (the plans no other plan beats on latency, memory and disk; the lowest-latency one is selected) |
| `--deployment-capabilities '<json>'` | What the executor supports, for example `{"supported_summaries": [{"family": "Kll", "readouts": ["Quantile"]}], "raw_data_retained": true, "query_time_results_retained": true}`. `supported_summaries` (default: every summary the planner can build) lists the sketch families and the statistics each may answer; a summary outside it is not offered. With `raw_data_retained: false` (default `true`), a plan that scans raw data at query time is invalid. With `query_time_results_retained: true` (default `false`), tumbling panes built at query time may be kept for the next evaluations (`query time, kept` in the label). The document repeats the inputs under `deployment_capabilities`; the page does not show them yet. |
| `--measured-operation-costs <file>` | A JSON table of measured CPU time per sketch operation (`[{"sketch": …, "operation": "update", "cpu_ns_per_operation": …}]`, as `convert_rqe_atomic_costs` writes it). A sketch node whose parameters match an entry exactly is priced from it; each node's `price_source` in the document says `measured` or `analytical`. |
| `--enforce-accuracy-guarantees` | Reject a plan whose accuracy guarantee does not meet its query's target, as `plan_installable` does with the default settings. Without it (the default), every query output is accepted regardless of its guarantee, the plans are compared on cost alone, and each output's line says `regardless of guarantee`, plus `ignoring a failed guarantee check (<cause>)` when its guarantee would not have met the target; the document's `accuracy_guarantees` is `ignored` or `enforced`. With `--plan-selection-settings`, this flag decides, whatever the file's `ignore_accuracy_guarantees` says. |
| `--exhaustive` | Compose and price every candidate instead of running the default search, and list them all. Use it for small workloads or to compare with the default search. |
| `--max-displayed <n>` | With `--exhaustive`, how many plans the document shows (default 64), in enumeration order, plus the selected plan. The planner still optimizes and prices every plan, so the selection does not depend on it. |
| `--max-combinations <n>` | With `--exhaustive` only: the most candidates a run plans (default 10000), counted at Stage 1 and again at Stage 2; a larger workload is refused with exit status 3 and no document. The default search has no limit. |
| `--out <file>` | Where to write the document |

Open the document with **Open stage document or workload…**, by dropping the file on the
page, or, if the file is under `tools/dag-viewer/`, with
`http://127.0.0.1:8000/?doc=<path relative to tools/dag-viewer>`.

## When something goes wrong

| Symptom | What to do |
| --- | --- |
| `Could not load the planner output: …` with a list of problems | The document is not a valid `asap-stage-pipeline/v1` document. Write it again with the current `stage_pipeline`. |
| `Planning failed: …` in the editor | The message comes from `stage_pipeline`, for example a query the PromQL frontend does not support. The server's terminal shows the full error. |
| `workload has more than 10000 Stage 1 candidates …` | Only `--exhaustive` counts candidates against `--max-combinations`. Drop `--exhaustive` to run the default search, or raise the limit. |
| `Planning failed` with a network error or `HTTP 501` | The page was opened without the server (for example as a file, or from another web server). Start `server.py` and open the address it prints. |
| The examples do not change after a planner change | Restart the server with `--regenerate`. |
| `stage_pipeline does not exist; start without --skip-build` | Start the server without `--skip-build`, or build it first: `cargo build -p asap-devtools --bin stage_pipeline` |

For the document format and the viewer's code, see
[`tools/dag-viewer/README.md`](../../tools/dag-viewer/README.md).
