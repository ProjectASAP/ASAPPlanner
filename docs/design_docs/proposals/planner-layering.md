# ASAPPlanner Planning Stages Design

Status: proposal. Audience: designers and developers of ASAPPlanner and of deployments such
as ASAPQuery-backend.

Read Goal for the motivation and assumptions, Stages for the overview, the
stage sections for the rules, the examples for why the rules are needed, and
Scenarios for how the design is extended.

## Goal

### Motivation

* Existing database query engines and optimizers do not consider ASAP
  primitives for optimizing the queries: summaries such as sketches that trade bounded error for lower
  cost.
* They also do not consider the query and data workloads of different use
  cases, which have the potential to share the common optimization with ASAP primitives. Example use case workloads can be streaming or batch data input, and repeated, batch or ad hoc queries.
* So existing query planners miss the opportunity to share the benefits of ASAP primitives across
  domains and use cases.

In order to achieve the goals, ASAPPlanner needs to abstract the modeling the following for use cases:

| Modeling | Contents |
|---|---|
| Query workload | Queries with recurrence (repeated, batch, ad hoc), predictability, time selection, and accuracy and latency requirements |
| Data workload | Arrival (streaming, at rest, or both), volume, rate, cardinality and distribution |
| Deployment inputs | Empirical cost model, empirical accuracy model and execution capabilities |
| ASAP replacement strategies | Rules that replace a sub-DAG of the query expression with summary expressions, and the summary families each computation may use |

ASAPPlanner takes a [query workload](https://github.com/ProjectASAP/ASAPPlanner/blob/main/crates/types/src/workload.rs), a [data workload](https://github.com/ProjectASAP/ASAPPlanner/blob/main/crates/types/src/workload.rs#L531) and the deployment's
inputs (TODO: define this data structure, [#525](https://github.com/ProjectASAP/ASAPPlanner/issues/525)), and returns one optimal physical plan. It decides what is computed, how
it is computed, and which plan is best. The deployment only supplies inputs and
executes the plan: it provides its empirical cost model, empirical accuracy
model and capabilities, but never plans queries or selects plans.

### Assumptions

ASAPPlanner relies on the assumptions below. 

1. **Query rewrite rules are given.** Rewrite and replacement rules (for
   example, `avg` as `sum`/`count`, or a TopK as a Count-Min Sketch with a
   heap) are written by developers or algorithm designers. ASAPPlanner applies them; it does not
   discover or generate them.
2. **Summary-family capabilities are given.** For each summary family, a
   developer declares which computations it can answer, which estimates it can
   read out, how it is sized for an accuracy target, its error bound, and
   whether it can be merged, subtracted or deleted. ASAPPlanner does not
   automatically discover these capabilities.
3. **Frontend semantics are given.** Each frontend preserves its source
   language's behavior, and ASAPPlanner obeys their semantic behavior.
4. **Workload descriptions are inputs.** Recurrence, predictability,
   requirements and the data workload are supplied with the workload.
   ASAPPlanner does not infer them from traffic.
5. **Cost and accuracy come from models.** ASAPPlanner does not measure
   execution. It uses the deployment's cost and accuracy models, or built-in
   defaults when the deployment supplies none.
6. **Optimal is relative to the candidate space.** The selected plan is the
   cheapest valid plan among the candidates produced by the given rules and
   capabilities, as estimated by the given models. It is not optimal over
   plans those rules cannot produce.
7. **The deployment executes the plan as given.** It does not change summary
   choices or materialization.

## Stages

In the diagram, × means the Cartesian product: each stage combines every option along one dimension with every option along the others.

```text
 Query workload
   (PromQL / SQL / MetricsQL,
    query recurrence,
    accuracy requirements,
    latency requirements)

 + Data workload
   (streaming vs. data at rest,
    data distribution,
    cardinality)

 + Deployment inputs
   (cost model,
    accuracy model,
    deployment capabilities)
                         │
                         ▼
┌────────────────────────────── ASAPPlanner ──────────────────────────────┐
│                                                                        │
│ 0. Language-specific frontends                                         │
│    Parse and convert source-language queries into a common logical     │
│    representation. Reject unsupported query expressions.               │
│                                                                        │
│    Output: CandidateLogicalDAGs                                        │
│                                                                        │
│                         │                                              │
│                         ▼                                              │
│ Logical planning — what to compute                                     │
│                                                                        │
│ 1. Logical ASAP-aware optimization                                     │
│    Explore semantically equivalent and legal logical candidates:       │
│                                                                        │
│      summary families                                                  │
│      × query rewrites                                                  │
│      × sharing one summary across multiple computations                │
│                                                                        │
│    Output: CandidateLogicalASAPDAGs                                    │
│                                                                        │
│                         │                                              │
│                         ▼                                              │
│ Physical planning — how to compute                                     │
│                                                                        │
│ 2. Physical ASAP-aware optimization                                    │
│    Explore physical implementations of each logical candidate:         │
│                                                                        │
│      materialization decisions                                         │
│      × physical operator implementations                               │
│      × parallelism and partitioning                                    │
│      × resource management                                             │
│                                                                        │
│    Output: CandidatePhysicalASAPDAGs                                   │
│                                                                        │
│                         │                                              │
│                         ▼                                              │
│ 3. Plan selection                                                      │
│    Evaluate complete physical candidates using the deployment's        │
│    empirical cost and accuracy models. Reject candidates that violate  │
│    accuracy, latency, or capability constraints.                       │
│                                                                        │
│    Choose the cheapest valid plan for the whole workload.              │
│                                                                        │
└────────────────────────────────┬───────────────────────────────────────┘
                                 │
                                 ▼
                    one selected PhysicalASAPDAG
                                 │
                                 ▼
┌────────────────────────────── Deployment ───────────────────────────────┐
│                                                                        │
│ 4. Execution                                                           │
│   deployment executes the selected DAG (plan).                         │
│                                                                        │
└────────────────────────────────────────────────────────────────────────┘
```

The planner receives three groups of inputs:

| Input | Contents |
|---|---|
| Query workload | Source-language expressions, recurrence, predictability, time selection, accuracy requirements, and latency requirements |
| Data workload | Data arrival pattern, sampling cadence, ingestion volume and rate, cardinality, and distribution |
| Deployment inputs | Empirical cost model, empirical accuracy model, and execution capabilities |

## Stages and their decisions

Stages 0 to 2 each output a candidate set holding every semantically equivalent
and legal candidate DAG of that stage; stage 3 is the only step that chooses
one candidate DAG as output. Candidate sets are internal to ASAPPlanner and may
be shared or enumerated lazily. Two kinds of removal are kept apart:

* **Pruning** removes an **invalid** candidate. Any stage may prune, but only
  when the candidate is provably invalid (for example, a summary family that
  cannot meet the query's accuracy target), and every pruned candidate carries
  a reason.
* **Selection** discards **valid** candidates. Only stage 3 does this, when it
  picks the cheapest one.

A candidate is a DAG for the **whole workload**, not for one query. Each stage
combines its choices for every sub-DAG with the candidates it receives (the ×
in the diagram), so the candidate set grows from stage to stage until
selection picks one. Example 1 traces this growth step by step.

| Stage | Input | Decides | Output |
|---|---|---|---|
| 0. Frontends | `query`, `language` | Parse and convert to a common logical form; reject what cannot be represented | `CandidateLogicalDAGs` |
| 1. Logical ASAP-aware optimization | Logical DAGs; accuracy requirements, `time_selection`, repetition interval | Summary replacement (Pass 1); ASAP-aware CSE (Pass 2) | `CandidateLogicalASAPDAGs` |
| 2. Physical ASAP-aware optimization | Logical ASAP DAGs; `recurrence`, `predictability`, `DataWorkload` | Materialization; physical operators; parallelism and resources (TODO) | `CandidatePhysicalASAPDAGs` |
| 3. Plan selection | Physical candidates; `requirements`; cost model, accuracy model, capabilities | Reject invalid candidates; pick the cheapest plan for the whole workload | One `PhysicalASAPDAG` |
| 4. Execution (deployment) | The selected `PhysicalASAPDAG` | Run ingestion, storage and query-time computation | Query results |

The sections below describe each stage.

### 0. Language-specific frontends

The frontend converts each query into a `LogicalDAG`. Nodes represent logical
query operations, including selectors, transformations, aggregations, grouping
and window semantics. They contain no ASAP summary choices. A construct that cannot be represented
faithfully is rejected.

The frontend preserves source-language behavior, including series identity,
evaluation timing and missing-data semantics.

### 1. Logical ASAP-aware optimization

Logical optimization runs in two passes. Pass 1 generates candidates for each
computation on its own; Pass 2 finds candidates that share computation across
sub-DAGs and queries. Materialization and execution
placement are decided in later stages.

#### Pass 1: Local candidate generation

For each eligible sub-DAG, Pass 1 identifies its computation semantics, applies
rewrite rules, and generates every candidate that is not provably unable to
meet its accuracy requirement.

Example for summary candidates:

| Original computation | Local candidates |
|---|---|
| `Sum(x) by (g)` | Exact grouped sum |
| `TopK(k, x) by (g)` | Exact sort and limit per group, Count-Min Sketch with a top-*k* heap per group, Hydra over all groups |
| `Distinct(x)` | Exact distinct, a specialized distinct summary, UnivMon |
| `Entropy(x)` | Exact entropy, a specialized entropy summary, UnivMon |
| `L2(x)` | Exact L2 norm, a specialized norm summary, UnivMon |
| `Quantile(x, window)` | Exact quantile, KLL over the requested window |

Each candidate records its input expression, filter, grouping, window,
supported estimates and accuracy requirement. Pass 2 uses these to decide
whether candidates can share a summary node.

A summary-based candidate uses three kinds of summary nodes:

* A **summary build node** builds and maintains a summary from input data, for
  example a KLL sketch over `latency_ms`.
* A **summary merge node** combines summaries into one, for example merging
  five 1-min tumbling-window KLLs into one 5-min KLL, or merging lower-level
  summaries into a coarser one.
* A **summary estimation node** computes an answer from a summary, for example
  the p99 estimate from a KLL, or the entropy estimate from a UnivMon.
* **summary subtract node** and **summary delete node** design is TODO. 

One summary build node can feed several estimation nodes, which is what Pass 2
exploits.

#### Pass 2: ASAP-aware common-subexpression elimination

ASAP-aware CSE extends traditional CSE with summary-specific sharing rules.
Computations can share work when they use identical expressions, when one
summary build node supports several estimates, or when one window summary can answer
their overlapping windows.

The rules compare computations by their **summary input data**: what a summary for
that computation would ingest, namely the data source, the filters, and the key
or value being summarized together with its grouping. The summary input data does
not include the window; the window-composition rule compares windows
separately.

The window-composition rule distinguishes the window a query reads from the
window summary that answers it:

* A **window** is the time range one query evaluation reads, for example the
  last 5 min. Consecutive evaluations of a repeating query read overlapping
  windows. Most summaries cannot remove old data, so one summary cannot simply
  slide forward with the window.
* A **window summary** keeps summaries so that many windows can be answered.
  Three window summaries are considered for now:
  * **Sliding window:** summaries over windows of a fixed length L that start
    every s (the slide), so several windows are active at once. Each arriving
    sample is inserted into every active window that contains it, at the cost
    of more ingestion work and memory. A query window of length W is answered
    from completed windows:
    * **L = W:** each evaluation reads one completed window, with no merge.
      For a 5-min window evaluated every 1 min, L = 5 min and s = 1 min, so 5
      windows are active and each sample updates all 5. This works even for
      summaries that cannot be merged.
    * **L shorter than W:** the query window is covered by W / L
      non-overlapping completed windows, which are merged[^sliding-merge]. For
      example, a 10-min window evaluated every 1 min merges two 5-min windows
      with a 1-min slide. This needs a mergeable summary.

    L must divide W, and s must divide both L and the evaluation interval, so
    the windows a query needs have always just completed.
  * **Tumbling window:** back-to-back, non-overlapping windows of one fixed
    length, each with one summary; a sliding window whose slide equals its
    length. A longer query window is answered by
    merging the tumbling windows it covers. The tumbling length must divide
    both the query window length and the evaluation interval, so that every
    query window starts and ends on a tumbling boundary: a 5-min window
    evaluated every 1 min uses 1-min tumbling windows and merges exactly 5 of
    them. It needs a mergeable summary.
  * **Exponential Histogram (EH):** a sequence of EH buckets that covers a
    long history. A query window is answered by merging the EH buckets it
    covers. Few EH buckets cover a long history, at the cost that an old
    query-window boundary may fall inside an EH bucket and is then
    approximate.
    * An **EH bucket** is one non-overlapping time range of the history with
      one summary of the data in it. Unlike tumbling windows, EH buckets are
      not all the same length: they grow with age, so recent data sits in
      short EH buckets and older data in longer ones. Adjacent EH buckets are
      merged into a longer one as they age.

  TODO: evaluate other sliding-window frameworks for sketches as further
  window summaries, such as Smooth Histograms[^smooth-histograms],
  MicroscopeSketch[^microscope-sketch] and Sliding Sketches[^sliding-sketches].

| ASAP-aware CSE rule | Sharing condition | Shared computation |
|---|---|---|
| Identical-expression rule | The input and computation semantics are identical. | One common computation node serving multiple consumers. |
| Summary-capability rule | The computations have the same summary input data and the same window, and one summary supports all requested computations and their accuracy requirements. | One summary build node feeding several estimation nodes, e.g. UnivMon → distinct count, entropy, L2 norm. |
| Window-composition rule | The computations have the same summary input data, and one window summary can answer the requested windows within their accuracy requirements. | One window summary feeding per-query merge (where needed) and estimation nodes, e.g. a sliding-window or tumbling-window KLL, or an Exponential Histogram with a KLL per EH bucket. |

The examples behind these rules:

* **Summary-capability rule (Example 2).** One UnivMon over `src_ip` from
  `flows` in the last minute serves three queries refreshed every 10 s:
  `COUNT(DISTINCT src_ip)`, the entropy of the `src_ip` distribution, and the
  L2 norm of per-`src_ip` counts. Each flow record updates the UnivMon once; a
  distinct-count, an entropy and an L2 estimation node each compute their
  statistic from it. The UnivMon is sized for the strictest of the three accuracy
  requirements.
* **Window-composition rule, sliding or tumbling window (Example 3,
  Pattern B).** For `quantile_over_time(0.99, latency_ms[5m])` repeated every
  minute, one window summary serves every evaluation. With a sliding-window
  KLL, each sample updates the 5 active windows, and each evaluation reads the
  one that has just completed. With 1-min tumbling-window KLLs, each sample
  updates one window, and each evaluation merges the latest 5 with a merge
  node; consecutive evaluations share 4 of them.
* **Window-composition rule, Exponential Histogram (Example 3, Pattern A).**
  One Exponential Histogram over the last 5 years, with a KLL per EH bucket,
  serves the p99
  queries over `[5y]`, `[1y]`, `[1y] offset 1y`, `[1y] offset 2y` and
  `[3y] offset 2y`. Each query's merge node merges the EH buckets covering
  its interval, and its estimation node computes p99 from the merged KLL.
* **Other quantiles share for free.** One KLL answers every quantile, so adding
  `quantile_over_time(0.5, latency_ms[5m])` to the sliding-window dashboard
  adds only a p50 estimation node next to the p99 one, reading the same KLL,
  with no new summary.

Rules are defined by each summary family's capabilities and semantic
requirements. A shared summary must meet the strictest accuracy requirement
among its consumers. Applying a rule adds a shared candidate and keeps the
independent candidates, so selection can compare both.

### 2. Physical ASAP-aware optimization

Physical optimization turns each logical candidate into physical candidates.
It makes two ASAP-specific decisions, described below. Parallelism, partitioning
and resource management are TODO.

#### Materialization

Materialization decides, for each sub-DAG, whether its output is stored, on disk
or in memory, across (batch) query executions. Stage 2 makes this decision in three steps:

1. **Materialize or not.** A sub-DAG that is not materialized always runs at
   query time and keeps nothing.
2. **If materialized, ingestion time or query time.** This is when the output
   is computed. A sub-DAG that runs at ingestion time is therefore always
   materialized, because its output must be kept until a query reads it.
3. **Where and how long to store it:** on disk or in memory, and for how long.
   The deployment's cost model estimates the cost of each choice.

This gives each sub-DAG three options:

* **Materialized at ingestion time:** the sub-DAG runs as data arrives, and its
  output is stored before any query asks for it. For example, the 1-min
  tumbling-window KLLs in Example 4, Pattern B.
* **Materialized at query time:** the sub-DAG runs when a query first needs
  it, and its output is stored so that later executions, or other queries in
  the same batch, reuse it instead of recomputing it. For example, an
  Exponential Histogram built when the batch in Example 4, Pattern A runs and
  read by all five of its queries.
* **Not materialized:** the sub-DAG runs at query time for each execution, and
  its output is discarded afterward.

Whichever option is chosen for each sub-DAG, the plan must also satisfy these
constraints:

* A materialized output is stored for as long as any of its consumers still
  needs it.
* Every node upstream of an ingestion-time node also runs at ingestion time.

The decision depends on the workload's `recurrence` and `predictability` and on
the `DataWorkload`. Typical outcomes:

* Read by repeated queries while data keeps arriving: materialize at ingestion
  time.
* Read by several queries in one batch, or over data at rest: materialize at
  query time.
* Read once by an ad hoc query: do not materialize.

A shared summary is materialized once for all its consumers. See Example 4.

#### Physical operator implementation

Physical operator implementation converts every node to physical operators, for
example TopK as a sort followed by a limit, or a KLL node as summary build,
merge and quantile estimation operators.

### 3. Plan selection

Selection rejects every candidate that misses an accuracy target or a latency
bound, or that needs a capability the deployment lacks, and then picks the
cheapest remaining plan. It is the only stage that uses the deployment's cost
and accuracy models, and the only stage that discards valid candidates;
earlier stages only prune provably invalid ones. Accuracy is
estimated by the deployment's accuracy model, not assumed from a summary's
nominal bound. Cost is evaluated for the whole workload rather than per query,
which is what lets one shared summary beat several cheaper independent ones:
the cost of a shared summary is estimated once, with the demand of all its
consumers.

### 4. Execution

Execution runs outside ASAPPlanner. The deployment runs the selected plan as
given: it does not choose among summaries or decide what to materialize.

## End-to-end examples

Each example's workload is shown as tables. Field names in code font are the
fields of
[`workload.rs`](https://github.com/ProjectASAP/ASAPPlanner/blob/main/crates/types/src/workload.rs).
Each query reads the event-time window [`as_of` − `lookback`, `as_of`] (fields
of `TimeSelection`). `as_of` is the window's end; `lookback` is its length. An
`as_of` of "evaluation time" means `as_of: None`: the window ends whenever the
query runs, so it moves forward with each evaluation. A fixed `as_of`, such as
T − 1 y, pins the window to a historical interval. Approximate accuracy targets are `EpsilonDelta`: the answer's error
is at most ε with probability at least 1 − δ.

| Example | Shows |
|---|---|
| 1. Aggregation over dimensions | How the candidate set grows through every stage |
| 2. One summary for several computations | Pass 2: summary-capability rule |
| 3. Aggregation over windows | Pass 1 and Pass 2: window-composition rule |
| 4. Materialization of window summaries | Stage 2: materialization, decoupled from stage 1 |

In the diagrams below, grey cylinders are input data, white boxes are exact
operations and summary merges, blue boxes are summary build nodes, and green
rounded boxes are summary estimation nodes.

### Shared data workload

Unless an example says otherwise, every example uses this data workload:

| `DataWorkload` field | Meaning | Value |
|---|---|---|
| `arrival` | Whether the data is at rest, still arriving, or both | `continuously_ingesting` |
| `data_ingestion_interval` | How often each series delivers one sample (the scrape interval in Prometheus). PromQL uses it as the look-back horizon of instant selectors. | 15 s |
| `ingestion_volume` | Total amount of ingested data | unknown |
| `ingestion_rate` | Samples arriving per second across all series | about 66,667 samples/s |
| `input_cardinality` | Number of distinct series (or keys) | 1,000,000 series |
| `distribution` | How samples are spread over keys | `zipf` |

With 1,000,000 series each sampled every 15 s, the ingestion rate is
1,000,000 / 15 ≈ 66,667 samples/s.

### Example 1: Aggregation over dimensions — the candidate set through every stage

**Query workload.** Two PromQL dashboard panels over the last minute. The
first needs an exact total; the second tolerates error.

| Query | Repeats | `lookback` | `as_of` | Accuracy requirement | Latency requirement |
|---|---|---|---|---|---|
| Q1: `sum by (job) (rate(http_requests_total[1m]))` | every 10 s | 1 m | evaluation time | exact | none |
| Q2: `topk by (job) (10, sum_over_time(http_requests_total[1m]))` | every 10 s | 1 m | evaluation time | ε = 0.01, δ = 0.001 | ≤ 100 ms |

This example follows the workload's candidate set through every stage. Each
candidate covers both queries.

**Stage 0: 1 candidate.** The frontend converts both queries into one workload
`LogicalDAG` with no summaries:

```mermaid
flowchart TB
  subgraph Q1["Q1 · sum by (job) (rate(http_requests_total[1m]))"]
    direction LR
    x1[("http_requests_total")]:::data --> x2["range 1m"]:::exact --> x3["rate"]:::exact --> x4["sum by (job)"]:::exact
  end
  subgraph Q2["Q2 · topk by (job) (10, sum_over_time(http_requests_total[1m]))"]
    direction LR
    y1[("http_requests_total")]:::data --> y2["range 1m"]:::exact --> y3["sum_over_time"]:::exact --> y4["topk by (job) (10)"]:::exact
  end
  Q1 ~~~ Q2
  classDef data fill:#f1f3f4,stroke:#5f6368,color:#000;
  classDef exact fill:#fff,stroke:#5f6368,color:#000;
  classDef summary fill:#e8f0fe,stroke:#1a73e8,stroke-width:2px,color:#000;
  classDef estimate fill:#e6f4ea,stroke:#188038,color:#000;
```

**Stage 1, Pass 1: 3 candidates.** Pass 1 finds local options for each query:

* **Q1** has one option, the exact per-series rate and per-`job` sum. Its
  accuracy requirement is exact, so no summary qualifies.
* **Q2** has three options. The exact one keeps a per-series sum and sorts
  within each `job`, which is costly at one million Zipf-distributed series.
  The `EpsilonDelta` target also admits a **Count-Min Sketch with a top-*k*
  heap per `job`**, and **Hydra over the whole `job` column**, where one sketch
  covers every (`job`, series) key and a top-*k* heap is still kept per `job`.

```mermaid
flowchart LR
  subgraph C["Q2's three local options"]
    direction TB
    subgraph E["Exact"]
      direction LR
      e1[("input")]:::data --> e2["sum_over_time<br/>per series"]:::exact --> e3["sort + limit 10<br/>per job"]:::exact
    end
    subgraph CM["Count-Min + heap per job"]
      direction LR
      c1[("input")]:::data --> cg["group by job"]:::exact
      cg --> ca["Count-Min + heap<br/>job A"]:::summary --> ra(["top 10<br/>job A"]):::estimate
      cg --> cb["Count-Min + heap<br/>job B"]:::summary --> rb(["top 10<br/>job B"]):::estimate
      cg --> cn["… one instance<br/>per job"]:::summary --> rn(["top 10<br/>per job"]):::estimate
    end
    subgraph H["Hydra"]
      direction LR
      h1[("input")]:::data --> h2["one Hydra over all<br/>(job, series) keys<br/>+ heap per job"]:::summary --> h3(["top 10<br/>for each job"]):::estimate
    end
  end
  classDef data fill:#f1f3f4,stroke:#5f6368,color:#000;
  classDef exact fill:#fff,stroke:#5f6368,color:#000;
  classDef summary fill:#e8f0fe,stroke:#1a73e8,stroke-width:2px,color:#000;
  classDef estimate fill:#e6f4ea,stroke:#188038,color:#000;
```

Combining them gives 1 × 3 = 3 workload candidates:

| Logical candidate | Q1 | Q2 |
|---|---|---|
| Exact | exact | exact |
| Count-Min | exact | Count-Min + heap |
| Hydra | exact | Hydra |

**Stage 1, Pass 2: 54 candidates.** Pass 2 applies two ASAP-aware CSE rules
to each of the 3 candidates, and keeps every original:

* **Identical-expression rule.** Both queries read the same range selector,
  `http_requests_total[1m]`, so Pass 2 adds a variant in which Q1 and Q2 share
  one input node. No summary is shared, because Q1 must be exact and no
  summary supports both queries.
* **Window-composition rule.** Each query reads a 1-min window every 10 s, so
  consecutive evaluations overlap by 50 s. For each query, Pass 2 adds two
  variants: a **sliding window** with L = 1 min and a 10-s slide, where each
  sample updates the 6 active windows, and **10-s tumbling windows**, merged 6
  at a time at every refresh. (Shorter sliding windows that are merged, such
  as 30-s windows with a 10-s slide, would add more candidates; this example
  leaves them out.)
  Tumbling windows need a mergeable summary. Q1's rates and sums, Q2's exact
  sums and Hydra merge exactly; Count-Min sketches do too, but their top-10
  heaps merge only approximately, so that candidate is kept and the accuracy
  model judges it in stage 3.

Each Pass 1 candidate therefore has 2 input choices (separate or shared) ×
3 window forms for Q1 (none, sliding, tumbling) × 3 for Q2, so stage 1 outputs
3 × 2 × 3 × 3 = 54 logical candidates.

The window-composition rule applied to Q2 in the Hydra candidate:

```mermaid
flowchart LR
  subgraph NO["Hydra · no window summary"]
    direction LR
    a1[("http_requests_total")]:::data --> a2["range 1m"]:::exact --> a3["Hydra"]:::summary --> a4(["top 10"]):::estimate
  end
  subgraph SL["Hydra · sliding window"]
    direction LR
    b1[("http_requests_total")]:::data --> b2["insert each sample into<br/>6 active 1-min windows"]:::summary --> b3(["top 10 from the<br/>completed window"]):::estimate
  end
  subgraph TU["Hydra · 10-s tumbling windows"]
    direction LR
    c1[("http_requests_total")]:::data --> c2["Hydra per<br/>10-s window"]:::summary --> c3["merge latest 6"]:::exact --> c4(["top 10"]):::estimate
  end
  classDef data fill:#f1f3f4,stroke:#5f6368,color:#000;
  classDef exact fill:#fff,stroke:#5f6368,color:#000;
  classDef summary fill:#e8f0fe,stroke:#1a73e8,stroke-width:2px,color:#000;
  classDef estimate fill:#e6f4ea,stroke:#188038,color:#000;
```

**Stage 2: 156 candidates.** Stage 2 picks, for each query, when its state is
computed and whether it is kept. What it can choose depends on the query's
window form from Pass 2:

| Window form | Physical options |
|---|---|
| None | **Raw:** rebuild the state from the last 1 min of raw samples at every refresh. It cannot be materialized: the window slides every 10 s, and most summaries cannot drop old data. |
| Sliding window | **Sliding, ingestion time:** insert each sample into the 6 active windows as it arrives (materialized at ingestion time). **Sliding, query time:** at each refresh, insert the last 10 s of raw samples into the 6 kept active windows (materialized at query time). Building the window from raw samples at query time without keeping it is the same plan as Raw. |
| 10-s tumbling windows | **Tumbling, ingestion time:** build each tumbling window as samples arrive and keep the last 6. **Tumbling, query time:** at each refresh, build only the newest tumbling window from the last 10 s of raw samples and reuse the 5 kept ones. **Tumbling, rebuilt:** at each refresh, build all 6 tumbling windows from the last 1 min of raw samples, merge them, and discard them (not materialized). |

```mermaid
flowchart LR
  in[("http_requests_total<br/>samples")]:::data

  subgraph R["Raw"]
    direction LR
    subgraph RQ["Query time, every 10 s"]
      r1["last 1 min"]:::exact --> r2["build state"]:::summary --> r3(["answer"]):::estimate
    end
  end

  subgraph SI["Sliding, ingestion time"]
    direction LR
    subgraph SII["Ingestion time"]
      s1["update 6 active<br/>windows per sample"]:::summary
    end
    subgraph SIQ["Query time, every 10 s"]
      s2(["answer from the<br/>completed window"]):::estimate
    end
    s1 --> s2
  end

  subgraph SQ["Sliding, query time"]
    direction LR
    subgraph SQQ["Query time, every 10 s"]
      t1["last 10 s"]:::exact --> t2["update 6 kept<br/>active windows"]:::summary --> t3(["answer from the<br/>completed window"]):::estimate
    end
  end

  subgraph TI["Tumbling, ingestion time"]
    direction LR
    subgraph TII["Ingestion time"]
      u1["build 10-s window<br/>keep last 6"]:::summary
    end
    subgraph TIQ["Query time, every 10 s"]
      u2["merge 6"]:::exact --> u3(["answer"]):::estimate
    end
    u1 --> u2
  end

  subgraph TQ["Tumbling, query time"]
    direction LR
    subgraph TQQ["Query time, every 10 s"]
      v1["last 10 s"]:::exact --> v2["build newest<br/>10-s window"]:::summary --> v3["merge with<br/>5 kept"]:::exact --> v4(["answer"]):::estimate
    end
  end

  subgraph TR["Tumbling, rebuilt"]
    direction LR
    subgraph TRQ["Query time, every 10 s"]
      w1["last 1 min"]:::exact --> w2["build 6<br/>10-s windows"]:::summary --> w3["merge 6"]:::exact --> w4(["answer"]):::estimate
    end
  end

  in --> r1
  in --> s1
  in --> t1
  in --> u1
  in --> v1
  in --> w1
  classDef data fill:#f1f3f4,stroke:#5f6368,color:#000;
  classDef exact fill:#fff,stroke:#5f6368,color:#000;
  classDef summary fill:#e8f0fe,stroke:#1a73e8,stroke-width:2px,color:#000;
  classDef estimate fill:#e6f4ea,stroke:#188038,color:#000;
```

Across the three window forms, each query has 1 + 2 + 3 = 6 physical options,
so each Q2 option gives 6 × 6 = 36 combinations. The shared-input variant only
changes the plan when both queries read raw samples at query time, which 4 of
the 6 options do (all except the two ingestion-time ones). That adds
4 × 4 = 16 shared-input plans, for 52 plans per Q2 option and
3 × 52 = 156 physical candidates.

**Stage 3: 1 plan.** Selection first rejects invalid candidates. The
deployment's accuracy model checks the summary candidates against ε = 0.01,
δ = 0.001, including the approximate heap merge of Count-Min with tumbling
windows. Its cost model estimates Q2's latency against the 100 ms bound; for
example, an exact top 10 rebuilt from one million series at every refresh may
miss it. Among the rest, selection picks the cheapest plan for the whole
workload. Typical winners:

* **Hydra for Q2, with both queries on tumbling windows at ingestion time**,
  when there are many small jobs: each sample updates one window, and each
  refresh merges 6 small summaries.
* **Count-Min + heap for Q2 on a sliding window at ingestion time**, with Q1
  on tumbling windows, when there are a few large jobs: the heaps do not merge
  cleanly, so updating 6 active windows per sample is worth it.
* **Query-time variants** of either, when ingestion-time work is expensive.
* **Raw with a shared input** for both queries, when storage is expensive and
  raw data is available at query time.

The other candidates, such as tumbling windows rebuilt at every refresh, stay
in the candidate set because they are valid, and selection rules them out on
cost.

### Example 2: One summary for several computations — the summary-capability rule in Pass 2

**Query workload.** A network-monitoring dashboard computes three statistics of
source IPs over the last minute.

```sql
-- Q1: Distinct(src_ip)
SELECT COUNT(DISTINCT src_ip)
FROM flows
WHERE ts >= now() - INTERVAL '1 minute';

-- Q2: Entropy(src_ip)
SELECT -SUM(p * LN(p))
FROM (
  SELECT COUNT(*) * 1.0 / SUM(COUNT(*)) OVER () AS p
  FROM flows
  WHERE ts >= now() - INTERVAL '1 minute'
  GROUP BY src_ip
);

-- Q3: L2(src_ip)
SELECT SQRT(SUM(c * c))
FROM (
  SELECT src_ip, COUNT(*) AS c
  FROM flows
  WHERE ts >= now() - INTERVAL '1 minute'
  GROUP BY src_ip
);
```

The SQL frontend does not yet recognize the Q2 and Q3 forms as `Entropy` and
`L2`. TODO: add this recognition to the per-language frontends.

| Query | Computation | Repeats | `lookback` | `as_of` | Accuracy requirement |
|---|---|---|---|---|---|
| Q1 | `Distinct(src_ip)` | every 10 s | 1 m | evaluation time | ε = 0.02, δ = 0.01 |
| Q2 | `Entropy(src_ip)` | every 10 s | 1 m | evaluation time | ε = 0.05, δ = 0.01 |
| Q3 | `L2(src_ip)` | every 10 s | 1 m | evaluation time | ε = 0.01, δ = 0.01 |

The data workload differs from the shared one in two fields:

| `DataWorkload` field | Value |
|---|---|
| `input_cardinality` | 10,000,000 distinct source IPs |
| `data_ingestion_interval` | not needed for SQL |

**Pass 1.** Rewrite rules recognize the three computations, and each gets its
local candidates from the Pass 1 table: exact, a specialized summary, or
UnivMon. Combined, that is 3 × 3 × 3 = 27 workload candidates.

**Pass 2.** All three computations have the same summary input data (`src_ip`
from `flows`, no other filter) and the same 1-min window. UnivMon supports all three
estimates, so the summary-capability rule adds a shared candidate: **one
UnivMon build node feeding three estimation nodes**. It must be sized for the strictest
requirement, ε = 0.01. The independent candidates are kept as well. Pass 2
also adds candidates where only two of the three share a UnivMon and the third
keeps any of its own 3 options (3 pairs × 3 = 9), so stage 1 outputs
27 + 1 + 9 = 37 candidates. The figure shows the all-three case. The
window-composition rule would also add sliding-window and 10-s tumbling-window
variants, exactly as in Example 1; they are left out here to keep the focus on
the summary-capability rule.

```mermaid
flowchart LR
  in[("flows.src_ip<br/>last 1m")]:::data

  subgraph P0["Stage 0 · LogicalDAGs"]
    direction TB
    q1["Distinct(src_ip)"]:::exact
    q2["Entropy(src_ip)"]:::exact
    q3["L2(src_ip)"]:::exact
  end

  subgraph P1["Stage 1, Pass 1 · local candidates per computation"]
    direction TB
    subgraph D["Distinct"]
      direction LR
      d0["exact distinct"]:::exact
      d1["distinct summary"]:::summary
      d2["UnivMon"]:::summary
    end
    subgraph E["Entropy"]
      direction LR
      e0["exact entropy"]:::exact
      e1["entropy summary"]:::summary
      e2["UnivMon"]:::summary
    end
    subgraph L["L2"]
      direction LR
      l0["exact L2"]:::exact
      l1["norm summary"]:::summary
      l2["UnivMon"]:::summary
    end
  end

  subgraph P2["Stage 1, Pass 2 · summary-capability rule adds a shared candidate"]
    direction LR
    u["one UnivMon<br/>sized for ε = 0.01"]:::summary
    u --> rd(["distinct count"]):::estimate
    u --> re(["entropy"]):::estimate
    u --> rl(["L2 norm"]):::estimate
  end

  in --> P0
  q1 --> D
  q2 --> E
  q3 --> L
  d2 -. "same summary input data<br/>and window" .-> u
  e2 -.-> u
  l2 -.-> u

  OUT[["CandidateLogicalASAPDAGs:<br/>27 Pass 1 candidates + 10 shared candidates"]]
  P1 --> OUT
  P2 --> OUT
  classDef data fill:#f1f3f4,stroke:#5f6368,color:#000;
  classDef exact fill:#fff,stroke:#5f6368,color:#000;
  classDef summary fill:#e8f0fe,stroke:#1a73e8,stroke-width:2px,color:#000;
  classDef estimate fill:#e6f4ea,stroke:#188038,color:#000;
```

The three UnivMon options from Pass 1 (dashed arrows) are merged by Pass 2 into
one shared UnivMon. The independent candidates are kept, so stage 1 outputs
both kinds.

**Stage 2.** The same options as in Example 1 apply: a summary without a
window summary is rebuilt from raw samples at every refresh, while a
sliding-window or tumbling-window UnivMon can be kept from ingestion time or
from query time (and tumbling windows can also be rebuilt). Because the
dashboard repeats over arriving data, ingestion-time window summaries are
usually cheapest. UnivMon merges exactly, so tumbling windows suit it.

**Stage 3.** Selection compares one UnivMon sized for ε = 0.01 against three
separate summaries, each sized for its own requirement. The shared candidate
usually wins because each flow record updates one summary instead of three.

### Example 3: Aggregation over windows — the window-composition rule in Pass 2

This example has two workload patterns that both lead to a shared window
summary.

**Pattern A: a batch of sub-interval queries over historical data.** An analyst
submits a batch of p99 latency reports over different historical intervals,
all executed together at time T.

| Workload field | Value (all five queries) |
|---|---|
| Entry type | `query_batch`, run once (`invocations: 1`) at T |
| `predictability` | `ad_hoc` |
| Accuracy requirement | ε = 0.005, δ = 0.01 |

| Query | `lookback` | `as_of` |
|---|---|---|
| `quantile_over_time(0.99, latency_ms[5y])` | 5 y | T |
| `quantile_over_time(0.99, latency_ms[1y])` | 1 y | T |
| `quantile_over_time(0.99, latency_ms[1y] offset 1y)` | 1 y | T − 1 y |
| `quantile_over_time(0.99, latency_ms[1y] offset 2y)` | 1 y | T − 2 y |
| `quantile_over_time(0.99, latency_ms[3y] offset 2y)` | 3 y | T − 2 y |

The data workload is the shared one, except `arrival` is `mixed`: five years
of data at rest, plus data still arriving.

* **Pass 1.** Each `quantile_over_time` gets an exact candidate and a KLL over
  its own interval: five independent KLL candidates over overlapping data.
* **Pass 2.** Every interval is a sub-interval of [T − 5 y, T], and KLL is
  mergeable. The window-composition rule adds a shared candidate: **one
  Exponential Histogram over [T − 5 y, T], with a KLL per EH bucket**, with one
  merge and estimation node per query that merges the EH buckets covering
  its interval. The independent candidates are kept.

Yearly tumbling windows would also work here, since every interval is a whole
number of years; the Exponential Histogram is shown because it also handles
intervals that are not. Counted at the workload level, Pass 1 gives each query
2 options (exact or KLL), so 2⁵ = 32 candidates, and Pass 2 adds one candidate
for every way of grouping two or more queries onto shared window summaries.
That quickly reaches hundreds of candidates, which is why candidate sets may be
enumerated lazily.

The five query intervals overlap, and all lie inside the last five years:

```mermaid
gantt
  title Pattern A · query intervals (T = batch execution time)
  dateFormat YYYY
  axisFormat %Y
  section Queries
  q1 · [5y]            :q1, 2021, 2026
  q2 · [1y]            :q2, 2025, 2026
  q3 · [1y] offset 1y  :q3, 2024, 2025
  q4 · [1y] offset 2y  :q4, 2023, 2024
  q5 · [3y] offset 2y  :q5, 2021, 2024
```

The shared candidate replaces five KLL sketches with one Exponential Histogram
and a merge and estimation node per query:

```mermaid
flowchart LR
  in[("latency_ms<br/>T − 5y to T")]:::data --> eh["Exponential Histogram<br/>KLL per EH bucket"]:::summary
  eh --> m1["merge EH buckets<br/>T−5y … T"]:::exact --> o1(["q1 p99"]):::estimate
  eh --> m2["merge EH buckets<br/>T−1y … T"]:::exact --> o2(["q2 p99"]):::estimate
  eh --> m3["merge EH buckets<br/>T−2y … T−1y"]:::exact --> o3(["q3 p99"]):::estimate
  eh --> m4["merge EH buckets<br/>T−3y … T−2y"]:::exact --> o4(["q4 p99"]):::estimate
  eh --> m5["merge EH buckets<br/>T−5y … T−2y"]:::exact --> o5(["q5 p99"]):::estimate
  classDef data fill:#f1f3f4,stroke:#5f6368,color:#000;
  classDef exact fill:#fff,stroke:#5f6368,color:#000;
  classDef summary fill:#e8f0fe,stroke:#1a73e8,stroke-width:2px,color:#000;
  classDef estimate fill:#e6f4ea,stroke:#188038,color:#000;
```

**Pattern B: one repeating query with overlapping windows.** A real-time p99 panel
over the last 5 min, refreshed every minute.

| Query | Repeats | `lookback` | `as_of` | Accuracy requirement | Latency requirement |
|---|---|---|---|---|---|
| `quantile_over_time(0.99, latency_ms[5m])` | every 1 min | 5 m | evaluation time | ε = 0.01, δ = 0.01 | ≤ 200 ms |

* **Pass 1.** One KLL over 5 min for each evaluation.
* **Pass 2.** Consecutive evaluations overlap by 4 of their 5 minutes. The
  window-composition rule adds two shared candidates for each Pass 1 option:
  a **sliding window**, where each sample updates the 5 active 5-min windows,
  and **1-min tumbling windows**, where each evaluation merges the latest 5.
  With exact and KLL from Pass 1, each in 3 window forms (none, sliding,
  tumbling), stage 1 outputs 2 × 3 = 6 candidates.

With 1-min tumbling windows, each evaluation merges five of them, and
consecutive evaluations share four:

```mermaid
gantt
  title Pattern B · 1-min tumbling KLL windows and 5-min evaluations
  dateFormat HH:mm
  axisFormat %H:%M
  section 1-min tumbling windows
  window 1 :p1, 00:00, 1m
  window 2 :p2, 00:01, 1m
  window 3 :p3, 00:02, 1m
  window 4 :p4, 00:03, 1m
  window 5 :p5, 00:04, 1m
  window 6 :p6, 00:05, 1m
  window 7 :p7, 00:06, 1m
  section Evaluations
  eval at 00:05 (windows 1–5) :e1, 00:00, 5m
  eval at 00:06 (windows 2–6) :e2, 00:01, 5m
  eval at 00:07 (windows 3–7) :e3, 00:02, 5m
```

Example 4 shows how stage 2 decides whether to store these window summaries.

### Example 4: Materialization of window summaries in physical planning

Stage 2 takes the shared window summaries from Example 3 and decides whether to
materialize them. That choice is driven by the workload's `recurrence`,
`predictability` and `data_workload.arrival`. This example shows only the
physical candidates of the shared logical candidate; every other logical
candidate from Example 3 gets its own physical candidates the same way.

**Pattern A (sub-interval batch).**

| Candidate | Materialized | When the Exponential Histogram is built |
|---|---|---|
| A1 | The Exponential Histogram, at query time | At query time, when the batch runs at T; read by all five queries, then discarded |
| A2 | The Exponential Histogram, at ingestion time | At ingestion time, with each new sample; old data backfilled once |
| A3 | Nothing | At query time, once per query: each of the five queries rebuilds it for itself and discards it |

```mermaid
flowchart LR
  subgraph A1["A1 · materialized at query time"]
    direction LR
    subgraph A1Q["Query time, once at T"]
      s3[("5 years of<br/>stored samples")]:::data --> h3["build Exponential<br/>Histogram once"]:::summary
      h3 --> r3(["q1 … q5<br/>estimates"]):::estimate
    end
  end
  subgraph A2["A2 · materialized at ingestion time"]
    direction LR
    subgraph A2I["Ingestion time, continuously"]
      s4[("each new sample<br/>+ one-time backfill")]:::data --> h4["maintain Exponential<br/>Histogram"]:::summary
    end
    subgraph A2Q["Query time, at T"]
      r4(["q1 … q5<br/>estimates"]):::estimate
    end
    h4 --> r4
  end
  subgraph A3["A3 · not materialized"]
    direction LR
    subgraph A3Q["Query time, once per query at T"]
      s6[("5 years of<br/>stored samples")]:::data --> h6["build Exponential<br/>Histogram, ×5"]:::summary --> r6(["one estimate<br/>per rebuild"]):::estimate
    end
  end
  classDef data fill:#f1f3f4,stroke:#5f6368,color:#000;
  classDef exact fill:#fff,stroke:#5f6368,color:#000;
  classDef summary fill:#e8f0fe,stroke:#1a73e8,stroke-width:2px,color:#000;
  classDef estimate fill:#e6f4ea,stroke:#188038,color:#000;
```

Which candidate wins depends on the workload:

* **As given** (`invocations: 1`, `ad_hoc`): selection picks A1. A2 would
  maintain the histogram for years only to serve one batch, and A3 builds it
  five times instead of once.
* **Repeated monthly and `Predictable { known_at }`:** A2 can win, because its
  maintenance cost is shared by many batches.
* **Data `"at_rest"`:** A2 is not generated, because there is no ingestion to
  maintain the histogram.

**Pattern B (overlapping windows, repeating).**

| Candidate | Materialized | At ingestion time | At query time |
|---|---|---|---|
| B1 | 1-min tumbling KLLs, kept 5 min | Build one tumbling KLL per minute | Merge the latest 5, read p99 |
| B2 | Nothing | Nothing | Read 5 min of raw samples, rebuild all 5 tumbling KLLs, merge them, read p99 |
| B3 | 1-min tumbling KLLs, at query time, kept 5 min | Nothing | Build only the newest tumbling KLL from raw samples, merge it with the 4 kept ones, read p99 |

```mermaid
flowchart LR
  subgraph B1["B1 · tumbling KLLs materialized at ingestion time"]
    direction LR
    subgraph B1I["Ingestion time"]
      s1[("samples")]:::data --> p1["1-min tumbling KLL<br/>kept 5 min"]:::summary
    end
    subgraph B1Q["Query time, every 1 min"]
      g1["merge latest 5"]:::exact --> r1(["p99"]):::estimate
    end
    p1 --> g1
  end
  subgraph B2["B2 · not materialized"]
    direction LR
    subgraph B2Q["Query time, every 1 min"]
      s2[("5 min of<br/>raw samples")]:::data --> k2["rebuild 5<br/>tumbling KLLs"]:::summary --> m2b["merge 5"]:::exact --> r2(["p99"]):::estimate
    end
  end
  subgraph B3["B3 · tumbling KLLs materialized at query time"]
    direction LR
    subgraph B3Q["Query time, every 1 min"]
      s5[("last 1 min of<br/>raw samples")]:::data --> k5["build newest<br/>1-min tumbling KLL"]:::summary --> g5["merge with<br/>4 kept"]:::exact --> r5(["p99"]):::estimate
      kp["4 kept tumbling KLLs<br/>from earlier evaluations"]:::summary --> g5
    end
  end
  classDef data fill:#f1f3f4,stroke:#5f6368,color:#000;
  classDef exact fill:#fff,stroke:#5f6368,color:#000;
  classDef summary fill:#e8f0fe,stroke:#1a73e8,stroke-width:2px,color:#000;
  classDef estimate fill:#e6f4ea,stroke:#188038,color:#000;
```

This table covers the tumbling-window candidate. The query repeats every
minute and the data is continuously ingesting, so B1 builds each tumbling KLL
once and reuses it in five evaluations, while B2 rescans raw data every time.
B3 also builds each tumbling KLL once, but at query time, so it needs raw data
at query time and adds the newest build to each evaluation's latency.
Selection usually picks B1. B3 can win when ingestion-time work is expensive,
and B2 only when storage is expensive and raw data is available at query time.

The sliding-window candidate from Example 3 gets its own physical candidates
the same way: kept from ingestion time (each sample updates the 5 active
windows) or from query time (each evaluation inserts the last minute of raw
samples into the 5 kept windows). It does more ingestion work than B1 but
needs no merge, so it wins only for a summary that merges poorly.

**What this shows.** The same logical candidate (one shared Exponential
Histogram, or 1-min tumbling KLL windows) yields different physical plans depending
only on recurrence, predictability and data arrival. This is why window-summary
replacement happens in logical planning, while materialization is decided
separately in physical planning.

## Scenarios

Each scenario lists what a developer or user gives (Assumptions 1–4), what
ASAPPlanner then does automatically, and what does not change. "Today" notes
where the implementation does not yet match this design.

### Adding a new query to a workload

**Given:** one more workload entry: the query, its recurrence (repeating demand
or batch), predictability, time selection and accuracy and latency
requirements. No code changes.

**Automatic:** the frontend converts the query (stage 0). Pass 1 generates its
exact and summary candidates from the existing rules and capabilities. Pass 2
checks whether it can share a summary with the queries already in the workload.
Physical planning and selection re-plan the whole workload, so adding a query can
change the plan of other queries, for example when a summary becomes shared.

**Unchanged:** rules, summary families, models and every other workload entry.

### Supporting a new query construct

For a function, operator or aggregate a frontend does not support yet.

**Given:**

1. The frontend conversion to the common IR (stage 0), or an explicit
   rejection.
2. If it is a new computation, its semantics in the IR (an `AggIntent`), and
   whether it is exact-only, mergeable, or approximable.
3. If it is approximable, the rewrite rules and the summary families that may
   answer it (Assumptions 1 and 2).
4. Its exact physical operator implementation.

**Automatic:** everything from Pass 1 onward, as for any other query.

**Unchanged:** the stages, the selection rule and other queries' candidates.

### Adding a new summary family

For example, a new quantile sketch.

**Given** (Assumption 2):

1. The family and its parameters.
2. The computations it answers and the estimates it reads out.
3. Its sizing rule for an accuracy target and its error bound.
4. Whether its states can be merged, subtracted or deleted.
5. Which state fields identify it, so that equal states are shared in Pass 2.
6. Its physical kernel: build, merge and estimate.

**Automatic:** Pass 1 offers the family wherever its declared computations
appear, sizes it per query and prunes it where it cannot meet the accuracy
target. Pass 2 shares it across queries, sized for the strictest consumer.
Selection compares it with every other candidate using the deployment's models.

**Unchanged:** frontends, rewrite rules, the stages and existing queries.

**Today:** the declarations are spread over several places (candidate table,
sizing and guarantee estimators, readout mapping, state naming, kernel and
physical capability lists); see
[extend-asap-aware-mapping.md](../../develop_docs/extend-asap-aware-mapping.md).
A family needs both a planner-side guarantee and a native kernel to be
selected and executed
([#523](https://github.com/ProjectASAP/ASAPPlanner/issues/523),
[#524](https://github.com/ProjectASAP/ASAPPlanner/issues/524)).

### Adding a better cost or accuracy estimation

**Given:** a cost model or accuracy model supplied by the deployment, for
example one fitted to its own measurements.

**Automatic:** only selection changes (stage 3). The candidate sets of stages
0–2 stay the same, except where a model also changes sizing or admits a
summary that has no built-in guarantee. The model is used for every
query in the workload, and a shared summary is costed once.

**Unchanged:** frontends, rules, summary families and the deployment's
execution.

**Today:** a deployment's accuracy model is consulted only for the final
check, not when Pass 1 admits candidates
([#523](https://github.com/ProjectASAP/ASAPPlanner/issues/523)); the input
structure for deployment inputs is TODO
([#525](https://github.com/ProjectASAP/ASAPPlanner/issues/525)).


[^smooth-histograms]: V. Braverman and R. Ostrovsky. [Smooth Histograms for Sliding Windows](https://web.cs.ucla.edu/~rafail/PUBLIC/82.pdf). FOCS 2007. An alternative to EH.
[^microscope-sketch]: Y. Wu et al. [MicroscopeSketch: Accurate Sliding Estimation Using Adaptive Zooming](https://yangtonghome.github.io/uploads/MicroscopeSketch_SIGKDD_23_final_paper.pdf). KDD 2023.
[^sliding-sketches]: X. Gou et al. [Sliding Sketches: A Framework using Time Zones for Data Stream Processing in Sliding Windows](https://dl.acm.org/doi/10.1145/3394486.3403144). KDD 2020.
[^sliding-merge]: A. Arasu and G. S. Manku. [Approximate Counts and Quantiles over Sliding Windows](https://dl.acm.org/doi/10.1145/1055558.1055598). PODS 2004.
