# ASAPPlanner Layering Design

Status: proposal. Audience: designers and developers of ASAPPlanner and of deployments such
as ASAPQuery-backend.

## Goal

ASAPPlanner takes a query workload, a data workload and the deployment's
inputs, and returns one optimal physical plan. It decides what is computed, how
it is computed, and which plan is best. The deployment only supplies inputs and
executes the plan: it supplies its own empirical cost estimation, empirical accuracy estimation and capabilities of deployment but never does the query planning or plan selection.

## Layers

```text
  Query workload (PromQL / SQL / MetricsQL, query repeating pattern, accuracy requirements, query latency requirement)
  + Data workload (data arrival pattern: streaming data vs data at rest, data distribution, cardinality) 
  + Deployment inputs: empirical cost estimation, empirical accuracy estimation, capabilities
                       │
┌───────────────────────┴───────── ASAPPlanner ────────────────────┐
│ 0. Frontends                                                     │
│    Parse + lower -> CandidatePreASAPDAGs                         │
│    Reject unsupported constructs, such as PromQL fill.           │
│                      │                                           │
│ Logical planning (what)                                          │
│ 1. Logical optimization                                          │
│    Summary families, rewrites, exact candidates                  │
│    -> CandidateLogicalPostASAPDAGs                               │
│                      │                                           │
│ Physical planning (how)                                          │
│ 2. Summary lifecycle planning                                    │
│    Per summary state: Ephemeral | Prepared | Shared |            │
│    ContinuouslyMaintained -> node timing, window framework,      │
│    retention -> CandidateLifecyclePostASAPDAGs                   │
│                      │                                           │
│ 3. Compilation                                                   │
│    Lower each node to operators; cut by timing                   │
│    -> CandidatePhysicalPostASAPDAGs                              │
│                      │                                           │
│ 4. Selection                                                     │
│    Cost every candidate with the deployment's cost model;        │
│    choose the cheapest admissible one for the workload.          │
└──────────────────────┬───────────────────────────────────────────┘
          one optimal PhysicalPostASAPDAG (the boundary)
┌──────────────────────┴──────── Deployment ───────────────────────┐
│ 5. Execution: ingest, panes, storage, readout, run               │
└──────────────────────────────────────────────────────────────────┘
```

Layers 0 to 3 each output a candidate set, `Candidate<DAG>s`, holding every
legal candidate of their stage (for example KLL and DDSketch for one quantile,
or each lifecycle assignment), and selection is the only step that chooses. Candidate sets are internal to ASAPPlanner: they may
be shared or enumerated lazily, and the deployment never sees them. Unsupported
or infeasible candidates are rejected with reasons, not silently dropped.

## DAGs and what each encodes

Each stage adds decisions to the DAG it receives. The table shows which
decisions each DAG carries.

| | `PreASAPDAG` | `LogicalPostASAPDAG` | `LifecyclePostASAPDAG` | `PhysicalPostASAPDAG` |
|---|---|---|---|---|
| Produced by | 0. Frontends | 1. Logical optimization | 2. Summary lifecycle planning | 3. Compilation; 4. selects one |
| Node | Query operation | Logical operation, including summary operations | Same, plus annotations | Physical operator |
| Logical optimization (summary family, rewrites) | No | Yes | Yes | Yes |
| Materialization decided (which summary states persist) | No | No | Yes | Yes |
| Data lifecycle (how each state is maintained: `Ephemeral`, `Prepared`, `Shared`, `ContinuouslyMaintained`) | No | No | Yes | Yes |
| Retention (how long each state lives) and window framework | No | No | Yes | Yes |
| Execution time (ingestion or query) | No | No | Yes, per node | Yes, as the precompute / query split |
| Physical optimization (operator choice, e.g. TopK as sort + limit) | No | No | No | Yes |
| Seen by the deployment | No | No | No | Only the selected one |

Name mapping to code:

| Design name | Current main | Target API (open PRs #508, #480) |
|---|---|---|
| `PreASAPDAG` | `Rc<QueryExpr>` | `PreASAPDAG` |
| `LogicalPostASAPDAG` | `Rc<SummaryNode>` tree; exported as `PostAsapDag` | `LogicalPostASAPDAG` |
| `LifecyclePostASAPDAG` | `SummaryMaintenanceLifecyclePlan`, one selected assignment beside the DAG | `LifecyclePostASAPDAG`; `SummaryMaintenanceLifecyclePlan` is merged into it |
| `PhysicalPostASAPDAG` | None | `PhysicalPostASAPDAG` |
| `CandidatePreASAPDAGs` | None; one `QueryExpr` root per entry | `CandidatePreASAPDAGs` |
| `CandidateLogicalPostASAPDAGs` | `PlanSpace` | `CandidateLogicalPostASAPDAGs` |
| `CandidateLifecyclePostASAPDAGs` | None | `CandidateLifecyclePostASAPDAGs` |
| `CandidatePhysicalPostASAPDAGs` | None | `CandidatePhysicalPostASAPDAGs` |

**Post-ASAP DAG to lifecycle DAG.** A summary state's *lifecycle*
(`Ephemeral`, `Prepared`, `Shared`, `ContinuouslyMaintained`) fixes several
separate aspects together: whether the state is materialized (kept across
executions, like a materialized view), when it is computed, how it is
maintained, how long it is retained, and its window framework. The table above
lists these aspects separately; the lifecycle is the one choice that sets them.
Choosing lifecycles is a workload-level decision, like a database's
materialized-view selection: it spans queries (a shared state is kept once) and
depends on workload demand (read and update rates, horizon). A lifecycle
assignment annotates every node with execution timing, and every stored state
with window framework and retention:

* a retained state (`ContinuouslyMaintained`, `Shared`, `Prepared`) and every
  node feeding it run at ingestion time;
* readouts, other consumers and `Ephemeral` states run at query time;
* an `Ephemeral` state that feeds a retained state runs at ingestion time,
  because query-time work may not feed ingestion-time work.

The result is still a Post-ASAP DAG, much as physical properties annotate
logical expressions in a database optimizer. This is the only source of
timing; logical optimization proposes computations, never timing or placement.

**Lifecycle DAG to physical DAG.** Compilation lowers each node to physical
operators and cuts the graph at the timing frontier (ingestion-time nodes read
by query-time nodes, plus an ingestion-time root) into a precompute and a query
DAG. Materialization is decided in layer 2 and realized here: the precompute
DAG's outputs at the cut are the materialized states, and the query DAG reads
them through typed input slots. A physical DAG corresponds to its
Post-ASAP DAG node by node; a node may expand into several operators, whose
helper operators are numbered from their source node. The one exception, a
`Fallback` node wrapping a whole Pre-ASAP expression, is removed by the
operator-flattening proposal ([operator sharing](operator-sharing.md), #469,
#481). Operator materialization (sort, aggregation, summary build) and
computing a shared subexpression once are compilation and runtime details.

Binding runtime sources is an execution step of a `PhysicalPostASAPDAG`, not
another DAG: the deployment supplies a source for each typed input slot, the
slots are checked against their contracts, and the graph runs.

## Responsibilities

| Layer | Owns | Does not own |
|---|---|---|
| 0. Frontends | Language semantics and lowering. A construct that cannot be represented faithfully is rejected, never ignored (for example PromQL `fill`). | Summaries, placement |
| 1. Logical optimization | All legal logical candidates: summary families, exact rewrites, compositions, series-identity typing. | Placement, timing |
| 2. Summary lifecycle planning | For each unique summary state and maintained population, the admissible lifecycle assignments and their timing, window framework and retention. | Cost values; operator implementation |
| 3. Compilation | All computation: value operations, aggregation, PromQL functions and subqueries, vector matching, comparisons and set operators, `histogram_quantile`, summary build, merge and estimate, sort, limit, joins. | Raw ingestion, pane construction, storage formats, decoding persisted state, scheduling |
| 4. Selection | Costing every candidate with the deployment's cost model and returning the cheapest admissible `PhysicalPostASAPDAG` for the whole workload that meets the accuracy requirements. A state shared by several queries is costed once with all consumers' demand (only when compilation installs one shared output: same window layout, evaluation interval and phase). Unknown cost stays unknown and such a candidate is not selected. | The cost values |
| 5. Deployment | Inputs: the cost model (build, per-update maintenance, read, store price per byte-second, retirement, query-time raw processing; optionally whole-plan quotes), accuracy requirements and capabilities (for example whether query-time raw data is available). Execution: ingestion and routing, panes and completeness, lateness and revisions, storage and codecs over Planner kernel states, reading stored state into typed inputs, query-time raw sources, the exact-engine fallback. Sampled or delta edge frames are rejected. | Any computation algorithm |

## The boundary

The deployment passes its inputs to ASAPPlanner and receives one optimal
`PhysicalPostASAPDAG`, which contains:

* a **precompute DAG**, whose inputs are raw-sample contracts (rows carrying
  series labels, timestamp and value; the label set is the complete series
  identity) and whose outputs are typed summary states;
* a **query DAG**, whose inputs are stored-state contracts, query-time
  raw-series contracts, or both;
* the lifecycle, window framework and retention of every stored output.

The deployment binds each input contract, stores each precompute output under
its own storage identity, and returns the query DAG's result. Semantic identity
of stored outputs is defined by the logical DAG they compute. Storage identity
and encoding belong to the deployment.

## Example

This traces `sum by (job) (rate(m[1m]))`, evaluated every 10 s, through the
four DAGs, and shows that only the deployment's store price changes the plan.

* `PreASAPDAG`: `sum by (job)` over `rate` over the range selector `m[1m]`.
* `LogicalPostASAPDAG`: a per-series Rate state feeding a grouped Sum state.
* `LifecyclePostASAPDAG`: one per lifecycle assignment, for example
  (a) both retained, (b) Rate retained and Sum `Ephemeral`, (c) both
  `Ephemeral`.
* `PhysicalPostASAPDAG`: one compilation, cut three ways. (a) Precompute builds
  Rate and Sum per pane; the query only reads Sum. (b) Precompute keeps Rate;
  the query builds Sum. (c) No precompute; the query reads raw series at `t_q`.

Selection returns (a) when storage is cheap, (b) when it is expensive, and (c)
when it is more expensive still. The deployment only changed its store price.
