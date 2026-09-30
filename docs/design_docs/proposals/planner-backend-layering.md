# Planner and deployment layering

Status: proposal. Most of it is implemented in open pull requests; see
[Implementation status](#implementation-status). Audience: designers of
ASAPPlanner and of deployments such as ASAPQuery-backend.

## Goal

A deployment selects and executes plans that ASAPPlanner compiled. ASAPPlanner
decides what can be computed and how it is computed. The deployment decides
where each piece runs, based on its own costs. It supplies data and state and
runs the compiled plans. It never re-derives the computation or keeps its own
operators.

## Layers

```text
                  PromQL / SQL / MetricsQL
                       │
┌───────────────────────── ASAPPlanner ────────────────────────────┐
│ 0. Frontends                                                     │
│    PromQL / SQL / MetricsQL: parse + lower -> PreASAPDAG         │
│    Reject unsupported constructs, such as PromQL fill.           │
│                      │                                           │
│ 1. Logical Post-ASAP (asap-aware-mapping)                        │
│    PlanSpace: WHAT to compute; no placement.                     │
│    Summary families, rewrites, exact candidates                  │
│    -> candidate PostASAPDAGs                                     │
│                      │                                           │
│ 2. Summary maintenance lifecycle                                 │
│    Per unique summary state / maintained population:             │
│    Ephemeral | Prepared | Shared | ContinuouslyMaintained        │
│    Chosen assignment -> node timing, window framework,           │
│    retention. The only source of timing.                         │
│    -> timed PostASAPDAG                                          │
│                      │                                           │
│ 3. Physical compile (asap-physical-operators)                    │
│    Compile once -> PhysicalDAG                                   │
│    Cut by timing -> PhysicalCandidate                            │
│    { precompute DAG, query DAG, typed InputContracts }           │
│    Operators + kernels implement all computation.                │
└──────────────────────┬───────────────────────────────────────────┘
          physical plan / DAG (the boundary)
┌──────────────────────┴──────── Deployment ──────────────┐
│ 4 Selection       price lifecycle assignments; choose    │
│ 5 Execution       ingest, panes, storage, readout, run   │
└─────────────────────────────────────────────────────────┘
```

### DAG names

This design uses three names consistently. They describe graph artifacts,
not three alternative outputs of the same stage.

| Design name | Meaning | Current Rust representation |
|---|---|---|
| `PreASAPDAG` | Frontend-lowered query semantics before summary rewrites | A graph rooted at `Rc<QueryExpr>` |
| `PostASAPDAG` | A logical computation candidate; the lifecycle assignment adds timing to this same logical graph | A graph rooted at `Rc<SummaryNode>`; exported as `PostAsapDag` for physical compilation |
| `PhysicalDAG` | Compiled operators and kernels with typed inputs | `CompiledPhysicalDag` |

These are the proposed naming conventions. This documentation revision does
not rename Rust types or APIs. Code signatures retain their current names.
`PostAsapDag` is the exported representation of `PostASAPDAG`, not a fourth
planning layer. `PhysicalCandidate` packages the precompute and query cuts
of a `PhysicalDAG` with their typed input contracts.

### Responsibilities

| Layer | Owns | Does not own |
|---|---|---|
| 0. Frontends | Language semantics and lowering into `PreASAPDAG`. A construct that cannot be represented faithfully is rejected, never ignored (for example PromQL `fill`). | Summaries, placement |
| 1. Logical candidate space (`PlanSpace`) | `PlanSpace` represents candidate `PostASAPDAG`s: summary families, exact rewrites, compositions, and series-identity typing. | Placement |
| 2. Summary maintenance lifecycle | For each unique summary state and maintained population, the lifecycle choices (`Ephemeral`, `Prepared`, `Shared`, `ContinuouslyMaintained`) and their costs under a caller-supplied cost model. A chosen assignment sets every node's execution timing, window framework and retention. | The cost values themselves |
| 3. Physical compilation | All computation: value operations, aggregation, PromQL functions and subqueries, vector matching, comparisons and set operators, `histogram_quantile`, summary build, merge and estimate, sort, limit, joins. Compiles a timed `PostASAPDAG` into a `PhysicalDAG`, then cuts it by timing into a `PhysicalCandidate`. | Raw ingestion, pane construction, storage formats, decoding persisted state, scheduling |
| 4. Deployment selection | Prices lifecycle assignments and, through its cost model, logical candidates. Shared state is counted once. It binds the chosen plan. | Re-lowering computation |
| 5. Deployment execution | Ingestion and routing, pane assignment and completeness, lateness and revisions, storage and codecs over Planner kernel states, reading stored state into typed inputs, query-time raw sources, the exact-engine fallback. | Any computation algorithm |

The deployment may call Planner's logical and lifecycle APIs. The rule is only
that every computation runs as a Planner-compiled physical DAG.

## The boundary

The deployment receives, per selected candidate:

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

## Timing and placement

Timing comes only from the lifecycle layer. Logical strategies propose
computation candidates, not execution timing or placement. The rules for
applying an assignment are:

* a retained state (`ContinuouslyMaintained`, `Shared`, `Prepared`) and every
  node feeding it run at ingestion time;
* readouts, other consumers and `Ephemeral` states run at query time;
* an `Ephemeral` state that feeds a retained state runs at ingestion time,
  because query-time work may not feed ingestion-time work.

A chosen lifecycle assignment produces a timed `PostASAPDAG`. Physical
compilation calls `compile` once to produce a `PhysicalDAG`, then
`cut_candidate(&compiled, &frontier_from_timing(&dag)?)` to form the
`PhysicalCandidate`. The timing frontier contains the ingestion-time nodes read
by query-time nodes, plus an ingestion-time root. An ingestion-time `Binary` is
the one exception. It lowers differently from a query-time one, so its timing
must match at compile time. Assignments that change this lowering need a
matching compilation; other timing cuts can reuse the compiled graph.

## Cost and selection

The deployment decides both placement and the summary family.

* **Placement.** For each assignment, the deployment prices every state's
  lifecycle with its own costs, then chooses the cheapest admissible
  assignment for the whole workload:
  * build cost;
  * maintenance per update (precompute CPU);
  * reads;
  * retention: summary store cost, meaning state bytes × retained panes of
    the installed window layout × cardinality × store price;
  * retirement;
  * for `Ephemeral`, processing of the raw samples read at query time.
* **Sharing.** A state shared by several queries is priced once, with all
  consumers' demand. This holds only when compilation would install one shared
  output: same window layout, evaluation interval and phase.
* **Family.** `PlanSpace` lists every family's candidates. The deployment's cost
  model, passed to Planner's global selection, chooses among them. Measured
  whole-plan quotes then compare the remaining workload candidates.
* Unknown cost stays unknown. It never becomes zero, and an alternative that
  cannot be priced is not selected.

## Query-time raw data and mixed placement

An `Ephemeral` state is built at query time, so the deployment must provide raw
data as a query-time source (for example Prometheus raw series). If it cannot,
that alternative is not offered.

A single query may mix `Ephemeral` and stored inputs, with bounded staleness:

* raw inputs are read at the query evaluation time `t_q`;
* each stored input uses its latest complete revision, with watermark `t_s`;
* the mix is admitted only if `t_q − t_s ≤ max_lag` for every stored input;
  `max_lag` is configurable and defaults to one slide of that output;
* the observed lag is reported with the result;
* beyond the bound, the query takes the exact fallback. A silently stale mix is
  never returned.

## Failure and compatibility

* **Fail closed.** A query or subtree that Planner cannot compile is answered
  whole by the deployment's exact engine. Nothing is approximated, ignored or
  computed by deployment-owned operators.
* **Development-stage compatibility.** Persisted plans and stored formats carry
  versions. A format change bumps the version and rejects old data with a clear
  error. Old data is not migrated and is never misread.

## Example: store cost changes placement

The following uses `sum by (job) (rate(m[1m]))`, evaluated every 10 s.

`PlanSpace` returns one logical candidate: a per-series Rate state feeding a
grouped Sum state. It no longer returns two placement variants. The lifecycle
layer lists choices for both states. The deployment prices them:

* When summary storage is cheap, both states are retained. The precompute DAG
  builds Rate and Sum per pane, and the query DAG only reads the Sum.
* When storage is expensive enough, Sum becomes `Ephemeral`. Precompute keeps
  only Rate, and the query DAG builds Sum at query time.
* When storage is more expensive still, both states become `Ephemeral`. The
  query DAG reads raw series at `t_q`.

All three are cuts of one compilation. The deployment's decision is only which
lifecycle assignment to buy.

## Implementation status

Planner (open, stacked on the #462 split #473 → #474 → #475):

* Lifecycle candidates and explicit choice: #476.
* Timing from lifecycle: #482.
* Timing cuts from one compilation: #479.
* Placement only through lifecycle: #485.
* Maintained-population lifecycles: #491.
* Pane construction out of the physical layer: #483.
* Current-series heap alternatives: #477.
* Physical-compile coverage: #484, #486, #487, #488, #489, #490, #492, #493.
  The remaining gaps are listed in
  `docs/develop_docs/physical-compile-coverage.md`, which #484 adds.
* Rejecting PromQL `fill`: #494.
* Kernel codec accessors and edge sampling: #495.
* Timing in the flattening proposal: #481.

ASAPQuery-backend (open, stacked): query-time raw source (#792), repin and
storage ownership (#793), lifecycle placement (#794, #804), PlanSpace candidates
(#795), Planner precompute DAGs (#796, #797, #800), Planner query DAGs (#798,
#799, #801), grouped TopK heaps (#802), bounded-lag mixed inputs (#803), codecs
over Planner kernels (#805), deployment-only materializations (#806).

Open items:

* Per-state mixed placement in the deployment's selection. Today the
  deployment decides states of one query together, in #804.
* Typing PromQL candidates with series identity up front, instead of the
  reselection in #801.
* The remaining physical-compile coverage gaps.
* Storing `sample_p` so that sampled edge frames are accepted again.
