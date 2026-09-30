# Planner and deployment layering

Status: proposal. Main implements layers 0 and 1 and the library lifecycle
helpers; open pull requests implement the rest (see
[Implementation status](#implementation-status)). Audience: designers of
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
│    Parse + lower -> CandidatePreASAPDAGs                         │
│    Reject unsupported constructs, such as PromQL fill.           │
│                      │                                           │
│ Logical planning (what)                                          │
│ 1. Logical Post-ASAP (asap-aware-mapping)                        │
│    WHAT to compute; no placement.                                │
│    Summary families, rewrites, exact candidates                  │
│    -> CandidatePostASAPDAGs                                      │
│                      │                                           │
│ Physical planning (how)                                          │
│ 2. Physical design: summary materialization                      │
│    Per unique summary state / maintained population, choose a    │
│    lifecycle: Ephemeral | Prepared | Shared |                    │
│    ContinuouslyMaintained. Each assignment -> node timing,       │
│    window framework, retention. The only source of timing.       │
│    -> CandidatePostASAPDAGs with materialization annotations     │
│                      │                                           │
│ 3. Physical implementation: compilation                          │
│    Lower each Post-ASAP node to operators; cut by timing         │
│    -> CandidatePhysicalDAGs                                      │
│    Each candidate: { precompute DAG, query DAG, InputContracts } │
│    Operators + kernels implement all computation.                │
└──────────────────────┬───────────────────────────────────────────┘
          CandidatePhysicalDAGs (the boundary)
┌──────────────────────┴──────── Deployment ──────────────┐
│ 4 Selection       price lifecycle assignments; choose    │
│ 5 Execution       ingest, panes, storage, readout, run   │
└─────────────────────────────────────────────────────────┘
```

### Candidate generation by ASAPPlanner

Every Planner layer outputs all legal candidates represented at that stage:

`CandidatePreASAPDAGs → CandidatePostASAPDAGs → CandidatePostASAPDAGs with materialization annotations → CandidatePhysicalDAGs`

No intermediate layer chooses a winning candidate. Frontend lowering can
produce a singleton candidate set for an unambiguous query; it need not invent
alternative parses. Logical planning exposes the supported computation
candidates; lifecycle enumeration exposes their admissible assignments;
physical compilation preserves their supported physical realizations and cuts.
Unsupported semantics and invalid or infeasible candidates are rejected with
reasons, rather than silently discarded by an intermediate cost selection.

“All candidates” means the legal candidate space under the supplied semantics,
accuracy requirements, evidence, and capabilities. It can be represented
compactly or enumerated lazily; it does not require eagerly materializing the
Cartesian product. The deployment selects from this space. Library helpers
that return one selected `PlanOutput` are optional selection APIs, not stages
of this candidate-preserving pipeline.

### DAG names

This design distinguishes individual DAGs from the collections of candidates
passed between layers. The design names are the target API; main still uses
the earlier types, where one exists.

| Design name | Meaning | Current main | Target API (implemented by open PRs #508, #480) |
|---|---|---|---|
| `PreASAPDAG` | Frontend-lowered query semantics before summary rewrites | `Rc<QueryExpr>` | `PreASAPDAG` |
| `CandidatePreASAPDAGs` | Frontend candidates, keyed by workload entry; deterministic frontends produce one per entry | None; one `QueryExpr` root per entry | `CandidatePreASAPDAGs` |
| `PostASAPDAG` | One shared logical computation graph | `Rc<SummaryNode>` tree; exported as `PostAsapDag` | `PostASAPDAG` |
| `CandidatePostASAPDAGs` | All legal logical candidates for the workload | `PlanSpace` | `CandidatePostASAPDAGs` |
| `CandidatePostASAPDAGs` with materialization annotations | The same Post-ASAP candidates, annotated with each admissible lifecycle assignment's timing, window framework and retention; still Post-ASAP DAGs, not physical ones | None; lifecycle helpers return one selected `SummaryMaintenanceLifecyclePlan` | `CandidatePostASAPDAGs` with timing |
| `PhysicalDAG` | Compiled operators and kernels with typed inputs, before runtime sources are bound | None | `PhysicalDAG` |
| `CandidatePhysicalDAGs` | Physical candidates with their timing cuts, metadata and diagnostics, before deployment selection | None | `CandidatePhysicalDAGs` |

A candidate collection shares graphs across its candidates; it is not a copy
of every complete DAG. Timing is attached to the shared logical graph, not
stored in a second graph representation. Compatible timing assignments share
one physical compilation, and each candidate's precompute and query DAGs are
cut from it on demand. The exported `PostAsapDag` form is an explicit
export/import format, not an additional planning layer (see
[Post-ASAP IR](../concepts/post-asap-ir.md#tree-and-exported-dag-forms)).

There are only two kinds of DAG after the frontend: Post-ASAP DAGs, whose
nodes are logical operations (optionally carrying materialization annotations),
and physical DAGs, whose nodes are selected physical operators.

Binding runtime sources is an execution step of a `PhysicalDAG`, not another
DAG. At execution the deployment supplies a source for each typed input slot,
the slots are checked against their contracts, and the graph runs; the bound
instance lives only for that execution and is not persisted or compared.

### Physical planning

Physical planning has two parts of different character, as in databases.

**Physical design (summary materialization).** In this document,
*materialization* means summary state kept across executions, like a
materialized view: whether a state is maintained, how it is refreshed and how
long it is retained. This is a workload-level decision, like a database's
materialized-view selection: it spans queries (shared state is kept once), it
depends on workload demand (read and update rates, horizon), and its result
persists. The Planner enumerates the admissible choices; the deployment prices
and chooses. The choice is recorded as annotations on the Post-ASAP nodes
(timing, window framework, retention), so the annotated graph is still a
Post-ASAP DAG, much as physical properties annotate logical expressions in a
database optimizer. Operator materialization (blocking operators such as sort,
aggregation or summary build) and computing a shared subexpression once are
details of compilation and the runtime, not separate layers.

**Physical implementation (compilation).** Compilation lowers each Post-ASAP
node to physical operators and cuts the graph by timing into a precompute and a
query DAG. Materialization is *decided* by physical design and *realized* here:
the precompute DAG's outputs at the cut are the materialized states, and the
query DAG reads them through typed input slots. A physical DAG corresponds to
its Post-ASAP DAG node by node. A node may expand into several operators (for
example TopK into sort then limit, or a multi-input merge into union then
merge); helper operators are numbered from their source node, so every operator
traces back to one Post-ASAP node. One exception is a `Fallback` node, which
wraps a whole Pre-ASAP expression and compiles to many operators; the
operator-flattening proposal ([operator sharing](operator-sharing.md), from
#469) removes it by making non-ASAP operators ordinary Post-ASAP nodes, and
#481 revises its export to emit one Post-ASAP node per non-ASAP operator. If a node gains alternative physical implementations, they become further candidates in
`CandidatePhysicalDAGs`, priced and chosen by the deployment.

### Responsibilities

| Layer | Owns | Does not own |
|---|---|---|
| 0. Frontends | Language semantics and lowering into `CandidatePreASAPDAGs`. A construct that cannot be represented faithfully is rejected, never ignored (for example PromQL `fill`). | Summaries, placement |
| 1. Logical Post-ASAP | `CandidatePostASAPDAGs`: all legal logical candidates, including summary families, exact rewrites, compositions, and series-identity typing. | Placement |
| 2. Physical design: summary materialization | For each unique summary state and maintained population, the lifecycle choices (`Ephemeral`, `Prepared`, `Shared`, `ContinuouslyMaintained`) and their costs under a caller-supplied cost model. Each assignment sets every node's execution timing, window framework and retention, producing `CandidatePostASAPDAGs` with materialization annotations. | The cost values themselves; operator implementation |
| 3. Physical implementation: compilation | All computation: value operations, aggregation, PromQL functions and subqueries, vector matching, comparisons and set operators, `histogram_quantile`, summary build, merge and estimate, sort, limit, joins. Lowers each node of `CandidatePostASAPDAGs` with materialization annotations to operators and cuts by timing into `CandidatePhysicalDAGs`; it does not select a winner. | Raw ingestion, pane construction, storage formats, decoding persisted state, scheduling |
| 4. Deployment selection | Prices lifecycle assignments and, through its cost model, logical candidates. Shared state is counted once. It binds the chosen plan. | Re-lowering computation |
| 5. Deployment execution | Ingestion and routing, pane assignment and completeness, lateness and revisions, storage and codecs over Planner kernel states, reading stored state into typed inputs, query-time raw sources, the exact-engine fallback. Sampled or delta edge frames are rejected. | Any computation algorithm |

The deployment may call Planner's logical and lifecycle APIs. The rule is only
that every computation runs as a Planner-compiled physical DAG.

## The boundary

The deployment receives `CandidatePhysicalDAGs`, containing all supported
physical candidates, for selection. Each candidate contains:

* a **precompute DAG**, whose inputs are raw-sample contracts (rows carrying
  series labels, timestamp and value; the label set is the complete series
  identity) and whose outputs are typed summary states;
* a **query DAG**, whose inputs are stored-state contracts, query-time
  raw-series contracts, or both;
* the lifecycle, window framework and retention of every stored output.

After selection, the deployment binds each input contract, stores each precompute output under
its own storage identity, and returns the query DAG's result. Semantic identity
of stored outputs is defined by the logical DAG they compute. Storage identity
and encoding belong to the deployment.

## Timing and placement

Timing comes only from physical design, that is, from the chosen lifecycle assignment. Logical strategies propose
computation candidates, not execution timing or placement. The rules for
applying an assignment are:

* a retained state (`ContinuouslyMaintained`, `Shared`, `Prepared`) and every
  node feeding it run at ingestion time;
* readouts, other consumers and `Ephemeral` states run at query time;
* an `Ephemeral` state that feeds a retained state runs at ingestion time,
  because query-time work may not feed ingestion-time work.

Physical design produces `CandidatePostASAPDAGs` with materialization
annotations, containing one candidate for each admissible lifecycle assignment
of a logical candidate. These are Post-ASAP graphs with execution timing
assigned, not a new DAG representation or a single selected plan. The
deployment prices the assignments and selects among the candidates. The timing
frontier contains the ingestion-time nodes read by query-time nodes, plus an
ingestion-time root.

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
* **Family.** Planner does not prune families: every summary-family candidate
  (for example KLL and DDSketch for one quantile) reaches the deployment. The
  deployment compares their physical candidates with its whole-plan quotes,
  alongside lifecycle and placement choices. Planner's global-selection helpers
  select only when a caller explicitly asks; no Planner layer calls them as an
  intermediate stage. Known gap: ASAPQuery-backend #795 currently calls
  `global_selection` before pricing and so pre-selects families; this is being
  changed.
* Unknown cost stays unknown. It never becomes zero, and an alternative that
  cannot be priced is not selected.

## Example: store cost changes placement

The following uses `sum by (job) (rate(m[1m]))`, evaluated every 10 s.

Consider one logical candidate: a per-series Rate state feeding a
grouped Sum state. Logical planning does not return placement variants. The
lifecycle layer lists choices for both states. The deployment prices them:

* When summary storage is cheap, both states are retained. The precompute DAG
  builds Rate and Sum per pane, and the query DAG only reads the Sum.
* When storage is expensive enough, Sum becomes `Ephemeral`. Precompute keeps
  only Rate, and the query DAG builds Sum at query time.
* When storage is more expensive still, both states become `Ephemeral`. The
  query DAG reads raw series at `t_q`.

All three are cuts of one compilation. The deployment's decision is only which
lifecycle assignment to buy.

## Implementation status

Main implements frontends, logical candidates (`PlanSpace`) and the library
lifecycle and selection helpers. The remaining layers are in open, stacked pull
requests.

* **Planner.** Split of #462: #473 → #474 → #475. Lifecycle candidates and
  timing: #476, #482, #479, #485, #491. Physical layer: #483, #477. Physical
  compile coverage: #484, #486–#490, #492–#495, #500–#507; the remaining gaps
  are listed in physical-compile-coverage.md, added by #484. Target DAG names:
  #508, #480.
* **ASAPQuery-backend.** #788 through #812: query-time raw sources, lifecycle
  placement, Planner precompute and query DAGs, and codecs over Planner
  kernels.

Open items:

* Family pruning in the backend: #795 pre-selects families through
  `global_selection` instead of pricing every family candidate.
* Removing edge sampling (`sample_p`) and delta-frame ingestion; the data plane
  does not ingest OTLP.
* The remaining physical-compile coverage gaps.
