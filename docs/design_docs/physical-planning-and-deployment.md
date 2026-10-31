# Physical Planning, Summary Maintenance, and Deployment

## 1. Architecture

A Post-ASAP computation is progressively realized through four layers:

```mermaid
flowchart LR
    L["Logical Post-ASAP DAG<br/><b>What computation?</b>"]
    M["Materialization<br/><b>How is state maintained?</b>"]
    P["Physical DAG(s)<br/><b>How is it executed?</b>"]
    D["Deployment<br/><b>How is it instantiated?</b>"]

    L -->|"Stage 2<br/>Materialization (#509)"| M
    M -->|"Physical Plan<br/>Compiler"| P
    P -->|"Deployment<br/>interface"| D
```

| Layer | Defines |
| --- | --- |
| **Logical Post-ASAP DAG** | Computation semantics |
| **Materialization** | Build, retention, reuse, and window strategy |
| **Physical DAG(s)** | Supported physical candidates, executable operators and typed input boundaries |
| **Deployment** | Selected candidate, concrete data/state bindings and operational lifecycle |

ASAPPlanner owns the first three layers and the shared physical operator
implementation library. Deployment systems such as ASAPQuery and asap-fusion
own deployment compilation and operation. Materialization is a planning contract
associated with the logical DAG, not a separate computation IR.

> **Status:** Stage 2 (physical ASAP-aware optimization) decides, per summary
> build, whether it is not materialized, materialized at ingestion time, or
> materialized at query time (shared within one evaluation, or kept across
> evaluations), and Stage 3 selects among the results. Sections 2 and 3
> describe the intended contract, which is wider than what Stage 2 offers
> today.

The Logical Post-ASAP DAG is preceded by the Pre-ASAP DAG, the
language-independent query semantics before summary selection. Both are
logical `OperatorNode` DAGs. `compile_physical_asap_dag` exports the selected
DAG as a `PhysicalASAPDAG`, which is the Physical Plan Compiler's input. Its per-node execution phase (ingestion or query time) is
decided by a `MaterializationAssignment`, as the layer contract below states.

### Layer contract

1. **Logical Post-ASAP** (`CandidateLogicalASAPDAGs`) decides what to compute: summary
   families, readouts and sharing. It does not decide placement; timing that a
   realization strategy writes while building a candidate is provisional.
2. **Materialization** (Stage 2, #509) chooses `NotMaterialized`,
   `MatAtIngestion(medium)`, `MatAtQuery(medium)` or
   `MatAtQueryKeptAcrossEvaluations(medium)` for each sub-DAG whose
   output may be stored (a `SummaryAgg` or a `MaintainPopulation`) and records
   it in a `MaterializationAssignment`. `apply_materialization_timings` writes
   every node's `ExecutionTiming` and `Materialization`: a `MatAtIngestion`
   state and all of its inputs run at ingestion time; readouts, other
   consumers, and the other states run at query time. The default assignment
   is `NotMaterialized` everywhere; `PlanOutput::physical_asap_dag` applies it
   only to nodes the optimization pass left untimed (a pass's own timing, such
   as Stage 3's ingestion-time panes, is kept).
3. **Physical compile** (executor) reads the plan's materialized views: each
   view's refresh is one compiled DAG (the precompute DAG) and the query run
   another, joined by the views. It does not see raw ingestion, panes,
   storage or stored-state readout.
4. **Backend** executes the compiled DAGs, serving raw data and stored views
   through its `DataStore`. A query-time summary requires the deployment to
   serve the state's raw input to the query run.

### Candidate generation and deployment selection

Planner exposes the supported, semantically legal **physical plan candidates**.
It does not discard a computation family or materialization placement merely
because a deployment-independent cost estimate prefers another candidate.
Logical candidates are an internal search stage, not the deployment handoff.

```text
Query semantics + accuracy and freshness requirements
                    ↓ Planner
Supported Physical DAG candidates + typed inputs/outputs + requirements
                    ↓ backend
Feasibility + runtime statistics + resource limits + ERP
                    ↓ backend deployment
Selected plan: view refresh DAGs + query DAGs + MaterializedViews
```

Planner owns operators, dependencies, sharing, and each candidate's
materialization frontier. The backend rejects candidates it cannot realize and
costs feasible candidates over a comparable workload and time horizon. It runs
the selected candidate; it does not lower the logical computation again, exchange
operators, or move an operator across the selected frontier. A missing quote is
not a zero-cost implementation. ERP evidence cannot authorize an illegal rewrite.

The candidate inventory must identify its supported search scope and budget.
If a configured exhaustive enumeration exceeds its budget, planning fails
explicitly instead of selecting from an undisclosed partial inventory. Reports
separate unsupported compilation, deployment infeasibility, missing evidence,
and a feasible candidate that loses on cost. Absence is not a cost comparison.

For `sum by(job)(rate(m[1m]))`, Rate remains per series before grouped Sum.
`CandidateLogicalASAPDAGs` offers one such candidate, with a per-series Rate state and a grouped
Sum state. Its materialization assignment places it: an ingestion-time Sum state
finalizes Rate and builds Sum within a bounded precompute run; a query-time Sum
over an ingestion-time Rate state leaves the Rate readout and Sum in the query DAG. Storing a
value requires its exact evaluation window, revision, readiness and serving
cadence to match the query contract.

For instant-vector TopK, CMS/CountSketch with a candidate heap requires explicit
series identity and a supported latest-value input protocol. Appending historical
sample values does not preserve instant-vector semantics. Replacement, rank
decrease, expiry, grouping and the required approximation guarantee must be
validated before admitting that physical candidate.

Planner's candidate space decides what to compute, not placement. For an
instant-vector PromQL TopK, Planner resolves rows that carry the complete series
identity and lists the current-series heap realizations per root with the other
candidates, unranked. Precompute or query-time placement of Rate and grouped Sum
is not a separate Planner candidate: the materialization assignment sets
each node's timing, and the physical compiler reads it.

This is the target ownership contract. A backend path that still reconstructs
operators from logical candidates has not completed this integration.

### Input semantics and summary semantics

`source`, `filter`, `grouping` and `window` describe input-data semantics but not
a complete summary computation: the same four fields can summarize different
value expressions or produce different states. The semantic information a summary
depends on, and where the IR records each part (field type, producing operator,
or coverage), is specified in
[Schema and physical data for ASAP primitives](proposals/asap-primitive-schema.md#4-proposed-node-field-design).

The canonical selected computation is authoritative. Those categories describe
what must be preserved, not a new flat IR or a second expression language.
Operator-defined behavior should be referenced through its canonical contract,
not independently configured in deployment metadata. Unsupported or unresolved
semantics cannot be treated as compatible.

Logical planning defines the semantics; physical compilation realizes them as
operators and input slots. Deployment serves, through its `DataStore`, raw
data and stored states that satisfy those requirements. A stored summary definition records or
references the relevant semantics for compatibility checks. Matching a definition
alone does not establish actual window coverage, revision compatibility or
readiness; those require runtime checks. Physical location, encoding, scheduling
and retention are separate execution/deployment contracts.

Persisted semantic identity, its wire format and any tenant or dataset binding
belong to the deployment. Planner provides the typed `PhysicalASAPDAG` that a
deployment canonicalizes; it does not define a stored-definition format.

### Running example

Suppose p50 and p99 are requested over the same latency samples in a five-minute window,
and one Planner candidate uses KLL with `k=200`. Assume query windows align with one-minute
pane boundaries and that the selected parameters satisfy the required guarantees.
Operator names below are illustrative; the example defines the design, not a
claim that the entire deployment integration is implemented.

The data source identifies where samples come from. Filters, grouping and the
window determine which samples enter each summary. Here `pane_duration: 1m`
means each stored pane covers one minute; the query range is five minutes.
Neither duration specifies how often maintenance runs or how long state is kept.

The example evolves through the architecture as follows:

```text
1. Logical Post-ASAP DAG

raw latency
     ↓
KLLBuild(k=200)
     ↓
KLLMerge
   ┌─┴─────┐
   ↓       ↓
  p50     p99

          │
          │ Stage 2 Materialization (#509)
          ▼

2. Materialization

KLLBuild(k=200)
  strategy = continuously maintain
  window   = 1-minute panes
  reuse    = p50 + p99
  query    = merge panes covering requested aligned 5 minutes

          │
          │ Physical Plan Compiler
          ▼

3. Physical DAGs

Precompute DAG (the pane view's refresh):
RawDataSlot<latency> → TimeRange(1m)
        ↓
NativeKllBuild(k=200)
        ↓
KllStateOutput

Query DAG:
MaterializedViewSlot<KllState>[5 panes]
        ↓
NativeKllMerge(k=200)
      ┌─┴────────┐
      ↓          ↓
 NativeP50   NativeP99

          │
          │ Deployment interface
          ▼

4. Deployment

Precompute:
OTLP latency source
        ↓
run KLL build over each complete 1-minute input pane
        ↓
store as latency-kll-1m/<window>

Query:
read_materialized_view resolves five latency-kll-1m states
        ↓
execute query Physical DAG
        ↓
return p50 / p99
```

Each stage adds a different class of decision while preserving the preceding
contracts. Here, continuous maintenance means recurring production of pane state;
the bounded build DAG does not itself implement an unbounded streaming window.

## 2. Logical Post-ASAP DAG → Materialization

The **Logical Post-ASAP DAG** defines computation semantics:

```text
Scan(latency)
     ↓
KLLBuild(k=200)
     ↓
KLLMerge
   ┌─┴────────────┐
   ↓              ↓
Quantile(.5)  Quantile(.99)
```

It establishes that KLL with `k=200` is used and that the merge is shared by the
two readouts. It does not determine when KLL states are built or retained.

**Stage 2 Materialization (#509)** will choose when states are built and how
long they are retained, using workload demand, window/freshness requirements
and supported physical implementations. Backend selection uses runtime
feasibility and cost after physical compilation. Retained state includes
maintained populations (for example, the current series of
`topk by(job)(1, m)`), which keep the latest sample per series at ingestion and
leave only the readout at query time.

For the running example, assume it selects:

```text
producer: KLLBuild(k=200)

strategy:
    continuously maintain

window realization:
    1-minute panes

query requirement:
    combine panes covering the requested aligned 5-minute range

reuse:
    one merged state serves p50 and p99
```

This is the running example's **Materialization**.

It specifies how the selected logical summary should be maintained,
but not its concrete operator implementation or storage location.

Physical feasibility may feed back into selection. For example, if the required
pane-based maintenance cannot be implemented, this materialization cannot be
selected. One-minute panes alone also cannot cover an arbitrarily phased query
window; that requires supported boundary handling or a different candidate.

## 3. Materialization → Physical DAG

Operator names in the diagrams of this section and §4 (`NativeKllBuild`,
`NativeKllMerge`, `NativeP50`, `NativeQuantile`, `KllStateOutput`) are
illustrative. The executor implements them as `SummaryAgg` (a KLL build),
`SummaryMerge` and `SummaryEvaluation` (a quantile readout).

The **Physical Plan Compiler** consumes both computation semantics and maintenance
requirements:

```text
Timed Post-ASAP DAG (PhysicalASAPDAG)
+ Materialization
+ physical capabilities
        ↓
Physical Plan Compiler
        ↓
Physical DAG(s)
```

For the running example, materialization creates two execution boundaries.

These two halves are the precompute DAG (a materialized view's refresh) and
the query DAG (which reads the view). *Maintenance* stays materialization's
word (section 2): it covers
how state is built, retained, reused and scheduled. A precompute DAG is the
physical object that maintenance compiles to, so reusing *maintenance* for it
collapses two layers: Stage 2 materialization (#509) owns maintenance, and
`asap_executor::physical_planner` owns the DAGs.

### Precompute Physical DAG

```text
RawDataSlot<latency>(
    read_raw_data(TimeSeries { latency })
)
        ↓
TimeRange(1m)
        ↓
NativeKllBuild(k=200)
        ↓
KllStateOutput(k=200)
```

This DAG refreshes one one-minute pane of the materialized view. Its raw data
slot reads the samples matching the source, filters and group, and its
`TimeRange` keeps those within the pane. The deployment supplies the samples
through its `DataStore` (§4).

### Query Physical DAG

```text
MaterializedViewSlot<KllState> × 5 (pane i, i = 0..4)(
    k = 200,
    read_materialized_view(pane_i, (T - i·1m - 1m, T - i·1m])
)
        ↓
NativeKllMerge(k=200)
      ┌─┴──────────────────┐
      ↓                    ↓
NativeQuantile(.50)   NativeQuantile(.99)
```

The Physical Plan Compiler chooses `NativeKllBuild`, `NativeKllMerge`, and the
physical quantile implementations, validates state compatibility, and preserves
the shared merge. It also resolves expressions, schemas, ordered dependencies
and execution properties.

The resulting Physical DAGs know that compatible KLL states are required, but
do not know where those states are stored.

For example:

```text
MaterializedViewSlot<KllState>
```

is physical, while:

```text
s3://.../latency-kll/12:01
```

is deployment-specific. Placement and scheduling also remain outside the Physical
DAG. If the required behavior cannot be realized, physical compilation fails.

### Materialization decides the compiled DAGs

Materialization frontiers are Planner decisions. A materialized view's refresh
and the query run that reads it are separate compiled DAGs, connected by the
view. For `sum by(job)(rate(m[1m]))`, two materialization choices for the single
logical candidate give:

```text
Plan A (Rate state at ingestion time, Sum at query time):
  refresh: counter samples → per-series Rate state            (the view)
  query:   per-series Rate states → Rate readout → grouped Sum → result

Plan B (Rate and Sum states at ingestion time):
  refresh: counter samples → per-series Rate → grouped Sum state (the view)
  query:   grouped Sum states → Sum readout → result
```

Both preserve reset-aware Rate before Sum. Summing raw counters before Rate is
not equivalent. Stages 2 and 3 choose between such plans; the executor never
moves an operator across a view.

Materialization decides timing; physical compilation reads it. A runtime
compiles each view's refresh with the view as the root and the views it reads
as materialized view slots, and the query run with the views it reaches from
the roots as materialized view slots
(`compile(dag, views, roots)` → `CompiledPhysicalDAG`). Lowering a node does
not depend on which views are slots. The executor cannot cut a DAG anywhere
else: inputs are read only at `Scan`s and at materialized views.

Every `Scan` leaf is a raw data slot, which the deployment's store reads at
run time (§4). A compile check (the one `plan_installable` runs) therefore compiles the
plan without a reader. That check compiles each materialized view's refresh
as well as the query run. A plan the executor cannot refresh a view of fails
the check, whether the view is refreshed at ingestion or on first read, so
`plan_installable` refuses it rather than returning it to install. SQL `now()` is the
query's evaluation time (`Scope::Query`); it has no value in an ingestion run.

A trailing window is a range `TimeRange(w)` over any `TimeShift`s (a tumbling
pane adds them, offsets summing to `s`) over a `Scan`. At evaluation time
`T` it keeps the rows whose time lies in `(T - s - w, T - s]`, which is the
window a PromQL range selector reads and a SQL filter against `now()`. Each
`TimeShift` passes the rows through. The same operator serves a SQL table and
a time series. §4 describes the instant window and stale markers.

## 4. Physical DAG → Deployment

This section defines the interface between the Planner's output, the
executor, and a deployment runtime.

> **Status:** implemented on `main`. Known limit: Stage 3 costs raw data by
> each scan's window lookback. It does not add the readers' range-evaluation
> span. The plan file's raw retention does.

### Layers and ownership

| Layer | Holds | Owner | Analogue |
| --- | --- | --- | --- |
| **Planner** | `InstallablePlan`: the selected physical ASAP DAG (each node's materialization and, for a materialized view, its retention) and the query texts | ASAPPlanner (deployment-agnostic) | Flink's JobGraph |
| **Executor: operator library** | `PhysicalOperator` and the native operators | `asap-executor` | DataFusion's `ExecutionPlan` nodes |
| **Executor: compiled plan** | `compile(dag, views, roots)` → `CompiledPhysicalDAG` with raw data and materialized view slots | `asap-executor` | DataFusion's physical planner, which resolves tables while planning; ours leaves them as slots |
| **Executor: deployment interface** | `DataStore` (`read_raw_data`, `read_materialized_view`), `RawDataRequest`, `MaterializedViewRequest`, `instantiate` | `asap-executor` defines it; the deployment implements it | DataFusion's `TableProvider` and catalog |
| **Deployment** | raw-data storage, the summary store (what a stored state is, where it lives, readiness, revisions), run scheduling, retention, transport | ASAPQuery, asap-fusion | catalog and runtime |

### The plan file

```rust
pub struct InstallablePlan {
    pub wire_version: u32,            // INSTALLABLE_PLAN_WIRE_VERSION; versions the whole file
    pub dag: PhysicalASAPDAG,         // materialized-view and query-time Scan nodes carry retention_ms
    pub query_texts: Vec<String>,     // query i is answered by dag.roots[i]
}
impl InstallablePlan {
    /// Reads `wire_version` before the rest, so an older file is refused by version.
    pub fn from_json(text: &str) -> Result<Self, InstallablePlanError>;
    /// Every materialized view, in dependency order: its node, refresh mode
    /// (at ingestion, or on first read and kept), coverage, inputs and retention.
    pub fn materialized_views(&self) -> Result<Vec<MaterializedView>, InstallablePlanError>;
    /// Per raw-data `Source` read at query time: how far back raw data must be kept.
    pub fn raw_data_retention(&self) -> Result<Vec<RawDataRetention>, InstallablePlanError>;
}
pub struct RawDataRetention {
    pub source: Source,
    pub lookback_ms: Option<u64>,     // None = unbounded
}
```

Both derivations are methods on `PhysicalASAPDAG` in `asap-types`, so a
runtime needs no dependency on the planner crate.

A **materialized view** is a node whose output is kept across evaluations
(`MatAtIngestion` or `MatAtQueryKeptAcrossEvaluations`). A `MatAtQuery` node is
not a view: its output is discarded after one evaluation. Everything a runtime
needs that the DAG determines is derived from the DAG, so the plan file has no
second copy to keep consistent. Stage 3's selection report is returned beside
the plan, not inside it.

**Retention.** Installation writes `retention_ms` on two kinds of node:

- **Materialized view:** how long the view's output must stay available after
  it is built.
- **Query-time `Scan`:** how far back raw data must be kept. This is the
  scan's window lookback (`end_offset + width`, from `scan_window`) plus the
  longest range-evaluation span of the queries reading it. It is never less
  than the retention of a view refreshed on first read from that scan.

`raw_data_retention()` is the per-source maximum over these scans. Example: for
`max_over_time(m[5m])` evaluated over the last hour, raw `m` is kept 65
minutes, because refreshing the pane for the oldest step reads that far back.
A pane chain kept across evaluations counts as all its panes. Reusing a pane is
optional for a runtime, and a restart or an older step rebuilds older panes
from raw data.

### Input slots and compilation

An **input slot** is a position in a compiled DAG where data is read at run
time, together with that position's schema, which is the DAG node's
`output_schema`. There are two kinds:

| Slot | Where | Holds | Read by |
| --- | --- | --- | --- |
| **Raw data slot** | every `Scan` leaf | the `Scan`'s `Source`, schema, compiled predicates, and window (the union of its readers' windows, `scan_window`) | `Source` |
| **Materialized view slot** | each view the DAG reads | the `MaterializedView`, its schema, and the window its state summarizes (`view_window`: the union of the windows of every `TimeRange` below it) | `MaterializedView` |

```rust
pub fn compile(dag: &PhysicalASAPDAG, views: &[MaterializedView], roots: &[NodeId])
    -> Result<CompiledPhysicalDAG, Error>;
```

`compile` opens no readers. The DAG that refreshes a view has the view as a
root; a DAG that reads it has the view in `views`. A runtime compiles each
DAG once, at install time. `CompiledPhysicalDAG` is derived inside the
executor and is not persisted: the plan file is the only handoff.

### DataStore

`DataStore` is the only data interface between the executor and a deployment.
Its two methods say what is read, because the two contracts differ:

```rust
pub trait DataStore {
    fn read_raw_data(&self, source: &Source, request: &RawDataRequest, context: RunContext)
        -> Result<OutputStream<'_, Batch>, Error>;
    fn read_materialized_view(&self, view: &MaterializedView, request: &MaterializedViewRequest, context: RunContext)
        -> Result<OutputStream<'_, Batch>, Error>;
}
pub struct RawDataRequest<'a> {
    pub time_range_ms: Option<(i64, i64)>, // (start, end], from the slot's window and the run's time
    pub schema: &'a SchemaRef,             // the slot's columns; the store returns exactly these
    pub predicates: &'a [ScalarExpr<()>],  // a hint: the executor re-applies every predicate
}
pub struct MaterializedViewRequest<'a> {
    pub time_range_ms: (i64, i64),         // (start, end], the interval the view's state summarizes in this run
    pub schema: &'a SchemaRef,             // group columns and one state column
}
```

| | `read_raw_data` | `read_materialized_view` |
| --- | --- | --- |
| Keyed by | `Source` | `MaterializedView` (its coverage: `definition_id` and selection) |
| Time range | optional, a hint | always given, exact |
| Predicates | a hint | none |
| Executor re-filters | yes | no |
| Charged to the run's memory | yes | no, held by the store |

- A **time series** is a table `(ts, value, *labels)`, as `Source::TimeSeries` declares.
- A **SQL table** is a table with a time index.
- A **materialized view** read returns exactly the states whose intervals tile the requested range. The store never counts an observation twice, never returns another definition's state, and never treats missing state as empty; it returns an error instead.

Only the definition side of the deployment's summary store crosses the
interface (the view's coverage and schema). Instances (their interval, group
values, placement, status) stay inside the store, which resolves a read to
them.

### Instantiation and runs

```rust
impl CompiledPhysicalDAG {
    pub fn instantiate(&self, store: Arc<dyn DataStore>)
        -> Result<PhysicalDAG<'static, Batch, SchemaRef>, Error>;
}
```

A deployment passes one store handle per run. The handle presents one
consistent state to every read of that run: a refresh that publishes or
replaces a state during the run is seen by the next run. How the store keeps
that snapshot is the deployment's choice. `instantiate` constructs the
runnable DAG without lowering again and opens no reader; a missing source or
view, or a batch in another schema, fails the read. Each run executes with
independent per-run state, and its `RunContext` carries the run's scope: an
ingestion window, or a query evaluation time `T`.

### Operators own semantics; the store only reads

- A raw data slot becomes a `Scan` over the store. At run time the `Scan` computes `time_range_ms` from its window and `T`, and passes the request. It then checks each batch against the slot's schema and re-applies the time range and the predicates.
- A materialized view slot becomes a `Scan` that asks for the view's window at `T`, `(T - o - w, T - o]`, and marks the returned states as held by the store.
- A `Scan` read by several windows requests the union of their ranges, and each `TimeRange` filters its own.
- `TimeRange` is a native operator for both sources:
  - a range window `TimeRange(w)` over shifts summing to `s` keeps rows with time in `(T - s - w, T - s]`;
  - an instant window keeps each series' latest row in its lookback, and a series whose latest sample is a Prometheus stale marker has no row;
  - an ordinary `NaN` sample is kept.
- In an ingestion run over `(window_start, window_end]`, each `TimeRange(w)` keeps `(window_end - w, window_end]`. A windowed raw data slot asks for `(window_start, window_end]`, and a materialized view slot for `(window_end - w, window_end]`. A view refreshed at ingestion whose raw windows, or the windows of the views it reads, end at different offsets cannot be built in one run, so installation refuses it.
- A store never implements PromQL or SQL semantics. The compile check compiles these operators, so a plan whose window the executor cannot run is rejected before it is installed.

### Refresh groups

Two views refreshed at ingestion can be built in one ingestion run, with the
run's interval equal to each one's window, exactly when they have the same
**refresh window**: windows of width `W` that end `p` after a multiple of `W`
(`0 ≤ p < W`), relative to the evaluation time. A view whose `view_window` is
`(T - o - W, T - o]` has the refresh window `(W, o mod W)`. All panes of a
pane chain have their newest pane's refresh window. The source, the summary
family and where the state is kept do not enter. A view with an unbounded
window is a group of its own.

A **refresh group** is the set of views refreshed at ingestion that have one
refresh window. A runtime refreshes the views of one refresh group in one
ingestion run:

- It compiles the DAG once per group, with every view of the group as a root
  and, as materialized view slots, only the views of other groups that the
  group reads. A view of the group that another view of the group reads is
  computed in the run, not read from a slot.
- The executor runs a node that several roots share once per run, so a `Scan`
  below several views of the group reads its raw data once per window.
- Views of different refresh windows are refreshed in different runs. With a
  10-second and a 60-second pane chain over one `Scan`, the 10-second chain
  needs five pane ends the 60-second chain does not, so the `Scan` is read
  once for each chain.

The refresh window is derived from the DAG alone (time ranges, shifts and pane
widths), like the views and their retention. Sealing, lateness, threading and
the alignment of the window grid stay the deployment's.

The plan's cost assumes this grouping: a stateless ingestion-time node is
charged once per refresh window of the views above it, not once per view. A
runtime that splits a group (for example, because its sources seal at
different times) computes the same states at a higher cost than priced, the
same status pane reuse has. A runtime that updates states sample by sample
passes each sample once whatever the windows, so it runs cheaper than priced.

### Running example

For the KLL p50/p99 example, with one-minute panes materialized at ingestion
over `Source::TimeSeries { metric: "latency" }`, plus a p99.9 with relative
error answered by a DDSketch chain with one-minute panes over the same `Scan`:

```text
Install (once):
    views     = plan.materialized_views()?          // [KLL and DDSketch pane nodes, refresh = at ingestion, retention]
    // both chains have refresh window (1m, 0): one refresh group
    refresh   = compile(dag, [], [kll_pane_0, dd_pane_0])
                // Scan → TimeRange(1m) → NativeKllBuild(k=200)
                //                      → NativeDDSketchBuild(α=0.01)
    query     = compile(dag, views[kll_pane_0..kll_pane_4], [p50, p99])  // 5 materialized view slots → NativeKllMerge → NativeP50, NativeP99
    (the p99.9 query DAG reads dd_pane_0..dd_pane_4 the same way)

Each complete pane (12:04, 12:05], in one ingestion run:
    refresh.instantiate(store)
        Scan calls read_raw_data(TimeSeries { latency }, time_range_ms = (12:04, 12:05]) once;
        both builds read its rows
    store the KllState and the DDSketchState in the summary store, each under its view's coverage and interval

Query at T = 12:05 over (12:00, 12:05]:
    the installed DAG has the chain expanded: five pane views, pane i under TimeShift(i·1m)
    query.instantiate(store)
        read_materialized_view(pane_0, (12:04, 12:05]) → states of latency-kll-1m, pane ending 12:05
        read_materialized_view(pane_1, (12:03, 12:04]) → states of latency-kll-1m, pane ending 12:04
        …
        read_materialized_view(pane_4, (12:00, 12:01]) → states of latency-kll-1m, pane ending 12:01
    NativeKllMerge executes once for both readouts → p50, p99
```

The deployment matches a view to stored states by its coverage
(`definition_id` and selection). At execution time it checks their actual
coverage, revisions and readiness. A compiled plan cannot establish future
readiness. The deployment does not replace `NativeKllMerge`, choose another
sketch, or maintain different windows: such changes require replanning.

### Design decisions

| Question | Decision | DataFusion, for comparison |
| --- | --- | --- |
| When is the store bound? | At `instantiate`, one handle per run; `compile` sees only slots | `scan` is called while planning and returns an `ExecutionPlan` |
| Is a materialized view read like a table? | No: `read_materialized_view`, keyed by the view, is exact and never re-filtered; raw data is read with `read_raw_data`. One trait holds both, so one handle per run keeps one snapshot | The materialized-views extension (`datafusion-contrib/datafusion-materialized-views`) makes a view a `TableProvider` that also implements `Materialized { query(), rewrite_readiness() }` |
| How is raw data addressed? | By `Source`, passed to `read_raw_data`; the store dispatches, however many `Scan`s read it | By table reference in the catalog |
| How is time passed? | `time_range_ms` in each request, because the evaluation time changes every run and every source has a time index | As an ordinary filter, with `now()` folded to a constant per query |
| Projection? | The request carries the slot's schema; the store returns exactly those columns | Yes |
| Filter pushdown? | A hint only; the executor always re-filters | `supports_filters_pushdown` returns `Unsupported`, `Inexact` or `Exact`, and only `Exact` filters are not re-applied |
| Limit, statistics, constraints, writes? | Not part of the interface | `limit`, `statistics()`, `constraints()`, `insert_into` |
| Where does retention live? | On the materialized-view node of the exported DAG | — |

Mapping the materialized-views extension's terms: its `query()` is the sub-DAG
under the view's node, and `coverage.definition_id` identifies that sub-DAG.
Its readiness is the summary store's status, which the deployment owns. Its
`mv_dependencies` build graph is `MaterializedView.inputs`.

## 5. Responsibility Boundary

The complete example makes the ownership boundary explicit:

| Stage | KLL example decision |
| --- | --- |
| **Logical Post-ASAP DAG** | Use `KLL(k=200)` with shared merge for p50/p99 |
| **Stage 2 Materialization (#509)** | Maintain 1-minute panes and reuse them for aligned five-minute queries |
| **Materialization** | Record pane/window/freshness/reuse requirements and each node's execution timing |
| **Physical Plan Compiler** | Lower to native KLL build, merge, and readout operators |
| **Physical DAG** | Define precompute and query DAGs with typed input/output boundaries |
| **Deployment interface** | Read the `latency` raw data slot with `read_raw_data`, and each pane's materialized view slot with `read_materialized_view`, from one `DataStore` |
| **Deployment** | Schedule one ingestion run per refresh group and pane, store and resolve panes, run queries |

```text
Logical:
    "Use KLL for p50/p99."

Materialization:
    "Maintain reusable 1-minute KLL panes."

Physical:
    "Execute NativeKllBuild and
     NativeKllMerge → {p50, p99}."

Deployment:
    "Read OTLP here, store panes here,
     and serve these five panes for this aligned query."
```

The deployment engine executes the instantiated Physical DAGs through ASAPPlanner's
shared physical operator implementation library, `asap-executor`, and
its DAG runtime. The merge executes once per run for both consumers. Execution
does not introduce additional planning decisions.

The physical layer does not own raw ingestion, storage formats, or decoding
persisted bytes into typed state. It compiles computation over input slots:
windows, summary build, merge (for example KLL merge), sketch estimates and
exact finalization. The deployment schedules each refresh group's runs, stores and
reads stored state, and serves raw data and views to the slots through its
`DataStore`. The plan file is the only persisted handoff; a compiled
physical DAG is derived from it by the executor and is not persisted.

Each maintained pane contributes its input samples once. A replacement snapshot
replaces that pane's state; query merging must not count both the old and new
snapshots as separate inputs.

## 6. Executable acceptance coverage

The tests cover physical candidate execution alongside independent
operator/runtime fixtures:

| Test | Contract exercised |
| --- | --- |
| `kll_pane_execution::five_panes_roundtrip_and_shared_merge_runs_once` | One-minute pane refreshes from raw data → five materialized view slots → shared native merge → p50/p99; counts every sample once, checks adjacent aligned windows and instruments one merge start per run |
| `kll_pane_execution::a_store_keyed_by_definition_and_time_range_answers_the_panes` | A store that finds states only by `(definition_id, time range)` answers the five-pane query: the panes share one definition and request the five ranges that tile the window |
| `kll_pane_execution::panes_reject_parameters_schema_and_missing_binding` | Parameter relabelling, incompatible schemas and missing views fail explicitly |
| `precompute_candidates::grouped_rate_can_be_materialized_before_or_after_grouped_sum` | Stored Rate state and stored Sum state answer like query time; summing counters before Rate does not |
| `sql_to_physical::sql_filter_grouped_sum_executes_and_rebinds` | SQL text → candidate search → physical compilation → shared Scan predicates and grouped summary execution; NULL samples are ignored and fresh bindings produce new results |

Pane construction, pane timestamp checks, stored identity, revisions, readiness
and complete coverage of required input samples are deployment
responsibilities. Real storage and HTTP execution belong to
deployment-repository E2E tests.
