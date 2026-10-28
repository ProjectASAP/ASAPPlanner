# Data store: reading raw data and materialized views

> **Status:** proposal. Today a deployment binds a run's inputs through
> `BindingDataSources { tables: DataSources, materialized_views:
> BTreeMap<NodeId, Arc<dyn TableProvider>> }`. This document proposes one
> trait, `DataStore`, with one method for raw data and one for materialized
> views, each keyed by what the plan names, and the contract a store must
> keep. How a store implements it is the deployment's design (for ASAPQuery,
> `summary-catalog-sds-architecture.md` in ASAPQuery-backend).

**Audience:** designers of the executor ↔ deployment boundary, and backend
developers implementing a store.

## 1. Problem

KLL p50/p99 of `latency` over five 1-minute panes kept as materialized views
(the running example of
[Physical planning and deployment](../physical-planning-and-deployment.md)).
The installed DAG expands the pane chain into five pane nodes, `pane_0` …
`pane_4`. Each is a materialized view, and all five have the same
`coverage.definition_id`. The refresh DAG of each pane reads raw data through
`Scan(Source::TimeSeries { metric: "latency" })`.

Today, before every run, a deployment builds two routing maps:

```rust
let mut tables = DataSources::default();
tables.register(Source::TimeSeries { metric: "latency".into() }, Arc::new(SampleBuffer::new(..)))?;
let mut materialized_views = BTreeMap::new();
for view in plan.materialized_views()? {
    let states = store.states_for(&view.coverage);
    materialized_views.insert(view.node_id, Arc::new(StoreProvider::new(states)) as Arc<dyn TableProvider>);
}
query.instantiate(&BindingDataSources { tables, materialized_views })?;
```

The store's real questions are "read raw data of table `S` for this request"
and "read the stored states of definition D for time range R". The current
shape has four problems:

- **Three names for one thing.** `Source` names the table, `DataSources` is a
  `Source → TableProvider` map, and `TableProvider` reads it. A deployment
  registers one provider per table, although its storage already dispatches
  by name.
- **Node ids leak into the deployment.** The summary store is keyed by
  definition and interval, so node ids are only routing.
- **`TableProvider` does not fit a view.** A view's rows are not re-filtered,
  its `projection` and `predicates` are always empty, and its states are not
  charged to the run's memory. Implementing a table trait invites a provider
  to ignore the time range, which the contract then forbids in prose.
- **Both maps are rebuilt every run,** although they are fixed once a plan is
  installed.

## 2. Components and ownership

```mermaid
flowchart LR
    subgraph planner["ASAPPlanner (offline)"]
        IP["<b>InstallablePlan</b> (plan file)<br/>dag: PhysicalASAPDAG<br/>query_texts<br/>materialized_views() → MaterializedView<br/>raw_data_retention()"]
    end

    subgraph executor["asap-executor (one run at evaluation time T)"]
        subgraph cdag["compiled query DAG"]
            TS["<b>raw data slot</b><br/>at each Scan, names its Source"]
            VS["<b>materialized view slot</b><br/>names its MaterializedView,<br/>computes time_range_ms (start, end]"]
            OPS["PhysicalOperators<br/>re-filter raw rows, merge states"]
        end
        TS -->|"rows"| OPS
        VS -->|"states, held outside the memory limit"| OPS
    end

    subgraph deployment["Deployment (for example ASAPQuery-backend)"]
        DST["<b>DataStore</b><br/>one consistent snapshot per run"]
        RAW[("raw data<br/>tables, time series")]
        CAT["<b>Summary catalog</b> (SDS definitions)<br/>Summary Descriptor, Data Descriptor"]
        INST[("<b>Summary instances</b><br/>interval, group values, completeness,<br/>state_reference, status, revision")]
        REF["<b>refresh scheduler</b><br/>AtIngestion: per pane, ingestion run<br/>OnFirstRead: before the query run"]
        RAW --> DST
        CAT -->|"definition"| DST
        INST -->|"states covering the time range"| DST
        RAW -->|"refresh DAG over raw data"| REF
        REF -->|"publish instance"| INST
    end

    IP -.->|"compile(dag)"| cdag
    IP -.->|"definition_id + selection"| CAT
    IP -.->|"retention_ms, refresh,<br/>raw_data_retention()"| REF
    DST -->|"instantiate(store)"| cdag
    DST -->|"read_raw_data(source, RawDataRequest)"| TS
    DST -->|"read_materialized_view(view, time_range_ms)<br/>group columns + state column,<br/>no instance identity"| VS
```

Arrows point the way data flows, from producer to consumer. An edge label
names the call that moves the data, and the consumer is the caller: the
executor calls `read_raw_data` and `read_materialized_view`, and the store
looks up the catalog and instances. Dashed edges happen once, at install;
solid edges happen in every run.

The interface between the executor and the deployment is one `DataStore`
handle per run. Raw data is read by the `Source` a `Scan` names, and a
materialized view by its `MaterializedView`. Everything right of it (catalog,
instances, refresh) is the deployment's.

| Component | Holds | Repository |
| --- | --- | --- |
| Plan file | `InstallablePlan`: the DAG, query texts, and from them `materialized_views()` and `raw_data_retention()` | ASAPPlanner (`asap-planner`) |
| Plan vocabulary | `Source`, `MaterializedView`, `ViewCoverage`, `RawDataWindow`, `ScanWindow` | ASAPPlanner (`asap-types`) |
| Executor | compiled DAGs, input slots, physical operators, `RunContext` | ASAPPlanner (`asap-executor`) |
| Store interface | the `DataStore` trait and `RawDataRequest` (`sources/data_store.rs`), and their contract (§3) | ASAPPlanner (`asap-executor`) |
| Store implementation | `impl DataStore`: raw data storage, and resolving a view read to stored states | deployment |
| Summary catalog and instances | definitions (descriptors) and instances (interval, group values, completeness, placement, state reference, status, revision) | deployment |
| Refresh, lifecycle, snapshot | refresh scheduling, retention, how a run's handle stays consistent | deployment |

Only definitions cross the interface. Instances never do:

| SDS part | Crosses the interface? | As |
| --- | --- | --- |
| Summary Descriptor (operator, parameters, fidelity, state schema) | yes, as the Planner states it | `MaterializedView.coverage.definition_id` and the slot's schema |
| Data Descriptor (source, population) | yes, as the Planner states it | `coverage.selection`; raw sources in `inputs.raw_data_windows` |
| Summary Instance (interval, group values, completeness, placement, `state_reference`, status, lifecycle) | **no** | resolved inside the store |

The time range in a view read is a predicate the store evaluates against
instance intervals. No instance identity, location or status reaches the
executor or the Planner.

## 3. Interface

```rust
/// A deployment's raw data and stored materialized views, as one run reads
/// them. The deployment passes one handle per run; see "Consistent snapshot".
pub trait DataStore {
    /// Rows of the raw table or time series `source` names. `request` is a
    /// hint: the executor re-applies its time range and predicates.
    fn read_raw_data(
        &self,
        source: &Source,
        request: &RawDataRequest,
        context: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error>;

    /// The stored states of `view`: the instances of `view.coverage` whose
    /// pane ends in `time_range_ms` (`(start, end]`), in the view's schema
    /// (group columns and one state column). Exact: never re-filtered.
    fn read_materialized_view(
        &self,
        view: &MaterializedView,
        time_range_ms: (i64, i64),
        context: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error>;
}

/// What a raw data slot reads in one run (was `ScanRequest`).
pub struct RawDataRequest<'a> {
    /// `(start, end]`: the union of the windows the `Scan`'s readers read
    /// (`PhysicalASAPDAG::scan_window`); `None` when a reader reads outside a
    /// window.
    pub time_range_ms: Option<(i64, i64)>,
    /// The slot's columns, from the `Scan`'s schema. The store returns rows
    /// with these columns, in this order.
    pub schema: &'a SchemaRef,
    /// The `Scan`'s predicates over `schema`; a hint only.
    pub predicates: &'a [ScalarExpr<()>],
}

// CompiledDAG::instantiate(&self, store: Arc<dyn DataStore>) -> Result<Instance, Error>
```

The method names say what is read, raw data or a materialized view, because
the two contracts differ:

| | `read_raw_data` | `read_materialized_view` |
| --- | --- | --- |
| Keyed by | `Source` | `MaterializedView` |
| Time range | optional, a hint | always given, exact |
| Executor re-filters | yes | no |
| Charged to the run's memory | yes | no, held by the store |

- **Removed:** `TableProvider`, `ScanRequest`, `DataSources` and
  `BindingDataSources`. `TableProvider::schema` is replaced by
  `RawDataRequest::schema`: the store returns the slot's columns, and the
  executor checks each batch against them, instead of matching columns by
  name at bind time. `TableProvider::boundedness` was a hint no slot needed.
- **Executor side.** `compile` records, for each materialized view slot, the
  `MaterializedView` it stands for. At run time the slot computes
  `time_range_ms`. For a pane view (one range `TimeRange` under it), that is
  `RawDataWindow::bounds_ms(T)` of its window. For any other view, it is
  `(T - lookback, T]`, where `lookback` is the largest `lookback_ms` over the
  view's raw-data windows and the views it reads. A time range is therefore
  always given; there is no `None`.
- **Contract of `read_materialized_view`.** It returns exactly the instances
  that cover `time_range_ms` (`(start, end]`, matched by pane end) for the
  view's definition and selection. The executor does not
  re-filter them, so the store must:
  - never count an observation twice;
  - never return an instance of another definition;
  - never treat missing state as empty.

  If it cannot prove that, it returns an error. The deployment decides the
  fallback, for example exact execution.
- **Consistent snapshot.** A `DataStore` handle presents one consistent
  state to every `read_raw_data` and `read_materialized_view` of its run. A
  refresh that publishes or replaces an instance during the run is not seen
  by it; the next run sees it. Otherwise, at `T = 12:05`, `pane_1` could be
  read before a late-data correction replaced it and `pane_0` after a new
  instance was published, and the merge would answer for a data set that
  never existed. How the store keeps the snapshot (a revision number, MVCC,
  locking) is the deployment's choice. The executor
  never reads a revision, so `Scope::Ingestion` and `Scope::Query` drop their
  `revision` field, and `RunContext` keeps only what the executor uses:
  scope, limits, cancellation and the query range.
- **A deployment without materialized views** implements
  `read_materialized_view` as an error. `test-support` provides
  `InMemoryDataStore` (batches by `Source` and by view) for tests.

## 4. Example

At `T = 12:05`, the query DAG reads `pane_0` … `pane_4`. Pane `i` sits under
`TimeShift(i·1m)`.

```text
read_materialized_view(pane_0, (12:04, 12:05]) → states of kll-200-latency, pane ending 12:05
read_materialized_view(pane_1, (12:03, 12:04]) → states of kll-200-latency, pane ending 12:04
…
read_materialized_view(pane_4, (12:00, 12:01]) → states of kll-200-latency, pane ending 12:01
NativeKllMerge merges the five states once → p50, p99
```

All five reads name the same definition. Only the time range differs. The
ingestion run that built pane `(12:04, 12:05]` read its input with
`read_raw_data(TimeSeries { metric: "latency" }, (12:04, 12:05])`.

## 5. Alternatives considered

| Alternative | Why not |
| --- | --- |
| Keep `DataSources` and `NodeId → TableProvider` (today) | Three names for raw data, node ids in the deployment, per-run maps, and a table contract that does not fit views (§1) |
| Two traits, `TableStore` and `MaterializedViewStore` | Two handles per run that must share one snapshot. The contracts differ per method, not per object, so one trait with two named methods is enough |
| One method `read(input)` over an enum of raw data and view | Hides which contract applies at the call site; the method names should say what is read |
| Key views by `definition_id` only | Two views can share a definition but differ in value selection, and panes differ in time. The read needs the full `MaterializedView` and a time range |
| Let the executor call SDS directly | Puts storage identity and instance lifecycle in the executor; SDS must stay in the deployment |
| Pass instances in the plan file | Instances exist only at run time; a plan file cannot name them |

## 6. Open points

- **Boundaries that are not views.** `precompute::compile` composes fragments
  whose `Scan` frontiers are bound by node id, and some tests cut at a
  `TimeRange`. Either keep a crate-internal or `test-support` node-id binding
  for them, or give each such boundary a `MaterializedView`.

## 7. Migration

This is a public API change in `asap_executor`:

- `instantiate` takes `Arc<dyn DataStore>` instead of `&BindingDataSources`;
- `TableProvider`, `ScanRequest`, `DataSources` and `BindingDataSources` are
  removed, and `RawDataRequest` replaces `ScanRequest`;
- `Scope` loses `revision`;
- a materialized view slot's request always carries a time range.

ASAPQuery-backend does not depend on `asap_executor` yet. Its integration
implements `DataStore` over its raw data storage and its SummaryStore
directly.
