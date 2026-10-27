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
   `MatAtIngestion(medium)` or `MatAtQuery(medium)` for each sub-DAG whose
   output may be stored (a `SummaryAgg` or a `MaintainPopulation`) and records
   it in a `MaterializationAssignment`. `apply_materialization_timings` writes
   every node's `ExecutionTiming` and `Materialization`: a `MatAtIngestion`
   state and all of its inputs run at ingestion time; readouts, other
   consumers, and the other states run at query time. The default assignment
   is `NotMaterialized` everywhere, which `PlanOutput::physical_asap_dag`
   applies.
3. **Physical compile** (Planner) reads timing: ingestion-time nodes form the
   precompute DAG and the rest form the query DAG, joined by typed outputs. It
   does not see raw ingestion, panes, storage or stored-state readout.
4. **Backend** binds and executes the timed DAG. A query-time summary requires
   the deployment to supply the state's raw input as a query-time source.

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
Binding feasibility + runtime statistics + resource limits + ERP
                    ↓ backend deployment compiler
Selected PrecomputePlan + QueryPlan + StoredOutputReferences
```

Planner owns operators, dependencies, sharing, and each candidate's
materialization frontier. The backend rejects candidates it cannot realize and
prices feasible candidates over a comparable workload and time horizon. It binds
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
operators and typed boundaries. Deployment binds concrete readers and state
records that satisfy those requirements. A stored summary definition records or
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

Precompute DAG:
TableSlot<latency> → TimeRange(1m)
        ↓
NativeKllBuild(k=200)
        ↓
KllStateOutput

Query DAG:
ViewSlot<KllState>[5 panes]
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
resolve five latency-kll-1m states
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

These two halves are named as `PhysicalASAPDAG` names them, `precompute`
and `query`. *Maintenance* stays materialization's word (section 2): it covers
how state is built, retained, reused and scheduled. A precompute DAG is the
physical object that maintenance compiles to, so reusing *maintenance* for it
collapses two layers: Stage 2 materialization (#509) owns maintenance, and
`asap_executor::physical_planner` owns the DAGs.

### Precompute Physical DAG

```text
TableSlot<latency>(
    bounded = true
)
        ↓
TimeRange(1m)
        ↓
NativeKllBuild(k=200)
        ↓
KllStateOutput(k=200)
```

This DAG refreshes one one-minute pane of the materialized view. Its table
slot reads the samples matching the source, filters and group, and its
`TimeRange` keeps those within the pane. The deployment supplies the samples
through a `TableProvider` (§4).

### Query Physical DAG

```text
ViewSlot<KllState>(
    k = 200,
    coverage = requested aligned 5m
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
ViewSlot<KllState>
```

is physical, while:

```text
s3://.../latency-kll/12:01
```

is deployment-specific. Placement and scheduling also remain outside the Physical
DAG. If the required behavior cannot be realized, physical compilation fails.

### Physical candidates include precompute computation

Materialization frontiers are Planner decisions. A candidate records both the
precompute Physical DAG and the query Physical DAG, with typed outputs connecting
them. The deployment compiler binds those outputs; it does not move operators.
Execution timing gives the frontier: ingestion-time nodes read by query-time
nodes. For `sum by(job)(rate(m[1m]))`, two materialization choices for the single
logical candidate give:

```text
Candidate A (Rate state at ingestion time, Sum at query time):
  precompute: counter samples → per-series Rate state
  materialized output: per-series Rate states for window/evaluation/revision
  query: stored Rate states → Rate readout → grouped Sum → result

Candidate B (Rate and Sum states at ingestion time):
  precompute: counter samples → per-series Rate → grouped Sum state
  materialized output: grouped Sum states for window/evaluation/revision
  query: stored grouped Sum states → Sum readout → result
```

Explicit frontiers passed to `compile_candidates` can also persist per-series
rate values; deriving that frontier from timing inside the physical planner is
not yet implemented.

Both preserve reset-aware Rate before Sum. Summing raw counters before Rate is
not equivalent. The counter-state build may be another precompute DAG; typed
state inputs do not imply that a deployment can construct or bind those states.

The shared library exposes `physical_planner::compile_candidates(...)` to lower
explicit frontier candidates to `PhysicalASAPDAG { precompute, query,
materialized_outputs }`. `select_candidate(...)` accepts deployment feasibility
and scoped complete-workload costs and chooses the lowest-cost feasible
candidate. Costs must describe the same workload and planning horizon; missing
feasibility is rejected before pricing. The optimizer supplies candidate
frontiers and cost evidence, including updates, retention, recurrence and sharing.
`enumerate_frontiers` constructs bounded, reachable antichain frontiers above explicit input boundaries, including query-only and fully precomputed results. It fails explicitly when the candidate budget is exceeded. Maintenance selection must still reject frontiers that violate window, freshness, or reuse requirements; deployment feasibility is checked before pricing.

Materialization decides timing; physical compilation reads it. Lowering a
node does not depend on the frontier, so each query DAG is lowered once and
different materialization assignments are different cuts of that lowering.
`compile(dag, inputs, roots)` yields the complete `CompiledPhysicalDAG`.
`frontier_from_timing(&timed_dag)` reads an assignment's timed DAG (from
`physical_asap_dag`) and returns its frontier: ingestion-time nodes read by
query-time nodes, or an ingestion-time root; a query-time node feeding an
ingestion-time node is rejected. `cut_candidate(&compiled, &frontier)` then
partitions the lowered operators: the frontier's ancestors form the precompute
DAG and the rest form the query DAG. Helper operators are numbered by their
Planner node (`u64::MAX - (node_id << 16) - index`), so a cut is byte-identical to
`compile_candidate` for that frontier. One exception: an ingestion-time
`Binary` lowers differently, so its timing must match at compile time.
`compile_candidate(s)` and `enumerate_frontiers` wrap the same path. Temporal
pane candidates remain a separate lowering.

Physical compilation opens no readers. Bounded precompute outputs become typed
query inputs. Their source, filters, grouping, build window, evaluation time, readiness and
revision contracts must accompany the selected materialization and be checked during
deployment binding. Type compatibility alone does not establish reuse legality.

Every `Scan` leaf is a table slot, which the deployment binds at run time
(§4). A compile check (the one `plan_installable` runs) therefore compiles the
plan without a reader. That check compiles each materialized view's refresh
as well as the query run. A plan the executor cannot refresh a view of fails
the check, whether the view is refreshed at ingestion or on first read, so no
materialization of an uncompilable computation is selected. SQL `now()` is the
query's evaluation time (`Scope::Query`); it has no value in an ingestion run.

A trailing window is a range `TimeRange(w)` over a `Scan`, below any
`TimeShift`s a tumbling pane adds (offsets summing to `s`). At evaluation time
`T` it keeps the rows whose time lies in `(T - s - w, T - s]`, which is the
window a PromQL range selector reads and a SQL filter against `now()`. Each
`TimeShift` passes the rows through. The same operator serves a SQL table and
a time series. §4 describes the instant window and stale markers.

The Planner integration test executes both candidates through the shared runtime
and reverses the selected frontier with two controlled cost fixtures. It also
rejects shadowed/duplicate boundaries and incomparable planning horizons. This
establishes Planner capability; it does not establish that ASAPQuery currently
supports persisting every scalar/result-output frontier.

## 4. Physical DAG → Deployment

This section defines the interface between the Planner's output, the
executor, and a deployment runtime.

> **Status:** this is the target interface. The code on `main` differs in
> four places:
> - raw data is bound through `RawSource` (here `TableProvider`) without a
>   scan request, or through `RawDataSampleSelector` / `TableRowSelector`
>   at the `TimeRange` node;
> - a boundary carries an `InputContract` instead of the node's schema;
> - a materialized view is bound as an arbitrary source operator;
> - `InstallablePlan` also carries fields derived from its DAG
>   (`stored_outputs`, `query_time_raw_windows`) and a `selection` report.

### Layers and ownership

| Layer | Holds | Owner | Analogue |
| --- | --- | --- | --- |
| **Planner** | `InstallablePlan`: the selected physical ASAP DAG (each node's materialization and, for a materialized view, its retention) and the query texts | ASAPPlanner (deployment-agnostic) | Flink's JobGraph |
| **Executor: operator library** | `PhysicalOperator` and the native operators | `asap-executor` | DataFusion's `ExecutionPlan` nodes |
| **Executor: compiled plan** | `compile` → `CompiledPhysicalDAG` with input slots; `frontier_from_timing`, `cut_candidate` | `asap-executor` | DataFusion's physical planner, which resolves tables while planning; ours leaves them as slots |
| **Executor: deployment interface** | `TableProvider`, `Bindings`, `instantiate` | `asap-executor` defines it; the deployment implements it | DataFusion's `TableProvider` and catalog |
| **Deployment** | raw-data storage, the summary store (what a stored state is, where it lives, readiness, revisions), run scheduling, retention, transport | ASAPQuery, asap-fusion | catalog and runtime |

### The plan file

```rust
pub struct InstallablePlan {
    pub wire_version: u32,            // versions the whole plan file
    pub dag: PhysicalASAPDAG,         // a materialized-view node carries its retention_ms
    pub query_texts: Vec<String>,     // query i is answered by dag.roots[i]
}
impl InstallablePlan {
    /// Every materialized view, in dependency order: its node, refresh mode
    /// (at ingestion, or on first read and kept), coverage, inputs and retention.
    pub fn materialized_views(&self) -> Vec<MaterializedView>;
}
```

A **materialized view** is a node whose output is kept across evaluations
(`MatAtIngestion` or `MatAtQueryKeptAcrossEvaluations`). A `MatAtQuery` node is
not a view: its output is discarded after one evaluation. Everything a runtime
needs that the DAG determines is derived from the DAG, so the plan file has no
second copy to keep consistent. Stage 3's selection report is returned beside
the plan, not inside it.

### Input slots and compilation

An **input slot** is a position in a compiled DAG where data is bound at run
time, together with that position's schema, which is the DAG node's
`output_schema`. There are two kinds:

| Slot | Where | Holds | Bound by |
| --- | --- | --- | --- |
| **Table slot** | every `Scan` leaf | the `Scan`'s `Source`, schema, compiled predicates, and window (the widths and offsets of the `TimeRange`/`TimeShift`s above it) | `Source` |
| **View slot** | a materialized view cut out of a query DAG | the view's schema | node id |

```rust
pub fn compile(dag: &PhysicalASAPDAG, views: &BTreeSet<NodeId>, roots: &[NodeId])
    -> Result<CompiledPhysicalDAG, Error>;
```

`compile` opens no readers. The DAG that refreshes a view has the view as a
root; a DAG that reads it has the view in `views`. A runtime compiles each
cut once, at install time. `CompiledPhysicalDAG` is derived inside the
executor and is not persisted: the plan file is the only handoff.

### TableProvider

`TableProvider` is the only data interface between the executor and a deployment:

```rust
pub trait TableProvider {
    fn schema(&self) -> SchemaRef; // contains the slot's columns; may have more
    fn boundedness(&self) -> Boundedness;
    fn scan(&self, request: &ScanRequest, context: RunContext)
        -> Result<OutputStream<'_, Batch>, Error>;
}
pub struct ScanRequest<'a> {
    pub time_range_ms: Option<(i64, i64)>, // (start, end], from the slot's window and the run's time
    pub projection: Option<&'a [usize]>,  // indices into the provider's schema
    pub predicates: &'a [ScalarExpr],     // a hint: the executor re-applies every predicate
}
```

- A **time series** is a table `(ts, value, *labels)`, as `Source::TimeSeries` declares.
- A **SQL table** is a table with a time index.
- A **materialized view** is a table in its state schema. The deployment's summary store implements `TableProvider` for each view, and `time_range_ms` selects the panes a run reads.

`ScanRequest` lets a provider skip rows and columns. A provider that ignores it
is still correct.

### Bindings and instantiation

```rust
pub struct Bindings {
    pub tables: DataSources,                             // Source → provider
    pub views: BTreeMap<NodeId, Arc<dyn TableProvider>>, // view slot → provider
}
impl CompiledPhysicalDAG {
    pub fn instantiate<'a>(&self, bindings: &'a Bindings)
        -> Result<PhysicalDAG<'a, Batch, SchemaRef>, Error>;
}
```

`instantiate` checks that every slot has exactly one binding. It also checks
that each provider's schema contains the slot's columns and that each
provider is bounded. It then constructs the runnable DAG without lowering
again. Each run executes with independent per-run state, and its `RunContext`
carries the run's scope: an ingestion window, or a query evaluation time `T`.

### Operators own semantics; providers only read

- A table slot becomes a `Scan` over its provider. At run time the `Scan` computes `time_range_ms` from its window and `T`, and passes the request. It then projects to the slot's schema and re-applies the time range and the predicates.
- A `Scan` read by several windows requests the union of their ranges, and each `TimeRange` filters its own.
- `TimeRange` is a native operator for both sources:
  - a range window `TimeRange(w)` over shifts summing to `s` keeps rows with time in `(T - s - w, T - s]`;
  - an instant window keeps each series' latest row in its lookback, and a series whose latest sample is a Prometheus stale marker has no row;
  - an ordinary `NaN` sample is kept.
- A provider never implements PromQL or SQL semantics. The compile check compiles these operators, so a plan whose window the executor cannot run is rejected before it is installed.

### Running example

For the KLL p50/p99 example, with one-minute panes materialized at ingestion
over `Source::TimeSeries { metric: "latency" }`:

```text
Install (once):
    views     = plan.materialized_views()        // [pane node, refresh = at ingestion]
    refresh   = compile(dag, {}, [pane])          // Scan → TimeRange(1m) → NativeKllBuild(k=200)
    query     = compile(dag, {pane}, [p50, p99])  // view slot → NativeKllMerge → NativeP50, NativeP99

Each complete pane (12:04, 12:05]:
    refresh.instantiate(&Bindings { tables: { latency → time-series provider } })
        Scan asks the provider for time_range_ms = (12:04, 12:05]
    store the KllState in the summary store under the view's coverage

Query at T = 12:05 over (12:00, 12:05]:
    query.instantiate(&Bindings { views: { pane → summary-store provider } })
        the view slot asks for time_range_ms = (12:00, 12:05]
        ├── latency-kll-1m/(12:00,12:01]
        ├── latency-kll-1m/(12:01,12:02]
        ├── latency-kll-1m/(12:02,12:03]
        ├── latency-kll-1m/(12:03,12:04]
        └── latency-kll-1m/(12:04,12:05]
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
| When is a provider resolved? | At `instantiate`, per run; `compile` sees only slots | `scan` is called while planning and returns an `ExecutionPlan` |
| Is a materialized view bound through `TableProvider`? | Yes: one data interface | The materialized-views extension (`datafusion-contrib/datafusion-materialized-views`) makes a view a `TableProvider` that also implements `Materialized { query(), rewrite_readiness() }` |
| How are table slots bound? | By `Source`, as a catalog: one provider per metric or table, however many `Scan`s read it | By table reference in the catalog |
| How is time passed? | `time_range_ms`, a field of `ScanRequest`, because the evaluation time changes every run and every source has a time index | As an ordinary filter, with `now()` folded to a constant per query |
| Projection? | Yes; a provider's schema contains the slot's columns | Yes |
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
| **Deployment interface** | Bind the `latency` table slot to a time-series `TableProvider`, and the pane view slot to the summary store's `TableProvider` |
| **Deployment** | Schedule pane refreshes, store and resolve panes, run queries |

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
     and bind these five panes for this aligned query."
```

The deployment engine executes the bound Physical DAGs through ASAPPlanner's
shared physical operator implementation library, `asap-executor`, and
its DAG runtime. The merge executes once per run for both consumers. Execution
does not introduce additional planning decisions.

The physical layer does not own raw ingestion, storage formats, or decoding
persisted bytes into typed state. It compiles computation over input slots:
windows, summary build, merge (for example KLL merge), sketch estimates and
exact finalization. The deployment schedules each pane's refresh, stores and
reads stored state, and binds raw data and views to slots through
`TableProvider`. The plan file is the only persisted handoff; a compiled
physical DAG is derived from it by the executor and is not persisted.

Each maintained pane contributes its input samples once. A replacement snapshot
replaces that pane's state; query merging must not count both the old and new
snapshots as separate inputs.

## 6. Executable acceptance coverage

The tests cover physical candidate execution alongside independent
operator/runtime fixtures:

| Test | Contract exercised |
| --- | --- |
| `kll_pane_execution::five_panes_roundtrip_and_shared_merge_runs_once` | Explicit one-minute precompute DAGs → real MessagePack state bytes → five required query inputs → shared native merge → p50/p99; counts every sample once, checks adjacent aligned windows and instruments one merge start per run |
| `kll_pane_execution::restored_panes_reject_corruption_parameters_schema_and_missing_binding` | Corrupt bytes, parameter relabelling, incompatible schemas and absent bindings fail explicitly |
| `precompute_candidates::grouped_rate_can_be_materialized_before_or_after_grouped_sum` | Cost changes select different legal precompute frontiers; both selected candidates execute with the same reset-sensitive result; uncompilable candidates are not priced |
| `sql_to_physical::sql_filter_grouped_sum_executes_and_rebinds` | SQL text → candidate search → physical compilation → shared Scan predicates and grouped summary execution; NULL samples are ignored and fresh bindings produce new results |

Pane construction, pane timestamp checks, stored identity, revisions, readiness
and complete coverage of required input samples are deployment
responsibilities. Real storage and HTTP execution belong to
deployment-repository E2E tests.
